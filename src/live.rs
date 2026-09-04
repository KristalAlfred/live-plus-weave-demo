use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// A read-only view of open-live. The gateway never writes there: open-live
/// discovers a guest's feed by itself, by polling weave's northbound API, and
/// this client only reports whether that has happened yet.
#[derive(Debug)]
pub struct OpenLive {
    http: reqwest::Client,
    base: String,
    api_key: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Source {
    pub id: String,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub provider: Option<Provider>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Provider {
    pub id: String,
    #[serde(rename = "externalId")]
    pub external_id: String,
}

impl OpenLive {
    pub fn new(http: reqwest::Client, base: String, api_key: Option<String>) -> Self {
        Self {
            http,
            base,
            api_key,
        }
    }

    pub async fn sources(&self) -> Result<Vec<Source>> {
        let mut request = self.http.get(format!("{}/api/v1/sources", self.base));
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }
        let response = request.send().await.context("open-live unreachable")?;
        let status = response.status();
        if !status.is_success() {
            bail!("open-live GET /api/v1/sources answered {status}");
        }
        response
            .json()
            .await
            .context("open-live returned a source list this gateway cannot read")
    }
}

impl Source {
    /// open-live's weave provider sets `externalId` to `<stream>/<index>`, and
    /// the gateway names each stream after its seat.
    pub fn seat(&self) -> Option<&str> {
        let provider = self.provider.as_ref()?;
        if provider.id != "weave" {
            return None;
        }
        provider.external_id.split('/').next()
    }
}
