use std::sync::{Arc, Mutex, RwLock};

use anyhow::Result;
use tokio::sync::Notify;

use crate::config::Config;
use crate::live::OpenLive;
use crate::seat::SeatStore;
use crate::snapshot::Snapshot;
use crate::weave::Weave;

pub struct App {
    pub cfg: Arc<Config>,
    pub seats: Mutex<SeatStore>,
    pub snapshot: RwLock<Snapshot>,
    pub weave: Weave,
    pub live: Option<OpenLive>,
    /// Woken when something happened that desired state should follow — a
    /// webhook delivery, a new invite, a revoke. The reconcile loop is the only
    /// thing that acts, so a nudge cannot race the tick.
    pub nudge: Notify,
}

impl App {
    pub fn new(cfg: Config) -> Result<Arc<Self>> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()?;
        let seats = SeatStore::load(&cfg.state)?;
        let weave = Weave::new(
            http.clone(),
            cfg.northbound_url.clone(),
            cfg.northbound_token.clone(),
            cfg.southbound_url.clone(),
            cfg.southbound_token.clone(),
        );
        let live = cfg
            .open_live_url
            .clone()
            .map(|base| OpenLive::new(http, base, cfg.open_live_api_key.clone()));
        Ok(Arc::new(Self {
            cfg: Arc::new(cfg),
            seats: Mutex::new(seats),
            snapshot: RwLock::new(Snapshot::default()),
            weave,
            live,
            nudge: Notify::new(),
        }))
    }
}
