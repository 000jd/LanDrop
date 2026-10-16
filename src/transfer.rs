//! Transfer manager: the heart of LanDrop.
//!
//! A [`TransferManager`] keeps all transfers (incoming and outgoing) in a
//! concurrent map, drives outgoing transfers via a background task that polls
//! the receiver until accepted, and broadcasts state changes to SSE
//! subscribers.
//!
//! State machine:
//! `preparing -> waiting -> transferring -> completed`
//! plus `rejected`, `failed`, `cancelled`, and a 120 s timeout on `waiting`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::events::EventBus;
use crate::peers::Peer;
use crate::protocol::{self, Offer, TransferView};

pub mod receiver;
pub mod sender;

/// How long a sender waits for the receiver to accept an offer.
pub const OFFER_TIMEOUT: Duration = Duration::from_secs(120);

/// How long a transfer may go without progress updates before it fails.
pub const STALL_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Terminal transfers are kept this long so the UI can show history.
const HISTORY_TTL: Duration = Duration::from_secs(60 * 60);

/// Status of a transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Preparing,
    Waiting,
    Accepted,
    Transferring,
    Completed,
    Rejected,
    Failed,
    Cancelled,
}

impl Status {
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Preparing => "preparing",
            Status::Waiting => "waiting",
            Status::Accepted => "accepted",
            Status::Transferring => "transferring",
            Status::Completed => "completed",
            Status::Rejected => "rejected",
            Status::Failed => "failed",
            Status::Cancelled => "cancelled",
        }
    }

    fn from_str(s: &str) -> Option<Status> {
        Some(match s {
            "preparing" => Status::Preparing,
            "waiting" => Status::Waiting,
            "accepted" => Status::Accepted,
            "transferring" => Status::Transferring,
            "completed" => Status::Completed,
            "rejected" => Status::Rejected,
            "failed" => Status::Failed,
            "cancelled" => Status::Cancelled,
            _ => return None,
        })
    }
}

impl Serialize for Status {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Status {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Status::from_str(&s).ok_or_else(|| serde::de::Error::custom(format!("unknown status {s}")))
    }
}

/// Direction of a transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Incoming,
    Outgoing,
}

impl Direction {
    pub fn as_str(&self) -> &'static str {
        match self {
            Direction::Incoming => "incoming",
            Direction::Outgoing => "outgoing",
        }
    }
}

/// Internal transfer record.
#[derive(Debug)]
pub struct Transfer {
    pub id: String,
    pub direction: Direction,
    pub filename: String,
    pub size: u64,
    pub peer_id: String,
    pub peer_name: String,
    pub peer_addr: String,
    pub status: Status,
    pub progress: f64, // 0.0..=1.0
    pub saved_path: Option<PathBuf>,
    pub note: String,
    pub error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    /// SHA-256 of the file, if the sender provided one.
    pub sha256: Option<String>,
    /// Bytes streamed so far (for speed / ETA).
    pub bytes_done: u64,
    /// Unix seconds when streaming started.
    pub started_at: Option<i64>,
    /// Per-transfer save directory (incoming, chosen by the user).
    pub save_dir: Option<PathBuf>,
    /// One-time upload token (incoming, accepted).
    pub upload_token: Option<String>,
    /// Cancellation signal (outgoing). Incoming transfers are interrupted by
    /// the status check inside the receive loop.
    pub cancel: Option<watch::Sender<bool>>,
}

impl Transfer {
    fn new_incoming(offer: &Offer, peer_addr: String) -> Self {
        let now = now_secs();
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            direction: Direction::Incoming,
            filename: protocol::sanitize_filename(&offer.filename),
            size: offer.size,
            peer_id: protocol::truncate_chars(&offer.sender_id, 64),
            peer_name: protocol::truncate_chars(&offer.sender_name, 64),
            peer_addr,
            status: Status::Waiting,
            progress: 0.0,
            saved_path: None,
            note: protocol::truncate_chars(&offer.note, 256),
            error: None,
            created_at: now,
            updated_at: now,
            sha256: offer.sha256.clone(),
            bytes_done: 0,
            started_at: None,
            save_dir: None,
            upload_token: None,
            cancel: None,
        }
    }

    fn new_outgoing(peer: &Peer, offer: &Offer) -> Self {
        let now = now_secs();
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            direction: Direction::Outgoing,
            filename: offer.filename.clone(),
            size: offer.size,
            peer_id: peer.id.clone(),
            peer_name: peer.name.clone(),
            peer_addr: format!("{}:{}", peer.addr, peer.port),
            status: Status::Preparing,
            progress: 0.0,
            saved_path: None,
            note: offer.note.clone(),
            error: None,
            created_at: now,
            updated_at: now,
            sha256: None,
            bytes_done: 0,
            started_at: None,
            save_dir: None,
            upload_token: None,
            cancel: None,
        }
    }

    pub fn view(&self) -> TransferView {
        let (bytes_done, speed_bps, eta_secs) = self.progress_stats();
        TransferView {
            id: self.id.clone(),
            direction: self.direction.as_str().to_string(),
            filename: self.filename.clone(),
            size: self.size,
            peer_id: self.peer_id.clone(),
            peer_name: self.peer_name.clone(),
            status: self.status.as_str().to_string(),
            progress: self.progress,
            bytes_done,
            speed_bps,
            eta_secs,
            saved_path: self
                .saved_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            note: self.note.clone(),
            error: self.error.clone(),
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }

    /// Bytes done, average speed (bytes/second) and estimated seconds left.
    pub fn progress_stats(&self) -> (u64, f64, Option<u64>) {
        let elapsed = match self.started_at {
            Some(start) => (now_secs() - start).max(0) as f64,
            None => 0.0,
        };
        let speed = if elapsed > 0.0 && self.bytes_done > 0 {
            self.bytes_done as f64 / elapsed
        } else {
            0.0
        };
        let eta = if speed > 0.0 && self.size > self.bytes_done {
            Some(((self.size - self.bytes_done) as f64 / speed).ceil() as u64)
        } else {
            None
        };
        (self.bytes_done, speed, eta)
    }
}

