//! axum HTTP server: local API, wire protocol (offer/upload), SSE, embedded UI.
//!
//! The server is split into [`handlers`] (route handlers) and this module
//! (router construction + shared state).

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};
use axum::Router;
use tokio::sync::RwLock;
use tower_http::trace::TraceLayer;

use crate::config::Config;
use crate::events::EventBus;
use crate::peers::PeerStore;
use crate::transfer::TransferManager;

pub mod handlers;

/// Shared application state handed to every handler.
#[derive(Clone)]
pub struct AppState {
    /// The config this run actually uses (includes run-only CLI overrides).
    pub cfg: Arc<RwLock<Config>>,
    /// The config as it exists **on disk**. Run-only overrides (`--port`,
    /// `--downloads`, `--name`) must never be written back, so persistence
    /// writes this copy; settings changed in the UI update both.
    pub disk_cfg: Arc<RwLock<Config>>,
    pub peers: Arc<PeerStore>,
    pub transfers: TransferManager,
    pub events: EventBus,
    /// Requests a graceful shutdown of this node; the local-only
    /// `POST /api/quit` uses it so a freshly installed build can retire an older
    /// running copy. Set by `start_node`.
    pub shutdown: Arc<std::sync::OnceLock<tokio::sync::watch::Sender<bool>>>,
    /// Desktop hook raising the app window (`POST /api/window/focus`).
    pub on_focus: Option<crate::app::FocusHook>,
    /// Devices the local UI has selected. The desktop window cannot expose its
    /// state to Rust, so a native file drop sends to whatever is selected here.
    pub ui_selection: Arc<RwLock<Vec<String>>>,
    /// Explicit config dir when the node runs with a custom one (staging and
    /// instance bookkeeping live there).
    pub config_dir: Option<PathBuf>,
}

impl AppState {
    pub fn new(cfg: Config, peers: Arc<PeerStore>, transfers: TransferManager) -> Self {
        let events = transfers.events.clone();
        Self {
            cfg: Arc::new(RwLock::new(cfg.clone())),
            disk_cfg: Arc::new(RwLock::new(cfg)),
            peers,
            transfers,
            events,
            shutdown: Arc::new(std::sync::OnceLock::new()),
            on_focus: None,
            ui_selection: Arc::new(RwLock::new(Vec::new())),
            config_dir: None,
        }
    }
}

/// Build the full axum router.
pub fn router(state: AppState) -> Router {
    Router::new()
        // Local UI + API
        .route("/", get(handlers::index))
        .route("/api/info", get(handlers::api_info))
        .route("/api/peers", get(handlers::api_peers))
        .route("/api/peers/manual", post(handlers::api_add_manual_peer))
        .route("/api/transfers", get(handlers::api_transfers))
        // Streaming uploads opt out of the small global body limit.
        .route(
            "/api/send",
            post(handlers::api_send).layer(DefaultBodyLimit::disable()),
        )
        .route(
            "/api/transfers/{id}/cancel",
            post(handlers::api_cancel_transfer),
        )
        .route(
            "/api/transfers/{id}/accept",
            post(handlers::api_accept_transfer),
        )
        .route(
            "/api/transfers/{id}/reject",
            post(handlers::api_reject_transfer),
        )
        .route("/api/events", get(handlers::api_events_sse))
        .route("/api/metrics", get(handlers::api_metrics))
        .route("/api/batches/{id}/accept", post(handlers::api_accept_batch))
        .route("/api/batches/{id}/reject", post(handlers::api_reject_batch))
        .route("/api/batches/{id}/cancel", post(handlers::api_cancel_batch))
        .route("/api/settings", get(handlers::api_get_settings))
        .route("/api/settings", post(handlers::api_save_settings))
        // Local-only helpers used by the desktop app: retire an older copy and
        // raise the window this process already has.
        .route("/api/quit", post(handlers::api_quit))
        .route("/api/window/focus", post(handlers::api_focus))
        // Local-only: the UI tells the node which devices are selected, and the
        // desktop window sends dropped paths to them.
        .route("/api/ui/selection", post(handlers::api_ui_selection))
        .route("/api/send-path", post(handlers::api_send_path))
        .route("/api/send-text", post(handlers::api_send_text))
        .route("/api/downloads/open", post(handlers::api_open_downloads))
        // Folder picker (machine-local): list subdirectories / create one.
        .route("/api/fs/list", get(handlers::api_list_dirs))
        .route("/api/fs/mkdir", post(handlers::api_mkdir))
        // Wire protocol (device to device)
        .route("/api/v1/offer", post(handlers::v1_offer))
        .route("/api/v1/offer/{id}", get(handlers::v1_offer_status))
        .route(
            "/api/v1/upload/{id}",
            post(handlers::v1_upload).layer(DefaultBodyLimit::disable()),
        )
        // Static UI
        .fallback(get(handlers::ui_static))
        // JSON bodies stay small; the two streaming routes above disable this
        // limit for their file payloads.
        .layer(DefaultBodyLimit::max(64 * 1024))
        // Reject cross-origin browser requests and DNS-rebinding Host headers.
        .layer(axum::middleware::from_fn(handlers::guard_request))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
