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
    pub cfg: Arc<RwLock<Config>>,
    pub peers: Arc<PeerStore>,
    pub transfers: TransferManager,
    pub events: EventBus,
    /// Explicit config dir when the node runs with a custom one (staging and
    /// instance bookkeeping live there).
    pub config_dir: Option<PathBuf>,
}

impl AppState {
    pub fn new(cfg: Config, peers: Arc<PeerStore>, transfers: TransferManager) -> Self {
        let events = transfers.events.clone();
        Self {
            cfg: Arc::new(RwLock::new(cfg)),
            peers,
            transfers,
            events,
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
        .route("/api/settings", get(handlers::api_get_settings))
        .route("/api/settings", post(handlers::api_save_settings))
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
