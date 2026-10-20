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
use std::path::{Path, PathBuf};
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

/// File name of the persisted terminal-transfer history.
pub const HISTORY_FILE_NAME: &str = "history.json";

/// Most recent terminal transfers kept in memory and in `history.json`.
pub const HISTORY_LIMIT: usize = 100;

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

    /// True for states that no longer change and belong in the history.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Status::Completed | Status::Rejected | Status::Failed | Status::Cancelled
        )
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
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
    /// Relative path the sender asked for (folder sends); sanitized on arrival,
    /// applied when choosing the destination directory.
    pub rel_path: Option<String>,
    /// Send batch (folder / multi-file selection), shared by every file of one
    /// send so the receiver asks once.
    pub batch_id: Option<String>,
    pub batch_name: Option<String>,
    /// Snippet payload (no upload follows; accepting completes the transfer).
    pub text: Option<String>,
    /// Six-digit verification PIN, shared by every file of a batch.
    pub pin: String,
    /// Sender's ephemeral X25519 key pair until the receiver's reply arrives.
    pub dh_keypair: Option<crate::crypto::KeyPair>,
    /// Established per-transfer session for encrypted payloads.
    pub session: Option<crate::crypto::SessionKey>,
    /// Receiver's ephemeral public key (hex), echoed in the offer response.
    pub dh_public: Option<String>,
    /// True when the payload is encrypted (X25519 + ChaCha20).
    pub encrypted: bool,
    /// Bytes streamed so far (for speed / ETA).
    pub bytes_done: u64,
    /// When streaming started (monotonic, for smooth speed/ETA).
    pub started_at: Option<std::time::Instant>,
    /// Per-transfer save directory (incoming, chosen by the user).
    pub save_dir: Option<PathBuf>,
    /// One-time upload token (incoming, accepted). It stays valid while the
    /// transfer is `Accepted` or `Transferring` so an interrupted upload can be
    /// resumed, and is burned as soon as the transfer reaches a terminal state.
    pub upload_token: Option<String>,
    /// Deterministic partial file: `.{filename}.part-{id}` in the destination
    /// directory. Kept on disk across an interrupted upload so the sender can
    /// resume from the byte offset reported by `HEAD`.
    pub part_path: Option<PathBuf>,
    /// True while an upload stream owns the partial file. Guards against two
    /// concurrent streams writing the same `.part-{id}` file.
    pub uploading: bool,
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
            rel_path: offer.rel_path.clone(),
            batch_id: offer.batch_id.clone(),
            batch_name: offer.batch_name.clone(),
            text: offer.text.clone(),
            pin: String::new(),
            dh_keypair: None,
            session: None,
            dh_public: None,
            encrypted: false,
            bytes_done: 0,
            started_at: None,
            save_dir: None,
            upload_token: None,
            part_path: None,
            uploading: false,
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
            rel_path: None,
            batch_id: offer.batch_id.clone(),
            batch_name: offer.batch_name.clone(),
            text: offer.text.clone(),
            pin: String::new(),
            dh_keypair: None,
            session: None,
            dh_public: None,
            encrypted: false,
            bytes_done: 0,
            started_at: None,
            save_dir: None,
            upload_token: None,
            part_path: None,
            uploading: false,
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
            batch_id: self.batch_id.clone(),
            batch_name: self.batch_name.clone(),
            text: self.text.clone(),
            pin: self.pin.clone(),
            encrypted: self.encrypted,
            updated_at: self.updated_at,
        }
    }

    /// Bytes done, average speed (bytes/second) and estimated seconds left.
    pub fn progress_stats(&self) -> (u64, f64, Option<u64>) {
        let elapsed = match self.started_at {
            Some(start) => start.elapsed().as_secs_f64().max(0.001),
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

/// A terminal transfer as stored in `history.json`.
///
/// Volatile fields (upload tokens, partial-file paths, cancellation channels)
/// are deliberately absent: a restored transfer can be inspected or cleared,
/// never resumed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: String,
    pub direction: Direction,
    pub filename: String,
    pub size: u64,
    pub peer_id: String,
    pub peer_name: String,
    pub peer_addr: String,
    pub status: Status,
    pub progress: f64,
    pub saved_path: Option<String>,
    pub note: String,
    pub error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub sha256: Option<String>,
    pub rel_path: Option<String>,
    pub batch_id: Option<String>,
    pub batch_name: Option<String>,
    pub text: Option<String>,
    pub pin: String,
    #[serde(default)]
    pub encrypted: bool,
    pub bytes_done: u64,
}

impl HistoryEntry {
    fn from_transfer(t: &Transfer) -> Self {
        Self {
            id: t.id.clone(),
            direction: t.direction,
            filename: t.filename.clone(),
            size: t.size,
            peer_id: t.peer_id.clone(),
            peer_name: t.peer_name.clone(),
            peer_addr: t.peer_addr.clone(),
            status: t.status,
            progress: t.progress,
            saved_path: t
                .saved_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            note: t.note.clone(),
            error: t.error.clone(),
            created_at: t.created_at,
            updated_at: t.updated_at,
            sha256: t.sha256.clone(),
            rel_path: t.rel_path.clone(),
            batch_id: t.batch_id.clone(),
            batch_name: t.batch_name.clone(),
            text: t.text.clone(),
            pin: t.pin.clone(),
            encrypted: t.encrypted,
            bytes_done: t.bytes_done,
        }
    }

    fn into_transfer(self) -> Transfer {
        Transfer {
            id: self.id,
            direction: self.direction,
            filename: self.filename,
            size: self.size,
            peer_id: self.peer_id,
            peer_name: self.peer_name,
            peer_addr: self.peer_addr,
            status: self.status,
            progress: self.progress,
            saved_path: self.saved_path.map(PathBuf::from),
            note: self.note,
            error: self.error,
            created_at: self.created_at,
            updated_at: self.updated_at,
            sha256: self.sha256,
            rel_path: self.rel_path,
            batch_id: self.batch_id,
            batch_name: self.batch_name,
            text: self.text,
            pin: self.pin,
            encrypted: self.encrypted,
            dh_keypair: None,
            session: None,
            dh_public: None,
            bytes_done: self.bytes_done,
            started_at: None,
            save_dir: None,
            upload_token: None,
            part_path: None,
            uploading: false,
            cancel: None,
        }
    }
}

/// Reply to an offer: the new transfer id plus the receiver's ephemeral key
/// when the sender asked for encryption.
#[derive(Debug, Clone)]
pub struct OfferReply {
    pub transfer_id: String,
    pub dh_pub: Option<String>,
}

/// Result of claiming the single upload slot for a transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadStart {
    Started,
    Unknown,
    /// Another stream currently owns the partial file.
    Busy,
    InvalidToken,
    NotUploadable,
}

