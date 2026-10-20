//! LanDrop app (Tauri v2) — one crate for the desktop window and the
//! Android/iOS app.
//!
//! Starts the LanDrop backend inside the process via `landrop::app::start_node`
//! (or attaches to an already-running node) and points the webview at it.
//! Desktop extras: tray icon, close-to-tray, build-aware single instance,
//! signal handling.
//! Mobile: paths come from the Tauri path API (Android has no XDG dirs) and the
//! window is the activity's webview. The desktop binary (`main.rs`) is a thin
//! shim over [`run`], which is also the mobile entry point.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};
use tracing::{info, warn};

use landrop::app::{start_node, NodeOptions, RunningNode};

#[cfg(desktop)]
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
};

struct NodeState {
    /// Node handle shared with the tray / quit path. Mobile keeps it for
    /// lifecycle hooks, so the fields are only read on desktop.
    #[cfg_attr(not(desktop), allow(dead_code))]
    node: Arc<Mutex<Option<RunningNode>>>,
    #[cfg_attr(not(desktop), allow(dead_code))]
    owns_backend: bool,
}

/// Tokio runtime for the backend, created once and kept for the process.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("landrop-rt")
            .build()
            .expect("build tokio runtime")
    })
}

fn init_tracing() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "info,mdns_sd=warn,reqwest=warn".into()),
            )
            .compact()
            .try_init();
    });
}

/// Per-platform node options: where the config lives and where files land.
fn node_options(app: &tauri::AppHandle) -> Result<NodeOptions> {
    // `LANDROP_CONFIG_DIR` wins on every platform: it keeps the shipped binary
    // smoke-testable in a sandbox (and lets two copies run side by side)
    // without touching the real config.
    if let Some(dir) = std::env::var_os("LANDROP_CONFIG_DIR") {
        let dir = PathBuf::from(dir);
        info!("LANDROP_CONFIG_DIR={} (sandboxed config)", dir.display());
        return Ok(NodeOptions {
            config_dir: Some(dir.to_string_lossy().into_owned()),
            ..NodeOptions::default()
        });
    }
    #[cfg(mobile)]
    {
        let paths = app.path();
        let config_dir = paths.app_config_dir()?;
        let downloads = paths
            .download_dir()
            .or_else(|_| paths.app_data_dir())
            .unwrap_or_else(|_| config_dir.clone());
        let cfg_path = config_dir.join(landrop::config::CONFIG_FILE_NAME);

        // First run on a phone: seed a friendly name and the real folder so the
        // settings UI shows something sane (no XDG dirs exist on Android).
        if !cfg_path.exists() {
            let mut seed = landrop::config::Config::default();
            seed.name = "Phone".to_string();
            seed.downloads = downloads.to_string_lossy().into_owned();
            if let Err(e) = seed.save_to(&cfg_path) {
                warn!("could not seed config at {}: {e:#}", cfg_path.display());
            }
        }
        // Only override the folder when the stored one is unusable (relative),
        // so a folder chosen in Settings is never clobbered.
        let stored = landrop::config::Config::load_from(&cfg_path).unwrap_or_default();
        let stored_ok = std::path::Path::new(&stored.downloads).is_absolute();
        info!(
            "mobile paths: config={} downloads={}",
            config_dir.display(),
            downloads.display()
        );
        Ok(NodeOptions {
            config_dir: Some(config_dir.to_string_lossy().into_owned()),
            downloads: (!stored_ok).then(|| downloads.to_string_lossy().into_owned()),
            ..NodeOptions::default()
        })
    }
    #[cfg(desktop)]
    {
        let _ = app;
        Ok(NodeOptions::default())
    }
}

/// What to do about a node that is already serving for this config dir.
enum Existing {
    /// Nothing running.
    None,
    /// Same build — safe to attach to its UI.
    SameBuild(String),
    /// An older (or unknown) build: leave it alone and serve our own UI.
    OtherBuild { port: u16, build: Option<String> },
}

/// Classify a live node for this config dir by the build it reports.
fn existing_node(dir: Option<&Path>) -> Existing {
    let Some(info) = landrop::instance::live_instance_in(dir) else {
        return Existing::None;
    };
    match landrop::instance::remote_build(info.port) {
        Some(build) if build == landrop::BUILD_ID => Existing::SameBuild(info.local_url()),
        other => Existing::OtherBuild {
            port: info.port,
            build: other,
        },
    }
}

