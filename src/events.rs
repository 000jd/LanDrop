//! Server-Sent Events event bus.
//!
//! A simple broadcast channel that fan-outs JSON-serializable events to all
//! connected SSE clients. Events are used by the UI for live peer lists,
//! transfer status and progress.
//!
//! The bus broadcasts `(event_name, json_payload)` pairs; the SSE handler
//! converts those into proper `event:` / `data:` frames.

use serde::Serialize;
use tokio::sync::{broadcast, watch};
use tracing::debug;

/// Capacity of the event channel; SSE clients that are too slow are dropped.
///
/// Progress is throttled per transfer, but a few transfers plus peer updates
/// can easily produce dozens of events per second, so leave plenty of room
/// before a client is considered lagged.
const CHANNEL_CAPACITY: usize = 1024;

/// A single event on the bus: a name plus a JSON payload.
#[derive(Debug, Clone)]
pub struct Event {
    pub name: String,
    pub json: String,
}

/// Live event bus shared across the app (cheap to clone).
#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<Event>,
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        Self { tx }
    }

    /// Broadcast an event with the given name. Serialization is best-effort:
    /// failures are logged, never panic.
    pub fn send<T: Serialize>(&self, name: &str, payload: &T) {
        let json = match serde_json::to_string(payload) {
            Ok(j) => j,
            Err(e) => {
                debug!("event {name}: serialization failed: {e}");
                return;
            }
        };
        let _ = self.tx.send(Event {
            name: name.to_string(),
            json,
        });
    }

    /// Subscribe to the stream (new subscribers miss past events).
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

/// Shared shutdown signal: `true` means "stop now".
pub type ShutdownRx = watch::Receiver<bool>;

/// Create a shutdown channel pair.
pub fn shutdown_channel() -> (watch::Sender<bool>, ShutdownRx) {
    watch::channel(false)
}

/// Wait until the shutdown signal fires or the runtime is cancelled.
pub async fn wait_shutdown(rx: &mut ShutdownRx) {
    if *rx.borrow() {
        return;
    }
    let _ = rx.changed().await;
}

/// Block forever while keeping the runtime alive (used by tests).
pub async fn park() {
    std::future::pending::<()>().await
}