/// Where an incoming file will be written: the per-transfer folder wins over
/// the default download directory, and folder sends recreate their tree.
pub(crate) fn destination_dir(
    default_dir: &std::path::Path,
    save_dir: Option<&std::path::Path>,
    rel_path: Option<&str>,
) -> PathBuf {
    let base = save_dir.unwrap_or(default_dir).to_path_buf();
    match rel_path.and_then(protocol::sanitize_rel_dirs) {
        Some(dirs) => dirs.iter().fold(base, |dir, part| dir.join(part)),
        None => base,
    }
}

/// Deterministic partial file for a transfer (`.{filename}.part-{id}`).
pub(crate) fn part_path_for(
    default_dir: &std::path::Path,
    save_dir: Option<&std::path::Path>,
    rel_path: Option<&str>,
    filename: &str,
    id: &str,
) -> PathBuf {
    destination_dir(default_dir, save_dir, rel_path).join(format!(".{filename}.part-{id}"))
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
    /// Auto-accept only offers from devices listed in `trusted_peers`.
    pub auto_accept_trusted_only: Arc<RwLock<bool>>,
    /// Device ids that may skip the accept prompt.
    pub trusted_peers: Arc<RwLock<Vec<String>>>,
    /// True when accepting an incoming file should ask for a save folder.
    pub ask_save_path: Arc<RwLock<bool>>,
    pub shared_token: Arc<RwLock<String>>,
    pub peers: Arc<crate::peers::PeerStore>,
    /// When true, incoming offers show an Accept / Reject notification instead
    /// of requiring the full window. Enabled by [`crate::app::start_node`].
    pub interactive_prompts: Arc<AtomicBool>,
    /// Process-wide counters (see [`crate::metrics`]).
    pub metrics: Arc<crate::metrics::Metrics>,
    /// Batches whose prompt task is already running: a 50-file folder must ask
    /// once, not fire 50 dialogs when its offers settle together.
    pub batch_prompts: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// Upload slots, so a large folder streams in a bounded queue instead of
    /// opening one parallel upload per file.
    pub upload_slots: Arc<tokio::sync::Semaphore>,
    /// Where terminal transfers are persisted (`<config_dir>/history.json`).
    /// `None` disables persistence (unit tests and embedded use).
    history_path: Arc<RwLock<Option<PathBuf>>>,
    /// Serializes history writes and the clear-file operation.
    history_lock: Arc<tokio::sync::Mutex<()>>,
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
            auto_accept_trusted_only: Arc::new(RwLock::new(false)),
            trusted_peers: Arc::new(RwLock::new(Vec::new())),
            ask_save_path: Arc::new(RwLock::new(false)),
            shared_token: Arc::new(RwLock::new(String::new())),
            peers,
            interactive_prompts: Arc::new(AtomicBool::new(false)),
            metrics: Arc::new(crate::metrics::Metrics::new()),
            batch_prompts: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            upload_slots: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_UPLOADS)),
            history_path: Arc::new(RwLock::new(None)),
            history_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    // -- persistent history ------------------------------------------------

    /// Point the manager at `<config_dir>/history.json`.
    pub async fn set_history_path(&self, path: Option<PathBuf>) {
        *self.history_path.write().await = path;
    }

    /// Restore terminal transfers from a history file.
    ///
    /// A missing file is not an error; a malformed one is reported so the
    /// caller can log it and keep starting. Returns how many entries loaded.
    pub async fn load_history(&self, path: &Path) -> Result<usize> {
        let raw = match tokio::fs::read_to_string(path).await {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e.into()),
        };
        let mut entries: Vec<HistoryEntry> = serde_json::from_str(&raw)?;
        entries.sort_by_key(|e| std::cmp::Reverse(e.updated_at));
        entries.truncate(HISTORY_LIMIT);
        let mut map = self.transfers.write().await;
        let mut loaded = 0;
        for entry in entries {
            // A history file only ever holds terminal states. Ignore anything
            // else rather than resurrecting an active-looking transfer.
            if !entry.status.is_terminal() || map.contains_key(&entry.id) {
                continue;
            }
            map.insert(entry.id.clone(), entry.into_transfer());
            loaded += 1;
        }
        Ok(loaded)
    }

    /// Rewrite the history file from the current terminal transfers.
    ///
    /// Best effort: a failure is logged, never propagated into a transfer.
    pub async fn persist_history(&self) {
        let Some(path) = self.history_path.read().await.clone() else {
            return;
        };
        let _guard = self.history_lock.lock().await;
        let entries = {
            let map = self.transfers.read().await;
            let mut entries: Vec<HistoryEntry> = map
                .values()
                .filter(|t| t.status.is_terminal())
                .map(HistoryEntry::from_transfer)
                .collect();
            entries.sort_by_key(|e| std::cmp::Reverse(e.updated_at));
            entries.truncate(HISTORY_LIMIT);
            entries
        };
        let written = tokio::task::spawn_blocking(move || -> Result<()> {
            let data = serde_json::to_vec_pretty(&entries)?;
            crate::config::write_atomic_0600(&path, &data)
        })
        .await;
        match written {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warn!("failed to persist transfer history: {e:#}"),
            Err(e) => warn!("history writer task failed: {e}"),
        }
    }

    /// Remove `history.json` entirely (used by "Clear history").
    async fn remove_history_file(&self) {
        let _guard = self.history_lock.lock().await;
        let Some(path) = self.history_path.read().await.clone() else {
            return;
        };
        match tokio::fs::remove_file(&path).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warn!("failed to remove {}: {e}", path.display()),
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
        let (view, part) = {
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
            // A cancelled transfer is terminal: the token can never be used
            // again and the partial file must not be left behind.
            t.upload_token = None;
            t.uploading = false;
            t.status = Status::Cancelled;
            t.updated_at = now_secs();
            (t.view(), t.part_path.take())
        };
        if let Some(part) = part {
            let _ = tokio::fs::remove_file(&part).await;
        }
        self.events.send("transfer_updated", &view);
        self.persist_history().await;
        true
    }

    /// Sweep stale transfers: expire un-accepted offers, accepted transfers
    /// whose upload never started, stalled transfers, and old history.
    pub async fn expire_stale(&self) {
        let now = now_secs();
        let mut expired: Vec<String> = Vec::new();
        let mut cancels: Vec<watch::Sender<bool>> = Vec::new();
        let mut parts: Vec<PathBuf> = Vec::new();
        let mut history_changed = false;
        {
            let mut map = self.transfers.write().await;
            // A batch streams through a few slots, so files queued behind the
            // semaphore can sit in `Accepted` for a long time. While any member
            // of the batch is actually streaming, its siblings are alive, not
            // stale — refresh them instead of failing them.
            let active_batches: std::collections::HashSet<String> = map
                .values()
                .filter(|t| t.status == Status::Transferring)
                .filter_map(|t| t.batch_id.clone())
                .collect();
            for (id, t) in map.iter_mut() {
                if t.status == Status::Accepted
                    && t.batch_id
                        .as_deref()
                        .is_some_and(|batch| active_batches.contains(batch))
                {
                    t.updated_at = now;
                    continue;
                }
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
                // Expiry is terminal: burn the token and remove the partial
                // file this transfer may have been resuming from.
                t.upload_token = None;
                t.uploading = false;
                if let Some(part) = t.part_path.take() {
                    parts.push(part);
                }
                if let Some(tx) = &t.cancel {
                    cancels.push(tx.clone());
                }
                expired.push(id.clone());
                history_changed = true;
            }
            // Keep the map bounded exactly like history.json: the newest
            // terminal entries stay, older ones are dropped.
            let mut terminal: Vec<(String, i64)> = map
                .iter()
                .filter(|(_, t)| t.status.is_terminal())
                .map(|(id, t)| (id.clone(), t.updated_at))
                .collect();
            if terminal.len() > HISTORY_LIMIT {
                terminal.sort_by_key(|(_, updated_at)| *updated_at);
                let excess = terminal.len() - HISTORY_LIMIT;
                for (id, _) in terminal.into_iter().take(excess) {
                    map.remove(&id);
                }
                history_changed = true;
            }
        }
        for tx in cancels {
            let _ = tx.send(true);
        }
        for part in parts {
            let _ = tokio::fs::remove_file(&part).await;
        }
        for id in expired {
            self.emit_update(&id, "transfer_failed").await;
        }
        if history_changed {
            self.persist_history().await;
        }
    }

    /// Remove every terminal transfer from the in-memory history.
    ///
    /// Active, accepted and waiting transfers are untouched. Returns how many
    /// entries disappeared.
    pub async fn clear_terminal(&self) -> usize {
        let mut parts: Vec<PathBuf> = Vec::new();
        let removed = {
            let mut map = self.transfers.write().await;
            let before = map.len();
            map.retain(|_, t| {
                let terminal = t.status.is_terminal();
                if terminal {
                    if let Some(part) = t.part_path.take() {
                        parts.push(part);
                    }
                }
                !terminal
            });
            before - map.len()
        };
        // Terminal transfers normally have no partial file left; remove any
        // residue a previous failure path might have missed.
        for part in parts {
            let _ = tokio::fs::remove_file(&part).await;
        }
        if removed > 0 {
            self.events.send(
                "transfers_cleared",
                &serde_json::json!({ "cleared": removed }),
            );
        }
        self.remove_history_file().await;
        removed
    }

    // -- incoming ----------------------------------------------------------

    /// Handle an incoming offer: validate it, create a pending transfer and
    /// either auto-accept or notify the user.
    ///
    /// When the offer carries `dh_pub`, a fresh receiver key pair is generated
    /// immediately, the session is bound to this transfer id, and the PIN
    /// becomes the cryptographic SAS instead of the legacy identifier hash.
    pub async fn handle_offer(&self, offer: Offer, peer_addr: String) -> Result<OfferReply> {
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
        // Every file of a batch shares the batch id (and therefore its PIN).
        let pin_key = offer
            .batch_id
            .clone()
            .unwrap_or_else(|| format!("{}:{}", t.filename, offer.size));
        let id = t.id.clone();
        let receiver_id = self.my_id.read().await.clone();
        let dh_pub = match offer.dh_pub.as_deref() {
            Some(peer_public) => {
                let keypair = crate::crypto::KeyPair::generate()
                    .ok_or_else(|| anyhow!("cannot generate an ephemeral key"))?;
                let session = crate::crypto::derive_session(&keypair.secret, peer_public, &id)
                    .ok_or_else(|| anyhow!("offer carries an invalid dh_pub"))?;
                t.pin = session.sas_pin(&offer.sender_id, &receiver_id, &pin_key);
                t.session = Some(session);
                t.dh_public = Some(keypair.public_hex.clone());
                t.encrypted = true;
                Some(keypair.public_hex)
            }
            None => {
                t.pin = protocol::transfer_pin(&offer.sender_id, &receiver_id, &pin_key);
                None
            }
        };
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

        let auto_accept = *self.auto_accept.read().await;
        let trusted_accept = !auto_accept
            && *self.auto_accept_trusted_only.read().await
            && self
                .trusted_peers
                .read()
                .await
                .iter()
                .any(|peer| peer == &offer.sender_id);
        if auto_accept || trusted_accept {
            if let Err(e) = self.accept(&id, None).await {
                warn!("auto-accept failed for {id}: {e:#}");
            }
        } else if self.interactive_prompts.load(Ordering::Relaxed) {
            match view.batch_id.clone() {
                // A folder or multi-file selection: one question for the batch,
                // asked once the batch stops growing (see `notify::prompt_batch`).
                Some(batch_id) => {
                    // One task per batch: the first offer schedules the prompt,
                    // the rest just join the batch.
                    if self.begin_batch_prompt(&batch_id) {
                        crate::notify::prompt_batch(
                            self.clone(),
                            batch_id,
                            view.batch_name
                                .clone()
                                .unwrap_or_else(|| "files".to_string()),
                            view.peer_name.clone(),
                            view.pin.clone(),
                        );
                    }
                }
                None => crate::notify::prompt_incoming(
                    self.clone(),
                    id.clone(),
                    view.peer_name.clone(),
                    view.filename.clone(),
                    view.pin.clone(),
                ),
            }
        }
        Ok(OfferReply {
            transfer_id: id,
            dh_pub,
        })
    }

    /// The receiver's ephemeral public key for an offer (hex), if encrypted.
    pub async fn dh_public(&self, id: &str) -> Option<String> {
        self.transfers
            .read()
            .await
            .get(id)
            .and_then(|t| t.dh_public.clone())
    }

    /// The established payload session for a transfer, if encrypted.
    pub async fn session(&self, id: &str) -> Option<crate::crypto::SessionKey> {
        self.transfers
            .read()
            .await
            .get(id)
            .and_then(|t| t.session.clone())
    }

    /// True the first time a prompt is scheduled for `batch_id`.
    pub fn begin_batch_prompt(&self, batch_id: &str) -> bool {
        self.batch_prompts
            .lock()
            .map(|mut set| set.insert(batch_id.to_string()))
            .unwrap_or(false)
    }

    /// Forget a batch's prompt once its decision has been made (or it gave up).
    pub fn end_batch_prompt(&self, batch_id: &str) {
        if let Ok(mut set) = self.batch_prompts.lock() {
            set.remove(batch_id);
        }
    }

    /// Ids of the incoming transfers that are still waiting and belong to
    /// `batch_id` — the fan-out list for a batch accept/reject.
    pub async fn waiting_in_batch(&self, batch_id: &str) -> Vec<String> {
        let map = self.transfers.read().await;
        map.values()
            .filter(|t| {
                t.direction == Direction::Incoming
                    && t.status == Status::Waiting
                    && t.batch_id.as_deref() == Some(batch_id)
            })
            .map(|t| t.id.clone())
            .collect()
    }

    /// `(files, bytes)` of the given transfers, for a batch summary line.
    pub async fn batch_totals(&self, ids: &[String]) -> (usize, u64) {
        let map = self.transfers.read().await;
        let mut bytes = 0u64;
        let mut files = 0usize;
        for id in ids {
            if let Some(t) = map.get(id) {
                files += 1;
                bytes += t.size;
            }
        }
        (files, bytes)
    }

    /// Accept an incoming transfer: generate a one-time upload token.
    ///
    /// `save_dir` optionally overrides the download folder for this transfer
    /// (the folder is validated by the caller before accept).
    pub async fn accept(&self, id: &str, save_dir: Option<PathBuf>) -> Result<()> {
        let token = uuid::Uuid::new_v4().to_string();
        let mut snippet_completed = false;
        // Snapshot the default folder before taking the transfer lock (the
        // receive path documents the same lock order: download_dir, transfers).
        let default_dir = self.download_dir.read().await.clone();
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
            if t.text.is_some() {
                // A snippet has no file behind it: accept == done.
                snippet_completed = true;
                t.status = Status::Completed;
                t.progress = 1.0;
                t.save_dir = None;
                t.upload_token = None;
                t.part_path = None;
                t.saved_path = None;
            } else {
                t.status = Status::Accepted;
                let part = part_path_for(
                    &default_dir,
                    save_dir.as_deref(),
                    t.rel_path.as_deref(),
                    &t.filename,
                    &t.id,
                );
                t.save_dir = save_dir;
                t.upload_token = Some(token);
                t.part_path = Some(part);
                t.uploading = false;
            }
            t.updated_at = now_secs();
        }
        if snippet_completed {
            // receive_stream never runs for a snippet, so this is its completion.
            self.metrics.transfer_finished();
            self.emit_update(id, "transfer_completed").await;
            self.persist_history().await;
            info!("accepted transfer {id} (text snippet, completed)");
        } else {
            self.emit_update(id, "transfer_updated").await;
            info!("accepted transfer {id}");
        }
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
            t.upload_token = None;
            t.uploading = false;
            t.updated_at = now_secs();
        }
        self.emit_update(id, "transfer_updated").await;
        self.persist_history().await;
        info!("rejected transfer {id}");
        Ok(())
    }

    /// Verify the upload token **without consuming it**.
    ///
    /// The token stays valid while the transfer is `Accepted` or `Transferring`
    /// so an interrupted upload can query its offset and resume.
    pub async fn verify_upload_token(&self, id: &str, token: &str) -> bool {
        let map = self.transfers.read().await;
        let Some(t) = map.get(id) else {
            return false;
        };
        matches!(t.status, Status::Accepted | Status::Transferring)
            && token_matches(t.upload_token.as_deref(), token)
    }

    /// Atomically verify the token and claim the single upload stream slot.
    pub async fn begin_upload(&self, id: &str, token: &str) -> UploadStart {
        let mut map = self.transfers.write().await;
        let Some(t) = map.get_mut(id) else {
            return UploadStart::Unknown;
        };
        // Check the token first: a terminal transfer has burned it, and a
        // reused token must read as "invalid" rather than as a state conflict.
        if !token_matches(t.upload_token.as_deref(), token) {
            return UploadStart::InvalidToken;
        }
        if !matches!(t.status, Status::Accepted | Status::Transferring) {
            return UploadStart::NotUploadable;
        }
        if t.uploading {
            return UploadStart::Busy;
        }
        t.uploading = true;
        UploadStart::Started
    }

    /// Release the upload slot after a stream exits (success or failure).
    pub async fn reset_uploading(&self, id: &str) {
        self.with_transfer(id, |t| t.uploading = false).await;
    }

    /// Current byte length of a transfer's partial file (`0` when it has not
    /// been created yet). `None` means the transfer is unknown.
    pub async fn upload_offset(&self, id: &str) -> Option<u64> {
        let part = {
            let map = self.transfers.read().await;
            map.get(id)?.part_path.clone()
        };
        match part {
            Some(path) => Some(
                tokio::fs::metadata(&path)
                    .await
                    .map(|meta| meta.len())
                    .unwrap_or(0),
            ),
            None => Some(0),
        }
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
    /// then atomically rename into the download dir. `offset` resumes an
    /// interrupted upload from that byte position.
    pub async fn receive_stream<B>(&self, id: &str, body: B, offset: u64) -> Result<PathBuf>
    where
        B: tokio::io::AsyncRead + Unpin,
    {
        self::receiver::receive_stream(self, id, body, offset).await
    }
}

