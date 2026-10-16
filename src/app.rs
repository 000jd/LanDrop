//! Reusable application startup, usable from the CLI binary (`main.rs`) and
//! from the Tauri desktop wrapper.
//!
//! [`start_node`] wires up config, peers, transfers, mDNS and the HTTP server,
//! then returns a [`RunningNode`] handle with the bound address, a config
//! snapshot, and a shutdown handle. Everything keeps working without the
//! desktop wrapper: the CLI binary calls this same function.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tracing::{error, info, warn};

use crate::config::Config;
use crate::events;
use crate::peers::{Peer, PeerStore};
use crate::server::{self, AppState};
use crate::transfer::TransferManager;

/// Options for starting a LanDrop node.
#[derive(Debug, Clone)]
pub struct NodeOptions {
    /// Listening port. If `None`, the configured default (8420) is used.
    /// If the port is busy, the next available port is tried when
    /// [`NodeOptions::port_scan`] is set.
    pub port: Option<u16>,
    /// Override the download directory (this run only; not persisted).
    pub downloads: Option<String>,
    /// Override the device friendly name (this run only; not persisted).
    pub name: Option<String>,
    /// Disable discovery (manual peers only).
    pub no_mdns: bool,
    /// Custom config directory (overrides the default config location).
    pub config_dir: Option<String>,
    /// When true and the preferred port is busy, try subsequent ports up to
    /// `port_scan_limit` attempts.
    pub port_scan: bool,
    /// Max ports to try when `port_scan` is set (default 20).
    pub port_scan_limit: u16,
    /// Start even when another instance already holds the single-instance lock
    /// for this config dir. The desktop app sets this when the running instance
    /// is a *different build*, so the new build still shows its own UI instead
    /// of silently attaching to the old one (or refusing to start).
    pub allow_second_instance: bool,
}

impl Default for NodeOptions {
    fn default() -> Self {
        Self {
            port: None,
            downloads: None,
            name: None,
            no_mdns: false,
            config_dir: None,
            port_scan: true,
            port_scan_limit: 20,
            allow_second_instance: false,
        }
    }
}

/// A running LanDrop node. Cloning is cheap (all state is `Arc`-based).
#[derive(Clone)]
pub struct RunningNode {
    /// The address the HTTP server is actually bound to.
    pub addr: SocketAddr,
    /// The config snapshot the node started with.
    pub config: Config,
    /// Send `true` to request a graceful shutdown.
    pub shutdown: watch::Sender<bool>,
    /// Shared app state (useful for tests / desktop integration).
    pub state: AppState,
    /// Handle to the HTTP server task (resolves when it shuts down).
    pub server_task: Arc<tokio::task::JoinHandle<()>>,
    server_done: watch::Receiver<bool>,
    _instance_lock: Option<Arc<crate::instance::InstanceLock>>,
}

impl RunningNode {
    /// Local UI URL, e.g. `http://127.0.0.1:8420`.
    pub fn local_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.addr.port())
    }

    /// LAN URLs for each non-loopback IPv4 address of this host.
    pub fn lan_urls(&self) -> Vec<String> {
        crate::discovery::local_ipv4_addrs()
            .into_iter()
            .map(|ip| format!("http://{}:{}", ip, self.addr.port()))
            .collect()
    }

    /// Request a graceful shutdown of the HTTP server and background tasks.
    pub fn shutdown(&self) {
        let _ = self.shutdown.send(true);
    }

    /// Wait until the HTTP server task has finished shutting down.
    pub async fn wait_for_shutdown(&self) {
        let mut done = self.server_done.clone();
        while !*done.borrow() {
            if done.changed().await.is_err() {
                break;
            }
        }
    }
}

