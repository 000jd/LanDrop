//! HTTP handlers for the local API, the wire protocol and the embedded UI.
//!
//! Handlers are deliberately thin: they parse/extract, call into
//! [`crate::transfer`] / [`crate::peers`] / [`crate::config`], and shape
//! responses. Streaming happens in the transfer modules.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, Multipart, Path, Query, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response, Sse};
use axum::Json;
use futures_util::stream::{StreamExt, TryStreamExt};
use rust_embed::RustEmbed;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::peers::{parse_peer_addr, Peer};
use crate::protocol::{AcceptInfo, Offer, TOKEN_HEADER, UPLOAD_TOKEN_HEADER};
use crate::server::AppState;
use crate::transfer::sender::spawn_send;

/// Version reported by /api/info and the UI.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Content-Security-Policy for the bundled UI (no remote origins).
const UI_CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'";

/// Cap on persisted manual peers.
const MAX_MANUAL_PEERS: usize = 64;
/// Refuse to stage uploads when free disk space drops below this.
const MIN_FREE_BYTES: u64 = 256 * 1024 * 1024;
/// How often the staging writer re-checks free disk space.
const DISK_CHECK_INTERVAL: u64 = 64 * 1024 * 1024;

/// Private per-user staging directory for outgoing files. Lives under the
/// node's config dir when one is known, else the default config location.
pub(crate) fn staging_dir_for(config_dir: Option<&std::path::Path>) -> PathBuf {
    config_dir
        .map(std::path::Path::to_path_buf)
        .or_else(crate::config::config_dir)
        .unwrap_or_else(std::env::temp_dir)
        .join("outbox")
}

/// Best-effort removal of a staged file (missing files are fine).
async fn cleanup_file(path: &std::path::Path) {
    if let Err(e) = tokio::fs::remove_file(path).await {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!("failed to remove staged file {}: {e}", path.display());
        }
    }
}

/// Best-effort removal of whatever file is currently staged.
async fn cleanup_staged(meta: &Option<(String, u64, PathBuf, Option<String>)>) {
    if let Some((_, _, path, _)) = meta {
        cleanup_file(path).await;
    }
}

// ---------------------------------------------------------------------------
// UI + static assets (embedded via rust-embed)
// ---------------------------------------------------------------------------

#[derive(RustEmbed)]
#[folder = "ui/"]
pub struct Assets;

/// Serve the index page.
pub async fn index() -> Response {
    match Assets::get("index.html") {
        Some(f) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                (header::CONTENT_SECURITY_POLICY, UI_CSP),
                // Revalidate every load: an upgraded binary must not be masked
                // by a cached page in the browser/webview.
                (header::CACHE_CONTROL, "no-cache"),
            ],
            f.data.into_owned(),
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "index.html not embedded").into_response(),
    }
}

