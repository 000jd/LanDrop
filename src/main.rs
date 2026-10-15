//! LanDrop — LAN file sharing with no cloud and no accounts.
//!
//! Run with `cargo run` or `landrop`; open http://localhost:8420.

use anyhow::Result;
use tracing::info;
use tracing_subscriber::EnvFilter;

use landrop::app::{start_node, NodeOptions};

/// Simple CLI flag parsing (no clap dependency).
struct Args {
    port: Option<u16>,
    downloads: Option<String>,
    name: Option<String>,
    no_mdns: bool,
    open: bool,
    hidden: bool,
    config_dir: Option<String>,
    no_port_scan: bool,
}

fn parse_args() -> Args {
    let mut args = Args {
        port: None,
        downloads: None,
        name: None,
        no_mdns: false,
        open: false,
        hidden: false,
        config_dir: None,
        no_port_scan: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--port" | "-p" => {
                let Some(value) = it.next() else {
                    eprintln!("--port requires a value (use --help)");
                    std::process::exit(2);
                };
                match value.parse::<u16>() {
                    Ok(p) => args.port = Some(p),
                    Err(_) => {
                        eprintln!("invalid --port '{value}' (use --help)");
                        std::process::exit(2);
                    }
                }
            }
            "--downloads" | "-d" => args.downloads = it.next(),
            "--name" | "-n" => args.name = it.next(),
            "--no-mdns" => args.no_mdns = true,
            "--open" | "-o" => args.open = true,
            "--hidden" => args.hidden = true,
            "--config-dir" => args.config_dir = it.next(),
            "--no-port-scan" => args.no_port_scan = true,
            "--help" | "-h" => {
                println!(
                    "LanDrop — LAN file sharing\n\n\
                     Usage: landrop [options]\n\n\
                     Options:\n\
                     \x20 -p, --port <port>       listening port (default 8420)\n\
                     \x20 -d, --downloads <dir>   download directory (this run only)\n\
                     \x20 -n, --name <name>       device friendly name (this run only)\n\
                     \x20     --no-mdns          disable discovery (manual peers only)\n\
                     \x20     --no-port-scan     fail if the port is busy (no fallback)\n\
                     \x20 -o, --open              open the UI in the browser\n\
                     \x20     --hidden            stay in the background (no browser)\n\
                     \x20     --config-dir <dir>  use a custom config directory\n"
                );
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown argument: {other} (use --help)");
                std::process::exit(2);
            }
        }
    }
    args
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,mdns_sd=warn,reqwest=warn")),
        )
        .compact()
        .init();

    let args = parse_args();

    if let Some(info) =
        landrop::instance::live_instance_in(args.config_dir.as_deref().map(std::path::Path::new))
    {
        let url = info.local_url();
        info!("LanDrop already running at {url}");
        if !args.hidden {
            let _ = open_browser(&url);
        }
        return Ok(());
    }

    let options = NodeOptions {
        port: args.port,
        downloads: args.downloads,
        name: args.name,
        no_mdns: args.no_mdns,
        config_dir: args.config_dir,
        port_scan: !args.no_port_scan,
        ..NodeOptions::default()
    };

    let node = start_node(options).await?;
    let url = node.local_url();

    if (args.open || std::env::var("LANDROP_OPEN").is_ok()) && !args.hidden {
        let _ = open_browser(&url);
    }

    // Graceful shutdown on SIGINT/SIGTERM: request it, then actually wait
    // (bounded) for the server task to finish before the runtime is dropped.
    let (shutdown_done_tx, shutdown_done_rx) = tokio::sync::oneshot::channel();
    {
        let node = node.clone();
        tokio::spawn(async move {
            wait_for_signal().await;
            info!("shutting down…");
            node.shutdown();
            let _ = shutdown_done_tx.send(());
        });
    }
    let _ = shutdown_done_rx.await;
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), node.wait_for_shutdown()).await;

    Ok(())
}

/// Wait for SIGINT or SIGTERM.
async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let (Ok(mut sigint), Ok(mut sigterm)) = (
            signal(SignalKind::interrupt()),
            signal(SignalKind::terminate()),
        ) else {
            // Could not install handlers; fall back to plain Ctrl-C.
            let _ = tokio::signal::ctrl_c().await;
            return;
        };
        tokio::select! {
            _ = sigint.recv() => {}
            _ = sigterm.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Open the UI in the default browser (best-effort, reaped).
fn open_browser(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let mut cmd = std::process::Command::new("xdg-open");
        cmd.arg(url);
        landrop::util::spawn_detached(&mut cmd)
    }
    #[cfg(target_os = "macos")]
    {
        let mut cmd = std::process::Command::new("open");
        cmd.arg(url);
        landrop::util::spawn_detached(&mut cmd)
    }
    #[cfg(target_os = "windows")]
    {
        let mut cmd = std::process::Command::new("cmd");
        cmd.args(["/c", "start", "", url]);
        landrop::util::spawn_detached(&mut cmd)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = url;
        Ok(())
    }
}
