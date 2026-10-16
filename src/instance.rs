//! Single-instance coordination.
//!
//! A small `instance.json` in the config dir records the live process id and
//! bound port so a second launch can reuse the already-running receiver
//! instead of starting another node. An exclusive file lock (Unix) closes the
//! race between two simultaneous launches.

use std::fs;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::config_dir;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceInfo {
    pub pid: u32,
    pub port: u16,
}

impl InstanceInfo {
    pub fn local_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

/// Held for the lifetime of a node; the lock is released when dropped.
pub struct InstanceLock {
    #[cfg(unix)]
    _lock: nix::fcntl::Flock<fs::File>,
    #[cfg(not(unix))]
    _file: fs::File,
}

fn info_path(dir: Option<&Path>) -> Option<PathBuf> {
    match dir {
        Some(d) => Some(d.join("instance.json")),
        None => config_dir().map(|d| d.join("instance.json")),
    }
}

fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        pid != std::process::id()
    }
}

fn port_open(port: u16) -> bool {
    TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(250),
    )
    .is_ok()
}

/// A live LanDrop node in the default config dir, if one is actually up.
pub fn live_instance() -> Option<InstanceInfo> {
    live_instance_in(None)
}

/// A live LanDrop node in this config dir, if one is actually up.
pub fn live_instance_in(dir: Option<&Path>) -> Option<InstanceInfo> {
    let path = info_path(dir)?;
    let raw = fs::read_to_string(&path).ok()?;
    let info: InstanceInfo = serde_json::from_str(&raw).ok()?;
    if pid_alive(info.pid) && port_open(info.port) {
        return Some(info);
    }
    let _ = fs::remove_file(&path);
    None
}

/// Try to take the single-instance lock for a config dir.
///
/// Returns `Ok(None)` when another process already holds it.
pub fn acquire(dir: Option<&Path>) -> Result<Option<InstanceLock>> {
    let Some(path) = info_path(dir) else {
        return Ok(None);
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    #[cfg(unix)]
    {
        match nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusiveNonblock) {
            Ok(lock) => Ok(Some(InstanceLock { _lock: lock })),
            Err((_file, e))
                if e == nix::errno::Errno::EWOULDBLOCK || e == nix::errno::Errno::EAGAIN =>
            {
                Ok(None)
            }
            Err((_file, e)) => Err(anyhow::anyhow!("failed to lock {}: {e}", path.display())),
        }
    }
    #[cfg(not(unix))]
    {
        Ok(Some(InstanceLock { _file: file }))
    }
}

/// Minimal HTTP/1.0 request to a LanDrop node on `port`; returns
/// `(status, body)`.
///
/// Deliberately tiny and bounded: this talks to a *local* LanDrop to identify it
/// and to ask it to quit or raise its window — not a general HTTP client (that
/// would drag hyper + TLS into the core binary for one JSON field).
fn http_request(port: u16, method: &str, path: &str) -> Option<(u16, String)> {
    http_request_with(port, method, path, None)
}

/// Like [`http_request`], with an optional JSON body (used to hand dropped paths
/// to the node). Still bounded and short-timeout: this only ever talks to a
/// local LanDrop.
fn http_request_with(
    port: u16,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> Option<(u16, String)> {
    use std::io::{Read, Write};
    /// `/api/info` is a few hundred bytes; never buffer more than this, in case
    /// something other than LanDrop is listening on that port.
    const MAX_BODY: u64 = 8 * 1024;

    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream =
        std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(400)).ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_millis(800)));
    let payload = body.unwrap_or("");
    let request = format!(
        "{method} {path} HTTP/1.0\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
    stream.write_all(request.as_bytes()).ok()?;
    let mut raw = String::new();
    let _ = stream.take(MAX_BODY).read_to_string(&mut raw);
    let status = raw
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())?;
    let body = raw
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    Some((status, body))
}

/// Read the build marker from a node's `/api/info`.
///
/// `None` means "could not identify it" — which is how older builds, that do
/// not report a build marker at all, are detected.
pub fn remote_build(port: u16) -> Option<String> {
    let (status, body) = http_request(port, "GET", "/api/info")?;
    if status != 200 {
        return None;
    }
    parse_build(&body)
}

/// Ask a node on `port` to shut down (its own local-only endpoint).
pub fn request_quit(port: u16) -> bool {
    matches!(http_request(port, "POST", "/api/quit"), Some((200, _)))
}

/// Ask a node on `port` to raise the window it already has (local-only).
pub fn request_focus(port: u16) -> bool {
    matches!(
        http_request(port, "POST", "/api/window/focus"),
        Some((200, _))
    )
}

/// Ask a node to send these local paths (files or folders) to whatever device(s)
/// its UI has selected. Returns the HTTP status so the caller can tell "no device
/// selected" (409) from a real failure.
pub fn request_send_path(port: u16, paths: &[String]) -> Option<u16> {
    let body = serde_json::json!({ "paths": paths }).to_string();
    http_request_with(port, "POST", "/api/send-path", Some(&body)).map(|(status, _)| status)
}