/// The transfer manager. Cheap to clone: all state lives behind `Arc`s.
#[derive(Clone)]
pub struct TransferManager {
    pub transfers: Arc<RwLock<HashMap<String, Transfer>>>,
    pub events: EventBus,
    pub my_id: Arc<RwLock<String>>,
    pub my_name: Arc<RwLock<String>>,
    pub my_port: Arc<RwLock<u16>>,
    pub download_dir: Arc<RwLock<PathBuf>>,
    pub auto_accept: Arc<RwLock<bool>>,
    /// True when accepting an incoming file should ask for a save folder.
    pub ask_save_path: Arc<RwLock<bool>>,
    pub shared_token: Arc<RwLock<String>>,
    pub peers: Arc<crate::peers::PeerStore>,
    /// When true, incoming offers show an Accept / Reject notification instead
    /// of requiring the full window. Enabled by [`crate::app::start_node`].
    pub interactive_prompts: Arc<AtomicBool>,
    /// Process-wide counters (see [`crate::metrics`]).
    pub metrics: Arc<crate::metrics::Metrics>,
}

impl TransferManager {
    pub fn new(peers: Arc<crate::peers::PeerStore>) -> Self {
        Self {
            transfers: Arc::new(RwLock::new(HashMap::new())),
            events: EventBus::new(),
            my_id: Arc::new(RwLock::new(String::new())),
            my_name: Arc::new(RwLock::new(String::new())),
            my_port: Arc::new(RwLock::new(8420)),
            download_dir: Arc::new(RwLock::new(PathBuf::new())),
            auto_accept: Arc::new(RwLock::new(false)),
            ask_save_path: Arc::new(RwLock::new(false)),
            shared_token: Arc::new(RwLock::new(String::new())),
            peers,
            interactive_prompts: Arc::new(AtomicBool::new(false)),
            metrics: Arc::new(crate::metrics::Metrics::new()),
        }
    }

    // -- identity ----------------------------------------------------------

    pub async fn set_identity(&self, id: &str, name: &str, port: u16) {
        *self.my_id.write().await = id.to_string();
        *self.my_name.write().await = name.to_string();
        *self.my_port.write().await = port;
    }

    pub async fn identity(&self) -> (String, String, u16) {
        (
            self.my_id.read().await.clone(),
            self.my_name.read().await.clone(),
            *self.my_port.read().await,
        )
    }

    // -- listing -----------------------------------------------------------

    /// Snapshot of all transfers, newest first.
    pub async fn list(&self) -> Vec<TransferView> {
        let mut t: Vec<TransferView> = self
            .transfers
            .read()
            .await
            .values()
            .map(Transfer::view)
            .collect();
        t.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        t
    }

    pub async fn get(&self, id: &str) -> Option<TransferView> {
        self.transfers.read().await.get(id).map(Transfer::view)
    }

    /// Run `f` with a mutable transfer; returns `None` if id is unknown.
    pub async fn with_transfer<F, R>(&self, id: &str, f: F) -> Option<R>
    where
        F: FnOnce(&mut Transfer) -> R,
    {
        let mut map = self.transfers.write().await;
        map.get_mut(id).map(f)
    }

    /// Send an event for a transfer, when it still exists.
    pub async fn emit_update(&self, id: &str, event: &str) {
        if let Some(v) = self.get(id).await {
            self.events.send(event, &v);
        }
    }

    // -- cancellation / expiry --------------------------------------------

