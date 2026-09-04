use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;

use crate::invite::SigningKey;

/// Everything the gateway is configured with. Flags and environment share one
/// definition, matching open-weave's own `clap` style.
#[derive(Debug, Parser)]
#[command(
    name = "greenroom",
    about = "Invite a guest into an open-live broadcast over open-weave"
)]
pub struct Args {
    /// Address the gateway listens on.
    #[arg(long, env = "GREENROOM_ADDR", default_value = "127.0.0.1:8090")]
    pub addr: SocketAddr,

    /// Base URL join links are built from. Guests must be able to reach it.
    #[arg(long, env = "GREENROOM_PUBLIC_URL", default_value = "http://localhost:8090")]
    pub public_url: String,

    /// Signs invite links. Generated at boot when unset, which invalidates
    /// links issued by an earlier run.
    #[arg(long, env = "GREENROOM_SIGNING_KEY")]
    pub signing_key: Option<String>,

    /// Bearer token the controller must present on the webhook route.
    #[arg(long, env = "GREENROOM_WEBHOOK_TOKEN")]
    pub webhook_token: Option<String>,

    /// Set to 1 or true to accept unauthenticated webhook deliveries.
    #[arg(long, env = "GREENROOM_AUTH_DISABLED")]
    pub auth_disabled: Option<String>,

    /// Seats are written here so a restart keeps its invites.
    #[arg(long, env = "GREENROOM_STATE", default_value = "greenroom-state.json")]
    pub state: PathBuf,

    /// Serve the pages from this directory instead of the embedded copies.
    #[arg(long, env = "GREENROOM_ASSETS")]
    pub assets: Option<PathBuf>,

    /// How often desired state is reconciled against weave and open-live.
    #[arg(long, env = "GREENROOM_RECONCILE_SECS", default_value = "3")]
    pub reconcile_secs: u64,

    /// How long a fresh invite stays usable.
    #[arg(long, env = "GREENROOM_INVITE_TTL_SECS", default_value = "3600")]
    pub invite_ttl_secs: i64,

    /// The node a guest's camera is delivered to, and the alias of its
    /// data-plane address. The alias decides which signalling URL the guest's
    /// browser is given, so it has to be one the browser can reach.
    #[arg(long, env = "GREENROOM_TARGET_NODE", default_value = "strom-node-1")]
    pub target_node: String,
    #[arg(long, env = "GREENROOM_TARGET_NETWORK", default_value = "docker-host")]
    pub target_network: String,
    #[arg(long, env = "GREENROOM_TARGET_LATENCY_MS", default_value = "200")]
    pub target_latency_ms: u32,

    #[arg(long, env = "WEAVE_NORTHBOUND_URL", default_value = "http://127.0.0.1:9080")]
    pub northbound_url: String,
    #[arg(long, env = "WEAVE_NORTHBOUND_TOKEN")]
    pub northbound_token: Option<String>,
    #[arg(long, env = "WEAVE_SOUTHBOUND_URL", default_value = "http://127.0.0.1:8081")]
    pub southbound_url: String,
    #[arg(long, env = "WEAVE_SOUTHBOUND_TOKEN")]
    pub southbound_token: Option<String>,

    /// open-live's API, read to show whether a guest's source landed there.
    /// Unset drops that column.
    #[arg(long, env = "OPEN_LIVE_URL", default_value = "http://127.0.0.1:3000")]
    pub open_live_url: String,
    #[arg(long, env = "OPEN_LIVE_API_KEY")]
    pub open_live_api_key: Option<String>,
}

#[derive(Debug)]
pub struct Config {
    pub addr: SocketAddr,
    pub public_url: String,
    pub signing_key: SigningKey,
    pub webhook_token: Option<String>,
    pub state: PathBuf,
    pub assets: Option<PathBuf>,
    pub reconcile: Duration,
    pub invite_ttl_secs: i64,
    pub target_node: String,
    pub target_network: Option<String>,
    pub target_latency_ms: u32,
    pub northbound_url: String,
    pub northbound_token: Option<String>,
    pub southbound_url: String,
    pub southbound_token: Option<String>,
    pub open_live_url: Option<String>,
    pub open_live_api_key: Option<String>,
}

impl Config {
    pub fn from_args(args: Args) -> Result<Self> {
        let auth_disabled = matches!(
            args.auth_disabled.as_deref().map(str::trim),
            Some("1") | Some("true")
        );
        let webhook_token = present(args.webhook_token);
        if webhook_token.is_none() && !auth_disabled {
            bail!(
                "GREENROOM_WEBHOOK_TOKEN is unset, so any caller could report a node as \
                 registered. Set it to the controller's WEAVE_WEBHOOK_TOKEN, or set \
                 GREENROOM_AUTH_DISABLED=1 to accept that."
            );
        }

        let signing_key = match present(args.signing_key) {
            Some(key) => SigningKey::from_secret(key.as_bytes()),
            None => {
                let key = SigningKey::generate().context("generating a signing key")?;
                tracing::warn!(
                    "GREENROOM_SIGNING_KEY unset; generated one. Links issued by an earlier \
                     run no longer verify."
                );
                key
            }
        };

        if args.reconcile_secs == 0 {
            bail!("GREENROOM_RECONCILE_SECS must be at least 1");
        }
        if args.invite_ttl_secs <= 0 {
            bail!("GREENROOM_INVITE_TTL_SECS must be positive");
        }

        Ok(Self {
            addr: args.addr,
            public_url: args.public_url.trim_end_matches('/').to_string(),
            signing_key,
            webhook_token,
            state: args.state,
            assets: args.assets,
            reconcile: Duration::from_secs(args.reconcile_secs),
            invite_ttl_secs: args.invite_ttl_secs,
            target_node: args.target_node,
            target_network: present(Some(args.target_network)),
            target_latency_ms: args.target_latency_ms,
            northbound_url: args.northbound_url.trim_end_matches('/').to_string(),
            northbound_token: present(args.northbound_token),
            southbound_url: args.southbound_url.trim_end_matches('/').to_string(),
            southbound_token: present(args.southbound_token),
            open_live_url: present(Some(args.open_live_url))
                .map(|url| url.trim_end_matches('/').to_string()),
            open_live_api_key: present(args.open_live_api_key),
        })
    }

    pub fn join_url(&self, token: &str) -> String {
        format!("{}/j/{token}", self.public_url)
    }
}

/// Blank and whitespace-only values read as unset, as they do in open-weave.
fn present(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.trim().is_empty())
}