/// Serve embedded static files (css/js).
pub async fn ui_static(uri: axum::http::Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if path.is_empty() {
        return index().await;
    }
    match Assets::get(path) {
        Some(f) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, mime_for(path)),
                (header::CONTENT_SECURITY_POLICY, UI_CSP),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            f.data.into_owned(),
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

fn mime_for(path: &str) -> &'static str {
    if path.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if path.ends_with(".js") {
        "application/javascript; charset=utf-8"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else if path.ends_with(".png") {
        "image/png"
    } else if path.ends_with(".html") {
        "text/html; charset=utf-8"
    } else {
        "application/octet-stream"
    }
}

// ---------------------------------------------------------------------------
// Local API
// ---------------------------------------------------------------------------

pub async fn api_info(State(state): State<AppState>) -> Response {
    let cfg = state.cfg.read().await;
    let port = *state.transfers.my_port.read().await;
    let ips = crate::discovery::local_ipv4_addrs()
        .into_iter()
        .map(|ip| ip.to_string())
        .collect::<Vec<_>>();
    Json(json!({
        "id": cfg.id,
        "name": cfg.name,
        "port": port,
        "downloads": cfg.downloads,
        "version": VERSION,
        "platform": std::env::consts::OS,
        "ips": ips,
    }))
    .into_response()
}

pub async fn api_peers(State(state): State<AppState>) -> Response {
    let my_id = state.transfers.my_id.read().await.clone();
    let my_port = *state.transfers.my_port.read().await;
    let my_ips: Vec<String> = crate::discovery::local_ipv4_addrs()
        .into_iter()
        .map(|ip| ip.to_string())
        .collect();
    let peers = state.peers.list_others(&my_id, my_port, &my_ips).await;
    Json(peers).into_response()
}

/// Add a manual peer by `ip:port` (persisted in config).
pub async fn api_add_manual_peer(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let address = body["address"].as_str().unwrap_or("").trim().to_string();
    if address.is_empty() {
        return err(StatusCode::BAD_REQUEST, "address is required");
    }
    let default_port = *state.transfers.my_port.read().await;
    let socket = match parse_peer_addr(&address, default_port) {
        Ok(s) => s,
        Err(e) => return err(StatusCode::BAD_REQUEST, e.to_string()),
    };

    // Upsert into the store (manual peers never expire).
    let name = format!("{}:{}", socket.ip(), socket.port());
    let peer = Peer {
        id: format!("manual-{}", socket),
        name,
        addr: socket.ip().to_string(),
        port: socket.port(),
        source: "manual".into(),
    };
    let changed = state.peers.upsert(peer.clone()).await;

    // Persist to config (bounded list; duplicates ignored).
    let addr_str = format!("{}:{}", socket.ip(), socket.port());
    {
        let mut cfg = state.cfg.write().await;
        if !cfg.manual_peers.contains(&addr_str) {
            if cfg.manual_peers.len() >= MAX_MANUAL_PEERS {
                return err(
                    StatusCode::BAD_REQUEST,
                    format!("too many manual peers (max {MAX_MANUAL_PEERS})"),
                );
            }
            cfg.manual_peers.push(addr_str.clone());
        }
    }
    if let Err(e) = persist_config(&state).await {
        tracing::warn!("failed to persist config: {e}");
    }

    if changed {
        state.events.send("peer_updated", &peer);
    }
    Json(json!({ "ok": true, "peer": peer })).into_response()
}

pub async fn api_transfers(State(state): State<AppState>) -> Response {
    Json(state.transfers.list().await).into_response()
}

/// Receive a file from the local UI and start an outgoing transfer.
///
/// Accessible from any device on the LAN (so a phone can open the UI and
/// send). The wire protocol still requires an Accept click on the receiver;
/// a shared token can be configured for extra protection.
///
/// The uploaded file is streamed to a private staging file while hashing it —
/// never buffered in memory — and removed once the transfer finishes.
pub async fn api_send(State(state): State<AppState>, mut multipart: Multipart) -> Response {
    let mut target: Option<String> = None;
    let mut file_meta: Option<(String, u64, PathBuf, Option<String>)> = None; // (name, size, staged, sha)

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => {
                cleanup_staged(&file_meta).await;
                return err(
                    StatusCode::BAD_REQUEST,
                    format!("invalid multipart body: {e}"),
                );
            }
        };
        let name = field.name().unwrap_or("").to_string();
        if name == "target" {
            match field.text().await {
                Ok(v) => target = Some(v),
                Err(e) => {
                    cleanup_staged(&file_meta).await;
                    return err(
                        StatusCode::BAD_REQUEST,
                        format!("invalid target field: {e}"),
                    );
                }
            }
            continue;
        }
        if name != "file" {
            continue;
        }
        if file_meta.is_some() {
            cleanup_staged(&file_meta).await;
            return err(StatusCode::BAD_REQUEST, "only one file per transfer");
        }

        let filename = crate::protocol::sanitize_filename(field.file_name().unwrap_or("unnamed"));

        // Stage into the private outbox dir, streaming + hashing in one pass.
        let dir = staging_dir_for(state.config_dir.as_deref());
        if let Err(e) = tokio::fs::create_dir_all(&dir).await {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to create staging dir: {e}"),
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = tokio::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).await;
        }
        let staged = dir.join(format!("{}-{filename}", uuid::Uuid::new_v4()));
        let mut opts = tokio::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        opts.mode(0o600);
        let mut file = match opts.open(&staged).await {
            Ok(f) => f,
            Err(e) => {
                return err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("failed to stage file: {e}"),
                )
            }
        };

        let mut hasher = Sha256::new();
        let mut size: u64 = 0;
        let mut next_disk_check: u64 = 0;
        let mut stream = field;
        loop {
            match stream.chunk().await {
                Ok(Some(c)) => {
                    if size >= next_disk_check {
                        let free = crate::util::free_space(&dir);
                        if free > 0 && free < MIN_FREE_BYTES {
                            cleanup_file(&staged).await;
                            return err(
                                StatusCode::INSUFFICIENT_STORAGE,
                                "not enough free disk space to stage this file",
                            );
                        }
                        next_disk_check = size + DISK_CHECK_INTERVAL;
                    }
                    if let Err(e) = file.write_all(&c).await {
                        cleanup_file(&staged).await;
                        return err(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("write failed: {e}"),
                        );
                    }
                    hasher.update(&c);
                    size += c.len() as u64;
                }
                Ok(None) => break,
                Err(e) => {
                    cleanup_file(&staged).await;
                    return err(StatusCode::BAD_REQUEST, format!("upload failed: {e}"));
                }
            }
        }
        if let Err(e) = file.flush().await {
            cleanup_file(&staged).await;
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("flush failed: {e}"),
            );
        }
        if let Err(e) = file.sync_all().await {
            cleanup_file(&staged).await;
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("sync failed: {e}"),
            );
        }
        drop(file);
        let sha = format!("{:x}", hasher.finalize());
        file_meta = Some((filename, size, staged, Some(sha)));
    }

    let target = match target {
        Some(t) if !t.trim().is_empty() => t.trim().to_string(),
        _ => {
            cleanup_staged(&file_meta).await;
            return err(StatusCode::BAD_REQUEST, "target is required");
        }
    };
    let (filename, size, staged, sha) = match file_meta {
        Some(f) => f,
        None => return err(StatusCode::BAD_REQUEST, "file is required"),
    };

    // Resolve the target: peer id or manual address.
    let peer = match state.peers.find(&target).await {
        Some(p) => p,
        None => match parse_peer_addr(&target, *state.transfers.my_port.read().await) {
            Ok(s) => Peer {
                id: format!("manual-{}", s),
                name: s.ip().to_string(),
                addr: s.ip().to_string(),
                port: s.port(),
                source: "manual".into(),
            },
            Err(e) => {
                cleanup_file(&staged).await;
                return err(StatusCode::BAD_REQUEST, format!("unknown target: {e}"));
            }
        },
    };

    let id = match spawn_send(&state.transfers, peer, filename, size, sha, staged.clone()).await {
        Ok(id) => id,
        Err(e) => {
            cleanup_file(&staged).await;
            return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
        }
    };

    Json(json!({ "transfer_id": id })).into_response()
}