/// Result of bringing the backend up for this process.
struct Backend {
    /// UI URL the window should load.
    url: String,
    /// The node this process owns, if any.
    node: Option<RunningNode>,
    /// Whether this process started (and must stop) the node.
    owns: bool,
    /// Another copy of the app is already on screen (or busy transferring):
    /// raise it, tell the user if it is an older build, and exit without
    /// opening a second window.
    already_running: bool,
}

/// Send dropped paths through the node this process runs (or is attached to).
///
/// The node walks folders, stages every file and queues one transfer per device
/// the UI has selected; this process only reports the outcome.
#[cfg(desktop)]
async fn send_dropped_paths(app: &tauri::AppHandle, paths: Vec<String>) {
    let port = {
        let state = app.state::<NodeState>();
        let node = state.node.lock().unwrap_or_else(|e| e.into_inner()).clone();
        match node {
            Some(node) => node.addr.port(),
            None => {
                // Attached to a node another process runs: use the port it
                // recorded for this config dir.
                let dir = node_options(app)
                    .ok()
                    .and_then(|options| options.config_dir)
                    .map(std::path::PathBuf::from);
                match landrop::instance::live_instance_in(dir.as_deref()) {
                    Some(info) => info.port,
                    None => {
                        warn!("drop ignored: no backend port");
                        return;
                    }
                }
            }
        }
    };

    match tokio::task::spawn_blocking(move || landrop::instance::request_send_path(port, &paths))
        .await
    {
        Ok(Some(200)) => info!("dropped paths queued for transfer"),
        Ok(Some(409)) => {
            warn!("drop ignored: choose a device in the app first");
            landrop::notify::notify(
                "LanDrop — nothing sent",
                "Choose the device(s) you want in the app window, then drop the files again.",
            );
        }
        Ok(Some(status)) => warn!("drop rejected with HTTP {status}"),
        other => warn!("drop failed: {other:?}"),
    }
}

/// Wait (bounded) until nothing answers on `port` any more.
fn wait_for_port_free(port: u16, timeout: std::time::Duration) {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(200))
            .is_err()
        {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
    warn!("port {port} is still busy after {timeout:?}");
}