/// True when a node reports a transfer that is still in flight.
///
/// Unknown answers count as "busy": better to leave an older instance alone than
/// to interrupt a transfer the user is watching.
pub fn has_active_transfers(port: u16) -> bool {
    const ACTIVE_IS_UNKNOWN: bool = true;
    let Some((200, body)) = http_request(port, "GET", "/api/transfers") else {
        return ACTIVE_IS_UNKNOWN;
    };
    let Ok(list) = serde_json::from_str::<serde_json::Value>(&body) else {
        return ACTIVE_IS_UNKNOWN;
    };
    let Some(list) = list.as_array() else {
        return ACTIVE_IS_UNKNOWN;
    };
    list.iter().any(|t| {
        let status = t.get("status").and_then(|s| s.as_str()).unwrap_or("");
        !matches!(status, "completed" | "failed" | "cancelled" | "rejected")
    })
}

/// Pull `"build":"…"` out of a JSON body without a parser.
pub fn parse_build(body: &str) -> Option<String> {
    let key = "\"build\":\"";
    let start = body.find(key)? + key.len();
    let rest = &body[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Record that this process owns the node on `port`.
pub fn write_info_in(dir: Option<&Path>, port: u16) {
    let Some(path) = info_path(dir) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let info = InstanceInfo {
        pid: std::process::id(),
        port,
    };
    if let Ok(raw) = serde_json::to_string(&info) {
        let _ = fs::write(path, raw);
    }
}

/// Remove the instance file if it belongs to this process.
pub fn clear_if_ours() {
    clear_if_ours_in(None);
}

/// Like [`clear_if_ours`], for an explicit config dir.
pub fn clear_if_ours_in(dir: Option<&Path>) {
    let Some(path) = info_path(dir) else {
        return;
    };
    let Ok(raw) = fs::read_to_string(&path) else {
        return;
    };
    let Ok(info) = serde_json::from_str::<InstanceInfo>(&raw) else {
        return;
    };
    if info.pid == std::process::id() {
        let _ = fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_is_exclusive_within_a_process() {
        let dir = tempfile::tempdir().unwrap();
        let first = acquire(Some(dir.path())).unwrap();
        assert!(first.is_some());
        // A second lock attempt on the same file must fail while held.
        let second = acquire(Some(dir.path())).unwrap();
        assert!(second.is_none());
        drop(first);
        // Once released, locking works again.
        let third = acquire(Some(dir.path())).unwrap();
        assert!(third.is_some());
    }

    #[tokio::test]
    async fn write_and_clear_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        // `live_instance` verifies the recorded port is actually serving.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        write_info_in(Some(dir.path()), port);
        let info = live_instance_in(Some(dir.path())).unwrap();
        assert_eq!(info.port, port);
        clear_if_ours_in(Some(dir.path()));
        assert!(!dir.path().join("instance.json").exists());
        drop(listener);
    }

    #[test]
    fn parses_the_build_marker_from_api_info() {
        let body = "HTTP/1.0 200 OK\r\n\r\n\
                    {\"id\":\"x\",\"name\":\"PC\",\"build\":\"1760558000\",\"platform\":\"linux\"}";
        assert_eq!(parse_build(body).as_deref(), Some("1760558000"));
        // Older builds do not report a marker at all.
        assert_eq!(parse_build("{\"id\":\"x\"}"), None);
        assert_eq!(parse_build("not json"), None);
    }

    /// The desktop app uses this to decide whether it may attach to a running
    /// node: same build -> attach, older/unknown -> start a fresh node.
    #[tokio::test]
    async fn remote_build_identifies_new_and_old_nodes() {
        async fn respond_with(body: Option<&'static str>) -> u16 {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let resp = match body {
                    Some(b) => format!(
                        "HTTP/1.0 200 OK\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\n\r\n{}",
                        b.len(),
                        b
                    ),
                    // An "old" node: nothing answers /api/info.
                    None => String::new(),
                };
                let _ = sock.write_all(resp.as_bytes()).await;
            });
            port
        }

        // A current node reports its marker.
        let port = respond_with(Some("{\"id\":\"x\",\"build\":\"12345\"}")).await;
        let seen = tokio::task::spawn_blocking(move || remote_build(port))
            .await
            .unwrap();
        assert_eq!(seen.as_deref(), Some("12345"));

        // A node that answers but carries no marker = older build.
        let port = respond_with(Some("{\"id\":\"x\",\"name\":\"PC\"}")).await;
        let seen = tokio::task::spawn_blocking(move || remote_build(port))
            .await
            .unwrap();
        assert_eq!(seen, None);

        // Nothing answering at all = cannot identify it.
        let port = respond_with(None).await;
        let seen = tokio::task::spawn_blocking(move || remote_build(port))
            .await
            .unwrap();
        assert_eq!(seen, None);
    }
}
