//! Configuration: paths, defaults, load/save.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

pub const APP_DIR_NAME: &str = "landrop";
pub const CONFIG_FILE_NAME: &str = "config.toml";
pub const DEFAULT_PORT: u16 = 8420;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    /// Stable random device id.
    pub id: String,
    /// User-editable friendly name.
    pub name: String,
    /// Listening port.
    pub port: u16,
    /// Directory where received files are saved.
    pub downloads: String,
    /// Optional shared token for device-to-device auth.
    pub token: String,
    /// Auto-accept incoming transfers (default: ask the user).
    pub auto_accept: bool,
    /// Auto-accept only offers from devices in [`Config::trusted_peers`].
    #[serde(default)]
    pub auto_accept_trusted_only: bool,
    /// Device ids trusted to skip the accept prompt.
    #[serde(default)]
    pub trusted_peers: Vec<String>,
    /// Ask where to save each incoming file (default: save to `downloads`).
    #[serde(default)]
    pub ask_save_path: bool,
    /// Start in the background when the user logs in (session autostart).
    #[serde(default)]
    pub start_on_login: bool,
    /// Manual peers persisted across restarts, as `ip:port`.
    pub manual_peers: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name: default_device_name(),
            port: DEFAULT_PORT,
            downloads: default_download_dir(),
            token: String::new(),
            auto_accept: false,
            auto_accept_trusted_only: false,
            trusted_peers: Vec::new(),
            ask_save_path: false,
            start_on_login: false,
            manual_peers: Vec::new(),
        }
    }
}

impl Config {
    /// Path to the config file, e.g. ~/.config/landrop/config.toml.
    pub fn config_path() -> Result<PathBuf> {
        config_dir()
            .map(|d| d.join(CONFIG_FILE_NAME))
            .context("could not determine config directory")
    }

    /// Load config, creating the file with defaults if it doesn't exist.
    pub fn load() -> Result<Config> {
        Self::load_or_init_at(&Self::config_path()?)
    }

    /// Load config from a specific path, creating it with defaults if missing.
    pub fn load_or_init_at(path: &Path) -> Result<Config> {
        if !path.exists() {
            let cfg = Config::default();
            cfg.save_to(path)?;
            return Ok(cfg);
        }
        Self::load_from(path)
    }

    /// Load config from a specific path (used by tests).
    pub fn load_from(path: &Path) -> Result<Config> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read config at {}", path.display()))?;
        let mut cfg: Config = toml::from_str(&raw)
            .with_context(|| format!("invalid config at {}", path.display()))?;
        // Ensure stable id even if an older config lacked one.
        if cfg.id.is_empty() {
            cfg.id = uuid::Uuid::new_v4().to_string();
        }
        Ok(cfg)
    }

    /// Write config atomically with 0600 permissions (Unix).
    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::config_path()?)
    }

    /// Write config to a specific path with 0600 permissions (Unix).
    ///
    /// Concurrent saves inside one process are serialized, so the file always
    /// holds one complete snapshot instead of interleaved writes.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        static SAVE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = SAVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
            }
        }
        let raw = toml::to_string(self).context("failed to serialize config")?;
        write_atomic_0600(path, raw.as_bytes())?;
        Ok(())
    }

    /// Directory for the download dir, created on demand.
    pub fn ensure_downloads(&self) -> Result<PathBuf> {
        let p = PathBuf::from(&self.downloads);
        fs::create_dir_all(&p)
            .with_context(|| format!("failed to create download dir {}", p.display()))?;
        Ok(p)
    }
}

/// Standard config directory: ~/.config/landrop on Linux, %APPDATA%/landrop on
/// Windows. Overridable with the `LANDROP_CONFIG_DIR` environment variable
/// (used by tests and multi-instance setups like run_two_local.sh).
pub fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("LANDROP_CONFIG_DIR") {
        return Some(PathBuf::from(dir));
    }
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .map(|p| p.join(APP_DIR_NAME))
    }
    #[cfg(not(target_os = "windows"))]
    {
        dirs::config_dir().map(|p| p.join(APP_DIR_NAME))
    }
}

/// Hostname-based default device name (this computer's name, not a hardcoded label).
fn default_device_name() -> String {
    let host = system_hostname();
    let short = host.split('.').next().unwrap_or(&host).to_string();
    if short.trim().is_empty() {
        "device".to_string()
    } else {
        short
    }
}