/// Start the backend in-process, or hand over to the copy that is already open.
///
/// One instance only: a second launch never opens a second window. Same build →
/// raise the running window and exit. Different build → retire the running copy
/// through its local-only `/api/quit` (unless it is transferring something) and
/// then take over; if that is not possible, tell the user and exit instead of
/// piling up windows or showing the older UI.
fn start_backend(app: &tauri::AppHandle) -> Result<Backend> {
    let options = node_options(app)?;
    #[cfg(desktop)]
    let options = {
        let mut options = options;
        let handle = app.clone();
        options.on_focus = Some(landrop::app::FocusHook(std::sync::Arc::new(move || {
            show_main(&handle)
        })));
        options
    };
    let config_dir: Option<PathBuf> = options.config_dir.clone().map(PathBuf::from);
    let dir_ref = config_dir.as_deref();

    match existing_node(dir_ref) {
        Existing::SameBuild(url) => {
            // The window is already open: raise it, then leave quietly.
            let port = url
                .rsplit(':')
                .next()
                .and_then(|p| p.parse::<u16>().ok())
                .unwrap_or_default();
            if landrop::instance::request_focus(port) {
                info!("LanDrop is already running at {url}; raised its window");
            } else {
                info!("LanDrop is already running at {url}");
            }
            return Ok(Backend {
                url,
                node: None,
                owns: false,
                already_running: true,
            });
        }
        Existing::OtherBuild { port, build } => {
            let which = match &build {
                Some(b) => format!("a different build ({b})"),
                None => "an older build (no build marker)".to_string(),
            };
            if landrop::instance::has_active_transfers(port) {
                warn!("{which} is on port {port} with a transfer in progress; leaving it alone");
                landrop::notify::notify(
                    "LanDrop is already running",
                    &format!(
                        "An older copy is transferring files on port {port}. Quit it from its \
                         tray menu, then open LanDrop again."
                    ),
                );
                return Ok(Backend {
                    url: String::new(),
                    node: None,
                    owns: false,
                    already_running: true,
                });
            }
            if !landrop::instance::request_quit(port) {
                warn!("{which} is on port {port} and cannot be asked to quit; leaving it alone");
                landrop::notify::notify(
                    "LanDrop is already running",
                    &format!(
                        "An older copy is running on port {port}. Quit it from its tray menu, \
                         then open LanDrop again."
                    ),
                );
                return Ok(Backend {
                    url: String::new(),
                    node: None,
                    owns: false,
                    already_running: true,
                });
            }
            // It accepted the quit request: wait for the port, then take over.
            info!("retiring the older instance on port {port} ({which})");
            wait_for_port_free(port, std::time::Duration::from_secs(5));
        }
        Existing::None => {}
    }

    match runtime().block_on(start_node(options)) {
        Ok(node) => {
            info!("backend started on {}", node.addr);
            Ok(Backend {
                url: node.local_url(),
                node: Some(node),
                owns: true,
                already_running: false,
            })
        }
        Err(e) => {
            // Lost the startup race against another launch: hand over if it is
            // the same build after all.
            match existing_node(dir_ref) {
                Existing::SameBuild(url) => Ok(Backend {
                    url,
                    node: None,
                    owns: false,
                    already_running: true,
                }),
                _ => Err(e),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Desktop-only extras
// ---------------------------------------------------------------------------

#[cfg(desktop)]
fn tray_icon() -> tauri::image::Image<'static> {
    let bytes: &[u8] = include_bytes!("../../assets/icon-32.rgba");
    tauri::image::Image::new_owned(bytes.to_vec(), 32, 32)
}

/// Window/taskbar icon, embedded so the app shows its own mark even when it is
/// run straight from the build directory (where no installed `.desktop` entry
/// exists to supply one). Regenerate with `make icons`.
#[cfg(desktop)]
fn window_icon() -> tauri::image::Image<'static> {
    let bytes: &[u8] = include_bytes!("../../assets/icon-64.rgba");
    tauri::image::Image::new_owned(bytes.to_vec(), 64, 64)
}

#[cfg(desktop)]
fn wants_hidden() -> bool {
    if std::env::args().any(|a| a == "--hidden") {
        return true;
    }
    match std::env::var("LANDROP_HIDDEN") {
        Ok(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "" | "0" | "false"),
        Err(_) => false,
    }
}

#[cfg(desktop)]
fn show_main(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

#[cfg(desktop)]
fn node_snapshot(app: &tauri::AppHandle) -> Option<RunningNode> {
    let state = app.state::<NodeState>();
    let node = state.node.lock().unwrap_or_else(|e| e.into_inner()).clone();
    node
}

#[cfg(desktop)]
fn open_dir(dir: &str) -> Result<(), String> {
    landrop::util::open_target(dir).map_err(|e| format!("failed to open {dir}: {e}"))
}

/// Open the download folder with the system file manager. Uses the node's live
/// settings when it owns the backend, else the persisted config.
#[cfg(desktop)]
fn open_downloads_path(app: &tauri::AppHandle) {
    let node = node_snapshot(app);
    tauri::async_runtime::spawn(async move {
        let dir = match &node {
            Some(node) => node.state.cfg.read().await.downloads.clone(),
            None => match landrop::config::Config::load() {
                Ok(cfg) => cfg.downloads,
                Err(e) => {
                    warn!("cannot resolve downloads path: {e:#}");
                    return;
                }
            },
        };
        if let Err(e) = open_dir(&dir) {
            warn!("{e}");
        }
    });
}

/// Tray icon plus menu (best effort: no tray host must not break startup).
#[cfg(desktop)]
fn setup_tray(app: &tauri::AppHandle) -> Result<()> {
    let open = MenuItem::with_id(app, "open", "Open LanDrop", true, None::<&str>)?;
    let downloads_item = MenuItem::with_id(app, "downloads", "Open Downloads", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &downloads_item, &quit])?;

    let tray = TrayIconBuilder::with_id("landrop-tray")
        .icon(tray_icon())
        .tooltip("LanDrop — running in background")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(move |app, event| match event.id.as_ref() {
            "open" => show_main(app),
            "downloads" => open_downloads_path(app),
            "quit" => shutdown_and_exit(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main(tray.app_handle());
            }
        })
        .build(app)?;
    app.manage(tray);
    Ok(())
}

/// Stop the backend only if this process started it, then exit.
#[cfg(desktop)]
fn shutdown_and_exit(app: &tauri::AppHandle) {
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        // Scope the tauri `State` borrow so no lock/borrow crosses an await.
        let (owns, node) = {
            let state = handle.state::<NodeState>();
            let node = state.node.lock().unwrap_or_else(|e| e.into_inner()).clone();
            (state.owns_backend, node)
        };
        if owns {
            if let Some(node) = &node {
                node.shutdown();
                // Wait (bounded) for the server to finish gracefully.
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    node.wait_for_shutdown(),
                )
                .await;
            }
            landrop::instance::clear_if_ours();
        }
        handle.exit(0);
    });
}