/// Constant-time token comparison (with a dummy comparison when no token is
/// set, so "never issued" and "wrong token" are not distinguishable by timing).
fn token_matches(expected: Option<&str>, provided: &str) -> bool {
    match expected {
        Some(expected) => constant_time_eq(expected.as_bytes(), provided.as_bytes()),
        None => {
            let _ = constant_time_eq(&[0u8; 32], provided.as_bytes());
            false
        }
    }
}

/// How many uploads may stream at the same time. Offers are registered for a
/// whole folder immediately; the data then flows through this many slots.
const MAX_CONCURRENT_UPLOADS: usize = 4;

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
            rel_path: None,
            batch_id: None,
            batch_name: None,
            text: None,
            dh_pub: None,
        }
    }

    #[tokio::test]
    async fn queued_batch_members_survive_while_a_sibling_streams() {
        let mgr = TransferManager::new(std::sync::Arc::new(crate::peers::PeerStore::new()));
        let offer = |name: &str| crate::protocol::Offer {
            sender_id: "s".into(),
            sender_name: "n".into(),
            filename: name.into(),
            size: 10,
            sha256: None,
            note: String::new(),
            rel_path: None,
            batch_id: Some("batch-1".into()),
            batch_name: Some("folder".into()),
            text: None,
            dh_pub: None,
        };

        let mut queued = Transfer::new_incoming(&offer("a.bin"), "10.0.0.2".into());
        queued.status = Status::Accepted;
        // Older than the accepted-upload timeout.
        queued.updated_at = now_secs() - (OFFER_TIMEOUT.as_secs() as i64 + 60);
        let mut streaming = Transfer::new_incoming(&offer("b.bin"), "10.0.0.2".into());
        streaming.status = Status::Transferring;
        streaming.updated_at = now_secs();
        let (queued_id, streaming_id) = (queued.id.clone(), streaming.id.clone());
        {
            let mut map = mgr.transfers.write().await;
            map.insert(queued_id.clone(), queued);
            map.insert(streaming_id.clone(), streaming);
        }

        mgr.expire_stale().await;
        assert_eq!(
            mgr.get(&queued_id).await.unwrap().status,
            "accepted",
            "a file queued behind the upload slots must not time out while its batch streams"
        );

        // The refresh above gave the queued file a fresh grace window, so it is
        // not expired immediately when its sibling finishes.
        mgr.expire_stale().await;
        assert_eq!(mgr.get(&queued_id).await.unwrap().status, "accepted");
        // Once the batch is idle and that grace window has passed, it expires.
        mgr.with_transfer(&streaming_id, |t| {
            t.status = Status::Completed;
            t.updated_at = now_secs();
        })
        .await;
        mgr.with_transfer(&queued_id, |t| {
            t.updated_at = now_secs() - (OFFER_TIMEOUT.as_secs() as i64 + 60);
        })
        .await;
        mgr.expire_stale().await;
        assert_eq!(mgr.get(&queued_id).await.unwrap().status, "failed");
    }

    #[tokio::test]
    async fn batch_prompts_are_scheduled_once() {
        let mgr = TransferManager::new(std::sync::Arc::new(crate::peers::PeerStore::new()));
        assert!(
            mgr.begin_batch_prompt("b1"),
            "the first offer schedules the prompt"
        );
        assert!(
            !mgr.begin_batch_prompt("b1"),
            "later files of the batch must not prompt again"
        );
        mgr.end_batch_prompt("b1");
        assert!(
            mgr.begin_batch_prompt("b1"),
            "a finished prompt frees the id again"
        );
    }

    #[test]
    fn speed_is_nonzero_for_sub_second_transfers() {
        let mut t = Transfer::new_outgoing(&peer(), &offer(1_000));
        t.status = Status::Transferring;
        t.bytes_done = 500;
        t.started_at = Some(std::time::Instant::now() - std::time::Duration::from_millis(50));
        let (_, speed, _) = t.progress_stats();
        assert!(
            speed > 0.0,
            "a 50 ms transfer must report a real speed, got {speed}"
        );
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
        t.started_at = Some(std::time::Instant::now() - std::time::Duration::from_secs(5));
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
        t.started_at = Some(std::time::Instant::now() - std::time::Duration::from_secs(2));
        let (_, _, eta) = t.progress_stats();
        assert_eq!(eta, None);
    }

    #[tokio::test]
    async fn history_round_trips_and_keeps_the_newest_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(HISTORY_FILE_NAME);
        let mgr = TransferManager::new(std::sync::Arc::new(crate::peers::PeerStore::new()));
        mgr.set_history_path(Some(path.clone())).await;
        {
            let mut map = mgr.transfers.write().await;
            for i in 0..(HISTORY_LIMIT + 5) {
                let mut t = Transfer::new_outgoing(&peer(), &offer(10));
                t.status = Status::Completed;
                t.created_at = 1_000 + i as i64;
                t.updated_at = t.created_at;
                t.bytes_done = 10;
                map.insert(t.id.clone(), t);
            }
        }
        mgr.persist_history().await;

        let raw = std::fs::read_to_string(&path).unwrap();
        let entries: Vec<HistoryEntry> = serde_json::from_str(&raw).unwrap();
        assert_eq!(entries.len(), HISTORY_LIMIT);
        assert_eq!(
            entries[0].updated_at,
            1_000 + HISTORY_LIMIT as i64 + 4,
            "newest entry comes first"
        );
        assert!(
            entries.iter().all(|e| e.updated_at >= 1_005),
            "only the oldest five were dropped"
        );

        // A fresh manager restores the same window.
        let restored = TransferManager::new(std::sync::Arc::new(crate::peers::PeerStore::new()));
        restored.set_history_path(Some(path.clone())).await;
        assert_eq!(restored.load_history(&path).await.unwrap(), HISTORY_LIMIT);
        assert_eq!(restored.list().await.len(), HISTORY_LIMIT);

        // Clearing removes both the records and the file on disk.
        assert_eq!(restored.clear_terminal().await, HISTORY_LIMIT);
        assert!(!path.exists(), "clear must wipe history.json");
    }
}