/// Start a full LanDrop node (config, peers, transfers, discovery, server).
///
/// CLI overrides (`--port`, `--downloads`, `--name`) apply to this run only;
/// only first-run defaults and an ephemeral port are persisted.
pub async fn start_node(options: NodeOptions) -> Result<RunningNode> {
    // Resolve paths explicitly — no process-global environment mutation.
    let cfg_path = match &options.config_dir {
        Some(dir) => PathBuf::from(dir).join(crate::config::CONFIG_FILE_NAME),
        None => Config::config_path()?,
    };
    let config_dir = cfg_path.parent().map(Path::to_path_buf);

    let mut cfg = Config::load_or_init_at(&cfg_path).context("failed to load config")?;

    // Bind first so discovery advertises the port we actually listen on.
    // (Previously mDNS started before bind, so a busy 8420 meant peers
    // connected to a port that was not serving.)
    let preferred: u16 = options.port.unwrap_or(cfg.port);
    let listener = bind_http_listener(preferred, &options).await?;
    let local = listener.local_addr()?;
    if preferred != 0 && local.port() != preferred {
        warn!("port {preferred} busy; bound to {} instead", local.port());
    }
    if cfg.port == 0 {
        // The config asked for an ephemeral port: remember the one we got.
        let mut persisted = cfg.clone();
        persisted.port = local.port();
        if let Err(e) = persisted.save_to(&cfg_path) {
            warn!("failed to persist config: {e:#}");
        }
        cfg.port = local.port();
    }
    // Snapshot of the file's contents *before* the run-only overrides below:
    // this is what later settings saves are allowed to write back.
    let disk_view = cfg.clone();

    // Run-only overrides; never written back to the config file.
    if let Some(p) = options.port {
        cfg.port = p;
    }
    if let Some(d) = &options.downloads {
        cfg.downloads = d.clone();
    }
    if let Some(n) = &options.name {
        let name = crate::protocol::truncate_chars(n.trim(), 64);
        if !name.is_empty() {
            cfg.name = name;
        }
    }
    cfg.ensure_downloads()?;

    // Single-instance guard for this config dir (closes the double-start race).
    let instance_lock = if options.allow_second_instance {
        None
    } else {
        match crate::instance::acquire(config_dir.as_deref())? {
            Some(lock) => Some(Arc::new(lock)),
            None => bail!("another LanDrop instance is already running for this config"),
        }
    };

    // Wire up shared state.
    let peers = Arc::new(PeerStore::new());
    // Load persisted manual peers.
    for addr in &cfg.manual_peers {
        match crate::peers::parse_peer_addr(addr, local.port()) {
            Ok(s) => {
                let p = Peer {
                    id: format!("manual-{s}"),
                    name: s.ip().to_string(),
                    addr: s.ip().to_string(),
                    port: s.port(),
                    source: "manual".into(),
                };
                peers.upsert(p).await;
            }
            Err(e) => warn!("ignoring invalid manual peer {addr}: {e}"),
        }
    }

    let transfers = TransferManager::new(peers.clone());
    transfers
        .set_identity(&cfg.id, &cfg.name, local.port())
        .await;
    *transfers.download_dir.write().await = PathBuf::from(&cfg.downloads);
    *transfers.auto_accept.write().await = cfg.auto_accept;
    *transfers.ask_save_path.write().await = cfg.ask_save_path;
    *transfers.shared_token.write().await = cfg.token.clone();
    transfers
        .interactive_prompts
        .store(true, std::sync::atomic::Ordering::Relaxed);

    let mut state = AppState::new(cfg.clone(), peers.clone(), transfers.clone());
    state.config_dir = config_dir.clone();
    state.disk_cfg = Arc::new(tokio::sync::RwLock::new(disk_view));

    // Shutdown channel (broadcast to discovery, sweeps and server).
    let (shutdown_tx, shutdown_rx) = events::shutdown_channel();

    // Discovery after bind, using the real listen port.
    if !options.no_mdns {
        let disc_shutdown = shutdown_rx.clone();
        tokio::spawn(crate::discovery::run(
            peers.clone(),
            transfers.my_id.clone(),
            transfers.my_name.clone(),
            transfers.my_port.clone(),
            transfers.events.clone(),
            disc_shutdown,
        ));
    } else {
        info!("discovery disabled; manual peers only");
    }

    // Periodic transfer expiry sweep (stops with the node).
    {
        let mgr = transfers.clone();
        let mut stop = shutdown_rx.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(10));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = ticker.tick() => mgr.expire_stale().await,
                    _ = events::wait_shutdown(&mut stop) => break,
                }
            }
        });
    }

    // Periodic peer expiry sweep, so the UI hears about devices leaving.
    {
        let store = peers.clone();
        let bus = state.events.clone();
        let mut stop = shutdown_rx.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(10));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        for peer in store.sweep().await {
                            info!("peer {} ({}) expired", peer.name, peer.addr);
                            bus.send("peer_removed", &peer);
                        }
                    }
                    _ = events::wait_shutdown(&mut stop) => break,
                }
            }
        });
    }

    // Remove staged-file leftovers from previous runs.
    {
        let outbox = server::handlers::staging_dir_for(config_dir.as_deref());
        if let Ok(mut entries) = tokio::fs::read_dir(&outbox).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let _ = tokio::fs::remove_file(entry.path()).await;
            }
        }
    }

    info!(
        "LanDrop {} — listening on http://{}",
        env!("CARGO_PKG_VERSION"),
        local
    );
    info!("downloads -> {}", cfg.downloads);
    crate::instance::write_info_in(config_dir.as_deref(), local.port());
    {
        let mut stop = shutdown_rx.clone();
        let dir = config_dir.clone();
        tokio::spawn(async move {
            let _ = stop.changed().await;
            crate::instance::clear_if_ours_in(dir.as_deref());
        });
    }

    // Spawn the HTTP server with graceful shutdown; the caller keeps running.
    let (server_done_tx, server_done) = watch::channel(false);
    let server_state = state.clone();
    let mut stop = shutdown_rx.clone();
    let server_task = tokio::spawn(async move {
        let result = axum::serve(
            listener,
            server::router(server_state).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            let _ = stop.changed().await;
        })
        .await;
        if let Err(e) = result {
            error!("HTTP server stopped with an error: {e}");
        }
        let _ = server_done_tx.send(true);
    });

    Ok(RunningNode {
        addr: local,
        config: cfg,
        shutdown: shutdown_tx,
        state,
        server_task: Arc::new(server_task),
        server_done,
        _instance_lock: instance_lock,
    })
}

async fn bind_http_listener(preferred: u16, options: &NodeOptions) -> Result<TcpListener> {
    // Port 0 means "let the OS pick"; do not scan 0,1,2,...
    if preferred == 0 {
        return TcpListener::bind("0.0.0.0:0".parse::<SocketAddr>()?)
            .await
            .context("cannot bind ephemeral HTTP port");
    }
    let limit = if options.port_scan {
        options.port_scan_limit
    } else {
        1
    };
    let mut last_err = None;
    let mut last_port = preferred;
    for offset in 0..limit {
        let Some(port) = preferred.checked_add(offset) else {
            break;
        };
        last_port = port;
        let addr: SocketAddr = format!("0.0.0.0:{port}").parse()?;
        match TcpListener::bind(addr).await {
            Ok(l) => return Ok(l),
            Err(e) => last_err = Some(e),
        }
    }
    Err(anyhow::anyhow!(
        "cannot bind any port in {preferred}..={last_port}: {last_err:?}"
    ))
}