/// Quit cleanly on SIGINT/SIGTERM (logout, kill) — desktop only.
#[cfg(all(unix, desktop))]
fn spawn_signal_watcher(handle: tauri::AppHandle) {
    tauri::async_runtime::spawn(async move {
        use tokio::signal::unix::{signal, SignalKind};
        let (Ok(mut sigint), Ok(mut sigterm)) = (
            signal(SignalKind::interrupt()),
            signal(SignalKind::terminate()),
        ) else {
            return;
        };
        tokio::select! {
            _ = sigint.recv() => {}
            _ = sigterm.recv() => {}
        }
        shutdown_and_exit(&handle);
    });
}

// ---------------------------------------------------------------------------
// Entry point (desktop binary + Android/iOS library)
// ---------------------------------------------------------------------------

/// Builds and runs the app. On Android/iOS this is the entry point Tauri calls.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    init_tracing();

    // NOTE: no `tauri-plugin-single-instance` here on purpose. That plugin
    // forwards a second launch to the already-running process and then exits
    // silently — which means a freshly installed build would never appear while
    // an older copy of the app is still running ("I installed the new .deb and
    // still see the old UI"). Our own instance logic is build-aware instead
    // (see `start_backend`): same build -> attach to its node, different build
    // -> serve our own node so the new UI is always shown.
    let builder = tauri::Builder::default();

    builder
        .setup(|app| {
            let handle = app.handle().clone();
            let backend = start_backend(&handle)
                .map_err(|e| -> Box<dyn std::error::Error> { e.to_string().into() })?;
            if backend.already_running {
                // One instance only: the window (or the busy older copy) is
                // already on screen, so this launch has nothing to show.
                handle.exit(0);
                return Ok(());
            }
            let node_watch = backend.node.clone();
            handle.manage(NodeState {
                node: Arc::new(Mutex::new(backend.node)),
                owns_backend: backend.owns,
            });

            // If the node stops on its own — `POST /api/quit` from a newer build
            // taking over — take the window down with it, so no empty shell (and
            // no stale instance record) is left behind.
            if let Some(node) = node_watch {
                let handle = handle.clone();
                tauri::async_runtime::spawn(async move {
                    node.wait_for_shutdown().await;
                    info!("backend stopped; exiting");
                    handle.exit(0);
                });
            }

            #[cfg(desktop)]
            if let Err(e) = setup_tray(&handle) {
                warn!("tray unavailable ({e}); running window-only");
            }

            let window_url: tauri::Url = backend
                .url
                .parse()
                .map_err(|e| -> Box<dyn std::error::Error> { format!("{e}").into() })?;
            let builder = WebviewWindowBuilder::new(app, "main", WebviewUrl::External(window_url))
                .title("LanDrop");
            #[cfg(desktop)]
            let builder = builder
                // Embedded 64x64 RGBA from `assets/icon-64.rgba` (regenerate with
                // `make icons`): a fixed asset, so failing here means the platform
                // rejected it and a clear panic beats a window without a mark.
                .icon(window_icon())
                .expect("embedded window icon")
                // 760 tall so every tab — Settings with both cards included —
                // fits without a scrollbar.
                .inner_size(960.0, 760.0)
                .min_inner_size(720.0, 520.0)
                .center()
                .visible(!wants_hidden());
            builder.build()?;

            #[cfg(all(unix, desktop))]
            spawn_signal_watcher(handle.clone());

            Ok(())
        })
        .on_window_event(|window, event| {
            #[cfg(desktop)]
            match event {
                tauri::WindowEvent::CloseRequested { api, .. } => {
                    api.prevent_close();
                    let _ = window.hide();
                }
                // Files and folders dropped on the window arrive here as paths.
                // The webview is a remote page, so instead of tunnelling data
                // through JavaScript the node walks the paths itself.
                tauri::WindowEvent::DragDrop(tauri::DragDropEvent::Drop { paths, .. }) => {
                    let paths: Vec<String> = paths
                        .iter()
                        .map(|p| p.to_string_lossy().into_owned())
                        .collect();
                    if paths.is_empty() {
                        return;
                    }
                    let handle = window.app_handle().clone();
                    tauri::async_runtime::spawn(async move {
                        send_dropped_paths(&handle, paths).await;
                    });
                }
                _ => {}
            }
            #[cfg(not(desktop))]
            let _ = (window, event);
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