/// Cancel a transfer if possible.
pub async fn api_cancel_transfer(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    if state.transfers.cancel(&id).await {
        Json(json!({ "ok": true })).into_response()
    } else {
        err(
            StatusCode::CONFLICT,
            "cannot cancel transfer in its current state",
        )
    }
}

/// Body for `POST /api/transfers/{id}/accept` (optional).
#[derive(Debug, Deserialize, Default)]
pub struct AcceptBody {
    #[serde(default)]
    save_dir: Option<String>,
}

/// Accept an incoming transfer (UI action).
///
/// May carry a per-transfer save folder: `{"save_dir": "/abs/path"}`.
/// Choosing a folder is restricted to this machine.
pub async fn api_accept_transfer(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let requested = if body.is_empty() {
        None
    } else {
        match serde_json::from_slice::<AcceptBody>(&body) {
            Ok(b) => b.save_dir,
            Err(e) => return err(StatusCode::BAD_REQUEST, format!("invalid accept body: {e}")),
        }
    };
    let save_dir = match requested {
        Some(raw) if !raw.trim().is_empty() => {
            if !is_local_peer(&addr) {
                return err(
                    StatusCode::FORBIDDEN,
                    "choosing a save folder is only available from this machine",
                );
            }
            match tokio::task::spawn_blocking(move || validate_dir(&raw)).await {
                Ok(Ok(p)) => Some(p),
                Ok(Err(e)) => return err(StatusCode::BAD_REQUEST, e),
                Err(e) => {
                    return err(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("folder validation failed: {e}"),
                    )
                }
            }
        }
        _ => None,
    };
    match state.transfers.accept(&id, save_dir).await {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => err(StatusCode::CONFLICT, e.to_string()),
    }
}

