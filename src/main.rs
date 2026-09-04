mod app;
mod assets;
mod config;
mod invite;
mod live;
mod reconcile;
mod routes;
mod seat;
mod snapshot;
mod weave;

use anyhow::{Context, Result};
use clap::Parser;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::app::App;
use crate::config::{Args, Config};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "greenroom=info,warn".into()),
        )
        .init();

    let cfg = Config::from_args(Args::parse())?;
    let addr = cfg.addr;
    let public_url = cfg.public_url.clone();
    let app = App::new(cfg)?;

    tokio::spawn(reconcile::run(app.clone()));

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    tracing::info!(%addr, %public_url, "greenroom is up");

    axum::serve(listener, routes::router(app))
        .with_graceful_shutdown(shutdown())
        .await
        .context("serving")?;
    Ok(())
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down");
}

pub fn now() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

pub fn rfc3339(seconds: i64) -> String {
    OffsetDateTime::from_unix_timestamp(seconds)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
        .format(&Rfc3339)
        .unwrap_or_default()
}
