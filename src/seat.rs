use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// One guest's place in the broadcast. The id is the whole identity: it is the
/// weave node id the page registers as, the weave stream name, and — through
/// open-live's provider — the source document id. One seat, one stream, one
/// source, stable across a guest rejoining.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Seat {
    pub seat: String,
    pub display_name: String,
    pub token: String,
    pub created_at: i64,
    pub expires_at: i64,
    /// Revoked seats stay until their stream is confirmed gone, so a failed
    /// delete is retried rather than leaking a stream nobody owns.
    #[serde(default)]
    pub revoked: bool,
    /// Set the first time the node was seen registered. It is what separates a
    /// guest who never showed up from one who has left.
    #[serde(default)]
    pub joined_at: Option<i64>,
}

impl Seat {
    pub fn expired(&self, now: i64) -> bool {
        now >= self.expires_at
    }
}

#[derive(Debug)]
pub struct SeatStore {
    path: PathBuf,
    seats: BTreeMap<String, Seat>,
}

impl SeatStore {
    pub fn load(path: &Path) -> Result<Self> {
        let seats = match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("reading seats from {}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => {
                return Err(error).with_context(|| format!("opening {}", path.display()));
            }
        };
        Ok(Self {
            path: path.to_path_buf(),
            seats,
        })
    }

    pub fn get(&self, seat: &str) -> Option<&Seat> {
        self.seats.get(seat)
    }

    /// Newest first, which is the order the operator page renders.
    pub fn list(&self) -> Vec<Seat> {
        let mut seats: Vec<Seat> = self.seats.values().cloned().collect();
        seats.sort_by_key(|seat| std::cmp::Reverse(seat.created_at));
        seats
    }

    pub fn insert(&mut self, seat: Seat) -> Result<()> {
        self.seats.insert(seat.seat.clone(), seat);
        self.save()
    }

    pub fn remove(&mut self, seat: &str) -> Result<Option<Seat>> {
        let removed = self.seats.remove(seat);
        if removed.is_some() {
            self.save()?;
        }
        Ok(removed)
    }

    /// Applies a change and persists only when something changed, so the tick
    /// does not rewrite the file every few seconds.
    pub fn update(&mut self, seat: &str, edit: impl FnOnce(&mut Seat) -> bool) -> Result<bool> {
        let Some(existing) = self.seats.get_mut(seat) else {
            return Ok(false);
        };
        if !edit(existing) {
            return Ok(false);
        }
        self.save()?;
        Ok(true)
    }

    /// Written through a temporary file in the same directory so a crash
    /// mid-write leaves the previous seats readable rather than a partial file.
    fn save(&self) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(&self.seats)?;
        let temporary = self.path.with_extension("json.tmp");
        std::fs::write(&temporary, &bytes)
            .with_context(|| format!("writing {}", temporary.display()))?;
        std::fs::rename(&temporary, &self.path)
            .with_context(|| format!("replacing {}", self.path.display()))?;
        Ok(())
    }
}