/// Reject an incoming transfer (local UI action).
pub async fn api_reject_transfer(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    match state.transfers.reject(&id).await {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => err(StatusCode::CONFLICT, e.to_string()),
    }
}

/// SSE stream of live events.
pub async fn api_events_sse(
    State(state): State<AppState>,
) -> Sse<
    impl tokio_stream::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
> {
    let rx = state.events.subscribe();
    let stream = tokio_stream::wrappers::BroadcastStream::new(rx).filter_map(|item| async {
        match item {
            Ok(ev) => Some(Ok::<_, std::convert::Infallible>(
                axum::response::sse::Event::default()
                    .event(ev.name)
                    .data(ev.json),
            )),
            // Lagged: the client missed events; ask it to resync.
            Err(_) => Some(Ok(axum::response::sse::Event::default()
                .event("resync")
                .data("{}"))),
        }
    });
    Sse::new(stream).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    )
}

#[derive(Debug, Deserialize)]
pub struct SettingsBody {
    name: Option<String>,
    port: Option<u16>,
    downloads: Option<String>,
    auto_accept: Option<bool>,
    ask_save_path: Option<bool>,
    start_on_login: Option<bool>,
}

pub async fn api_get_settings(State(state): State<AppState>) -> Response {
    let port = *state.transfers.my_port.read().await;
    let cfg = state.cfg.read().await;
    Json(json!({
        "id": cfg.id,
        "name": cfg.name,
        "port": port,
        "downloads": cfg.downloads,
        "auto_accept": cfg.auto_accept,
        "ask_save_path": cfg.ask_save_path,
        "start_on_login": cfg.start_on_login || crate::autostart::is_enabled(),
    }))
    .into_response()
}

pub async fn api_save_settings(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<SettingsBody>,
) -> Response {
    if !is_local_peer(&addr) {
        return err(
            StatusCode::FORBIDDEN,
            "settings can only be changed from this machine",
        );
    }
    let new_name;
    let new_downloads;
    let new_auto_accept;
    let new_ask_save_path;
    {
        let mut cfg = state.cfg.write().await;
        if let Some(name) = &body.name {
            let name = crate::protocol::truncate_chars(name.trim(), 64);
            if !name.is_empty() {
                cfg.name = name;
            }
        }
        if let Some(port) = body.port {
            if port > 0 {
                cfg.port = port;
            }
        }
        if let Some(dl) = &body.downloads {
            let dl = dl.trim().to_string();
            if !dl.is_empty() {
                cfg.downloads = dl;
            }
        }
        if let Some(a) = body.auto_accept {
            cfg.auto_accept = a;
        }
        if let Some(a) = body.ask_save_path {
            cfg.ask_save_path = a;
        }
        if let Some(on) = body.start_on_login {
            cfg.start_on_login = on;
        }
        new_name = cfg.name.clone();
        new_downloads = cfg.downloads.clone();
        new_auto_accept = cfg.auto_accept;
        new_ask_save_path = cfg.ask_save_path;
    }
    // Propagate to live state. Port changes apply on restart — do not
    // advertise a port we are not bound to.
    *state.transfers.my_name.write().await = new_name;
    *state.transfers.download_dir.write().await = PathBuf::from(new_downloads);
    *state.transfers.auto_accept.write().await = new_auto_accept;
    *state.transfers.ask_save_path.write().await = new_ask_save_path;
    if let Some(on) = body.start_on_login {
        if let Err(e) = crate::autostart::set_enabled(on) {
            tracing::warn!("failed to update login autostart: {e:#}");
        }
    }

    match persist_config(&state).await {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to persist settings: {e}"),
        ),
    }
}