fn system_hostname() -> String {
    if let Ok(h) = std::env::var("HOSTNAME").or_else(|_| std::env::var("COMPUTERNAME")) {
        if !h.trim().is_empty() {
            return h;
        }
    }
    #[cfg(unix)]
    {
        if let Ok(h) = nix::unistd::gethostname() {
            if let Ok(s) = h.into_string() {
                if !s.trim().is_empty() {
                    return s;
                }
            }
        }
    }
    "device".to_string()
}

/// Default download dir: ~/Downloads/LanDrop (or <config>/downloads as fallback).
fn default_download_dir() -> String {
    let dir = dirs::download_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")))
        .join("LanDrop");
    dir.to_string_lossy().into_owned()
}

/// Write file atomically: write a uniquely named temp sibling, set 0600,
/// fsync, rename over target, fsync the directory.
pub fn write_atomic_0600(path: &Path, data: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("path has no parent: {}", path.display()))?;
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config".into());
    // Unique per call: two concurrent writers must never share a temp path.
    let tmp = parent.join(format!(".{file_name}.tmp-{}", uuid::Uuid::new_v4()));

    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp)?;
    if let Err(e) = io::Write::write_all(&mut f, data).and_then(|_| f.sync_all()) {
        drop(f);
        let _ = fs::remove_file(&tmp);
        return Err(e.into());
    }
    drop(f);
    fs::rename(&tmp, path)?;
    // Persist the rename itself (best-effort).
    #[cfg(unix)]
    {
        if let Ok(dir) = fs::File::open(parent) {
            let _ = dir.sync_all();
        }
        use std::os::unix::fs::PermissionsExt;
        // 0600 for the (possibly pre-existing) target.
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_dir_honors_env_override() {
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("LANDROP_CONFIG_DIR", dir.path()) };
        let got = config_dir().unwrap();
        unsafe { std::env::remove_var("LANDROP_CONFIG_DIR") };
        assert_eq!(got, dir.path());
    }

    #[test]
    fn defaults_are_sane() {
        let c = Config::default();
        assert!(!c.id.is_empty());
        assert!(!c.name.is_empty());
        assert_eq!(c.port, 8420);
        assert!(!c.auto_accept);
        assert!(!c.auto_accept_trusted_only);
        assert!(c.trusted_peers.is_empty());
        assert!(!c.ask_save_path);
        assert!(c.token.is_empty());
    }

    #[test]
    fn ask_save_path_round_trips_and_defaults_off() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE_NAME);
        // Older configs (without the key) still parse and default to off.
        std::fs::write(&path, "name = \"old\"\nport = 9000\n").unwrap();
        let old = Config::load_from(&path).unwrap();
        assert!(!old.ask_save_path);

        let c = Config {
            ask_save_path: true,
            ..Config::default()
        };
        c.save_to(&path).unwrap();
        let back = Config::load_from(&path).unwrap();
        assert!(back.ask_save_path);
    }

    #[test]
    fn round_trip_via_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join(CONFIG_FILE_NAME);
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();

        let c = Config {
            name: "test-box".into(),
            port: 9000,
            downloads: "/tmp/x".into(),
            token: "secret".into(),
            auto_accept: true,
            manual_peers: vec!["10.0.0.5:8420".into()],
            ..Config::default()
        };

        write_atomic_0600(&path, toml::to_string(&c).unwrap().as_bytes()).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let back: Config = toml::from_str(&raw).unwrap();
        assert_eq!(back, c);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let meta = std::fs::metadata(&path).unwrap();
            assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn load_or_init_creates_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join(CONFIG_FILE_NAME);
        let cfg = Config::load_or_init_at(&path).unwrap();
        assert!(path.exists());
        assert!(!cfg.id.is_empty());
        let again = Config::load_or_init_at(&path).unwrap();
        assert_eq!(again.id, cfg.id);
    }

    #[test]
    fn concurrent_saves_stay_valid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE_NAME);
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let p = path.clone();
                std::thread::spawn(move || {
                    let c = Config {
                        name: format!("device-{i}"),
                        ..Config::default()
                    };
                    c.save_to(&p).unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        // Whatever order the writers finished in, the file is one complete,
        // parseable snapshot.
        let raw = std::fs::read_to_string(&path).unwrap();
        let back: Config = toml::from_str(&raw).unwrap();
        assert!(back.name.starts_with("device-"));
        assert!(!back.id.is_empty());
    }
}
