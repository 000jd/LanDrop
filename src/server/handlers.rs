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

use crate::peers::{parse_peer_addr_with, Peer};
use crate::protocol::{AcceptInfo, Offer, TOKEN_HEADER, UPLOAD_TOKEN_HEADER};
use crate::server::AppState;
use crate::transfer::sender::spawn_send_in;

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
    let disk = crate::util::disk_usage(std::path::Path::new(&cfg.downloads));
    Json(json!({
        "id": cfg.id,
        "name": cfg.name,
        "port": port,
        "downloads": cfg.downloads,
        "version": VERSION,
        "build": crate::BUILD_ID,
        "platform": std::env::consts::OS,
        "disk_free": disk.map(|(f, _)| f),
        "disk_total": disk.map(|(_, t)| t),
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
    // Add the last readiness probe so the UI can flag a device that announces
    // itself but never answers (usually a firewall on that device).
    let mut out = Vec::with_capacity(peers.len());
    for peer in peers {
        let reachable = state.peers.is_reachable(&peer.id).await;
        let mut view = serde_json::to_value(&peer).unwrap_or_default();
        if let Some(obj) = view.as_object_mut() {
            obj.insert(
                "reachable".into(),
                match reachable {
                    Some(ok) => serde_json::Value::Bool(ok),
                    None => serde_json::Value::Null,
                },
            );
        }
        out.push(view);
    }
    Json(out).into_response()
}

/// Add a manual peer by `ip:port` (persisted in config).
pub async fn api_add_manual_peer(
    State(state): State<AppState>,
    ConnectInfo(request_addr): ConnectInfo<SocketAddr>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let address = body["address"].as_str().unwrap_or("").trim().to_string();
    if address.is_empty() {
        return err(StatusCode::BAD_REQUEST, "address is required");
    }
    let default_port = *state.transfers.my_port.read().await;
    // Loopback is only a valid peer when the request came from this machine
    // (the local UI, tests, the two-instance dev script).
    let socket = match parse_peer_addr_with(&address, default_port, is_local_peer(&request_addr)) {
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
                    StatusCode::FORBIDDEN,
                    format!("too many manual peers (max {MAX_MANUAL_PEERS})"),
                );
            }
            cfg.manual_peers.push(addr_str.clone());
        }
        // Manual peers are never a run-only override, so the on-disk copy
        // mirrors the list as-is.
        state.disk_cfg.write().await.manual_peers = cfg.manual_peers.clone();
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
pub async fn api_send(
    State(state): State<AppState>,
    ConnectInfo(request_addr): ConnectInfo<SocketAddr>,
    mut multipart: Multipart,
) -> Response {
    let mut targets: Vec<String> = Vec::new();
    let mut rel_path: Option<String> = None;
    let mut batch_id: Option<String> = None;
    let mut batch_name: Option<String> = None;
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
                Ok(v) => {
                    if targets.len() < 32 && !v.trim().is_empty() {
                        targets.push(v.trim().to_string());
                    }
                }
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
        if name == "rel_path" {
            // Folder sends: the receiver recreates the tree, sanitized on its side.
            if let Ok(v) = field.text().await {
                let v = v.trim();
                if !v.is_empty() {
                    rel_path = Some(crate::protocol::truncate_chars(v, 1024));
                }
            }
            continue;
        }
        if name == "batch_id" || name == "batch_name" {
            // Optional: the UI groups a multi-file send so the receiver is asked
            // once for the whole thing.
            if let Ok(v) = field.text().await {
                let v = crate::protocol::truncate_chars(v.trim(), 128);
                if !v.is_empty() {
                    if name == "batch_id" {
                        batch_id = Some(v);
                    } else {
                        batch_name = Some(v);
                    }
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

    if targets.is_empty() {
        cleanup_staged(&file_meta).await;
        return err(StatusCode::BAD_REQUEST, "target is required");
    }
    let (filename, size, staged, sha) = match file_meta {
        Some(f) => f,
        None => return err(StatusCode::BAD_REQUEST, "file is required"),
    };

    // Resolve every target first: peer id or manual address.
    let mut peers: Vec<Peer> = Vec::new();
    for target in &targets {
        let peer = match state.peers.find(target).await {
            Some(p) => p,
            None => {
                let my_port = *state.transfers.my_port.read().await;
                let allow_loopback = is_local_peer(&request_addr);
                match parse_peer_addr_with(target, my_port, allow_loopback) {
                    Ok(socket) => Peer {
                        id: format!("manual-{socket}"),
                        name: socket.ip().to_string(),
                        addr: socket.ip().to_string(),
                        port: socket.port(),
                        source: "manual".into(),
                    },
                    Err(e) => {
                        cleanup_file(&staged).await;
                        return err(StatusCode::BAD_REQUEST, format!("unknown target: {e}"));
                    }
                }
            }
        };
        if !peers.iter().any(|p| p.id == peer.id) {
            peers.push(peer);
        }
    }

    let batch = batch_id.map(|id| crate::transfer::sender::SendBatch {
        id,
        name: batch_name.unwrap_or_else(|| filename.clone()),
    });

    // One upload, one hash, one staged file — then a hard-linked copy per device,
    // because each transfer removes its own staged file when it ends.
    let staging = staging_dir_for(state.config_dir.as_deref());
    if let Err(e) = tokio::fs::create_dir_all(&staging).await {
        cleanup_file(&staged).await;
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to create staging dir: {e}"),
        );
    }
    let mut transfer_ids = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    for peer in peers {
        let staged_for_peer = match stage_by_link_or_copy(&staged, &staging).await {
            Ok(path) => path,
            Err(e) => {
                errors.push(format!("{}: {e}", peer.name));
                continue;
            }
        };
        let meta = crate::transfer::sender::SendMeta {
            batch: batch.clone(),
            rel_path: rel_path.clone(),
        };
        match spawn_send_in(
            meta,
            &state.transfers,
            peer.clone(),
            filename.clone(),
            size,
            sha.clone(),
            staged_for_peer.clone(),
        )
        .await
        {
            Ok(id) => transfer_ids.push(id),
            Err(e) => {
                cleanup_file(&staged_for_peer).await;
                errors.push(format!("{}: {e}", peer.name));
            }
        }
    }
    cleanup_file(&staged).await;

    if transfer_ids.is_empty() {
        let reason = errors
            .first()
            .cloned()
            .unwrap_or_else(|| "no targets".to_string());
        return err(StatusCode::INTERNAL_SERVER_ERROR, reason);
    }
    Json(json!({
        "transfer_id": transfer_ids[0],
        "transfer_ids": transfer_ids,
        "errors": errors,
    }))
    .into_response()
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
    // Mirror only the fields this request actually asked for into the on-disk
    // copy: everything else keeps whatever the config file already had, so a
    // run-only override can never be persisted by an unrelated settings save.
    {
        let cfg = state.cfg.read().await;
        let mut disk = state.disk_cfg.write().await;
        if body.name.as_deref().is_some_and(|n| !n.trim().is_empty()) {
            disk.name = cfg.name.clone();
        }
        if body.port.is_some_and(|p| p > 0) {
            disk.port = cfg.port;
        }
        if body
            .downloads
            .as_deref()
            .is_some_and(|d| !d.trim().is_empty())
        {
            disk.downloads = cfg.downloads.clone();
        }
        if let Some(a) = body.auto_accept {
            disk.auto_accept = a;
        }
        if let Some(a) = body.ask_save_path {
            disk.ask_save_path = a;
        }
        if let Some(on) = body.start_on_login {
            disk.start_on_login = on;
        }
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
        match crate::util::open_target(&dir) {
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

/// Directories that are never a sensible download target: accepting a file into
/// a system path is a mistake (or worse) even when the account happens to have
/// write access there.
const FORBIDDEN_DIRS: &[&str] = &[
    "/etc", "/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/libx32", "/boot", "/sys",
    "/proc", "/dev", "/run", "/var", "/snap",
];

/// Roots that stay usable even though they live under a refused path (removable
/// media and temporary folders).
const ALLOWED_UNDER: &[&str] = &["/run/media", "/media", "/mnt", "/tmp", "/var/tmp"];

/// True for system directories (and anything below them) that the folder picker
/// must not accept. Best-effort: on Windows the Linux-style prefixes simply
/// never match.
fn is_system_dir(path: &std::path::Path) -> bool {
    let s = path.to_string_lossy();
    let under = |p: &str| s == p || s.starts_with(&format!("{p}/"));
    if ALLOWED_UNDER.iter().any(|p| under(p)) {
        return false;
    }
    FORBIDDEN_DIRS.iter().any(|p| under(p))
}

/// Validate a user-chosen save folder: absolute, existing, a directory, and not
/// a system path.
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
    if is_system_dir(&canon) {
        return Err(format!(
            "{} is a system directory — pick a folder you own",
            canon.display()
        ));
    }
    Ok(canon)
}

/// Accept every waiting member of a batch (one request for a whole folder).
///
/// Same locality policy as accepting a single transfer: a device on the LAN may
/// accept, but settings stay machine-local.
pub async fn api_accept_batch(
    State(state): State<AppState>,
    Path(batch_id): Path<String>,
) -> Response {
    let ids = state.transfers.waiting_in_batch(&batch_id).await;
    if ids.is_empty() {
        return err(StatusCode::NOT_FOUND, format!("unknown batch {batch_id}"));
    }
    let mut accepted = 0usize;
    let mut errors = Vec::new();
    for id in &ids {
        match state.transfers.accept(id, None).await {
            Ok(_) => accepted += 1,
            Err(e) => errors.push(format!("{id}: {e}")),
        }
    }
    Json(json!({ "accepted": accepted, "errors": errors })).into_response()
}

/// Cancel every active member of a batch in one request.
///
/// Same policy as cancelling a single transfer.
pub async fn api_cancel_batch(
    State(state): State<AppState>,
    Path(batch_id): Path<String>,
) -> Response {
    let ids: Vec<String> = {
        let map = state.transfers.list().await;
        map.into_iter()
            .filter(|t| t.batch_id.as_deref() == Some(batch_id.as_str()))
            .filter(|t| {
                matches!(
                    t.status.as_str(),
                    "preparing" | "waiting" | "accepted" | "transferring"
                )
            })
            .map(|t| t.id)
            .collect()
    };
    if ids.is_empty() {
        return err(
            StatusCode::NOT_FOUND,
            format!("no active transfers in batch {batch_id}"),
        );
    }
    let mut cancelled = 0usize;
    for id in &ids {
        if state.transfers.cancel(id).await {
            cancelled += 1;
        }
    }
    Json(json!({ "cancelled": cancelled })).into_response()
}

/// Reject every waiting member of a batch.
pub async fn api_reject_batch(
    State(state): State<AppState>,
    Path(batch_id): Path<String>,
) -> Response {
    let ids = state.transfers.waiting_in_batch(&batch_id).await;
    if ids.is_empty() {
        return err(StatusCode::NOT_FOUND, format!("unknown batch {batch_id}"));
    }
    let mut rejected = 0usize;
    for id in &ids {
        if state.transfers.reject(id).await.is_ok() {
            rejected += 1;
        }
    }
    Json(json!({ "rejected": rejected })).into_response()
}

/// Process-wide counters since startup (transfers, bytes moved).
pub async fn api_metrics(State(state): State<AppState>) -> Response {
    Json(state.transfers.metrics.snapshot()).into_response()
}

/// Quit this node (local requests only).
///
/// A freshly installed build calls this before taking over the port, so exactly
/// one instance stays alive no matter how often the app is opened.
pub async fn api_quit(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Response {
    if !is_local_peer(&addr) {
        return err(
            StatusCode::FORBIDDEN,
            "quitting is only available from this machine",
        );
    }
    match state.shutdown.get() {
        Some(tx) => {
            let _ = tx.send(true);
            Json(json!({ "ok": true })).into_response()
        }
        None => err(
            StatusCode::SERVICE_UNAVAILABLE,
            "this node cannot quit itself",
        ),
    }
}

/// Raise the app window (local requests only).
///
/// A second launch of the same build calls this and then exits, so the user ends
/// up with the window that is already open instead of a pile of copies.
/// Browser and CLI mode have no window and answer 501.
pub async fn api_focus(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Response {
    if !is_local_peer(&addr) {
        return err(
            StatusCode::FORBIDDEN,
            "raising the window is only available from this machine",
        );
    }
    match &state.on_focus {
        Some(hook) => {
            (hook.0)();
            Json(json!({ "ok": true })).into_response()
        }
        None => err(StatusCode::NOT_IMPLEMENTED, "no window in this mode"),
    }
}

/// Cap on files collected from dropped folders in one request.
const MAX_DROPPED_FILES: usize = 500;

/// Remember which devices the local UI has selected (see [`api_send_path`]).
pub async fn api_ui_selection(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if !is_local_peer(&addr) {
        return err(
            StatusCode::FORBIDDEN,
            "the selection is only settable from this machine",
        );
    }
    let targets: Vec<String> = body["targets"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .take(32)
                .collect()
        })
        .unwrap_or_default();
    *state.ui_selection.write().await = targets;
    Json(json!({ "ok": true })).into_response()
}

/// Send files and folders **by path** (local requests only).
///
/// A browser cannot hand a path to the server, but an operating-system drop event
/// can, so the desktop window forwards dropped paths here and the node does the
/// work: folders are walked (symlinks skipped), relative paths are preserved for
/// the receiver, and every file is staged, hashed and queued as its own transfer
/// — one staged copy per device, since each transfer removes its own.
pub async fn api_send_path(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if !is_local_peer(&addr) {
        return err(
            StatusCode::FORBIDDEN,
            "sending by path is only available from this machine",
        );
    }
    let roots: Vec<PathBuf> = body["paths"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|v| v.as_str())
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .take(64)
                .collect()
        })
        .unwrap_or_default();
    if roots.is_empty() {
        return err(StatusCode::BAD_REQUEST, "paths are required");
    }

    let targets: Vec<String> = match body["targets"].as_array() {
        Some(list) => list
            .iter()
            .filter_map(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        None => state.ui_selection.read().await.clone(),
    };
    if targets.is_empty() {
        return err(StatusCode::CONFLICT, "no device selected");
    }

    let (files, skipped) = collect_sendable(&roots, MAX_DROPPED_FILES);
    // One batch per request: a dropped folder (or selection) is one question on
    // the other side, whatever the number of files or devices.
    let batch = crate::transfer::sender::SendBatch::new(match roots.as_slice() {
        [single] if single.is_dir() => single
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("folder")
            .to_string(),
        _ => match files.len() {
            1 => "1 file".to_string(),
            n => format!("{n} files"),
        },
    });

    let mut peers = Vec::new();
    for target in &targets {
        if let Some(peer) = state.peers.find(target).await {
            peers.push(peer);
            continue;
        }
        let my_port = *state.transfers.my_port.read().await;
        match crate::peers::parse_peer_addr_with(target, my_port, true) {
            Ok(socket) => peers.push(crate::peers::Peer {
                id: format!("manual-{socket}"),
                name: socket.ip().to_string(),
                addr: socket.ip().to_string(),
                port: socket.port(),
                source: "manual".into(),
            }),
            Err(e) => return err(StatusCode::BAD_REQUEST, format!("unknown target: {e}")),
        }
    }

    let dir = staging_dir_for(state.config_dir.as_deref());
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to create staging dir: {e}"),
        );
    }

    let mut queued = 0usize;
    let mut errors: Vec<String> = Vec::new();
    for (path, rel_dir) in &files {
        let filename = crate::protocol::sanitize_filename(
            path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unnamed"),
        );
        let size = match tokio::fs::metadata(path).await {
            Ok(m) if m.is_file() => m.len(),
            Ok(_) => {
                errors.push(format!("{filename}: not a regular file"));
                continue;
            }
            Err(e) => {
                errors.push(format!("{filename}: {e}"));
                continue;
            }
        };
        let sha = match crate::protocol::sha256_file(path) {
            Ok(sha) => Some(sha),
            Err(e) => {
                errors.push(format!("{filename}: {e}"));
                continue;
            }
        };
        for peer in &peers {
            let staged = match stage_by_link_or_copy(path, &dir).await {
                Ok(p) => p,
                Err(e) => {
                    errors.push(format!("{filename}: {e}"));
                    break;
                }
            };
            match crate::transfer::sender::spawn_send_in(
                crate::transfer::sender::SendMeta {
                    batch: Some(batch.clone()),
                    rel_path: rel_dir.clone(),
                },
                &state.transfers,
                peer.clone(),
                filename.clone(),
                size,
                sha.clone(),
                staged.clone(),
            )
            .await
            {
                Ok(_) => queued += 1,
                Err(e) => {
                    cleanup_file(&staged).await;
                    errors.push(format!("{filename} → {}: {e}", peer.name));
                }
            }
        }
    }

    Json(json!({
        "queued": queued,
        "files": files.len(),
        "skipped": skipped,
        "devices": peers.len(),
        "errors": errors,
    }))
    .into_response()
}

/// Stage a file for upload: a hard link when the filesystem allows it (no data
/// copied at all), otherwise a real copy.
async fn stage_by_link_or_copy(
    src: &std::path::Path,
    dir: &std::path::Path,
) -> std::io::Result<PathBuf> {
    let name = src.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let staged = dir.join(format!(".{name}.send-{}", uuid::Uuid::new_v4()));
    if std::fs::hard_link(src, &staged).is_ok() {
        return Ok(staged);
    }
    tokio::fs::copy(src, &staged).await?;
    Ok(staged)
}

/// Collect the files under `roots`: folders are walked, symlinks skipped, and
/// each entry carries the relative directory the receiver should recreate.
fn collect_sendable(roots: &[PathBuf], limit: usize) -> (Vec<(PathBuf, Option<String>)>, usize) {
    let mut out: Vec<(PathBuf, Option<String>)> = Vec::new();
    let mut skipped = 0usize;
    for root in roots {
        let Ok(meta) = std::fs::symlink_metadata(root) else {
            skipped += 1;
            continue;
        };
        if meta.file_type().is_symlink() {
            skipped += 1;
            continue;
        }
        if meta.is_file() {
            out.push((root.clone(), None));
            continue;
        }
        let base = root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("folder")
            .to_string();
        let mut stack = vec![(root.clone(), base)];
        while let Some((dir, rel)) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                skipped += 1;
                continue;
            };
            for entry in entries.flatten() {
                if out.len() >= limit {
                    skipped += 1;
                    continue;
                }
                let Ok(kind) = entry.file_type() else {
                    skipped += 1;
                    continue;
                };
                if kind.is_symlink() {
                    continue;
                }
                let path = entry.path();
                if kind.is_dir() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    stack.push((path, format!("{rel}/{name}")));
                } else if kind.is_file() {
                    out.push((path, Some(rel.clone())));
                }
            }
        }
    }
    (out, skipped)
}

/// Persist the current config next to the node's config file (honours a
/// custom `--config-dir`, falling back to the default location).
///
/// Writes [`AppState::disk_cfg`], not `cfg`: run-only CLI overrides
/// (`--port/--downloads/--name`) stay out of the file no matter how often the
/// settings UI is saved.
async fn persist_config(state: &AppState) -> Result<(), String> {
    let snapshot = state.disk_cfg.read().await.clone();
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

#[cfg(test)]
mod tests {
    use super::is_system_dir;
    use std::path::Path;

    #[test]
    fn system_directories_are_refused_but_user_folders_are_not() {
        for p in [
            "/etc", "/etc/ssl", "/usr/bin", "/boot", "/var/lib", "/dev/shm",
        ] {
            assert!(is_system_dir(Path::new(p)), "{p} should be refused");
        }
        for p in [
            "/home/me/Downloads/LanDrop",
            "/tmp/landrop-test",
            "/var/tmp/landrop",
            "/media/usb/stick",
            "/run/media/me/disk",
            "/mnt/data",
        ] {
            assert!(!is_system_dir(Path::new(p)), "{p} should be allowed");
        }
    }
}