/// Open the download folder with the system file manager.
pub async fn api_open_downloads(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Response {
    if !is_local_peer(&addr) {
        return err(
            StatusCode::FORBIDDEN,
            "opening the download folder is only available from this machine",
        );
    }
    let dir = state.cfg.read().await.downloads.clone();
    #[cfg(target_os = "linux")]
    {
        match crate::util::spawn_detached(std::process::Command::new("xdg-open").arg(&dir)) {
            Ok(()) => Json(json!({ "ok": true, "path": dir })).into_response(),
            Err(e) => err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to open folder: {e}"),
            ),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = dir;
        err(
            StatusCode::NOT_IMPLEMENTED,
            "opening the download folder is only implemented on Linux",
        )
    }
}

// ---------------------------------------------------------------------------
// Wire protocol (device to device)
// ---------------------------------------------------------------------------

/// POST /api/v1/offer — receive a transfer offer from another device.
pub async fn v1_offer(
    State(state): State<AppState>,
    headers: HeaderMap,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(offer): Json<Offer>,
) -> Response {
    if let Err(e) = check_shared_token(&state, &headers).await {
        return err(e.0, e.1);
    }
    // Peer address for logging/identification.
    let peer_addr = addr.ip().to_string();
    match state.transfers.handle_offer(offer, peer_addr).await {
        Ok(id) => Json(json!({ "transfer_id": id, "status": "pending" })).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e.to_string()),
    }
}

/// GET /api/v1/offer/{id} — poll acceptance status.
pub async fn v1_offer_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(e) = check_shared_token(&state, &headers).await {
        return err(e.0, e.1);
    }
    match state.transfers.offer_status(&id).await {
        Some((status, url, token)) => {
            let body = if status == "accepted" {
                serde_json::to_value(AcceptInfo {
                    status,
                    upload_url: url.unwrap_or_default(),
                    upload_token: token.unwrap_or_default(),
                })
                .unwrap_or_default()
            } else {
                json!({ "status": status })
            };
            Json(body).into_response()
        }
        None => err(StatusCode::NOT_FOUND, "unknown transfer"),
    }
}

