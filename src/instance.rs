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
}