    /// Cancel a transfer (UI button).
    ///
    /// Outgoing: signals the send task to stop. Incoming: allowed while
    /// waiting for or streaming the upload — the receive loop checks the
    /// status and aborts, and any unconsumed upload token is burned.
    pub async fn cancel(&self, id: &str) -> bool {
        let mut map = self.transfers.write().await;
        let Some(t) = map.get_mut(id) else {
            return false;
        };
        let allowed = matches!(
            t.status,
            Status::Waiting | Status::Preparing | Status::Accepted | Status::Transferring
        );
        if !allowed {
            return false;
        }
        if let Some(tx) = &t.cancel {
            let _ = tx.send(true);
        }
        if t.direction == Direction::Incoming {
            t.upload_token = None;
        }
        t.status = Status::Cancelled;
        t.updated_at = now_secs();
        let v = t.view();
        drop(map);
        self.events.send("transfer_updated", &v);
        true
    }

    /// Sweep stale transfers: expire un-accepted offers, accepted transfers
    /// whose upload never started, stalled transfers, and old history.
    pub async fn expire_stale(&self) {
        let now = now_secs();
        let mut expired: Vec<String> = Vec::new();
        let mut cancels: Vec<watch::Sender<bool>> = Vec::new();
        {
            let mut map = self.transfers.write().await;
            for (id, t) in map.iter_mut() {
                let reason = match t.status {
                    Status::Waiting if now - t.created_at > OFFER_TIMEOUT.as_secs() as i64 => {
                        Some(format!(
                            "timed out waiting for acceptance ({}s)",
                            OFFER_TIMEOUT.as_secs()
                        ))
                    }
                    Status::Accepted if now - t.updated_at > OFFER_TIMEOUT.as_secs() as i64 => {
                        Some(format!(
                            "timed out waiting for the upload to start ({}s)",
                            OFFER_TIMEOUT.as_secs()
                        ))
                    }
                    Status::Transferring if now - t.updated_at > STALL_TIMEOUT.as_secs() as i64 => {
                        Some(format!(
                            "transfer stalled (no progress for {}s)",
                            STALL_TIMEOUT.as_secs()
                        ))
                    }
                    _ => None,
                };
                let Some(reason) = reason else { continue };
                t.status = Status::Failed;
                t.error = Some(reason);
                t.updated_at = now;
                if let Some(tx) = &t.cancel {
                    cancels.push(tx.clone());
                }
                expired.push(id.clone());
            }
            // Drop terminal history after a while so the map stays bounded.
            map.retain(|_, t| {
                let terminal = matches!(
                    t.status,
                    Status::Completed | Status::Rejected | Status::Failed | Status::Cancelled
                );
                !terminal || now - t.updated_at <= HISTORY_TTL.as_secs() as i64
            });
        }
        for tx in cancels {
            let _ = tx.send(true);
        }
        for id in expired {
            self.emit_update(&id, "transfer_failed").await;
        }
    }

    // -- incoming ----------------------------------------------------------

    /// Handle an incoming offer: validate it, create a pending transfer and
    /// either auto-accept or notify the user.
    pub async fn handle_offer(&self, offer: Offer, peer_addr: String) -> Result<String> {
        if offer.sender_id.trim().is_empty() {
            bail!("offer has no sender id");
        }
        if offer.filename.trim().is_empty() {
            bail!("offer has no filename");
        }
        let sha256 = match &offer.sha256 {
            Some(s) => match protocol::normalize_sha256(s) {
                Some(n) if protocol::is_sha256_hex(&n) => Some(n),
                Some(_) => bail!("offer carries a malformed sha256"),
                None => None,
            },
            None => None,
        };

        let mut t = Transfer::new_incoming(&offer, peer_addr);
        t.sha256 = sha256;
        let id = t.id.clone();
        let view = t.view();
        {
            let mut map = self.transfers.write().await;
            map.insert(id.clone(), t);
        }
        self.events.send("transfer_updated", &view);
        info!(
            "incoming offer from {} ({}) for {} ({} bytes)",
            view.peer_name, view.peer_id, view.filename, offer.size
        );

        if *self.auto_accept.read().await {
            if let Err(e) = self.accept(&id, None).await {
                warn!("auto-accept failed for {id}: {e:#}");
            }
        } else if self.interactive_prompts.load(Ordering::Relaxed) {
            crate::notify::prompt_incoming(
                self.clone(),
                id.clone(),
                view.peer_name.clone(),
                view.filename.clone(),
            );
        }
        Ok(id)
    }