/// POST /api/v1/upload/{id} — stream the actual file.
pub async fn v1_upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Body,
) -> Response {
    if let Err(e) = check_shared_token(&state, &headers).await {
        return err(e.0, e.1);
    }
    // One-time upload token required.
    let token = match headers
        .get(UPLOAD_TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
    {
        Some(t) => t.to_string(),
        None => return err(StatusCode::UNAUTHORIZED, "missing upload token"),
    };
    // Atomically verify and consume the one-time token.
    if !state.transfers.take_upload_token(&id, &token).await {
        return err(StatusCode::UNAUTHORIZED, "invalid upload token");
    }

    // Stream body -> temp file -> verify -> rename.
    let stream = body
        .into_data_stream()
        .map_err(|e| std::io::Error::other(format!("{e}")));
    let reader = tokio_util::io::StreamReader::new(stream);
    match state.transfers.receive_stream(&id, reader).await {
        Ok(path) => Json(json!({ "ok": true, "saved": path })).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Validate the optional shared token header on wire-protocol requests.
/// When a token is configured, requests without it are rejected.
async fn check_shared_token(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<(), (StatusCode, String)> {
    let configured = state.cfg.read().await.token.clone();
    if configured.is_empty() {
        return Ok(());
    }
    let provided = headers
        .get(TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if provided != configured {
        Err((
            StatusCode::UNAUTHORIZED,
            "invalid or missing X-LanDrop-Token".to_string(),
        ))
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Folder picker (machine-local)
// ---------------------------------------------------------------------------

/// How many subdirectories a single listing returns.
const MAX_FS_ENTRIES: usize = 2000;

#[derive(Debug, Deserialize)]
pub struct FsListQuery {
    path: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct MkdirBody {
    parent: String,
    name: String,
}

/// `GET /api/fs/list?path=…` — list subdirectories for the folder picker.
pub async fn api_list_dirs(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Query(q): Query<FsListQuery>,
) -> Response {
    if !is_local_peer(&addr) {
        return err(
            StatusCode::FORBIDDEN,
            "browsing folders is only available from this machine",
        );
    }
    let downloads = state.cfg.read().await.downloads.clone();
    // Without an explicit path, start where files are actually saved: the home
    // directory does not exist on Android, and starting at the current folder
    // is the friendlier behaviour anyway.
    let start = q.path.clone().filter(|p| !p.trim().is_empty());
    let fallback = PathBuf::from(&downloads);
    match tokio::task::spawn_blocking(move || list_dirs(start.as_deref(), &fallback)).await {
        Ok(Ok((path, parent, dirs, truncated))) => {
            let places = fs_places(&downloads);
            Json(json!({
                "path": path,
                "parent": parent,
                "dirs": dirs,
                "places": places,
                "truncated": truncated,
            }))
            .into_response()
        }
        Ok(Err(e)) => err(StatusCode::BAD_REQUEST, e),
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("listing failed: {e}"),
        ),
    }
}

/// `POST /api/fs/mkdir {parent, name}` — create a folder from the picker.
pub async fn api_mkdir(
    State(_state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<MkdirBody>,
) -> Response {
    if !is_local_peer(&addr) {
        return err(
            StatusCode::FORBIDDEN,
            "creating folders is only available from this machine",
        );
    }
    match tokio::task::spawn_blocking(move || create_subdir(&body.parent, &body.name)).await {
        Ok(Ok(path)) => Json(json!({ "path": path })).into_response(),
        Ok(Err(e)) => err(StatusCode::BAD_REQUEST, e),
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("creating folder failed: {e}"),
        ),
    }
}

/// Directory listing: `(canonical path, parent, entries, truncated)`.
fn list_dirs(
    input: Option<&str>,
    fallback: &std::path::Path,
) -> Result<(String, Option<String>, Vec<serde_json::Value>, bool), String> {
    let requested = match input {
        Some(p) if !p.trim().is_empty() => PathBuf::from(p.trim()),
        _ => {
            if fallback.is_dir() {
                fallback.to_path_buf()
            } else {
                dirs::home_dir().ok_or("no folder to start from")?
            }
        }
    };
    let path = std::fs::canonicalize(&requested)
        .map_err(|e| format!("cannot open {}: {e}", requested.display()))?;
    if !path.is_dir() {
        return Err(format!("{} is not a directory", path.display()));
    }
    let reader =
        std::fs::read_dir(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mut dirs: Vec<serde_json::Value> = Vec::new();
    let mut truncated = false;
    for entry in reader.flatten() {
        if dirs.len() >= MAX_FS_ENTRIES {
            truncated = true;
            break;
        }
        let child = entry.path();
        if !child.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        dirs.push(json!({ "name": name, "path": child.to_string_lossy() }));
    }
    dirs.sort_by(|a, b| {
        let an = a["name"].as_str().unwrap_or("").to_lowercase();
        let bn = b["name"].as_str().unwrap_or("").to_lowercase();
        an.cmp(&bn)
    });
    let parent = path.parent().map(|p| p.to_string_lossy().into_owned());
    Ok((path.to_string_lossy().into_owned(), parent, dirs, truncated))
}

/// Shortcut locations offered by the picker.
fn fs_places(default_downloads: &str) -> Vec<serde_json::Value> {
    let mut places: Vec<(String, PathBuf)> = Vec::new();
    if let Some(home) = dirs::home_dir() {
        places.push(("Home".into(), home));
    }
    if let Some(d) = dirs::download_dir() {
        places.push(("Downloads".into(), d));
    }
    if let Some(d) = dirs::document_dir() {
        places.push(("Documents".into(), d));
    }
    if let Some(d) = dirs::desktop_dir() {
        places.push(("Desktop".into(), d));
    }
    let configured = PathBuf::from(default_downloads);
    if configured.is_dir() {
        places.push(("LanDrop folder".into(), configured));
    }
    let mut seen = std::collections::HashSet::new();
    places
        .into_iter()
        .filter(|(_, p)| seen.insert(p.clone()) && p.is_dir())
        .map(|(name, p)| json!({ "name": name, "path": p.to_string_lossy() }))
        .collect()
}

/// Create one subdirectory (used by the picker's "new folder" field).
fn create_subdir(parent: &str, name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("folder name is required".into());
    }
    if name.chars().count() > 100 {
        return Err("folder name is too long".into());
    }
    if name.contains(['/', '\\', '\0']) || name == "." || name == ".." {
        return Err("invalid folder name".into());
    }
    let parent = std::fs::canonicalize(parent.trim())
        .map_err(|e| format!("cannot open {}: {e}", parent.trim()))?;
    if !parent.is_dir() {
        return Err(format!("{} is not a directory", parent.display()));
    }
    let target = parent.join(name);
    std::fs::create_dir(&target).map_err(|e| format!("cannot create {}: {e}", target.display()))?;
    Ok(target.to_string_lossy().into_owned())
}

/// Validate a user-chosen save folder (absolute, existing, canonicalized).
fn validate_dir(raw: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(raw.trim());
    if !path.is_absolute() {
        return Err("save folder must be an absolute path".into());
    }
    let canon =
        std::fs::canonicalize(&path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    if !canon.is_dir() {
        return Err(format!("{} is not a directory", canon.display()));
    }
    Ok(canon)
}

/// Persist the current config next to the node's config file (honours a
/// custom `--config-dir`, falling back to the default location).
async fn persist_config(state: &AppState) -> Result<(), String> {
    let snapshot = state.cfg.read().await.clone();
    let dir = state.config_dir.clone();
    match tokio::task::spawn_blocking(move || match dir {
        Some(dir) => snapshot.save_to(&dir.join(crate::config::CONFIG_FILE_NAME)),
        None => snapshot.save(),
    })
    .await
    {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e.to_string()),
        Err(e) => Err(e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Request guards
// ---------------------------------------------------------------------------

/// Reject cross-origin browser requests and non-IP `Host` headers.
///
/// Browsers always send `Host`; requiring it to be an IP literal or
/// `localhost` blocks DNS-rebinding attacks, and rejecting mismatched
/// `Origin` blocks cross-site requests from pages the user happens to visit.
/// Non-browser clients (curl, the device-to-device protocol) send no
/// `Origin` and are unaffected.
pub async fn guard_request(req: Request, next: Next) -> Response {
    let host = header_str(req.headers(), header::HOST).unwrap_or_default();
    let host_part = host_name(&host);
    if !host_part.eq_ignore_ascii_case("localhost")
        && host_part.parse::<std::net::IpAddr>().is_err()
    {
        return err(StatusCode::FORBIDDEN, "unexpected Host header");
    }
    if let Some(origin) = header_str(req.headers(), header::ORIGIN) {
        let origin_host = origin
            .split_once("://")
            .map(|(_, rest)| rest)
            .unwrap_or(&origin);
        let origin_host = origin_host.split('/').next().unwrap_or("");
        if !host_name(origin_host).eq_ignore_ascii_case(host_part) {
            return err(StatusCode::FORBIDDEN, "cross-origin request rejected");
        }
    }

    let is_api = req.uri().path().starts_with("/api/");
    let mut resp = next.run(req).await;
    if is_api {
        resp.headers_mut().insert(
            header::CACHE_CONTROL,
            header::HeaderValue::from_static("no-store"),
        );
    }
    resp
}

fn header_str(headers: &HeaderMap, name: header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// `example.com:1234` / `[::1]:1234` / `127.0.0.1` -> host part.
fn host_name(host: &str) -> &str {
    if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        host.split(':').next().unwrap_or(host)
    }
}

/// True when the request comes from this machine (loopback or one of our
/// own interface addresses).
fn is_local_peer(addr: &SocketAddr) -> bool {
    addr.ip().is_loopback()
        || crate::discovery::local_ipv4_addrs()
            .iter()
            .any(|ip| ip == &addr.ip())
}

/// JSON error response.
fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "error": msg.into() }))).into_response()
}
