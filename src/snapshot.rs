use std::collections::BTreeMap;

use serde::Serialize;
use weave_core::{HopStatus, LinkCondition, NodeStatus};

use crate::config::Config;
use crate::live::Source;
use crate::seat::Seat;
use crate::weave::StreamStatus;

/// What the last reconcile found, keyed by seat. Everything the pages show is
/// derived from this and the seat store, so one tick produces one consistent
/// view rather than each request asking three services again.
#[derive(Debug, Default)]
pub struct Snapshot {
    pub reconciled_at: String,
    pub nodes: BTreeMap<String, NodeFacts>,
    pub streams: BTreeMap<String, StreamStatus>,
    pub sources: BTreeMap<String, Source>,
    /// The hop status the guest's own page last reported. The gateway proxies
    /// those calls, so it reads them on the way past — no extra weave call
    /// buys the sender's own view of its camera.
    pub reported: BTreeMap<String, Vec<HopStatus>>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct NodeFacts {
    pub status: NodeStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Pending,
    Active,
    Done,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct Step {
    pub step: &'static str,
    pub state: State,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SeatView {
    pub seat: String,
    pub display_name: String,
    pub join_url: String,
    pub created_at: String,
    pub expires_at: String,
    pub stage: &'static str,
    pub chain: Vec<Step>,
}

fn step(step: &'static str, state: State, detail: Option<String>) -> Step {
    Step {
        step,
        state,
        detail,
    }
}

impl SeatView {
    pub fn build(seat: &Seat, snapshot: &Snapshot, cfg: &Config, now: i64) -> Self {
        let node = snapshot.nodes.get(&seat.seat);
        let stream = snapshot.streams.get(&seat.seat);
        let source = snapshot.sources.get(&seat.seat);
        let sender = snapshot
            .reported
            .get(&seat.seat)
            .and_then(|hops| hops.iter().find(|hop| hop.id.ends_with("-sender")));

        let joined = joined_step(seat, node, now);
        let routed = routed_step(stream, sender, cfg);
        let flowing = flowing_step(stream, sender);
        let landed = landed_step(source, cfg);

        let chain = vec![
            step("invited", State::Done, Some(invited_detail(seat, now))),
            joined.clone(),
            routed.clone(),
            flowing.clone(),
            landed.clone(),
        ];

        let stage = if seat.revoked {
            "revoked"
        } else if joined.state == State::Failed {
            "left"
        } else if seat.joined_at.is_none() && seat.expired(now) {
            "expired"
        } else if flowing.state == State::Done {
            "flowing"
        } else if landed.state == State::Done {
            "open-live"
        } else if routed.state == State::Done {
            "routed"
        } else if joined.state == State::Done {
            "joined"
        } else {
            "invited"
        };

        Self {
            seat: seat.seat.clone(),
            display_name: seat.display_name.clone(),
            join_url: cfg.join_url(&seat.token),
            created_at: crate::rfc3339(seat.created_at),
            expires_at: crate::rfc3339(seat.expires_at),
            stage,
            chain,
        }
    }
}

fn invited_detail(seat: &Seat, now: i64) -> String {
    if seat.revoked {
        return "link revoked".to_string();
    }
    if seat.joined_at.is_none() && seat.expired(now) {
        return "link expired unused".to_string();
    }
    "link created".to_string()
}

fn joined_step(seat: &Seat, node: Option<&NodeFacts>, now: i64) -> Step {
    match node.map(|node| node.status) {
        Some(NodeStatus::Ready | NodeStatus::Degraded) => step(
            "joined",
            State::Done,
            Some("browser node registered".to_string()),
        ),
        Some(NodeStatus::Offline) => step(
            "joined",
            State::Failed,
            Some("browser node stopped heartbeating".to_string()),
        ),
        Some(NodeStatus::Unknown) | None => {
            if seat.joined_at.is_some() {
                step(
                    "joined",
                    State::Failed,
                    Some("browser node is gone".to_string()),
                )
            } else if seat.expired(now) {
                step("joined", State::Pending, Some("never opened".to_string()))
            } else {
                step(
                    "joined",
                    State::Pending,
                    Some("waiting for the guest to open the link".to_string()),
                )
            }
        }
    }
}

/// Routed is about the control plane having a plan: a stream declared, placed
/// onto a gateway node, and a hop the page could act on.
fn routed_step(stream: Option<&StreamStatus>, sender: Option<&HopStatus>, cfg: &Config) -> Step {
    let Some(stream) = stream else {
        return step("routed", State::Pending, None);
    };
    let target = &cfg.target_node;
    match stream.status.as_str() {
        "pending" => step(
            "routed",
            State::Active,
            stream
                .reason()
                .or_else(|| Some("weave is placing the stream".to_string())),
        ),
        "failed" => step("routed", State::Failed, stream.reason()),
        _ => {
            if matches!(sender.map(|hop| hop.state), Some(weave_core::HopState::Failed)) {
                return step(
                    "routed",
                    State::Failed,
                    Some("the browser could not open the WHIP session".to_string()),
                );
            }
            let detail = match stream.output() {
                Some(output) => format!("WHIP into {target}, out as {}", output.url),
                None => format!("WHIP into {target}"),
            };
            step("routed", State::Done, Some(detail))
        }
    }
}

fn flowing_step(stream: Option<&StreamStatus>, sender: Option<&HopStatus>) -> Step {
    let rate = sender
        .and_then(|hop| hop.stats.as_ref())
        .map(|stats| stats.egress_rate_mbps)
        .filter(|rate| *rate > 0.0)
        .map(|rate| format!("{rate:.2} Mb/s from the browser"));
    let sending = matches!(
        sender.map(|hop| hop.egress),
        Some(LinkCondition::Flowing | LinkCondition::Connected)
    );

    match stream.map(|stream| stream.status.as_str()) {
        Some("flowing") => step(
            "flowing",
            State::Done,
            rate.or_else(|| Some("media is moving".to_string())),
        ),
        // Degraded covers both "still flowing, but something is wrong" and
        // "the path broke". Whether the browser is sending is what tells them
        // apart, so it decides which one this reads as.
        Some("degraded") if sending => step(
            "flowing",
            State::Active,
            stream.and_then(StreamStatus::reason).or(rate),
        ),
        Some("degraded") => step(
            "flowing",
            State::Failed,
            stream.and_then(StreamStatus::reason),
        ),
        Some("awaiting_input") if sending => step(
            "flowing",
            State::Active,
            Some("browser is sending; the gateway has no consumer yet".to_string()),
        ),
        Some("awaiting_input") => step(
            "flowing",
            State::Pending,
            Some("waiting for the browser to send".to_string()),
        ),
        _ if sending => step("flowing", State::Active, rate),
        _ => step("flowing", State::Pending, None),
    }
}

fn landed_step(source: Option<&Source>, cfg: &Config) -> Step {
    if cfg.open_live_url.is_none() {
        return step(
            "open-live",
            State::Pending,
            Some("open-live is not being watched".to_string()),
        );
    }
    match source {
        Some(source) if source.status.as_deref() == Some("active") => step(
            "open-live",
            State::Done,
            Some(format!("source {} is active", source.id)),
        ),
        Some(source) => step(
            "open-live",
            State::Active,
            Some(format!(
                "source {} is {}",
                source.id,
                source.status.as_deref().unwrap_or("unknown")
            )),
        ),
        None => step(
            "open-live",
            State::Pending,
            Some("open-live has not discovered the stream yet".to_string()),
        ),
    }
}
