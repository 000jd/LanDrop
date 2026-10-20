# LanDrop desktop (Tauri) and Android builds

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

## Icons and desktop integration

`assets/icon.svg` is the only source; `make icons` (or
`python3 scripts/generate_icons.py`) renders every derivative:

- `assets/icon-{32,128}.png`, `icon.png` (256), `icon-512.png`, `icon.ico`,
  `icon.icns`, plus the raw `icon-32.rgba` / `icon-64.rgba` used by the tray and
  window.
- the Android launcher PNGs in every density bucket, the 108dp adaptive
  foregrounds, and the VectorDrawable XMLs
  (`drawable/ic_launcher_{background,foreground}.xml`). The generator also
  verifies `AndroidManifest.xml` has both `android:icon` and
  `android:roundIcon`; pass `--verify-android` to check without writing.

The Tauri bundles list all PNG sizes, so `.deb`/`.rpm` install the same
32/128/256/512 hicolor set and the AppImage picks the 512px icon. The packaging
tool writes `StartupWMClass=landrop-desktop`; `scripts/install-desktop-app.sh`
uses the same value for a user-local install. `src/notify.rs` looks for both
`landrop.*` and `landrop-desktop.*` under `~/.local/share/icons/hicolor` and
`/usr/share/icons/hicolor`, so notifications find the logo however the app was
installed.

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

## Android builds

The Android app is the same Tauri project (`desktop/`) built for a mobile target;
the backend runs inside the app process and the webview talks to it on
`127.0.0.1` (cleartext is allowed for localhost only, see
`desktop/gen/android/app/src/main/res/xml/network_security_config.xml`).

### One-time setup

```bash
# Rust targets
make mobile-targets            # rustup target add aarch64-linux-android x86_64-linux-android

# Android SDK + NDK (command-line tools are enough — no Android Studio required)
export ANDROID_HOME="$HOME/Android/Sdk"
export NDK_HOME="$ANDROID_HOME/ndk/28.2.13676358"   # NDK >= 28
"$ANDROID_HOME/cmdline-tools/latest/bin/sdkmanager" \
  "platform-tools" "platforms;android-34" "ndk;28.2.13676358"

export JAVA_HOME=/usr/lib/jvm/java-17-openjdk-amd64  # JDK 17

# Tauri CLI (v2) and the mobile project (committed under desktop/gen/android)
cargo install tauri-cli --version '^2'
make mobile-init
```

`scripts/build-artifacts.sh` sets all of the above itself and produces the signed
phone APK in one step.

### Building

| Goal | Command | Output |
|---|---|---|
| Emulator (x86_64) | `make mobile-build` | `desktop/gen/android/app/build/outputs/apk/universal/release/app-universal-release-unsigned.apk` |
| Real phone (aarch64) | `make mobile-build-phone` | same path, aarch64 build |
| Sign it (local debug keystore, fine for sideloading) | `make mobile-sign` | `bin/LanDrop-android-arm64.apk` |
| Install over USB (adb) | `make mobile-install` | — |
| Live reload | `make mobile-dev` | — |

Copying the APK by hand? Use the timestamped copy the release script writes
(`bin/LanDrop-arm64-<date>.apk`): phones keep every same-named download, and
tapping an old one is the usual reason an update "does not show up". After
installing, **Settings → This device → Version** shows the build time, so you can
confirm which copy is running.

For a real release, use your own keystore instead of the debug one
(`key.properties` with `storeFile`/`storePassword`/`keyAlias`/`keyPassword`), then
`make mobile-build-phone`.

### Android behaviour worth knowing

- Received files land in the app's own folder on shared storage
  (`Android/data/dev.landrop.app/files/Download`); no runtime permissions needed.
- The in-app folder picker starts there. Scoped storage hides most of the rest of
  the device, so a system-wide picker needs Android's Storage Access Framework —
  that is the next step on the roadmap (see `PLAN.md`).
- There is no tray and no system dialog on Android: incoming requests appear in
  the app UI (and in the notification, if the platform allows one).
- Keep the app in the foreground while transferring; Android may suspend
  background apps. A foreground service is on the roadmap.
- `LANDROP_CONFIG_DIR` is honoured, which makes sandboxed smoke tests easy
  (`LANDROP_CONFIG_DIR=/tmp/ld ./landrop-desktop`).
