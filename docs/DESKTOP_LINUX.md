# LanDrop on Linux Desktop (Tauri)

The Tauri desktop wrapper runs the LanDrop backend inside the process and
opens a native window. It is Linux-first.

## Linux dependencies

Tauri v2 needs these system packages to build and run. On Debian/Ubuntu:

```bash
sudo apt install \
  libwebkit2gtk-4.1-dev \
  build-essential \
  curl \
  wget \
  file \
  libxdo-dev \
  libssl-dev \
  libayatana-appindicator3-dev \
  librsvg2-dev
```

- `libwebkit2gtk-4.1-dev` — the webview engine
- `libayatana-appindicator3-dev` — system tray support
- `libxdo-dev` — window management helpers used by some plugins
- `librsvg2-dev` — SVG icon rendering for bundles

On Fedora:

```bash
sudo dnf install webkit2gtk4.1-devel openssl-devel \
  libxdo-devel libayatana-appindicator-devel librsvg2-devel
```

On Arch:

```bash
sudo pacman -S webkit2gtk-4.1 base-devel curl wget file openssl \
  appindicator-gtk3 librsvg
```

## Tauri CLI

```bash
cargo install tauri-cli --version '^2' --locked
```

## Build and run

```bash
make desktop-dev      # cargo tauri dev — live dev window
make desktop-build    # cargo tauri build — release bundles in target/release/bundle/
```

Or from the desktop crate directly:

```bash
cd desktop
cargo tauri build --no-bundle   # just the binary (fast)
```

The release binary is `target/release/landrop-desktop`; bundles
(`.deb`, `.rpm`, `.appimage`) land in `target/release/bundle/`.

## What the desktop app does

1. Starts the LanDrop backend via `landrop::app::start_node` (same code path
   as the CLI).
2. Opens a native window (1100×760, min 900×600) pointing at the local UI.
3. Adds a tray icon with **Open LanDrop / Open Downloads / Quit**.
4. Single-instance: launching a second copy focuses the existing window.
5. Closing the window or choosing Quit shuts the backend down gracefully.

The web UI inside the window is the same embedded UI as browser mode — a
phone can still reach it via `http://<ip>:8420` while the desktop app runs.

## Troubleshooting

- **Tray icon missing**: make sure `libayatana-appindicator3-dev` is installed
  before building, and that your desktop environment supports status notifiers
  (GNOME needs the AppIndicator extension).
- **Window shows "not found"**: the backend port changed (e.g. 8420 was busy
  and the app fell back to 8421+). The window always points at the actual
  bound address, so this should not happen; check the log output for the
  `listening on` line.
- **Missing webkit**: rebuild after installing `libwebkit2gtk-4.1-dev`; a
  partial build can leave stale artifacts (`cargo clean` if in doubt).

## Browser mode (no desktop deps)

The plain binary `bin/landrop` never needs these packages — the Tauri deps
are only required for the desktop wrapper.
