//! LanDrop app (Tauri v2) — one crate for the desktop window and the
//! Android/iOS app.
//!
//! Starts the LanDrop backend inside the process via `landrop::app::start_node`
//! (or attaches to an already-running node) and points the webview at it.
//! Desktop extras: tray icon, close-to-tray, single instance, signal handling.
//! Mobile: paths come from the Tauri path API (Android has no XDG dirs) and the
//! window is the activity's webview. The desktop binary (`main.rs`) is a thin
//! shim over [`run`], which is also the mobile entry point.

use std::path::PathBuf;
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

/// Start the backend in-process, or attach to a live one.
///
/// Returns `(ui_url, node, owns_backend)`.
fn start_backend(app: &tauri::AppHandle) -> Result<(String, Option<RunningNode>, bool)> {
    let options = node_options(app)?;
    let config_dir: Option<PathBuf> = options.config_dir.clone().map(PathBuf::from);
    let dir_ref = config_dir.as_deref();

    if let Some(info) = landrop::instance::live_instance_in(dir_ref) {
        info!("attaching to existing LanDrop at {}", info.local_url());
        return Ok((info.local_url(), None, false));
    }
    match runtime().block_on(start_node(options)) {
        Ok(node) => {
            info!("backend started on {}", node.addr);
            Ok((node.local_url(), Some(node), true))
        }
        Err(e) => {
            // Lost the startup race against another launch: attach instead.
            if let Some(info) = landrop::instance::live_instance_in(dir_ref) {
                info!("attached to existing LanDrop at {}", info.local_url());
                Ok((info.local_url(), None, false))
            } else {
                Err(e)
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
    #[cfg(target_os = "linux")]
    {
        let mut cmd = std::process::Command::new("xdg-open");
        cmd.arg(dir);
        landrop::util::spawn_detached(&mut cmd).map_err(|e| format!("failed to open {dir}: {e}"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err(format!(
            "opening folders is not implemented on this platform ({dir})"
        ))
    }
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

    let builder = tauri::Builder::default();
    #[cfg(desktop)]
    let builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
        show_main(app);
    }));

    builder
        .setup(|app| {
            let handle = app.handle().clone();
            let (url, node, owns_backend) = start_backend(&handle)
                .map_err(|e| -> Box<dyn std::error::Error> { e.to_string().into() })?;
            handle.manage(NodeState {
                node: Arc::new(Mutex::new(node)),
                owns_backend,
            });

            #[cfg(desktop)]
            if let Err(e) = setup_tray(&handle) {
                warn!("tray unavailable ({e}); running window-only");
            }

            let window_url: tauri::Url = url
                .parse()
                .map_err(|e| -> Box<dyn std::error::Error> { format!("{e}").into() })?;
            let builder = WebviewWindowBuilder::new(app, "main", WebviewUrl::External(window_url))
                .title("LanDrop");
            #[cfg(desktop)]
            let builder = builder
                .inner_size(1100.0, 760.0)
                .min_inner_size(900.0, 600.0)
                .center()
                .visible(!wants_hidden());
            builder.build()?;

            #[cfg(all(unix, desktop))]
            spawn_signal_watcher(handle.clone());

            Ok(())
        })
        .on_window_event(|window, event| {
            #[cfg(desktop)]
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
            #[cfg(not(desktop))]
            let _ = (window, event);
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