    /// Accept an incoming transfer: generate a one-time upload token.
    ///
    /// `save_dir` optionally overrides the download folder for this transfer
    /// (the folder is validated by the caller before accept).
    pub async fn accept(&self, id: &str, save_dir: Option<PathBuf>) -> Result<()> {
        let token = uuid::Uuid::new_v4().to_string();
        {
            let mut map = self.transfers.write().await;
            let t = map
                .get_mut(id)
                .ok_or_else(|| anyhow!("unknown transfer {id}"))?;
            if t.direction != Direction::Incoming {
                bail!("cannot accept an outgoing transfer");
            }
            if t.status != Status::Waiting {
                bail!(
                    "transfer {id} is not pending (status={})",
                    t.status.as_str()
                );
            }
            t.status = Status::Accepted;
            t.save_dir = save_dir;
            t.upload_token = Some(token);
            t.updated_at = now_secs();
        }
        self.emit_update(id, "transfer_updated").await;
        info!("accepted transfer {id}");
        Ok(())
    }

    /// Reject an incoming transfer.
    pub async fn reject(&self, id: &str) -> Result<()> {
        {
            let mut map = self.transfers.write().await;
            let t = map
                .get_mut(id)
                .ok_or_else(|| anyhow!("unknown transfer {id}"))?;
            if t.direction != Direction::Incoming {
                bail!("cannot reject an outgoing transfer");
            }
            if t.status != Status::Waiting {
                bail!("transfer {id} is not pending");
            }
            t.status = Status::Rejected;
            t.updated_at = now_secs();
        }
        self.emit_update(id, "transfer_updated").await;
        info!("rejected transfer {id}");
        Ok(())
    }

    /// Atomically verify and consume the one-time upload token.
    pub async fn take_upload_token(&self, id: &str, token: &str) -> bool {
        let mut map = self.transfers.write().await;
        let Some(t) = map.get_mut(id) else {
            return false;
        };
        let ok = match t.upload_token.as_deref() {
            Some(expected) => constant_time_eq(expected.as_bytes(), token.as_bytes()),
            // No token set: this request can never succeed, but still run a
            // comparison so the "already used / never issued" case is not
            // distinguishable by timing on the first call.
            None => {
                let _ = constant_time_eq(&[0u8; 32], token.as_bytes());
                false
            }
        };
        if ok {
            t.upload_token = None;
        }
        ok
    }

    /// Acceptance status for a polling sender.
    pub async fn offer_status(&self, id: &str) -> Option<(String, Option<String>, Option<String>)> {
        let map = self.transfers.read().await;
        let t = map.get(id)?;
        let status = t.status.as_str().to_string();
        let url = (t.status == Status::Accepted).then(|| format!("/api/v1/upload/{id}"));
        let token = t.upload_token.clone();
        Some((status, url, token))
    }

    /// Stream an upload body to a temp file, verifying size and checksum,
    /// then atomically rename into the download dir.
    pub async fn receive_stream<B>(&self, id: &str, body: B) -> Result<PathBuf>
    where
        B: tokio::io::AsyncRead + Unpin,
    {
        self::receiver::receive_stream(self, id, body).await
    }
}

/// Comparison without early exit, so token checks do not leak timing.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// Seconds since the epoch.
pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer() -> Peer {
        Peer {
            id: "p".into(),
            name: "peer".into(),
            addr: "10.0.0.2".into(),
            port: 8420,
            source: "manual".into(),
        }
    }

    fn offer(size: u64) -> Offer {
        Offer {
            sender_id: "s".into(),
            sender_name: "sender".into(),
            filename: "file.bin".into(),
            size,
            sha256: None,
            note: String::new(),
        }
    }

    #[test]
    fn progress_stats_are_empty_before_streaming() {
        let t = Transfer::new_outgoing(&peer(), &offer(1_000));
        let (done, speed, eta) = t.progress_stats();
        assert_eq!(done, 0);
        assert_eq!(speed, 0.0);
        assert_eq!(eta, None);
    }

    #[test]
    fn progress_stats_compute_speed_and_eta() {
        let mut t = Transfer::new_outgoing(&peer(), &offer(1_000_000));
        t.status = Status::Transferring;
        t.bytes_done = 250_000;
        t.started_at = Some(now_secs() - 5);
        let (done, speed, eta) = t.progress_stats();
        assert_eq!(done, 250_000);
        // ~50 KB/s over 5 s; 750 KB left → ~15 s (allow scheduling slack).
        assert!((40_000.0..=60_000.0).contains(&speed), "speed {speed}");
        let eta = eta.expect("eta is estimated while streaming");
        assert!((10..=25).contains(&eta), "eta {eta}");
    }

    #[test]
    fn progress_stats_finish_without_eta() {
        let mut t = Transfer::new_outgoing(&peer(), &offer(1_000));
        t.status = Status::Completed;
        t.bytes_done = 1_000;
        t.started_at = Some(now_secs() - 2);
        let (_, _, eta) = t.progress_stats();
        assert_eq!(eta, None);
    }
}
