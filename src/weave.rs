use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use weave_core::{
    API_V1, DataPlaneAddr, DeviceKind, HopStatus, NodeCapabilities, NodeDescriptor, NodeEndpoint,
    NodeHeartbeat, NodeRegistration, NodeStatus, PROTOCOL_VERSION, Reachability, RoleSet,
    Signalling, SocketRole, SrtEndpoint, StreamDefinition, StreamTransport, Transport,
    TransportOffer,
};

/// A response from southbound handed back to the guest's browser as it
/// arrived. The pass-through routes forward bytes rather than re-serializing,
/// so a field this gateway does not know about still reaches the page.
pub struct Proxied {
    pub status: reqwest::StatusCode,
    pub content_type: Option<String>,
    pub body: bytes::Bytes,
}

#[derive(Debug)]
pub struct Weave {
    http: reqwest::Client,
    northbound: String,
    northbound_token: Option<String>,
    southbound: String,
    southbound_token: Option<String>,
}

/// `GET /v1/status` on northbound. Shaped by the controller's rollup rather
/// than by `weave-core`, so it is declared here.
#[derive(Debug, Default, Deserialize)]
pub struct StatusRollup {
    #[serde(default)]
    pub streams: Vec<StreamStatus>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StreamStatus {
    pub name: String,
    pub status: String,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub endpoints: Option<StreamEndpoints>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StreamEndpoints {
    #[serde(default)]
    pub outputs: Vec<Option<EndpointAddr>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EndpointAddr {
    pub node: String,
    pub url: String,
}

impl Weave {
    pub fn new(
        http: reqwest::Client,
        northbound: String,
        northbound_token: Option<String>,
        southbound: String,
        southbound_token: Option<String>,
    ) -> Self {
        Self {
            http,
            northbound,
            northbound_token,
            southbound,
            southbound_token,
        }
    }

    // --- southbound, on the guest's behalf ---

    /// The page reports how its hops are doing; everything else about the node
    /// is decided here. A browser sends its camera over WHIP and nothing else,
    /// so those are the only capabilities claimed, and the seat is pinned so a
    /// guest cannot register as somebody else's node.
    pub async fn register(&self, seat: &str, hop_status: Vec<HopStatus>) -> Result<Proxied> {
        let registration = NodeRegistration {
            protocol_version: PROTOCOL_VERSION,
            node: NodeDescriptor {
                id: seat.to_string(),
                endpoint: format!("browser://{seat}"),
                status: NodeStatus::Ready,
                capabilities: NodeCapabilities {
                    transports: vec![TransportOffer::with_roles(
                        Transport::Whip,
                        RoleSet::only(SocketRole::Connect),
                    )],
                    devices: [DeviceKind::Capture].into_iter().collect(),
                    data_plane: BTreeMap::from([(
                        weave_core::DEFAULT_DATA_PLANE_ALIAS.to_string(),
                        DataPlaneAddr {
                            host: "browser".to_string(),
                            reachability: Reachability::OutboundOnly,
                            signalling: Signalling::default(),
                        },
                    )]),
                    ..NodeCapabilities::default()
                },
            },
            endpoints: Vec::new(),
            hop_status,
        };
        self.southbound_post("/nodes/register", &registration).await
    }

    pub async fn heartbeat(&self, seat: &str, hop_status: Vec<HopStatus>) -> Result<Proxied> {
        let heartbeat = NodeHeartbeat {
            node_id: seat.to_string(),
            status: NodeStatus::Ready,
            endpoints: Vec::new(),
            hop_status,
        };
        self.southbound_post(&format!("/nodes/{seat}/heartbeat"), &heartbeat)
            .await
    }

    pub async fn desired(&self, seat: &str) -> Result<Proxied> {
        let request = self
            .http
            .get(self.southbound_url(&format!("/nodes/{seat}/desired")));
        self.proxied(self.with_southbound_token(request)).await
    }

    pub async fn nodes(&self) -> Result<Vec<NodeDescriptor>> {
        let request = self.http.get(self.southbound_url("/nodes"));
        let response = self
            .with_southbound_token(request)
            .send()
            .await
            .context("weave southbound unreachable")?;
        let status = response.status();
        if !status.is_success() {
            bail!("weave southbound GET /v1/nodes answered {status}");
        }
        response.json().await.context("reading weave's node list")
    }

    // --- northbound, as the operator ---

    /// The guest's camera into the gateway node, as a stream named after the
    /// seat. `network` picks which of the target node's data-plane addresses
    /// the signalling URL is built from, and so decides whether the guest's
    /// browser can reach it at all.
    pub fn stream_for(
        &self,
        seat: &str,
        target_node: &str,
        network: Option<&str>,
        latency_ms: u32,
    ) -> StreamDefinition {
        StreamDefinition {
            name: seat.to_string(),
            enabled: true,
            source: StreamTransport::Device(NodeEndpoint {
                node: seat.to_string(),
                network: None,
            }),
            destinations: vec![StreamTransport::Srt(SrtEndpoint {
                node: Some(target_node.to_string()),
                remote: None,
                via: Vec::new(),
                network: network.map(str::to_string),
                latency: Some(latency_ms),
                format: None,
                accepts: None,
            })],
        }
    }

    pub async fn declare_stream(&self, stream: &StreamDefinition) -> Result<()> {
        let request = self.http.post(self.northbound_url("/streams")).json(stream);
        let response = self
            .with_northbound_token(request)
            .send()
            .await
            .context("weave northbound unreachable")?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!("weave refused stream {}: {status} {}", stream.name, body.trim());
        }
        Ok(())
    }

    /// `true` when the stream was there to delete, `false` when it was already
    /// gone — which is success for a caller reconciling towards absence.
    pub async fn delete_stream(&self, name: &str) -> Result<bool> {
        let request = self.http.delete(self.northbound_url(&format!("/streams/{name}")));
        let response = self
            .with_northbound_token(request)
            .send()
            .await
            .context("weave northbound unreachable")?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(false);
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!("weave refused to delete stream {name}: {status} {}", body.trim());
        }
        Ok(true)
    }

    /// Desired state — every stream weave holds, updated the moment one is
    /// accepted. The status rollup lags it by a controller tick, so asking
    /// that instead makes a fresh stream look absent and get declared again.
    pub async fn streams(&self) -> Result<Vec<StreamDefinition>> {
        let request = self.http.get(self.northbound_url("/streams"));
        let response = self
            .with_northbound_token(request)
            .send()
            .await
            .context("weave northbound unreachable")?;
        let status = response.status();
        if !status.is_success() {
            bail!("weave northbound GET /v1/streams answered {status}");
        }
        response.json().await.context("reading weave's stream list")
    }

    pub async fn status(&self) -> Result<StatusRollup> {
        let request = self.http.get(self.northbound_url("/status"));
        let response = self
            .with_northbound_token(request)
            .send()
            .await
            .context("weave northbound unreachable")?;
        let status = response.status();
        if !status.is_success() {
            bail!("weave northbound GET /v1/status answered {status}");
        }
        response.json().await.context("reading weave's status rollup")
    }

    // --- plumbing ---

    fn northbound_url(&self, path: &str) -> String {
        format!("{}{API_V1}{path}", self.northbound)
    }

    fn southbound_url(&self, path: &str) -> String {
        format!("{}{API_V1}{path}", self.southbound)
    }

    fn with_northbound_token(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.northbound_token {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }

    fn with_southbound_token(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.southbound_token {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }

    async fn southbound_post<T: serde::Serialize>(&self, path: &str, body: &T) -> Result<Proxied> {
        let request = self.http.post(self.southbound_url(path)).json(body);
        self.proxied(self.with_southbound_token(request)).await
    }

    async fn proxied(&self, request: reqwest::RequestBuilder) -> Result<Proxied> {
        let response = request.send().await.context("weave southbound unreachable")?;
        let status = response.status();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let body = response.bytes().await.context("reading weave's response")?;
        Ok(Proxied {
            status,
            content_type,
            body,
        })
    }
}

impl StreamStatus {
    /// Weave leaves this out rather than blank, but a reason that arrived
    /// empty would render as a detail line with nothing in it.
    pub fn reason(&self) -> Option<String> {
        self.reason
            .as_deref()
            .map(str::trim)
            .filter(|reason| !reason.is_empty())
            .map(str::to_string)
    }

    /// The address an SRT consumer dials, which is what open-live picks up.
    pub fn output(&self) -> Option<&EndpointAddr> {
        self.endpoints
            .as_ref()?
            .outputs
            .iter()
            .flatten()
            .find(|output| !output.node.is_empty())
    }
}
