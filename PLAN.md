# LanDrop — Implementation Plan

LAN file sharing, no cloud, no accounts. Rust backend + embedded web UI.

## Stack

- **Rust** (tokio, axum, serde, tracing, sha2, mdns-sd) — single binary, no external server.
- **mdns-sd** for discovery (`_landrop._tcp`), plus manual peer fallback.
- **Tauri** optional (platform assets) — the web UI is the primary interface and works without it.
- HTML/CSS/JS served from `rust-embed`; works fully offline, no Node build step.

## Architecture

```
browser UI (localhost:PORT)  ──HTTP──►  axum server
                                          ├── /api/*          local control API
                                          ├── /api/v1/*       wire protocol (offer/upload)
                                          ├── /api/events     SSE
                                          └── /api/send       multipart upload from UI
       peers store (mdns + manual) ◄──── mdns-sd task + /api/peers/manual
       transfer manager ── sender/receiver state machines, streaming, hashing
       config (TOML, 0600)            download dir, name, port, token, auto-accept
```

## Data flow

1. **Offer**: sender `POST /api/v1/offer` → receiver creates pending transfer → returns id.
2. **Decision**: receiver UI accepts/rejects; sender polls `GET /api/v1/offer/{id}`.
3. **Accept**: receiver generates one-time upload token, returns `upload_url` + token.
4. **Upload**: sender `POST /api/v1/upload/{id}` with `X-LanDrop-Upload-Token`, streams body
   to temp file, computes SHA-256, validates size+checksum, renames into download dir
   with collision-safe name.
5. **Lifecycle**: preparing → waiting → accepted → transferring → completed | rejected |
   failed | cancelled; 120 s offer timeout; all states broadcast over SSE.

## Security

- Filename sanitization (strip paths, control chars; never allow `..`).
- Collision-safe destination names (`file.txt`, `file-1.txt`, …).
- One-time upload token; optional shared token on all `/api/v1/*` requests when configured.
- Streamed `io::copy`-style transfer — no full-file buffering; temp file + atomic rename.
- SHA-256 verification when the sender supplies a checksum.

## Config

- `~/.config/landrop/config.toml` (Linux) / `%APPDATA%\landrop\config.toml` (Windows), mode 0600.
- Defaults: port 8420, download dir `~/Downloads/LanDrop`, no token, auto-accept off.

## Project layout

```
landrop/
  PLAN.md
  Cargo.toml
  Makefile
  README.md
  build.rs
  src/
    main.rs            flags (--port --downloads --no-mdns --open), graceful shutdown
    config.rs          config load/save
    peers.rs           peer store + mdns discovery + manual peers
    protocol.rs        shared wire types + helpers (sanitize, unique path, sha256)
    transfer.rs        manager, sender, receiver state machines
    server.rs          axum app, handlers, SSE, UI embedding
  ui/
    index.html
    styles.css
    app.js
  tests/
    integration.rs     two in-process instances, real end-to-end transfer
  scripts/
    run_two_local.sh
```

## Milestones

1. Scaffold + deps → `cargo build` clean.
2. config/protocol/peers + unit tests.
3. transfer manager + server + UI.
4. Integration test: A → B offer, accept, upload, checksum verify.
5. Makefile, README, run_two_local.sh, manual end-to-end run.

## Verification

- `cargo fmt --check`, `cargo clippy`, `cargo build`, `cargo test`.
- Manual: two local instances on 8420/8421 (mDNS loopback caveat → manual peer fallback).
- Firewall note: allow UDP 5353 (mDNS) and TCP 8420/8421.

## Review fixes (round 2)

Priority order, from the verified code review:

1. **Control-plane security** — same-origin only (drop permissive CORS), Origin/Host validation (DNS
   rebinding/CSRF), machine-local `POST /api/settings`, per-route body limits (small JSON default;
   streaming routes opt out), outbound peer-target validation (reject unspecified/multicast/link-local).
2. **Sender hardening** — hard timeouts on offer/poll/upload; trust only same-peer `upload_url`;
   delete staged files on every path; cancel checks before upload and while streaming; sticky terminals.
3. **Receiver/manager** — enforce expected size while streaming (incl. `size == 0`); atomic one-time
   token consume; cancel interrupts a receive; expiry covers `Accepted`/stalled transfers and signals
   cancel; atomic filename reservation; normalize + validate sha256; cap filenames/names.
4. **Config/app/instance** — serialized saves with unique temp names; CLI overrides not persisted;
   explicit config-dir plumbing (no `env::set_var`); real `wait_for_shutdown`; `--port` parse errors;
   flock-based single-instance guard; private staging dir under the config dir.
5. **Peers/discovery/notify** — address/port update when a peer moves; IPv6 `[addr]:port` parsing;
   TXT `ver` enforcement; no loopback A-record fallback; instance-name truncation fix; kdialog 127 →
   Ignored; reaped helper processes; prompt timeout < offer timeout.
6. **UI** — direction-aware Cancel; stop polling when SSE recovers; live progress; proper escaping;
   toast dedupe; file-input reset; adaptive peer polling; incoming-offer toast.
7. **Desktop wrapper** — best-effort tray; live downloads path; non-blocking shutdown; signals.
8. **Tests, docs, verification** — e2e tests for cancel, size/checksum mismatch, traversal, token
   reuse, concurrent same-name transfers, large files; README/PLAN updates; fmt/clippy/test + smoke.

Status: **implemented and verified.** `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`
and the full suite (34 unit + 16 integration tests) are green, and the release binary was
smoke-tested end to end: Host/Origin guards, machine-local settings, token reuse 401, size/body
limits, graceful SIGTERM, and the two-instance script performing a real transfer with automatic
manual-peer registration.

## Round 3: save-location picker, ask-on-receive, ETA + progress (planned)

Requests:

1. Settings: choose the download folder with a browse UI (click through folders) instead of
   typing the path by hand. Same picker should appear when the user wants to change where a
   file is saved.
2. Settings: a toggle "ask where to save each received file". Off = files go straight to the
   configured download folder. On = accepting an incoming transfer first opens the folder
   picker (same browse UI), plus a "use default folder" escape hatch.
3. Send/receive: show the approximate time remaining for large files (like a download manager).
4. Receive progress without opening the Receive tab: system notification with a progress bar,
   plus an always-visible in-app activity strip.

Decisions:

- The folder picker is served by the backend (`GET /api/fs/list`, `POST /api/fs/mkdir`), so it
  works identically in browser, phone and desktop modes, with no new dependencies (no native
  OS dialog, no Tauri dialog plugin). Path listing/creation and per-transfer save paths are
  **machine-local**, using the same gate as settings.
- `ask_save_path` defaults to off, so existing behavior is unchanged until enabled.
- Speed/ETA are computed server-side (bytes done since the transfer started) and exposed on
  the transfer view, so the UI and notifications show the same numbers.
- Incoming transfers emit progress notifications via `notify-send` (replace-id + `int:value`
  hint where supported), throttled to one update every ~2 s; the final update doubles as the
  completion notification.

Phases: plan file (this section) → backend fields + APIs → accept-with-path → progress
notifications → UI (picker dialog, settings, ETA, activity strip) → tests → docs + verification.

Status: **implemented and verified.** Folder picker (`/api/fs/list`, `/api/fs/mkdir`, browser-driven
dialog), `ask_save_path` with per-transfer save folder, speed/ETA on every transfer, progress
notifications (gdbus replace-in-place; notify-send fallback) and the top-bar activity strip.
Verified live with the release binary: the picker created and selected a folder, a throttled
40 MB transfer saved there while the default folder stayed untouched, and the UI showed
`31% · 919 KB/s · ~31s left` counting down. 60 tests green, `cargo fmt --check` and
`clippy --all-targets -- -D warnings` clean.

## Round 4: Android app (Tauri v2 mobile) + repo management (planned)

Goal: the same app — all features, same HUD UI — as an Android build, in the same crate
(`desktop/`, the Tauri app), plus a properly tracked repository.

Research (v2.tauri.app/develop, /develop/plugins/develop-mobile, /start/prerequisites and the
Tauri discussions): mobile is the same Rust core + same webview; `cargo tauri android init`
generates `gen/android` which **is meant to be committed** (it carries its own .gitignore for
build artifacts) so the manifest/network config can be customised; prerequisites are the SDK,
platform-tools, NDK side-by-side, cmdline-tools, `JAVA_HOME`/`NDK_HOME`, and the four Rust
android targets; NDK ≥ 28 satisfies the 16 KB page requirement.

Environment on this machine: JDK 17, SDK at `~/Android/Sdk` (platforms 32/36, emulator, adb,
cmdline-tools, NDK 28.2), `/dev/kvm`, and a `Pixel_6` AVD already exists — full emulator testing
is possible.

Design decisions:

- One Tauri app crate for desktop **and** mobile: move node startup into the Tauri `setup`
  callback (works for both), gate the tray / single-instance / close-to-tray on `desktop`,
  and add `#[cfg_attr(mobile, tauri::mobile_entry_point)]`.
- Paths on mobile: `dirs` has no XDG dirs on Android, so the mobile entry point resolves the
  config dir (`app_config_dir`) and download dir (`download_dir`, the app's external Downloads
  folder — no runtime permissions needed) via Tauri's path API and passes them as
  `NodeOptions`; first run gets a friendly default name ("Phone").
- Android WebView: cleartext HTTP only for `127.0.0.1` via a network security config (the UI is
  served by the in-process backend on localhost), INTERNET permission comes from the template.
- Discovery: the UDP beacon (broadcast) + mDNS work without native code; the Android
  multicast lock (for unsolicited multicast) and a foreground service (background receiving)
  are documented follow-ups.
- Repo: dedicated git repository **inside the project** (the parent `~/Documents/Devlopment`
  repo holds unrelated projects), with a .gitignore that keeps `gen/android` sources and drops
  the Gradle/build artifacts, plus a first commit.
- Tests: desktop suite stays green; the Android APK is built for x86_64 (emulator) and aarch64
  (real phones), installed on the Pixel 6 emulator, launched, screenshotted (HUD UI), checked
  via `adb forward` (API reachable, settings, folder listing) and via a real end-to-end transfer
  from the desktop app to the emulator app through the forwarded port.

Phases: plan → repo init + .gitignore → android targets → mobile-ready code changes → android
init/build → emulator install + screenshots + transfer test → Makefile/README → final checks.

Status: **implemented and verified.** The app is one crate for desktop + Android
(`desktop/src/lib.rs` holds the logic, `main.rs` is the desktop shim, `[lib] crate-type =
["staticlib", "cdylib", "rlib"]`); mobile paths come from the Tauri path API with a seeded
"Phone" name; cleartext is restricted to localhost via a network security config. Verified on
a Pixel 6 emulator: the APK builds, installs and runs with the full HUD UI, the backend serves
inside the app (`platform: android`, downloads in the app's external folder), a real 3 MB
transfer from the desktop node was accepted **by tapping ACCEPT in the Android UI** and the
SHA-256 matched on both sides; the folder picker works on Android (mobile layout). Desktop
suite still green (60 tests), clippy clean on both targets, repo initialised with a dedicated
`.gitignore` (gen/android committed, build output ignored).

## Round 5: glassmorphism redesign (soft-UI reference) — planned

Goal: replace the monochrome HUD look with the reference design language across **all three
surfaces** (browser UI, desktop app, Android app — they share the embedded `ui/`), keeping
every feature and both colour modes.

Tokens read from the reference image:

- Page: soft mint/sage gradient; one large rounded **glass shell** (radius ~30px, blur, thin
  white border, soft green-tinted shadow) holding the whole app.
- Sidebar icon **rail** with circular buttons (active = solid dark circle); top bar with
  pill-shaped controls; white rounded **cards** (radius 20) with very soft shadows.
- **Floating dark stat chips** (near-black, radius 14) with big bold numbers and small grey
  captions; **mini tiles** (bordered rounded squares with a small icon) inside cards;
  semicircular **gauge** (amber arc on a light track).
- Accent: amber `#f4a91b` for markers/highlights; text `#141917 / #5f6b66 / #8d9a94`.
- Typography: humanist sans, normal case — 30-34px bold numbers, 15-16px card titles,
  11-12px grey labels (replaces the mono/uppercase HUD).

Mapping onto LanDrop (no fake controls):

- Rail: Send / Receive / Settings + theme toggle at the bottom (dark circle).
- Hero card with the dropzone and three live chips: **Devices**, **Active transfers**, **Port**.
- Devices card with mini tiles for the selected device (address / port / source).
- Send card: chosen-file tile, target row, dark Send pill; transfers list below.
- New **disk gauge** (free space of the download folder) — needs `util::disk_usage` and two
  new `/api/info` fields.
- Receive panel: pending requests with Accept/Reject, active & recent, activity pill in the
  top bar. Settings: white cards with the existing controls plus value tiles.
- Dark mode: same composition as "dark glass" (graphite-green glass, same accents).

Phases: plan + token research → disk-usage API → UI rewrite (html/css/js) → desktop
screenshots (light/dark/phone) → Android APK rebuild + emulator screenshot → docs + commit →
final checks.

Status: **implemented and verified.** All three surfaces now use the soft-glass language
(shared `ui/`): mint gradient + frosted shell, sticky icon rail with the theme toggle, live
near-black stat chips, white cards with mini tiles, amber gauge driven by the new
`disk_free`/`disk_total` in `/api/info`, pill buttons/inputs, restyled picker and glass
toasts; light is the default theme and dark is the same composition in dark glass. Verified
with real screenshots (light, dark, receive, settings, 390px phone) and on the Pixel 6
emulator; a real click on the rail theme toggle was checked in-browser, 41 unit + 20
integration tests green, clippy clean on desktop and Android targets, desktop bundles and the
signed arm64 phone APK rebuilt.

## Round 6: retro-modern (neobrutalist) theme — planned

Goal: the reference's retro-modern language across web + desktop + Android, keeping the
layout that is already agreed (top tabs, Pick a file / Choose a device / Send, stacked
Receive and Settings) and adding the reference's mobile bottom navigation.

Tokens read from the reference image:

- Warm cream surfaces (`#fbf1e0` panels, `#fffaf0` cards) on a warm grey page, with
  **bold red** `#e4572e` as the single accent and near-black ink.
- **Thick black outlines (2px) with hard offset shadows** (`5px 5px 0`), radius ~16 for
  cards, pills for buttons/tabs/badges.
- Chunky bold typography (700-800 weights, normal case), small grey captions.
- Signature elements to reuse: the **red marquee ribbon** with ★ separators, the
  **dashed upload box** (already our dropzone), **underline form fields**, red icon tiles in
  bordered cards, a **bottom nav** with red active items, red pill "primary" buttons with
  black outlines (the Premium card treatment).

Mapping: Send/Save/Accept = red pill; secondary = cream pill; tabs = segmented control with a
red active pill; hints = small bordered chips; status = black for completed, red for
waiting/failed (text distinguishes), red progress fill; theme toggle stays in the top bar and
also appears in the mobile bottom nav.

Phases: plan → theme rewrite (css/html/app.js) → screenshots (light/dark/receive/settings/
mobile with bottom nav) → desktop bundles + phone APK → docs + commit → final checks.

## Round 7: third-party review — verified, then fixed (planned)

An external review listed 20 findings plus 15 suggestions. Every item was checked
against the code before acting; three claims did not survive that check.

### Verdicts

| # | Finding | Verdict | Action |
|---|---|---|---|
| 1 | MSRV: `Ipv4Addr::is_broadcast` needs Rust 1.79 | **wrong claim, right conclusion** — the local `rust-src` shows `#[stable(since = "1.7.0", feature = "ip_17")]`, so nothing in our code needs 1.79. But the declared MSRV was wrong anyway: a locked dependency (`idna_adapter`, via `url`) uses **edition 2024**, which needs **1.85+**, and the committed `Cargo.lock` was version 4 (older cargo cannot even read it) | declare and verify 1.85: `rust-version` in both manifests, README, and a CI job pinned to it |
| 2 | `parse_peer_addr` accepts loopback although the comment claims SSRF protection | valid | allow loopback **only for requests from this machine** (keeps the two-instance dev flow and tests working), reject it from the LAN; fix the comment; tests |
| 3 | subnet-directed broadcast (`192.168.1.255`) accepted | valid | reject a `.255` last octet with a comment explaining the /24 heuristic |
| 4 | `LANADROP_BUILD_ID` typo | valid | rename to `LANDROP_BUILD_ID` |
| 5 | ephemeral port persistence wasted | **wrong** as stated (the persisted port *is* used on the next run) — but it exposed a **real** bug: CLI overrides live in `state.cfg`, so a later settings save writes `--port/--downloads/--name` into the config file, contradicting "this run only" | keep a separate on-disk config image; UI changes update it, run-only overrides do not; integration test |
| 6 | mDNS: no unregister when advertise fails | mostly moot; defensive improvement | on failure, drop the cached fullname and unregister before the next attempt |
| 7 | UDP beacon every 2 s forever | valid (politeness) | adaptive: 2 s while the peer set is settling/changing, back off to 20 s when stable, speed up on change |
| 8 | `validate_dir` allows `/etc` etc. | valid | refuse system directories (and anything under them), keep home / downloads / removable media / tmp |
| 9 | stale mDNS registration after SIGKILL (60 s TTL) | valid, documentation gap | document in README troubleshooting (standard mDNS behaviour) |
| 10 | reqwest redirect policy / certs | no issue (reviewer agrees) | — |
| 11 | SSE channel capacity 256 | valid, cheap | raise to 1024 |
| 12 | `remote_build` unbounded `read_to_string` | valid | cap at 8 KiB |
| 13 | temp file name could escape the dir | no issue (name is sanitized first) | — |
| 14 | notification body passed positionally to `gdbus` | valid (option injection, not shell) | pass positionals after `--` and strip control characters |
| 15 | autostart `Exec=` quoting | valid | escape per the Desktop Entry spec |
| 16 | token compare short-circuits when no token is set | valid (micro) | compare against a fixed dummy so the path is constant-time too |
| 17 | 400 for "too many manual peers" | valid | use 403 |
| 18 | mDNS hostname collisions | no issue (12 hex chars) | — |
| 19 | silent offer when no notification tool exists | valid | log a warning when no tool is available |
| 20 | `Cargo.lock` committed without `[patch]` | no issue | — |

### Also implemented (from the suggestion list)

- `--log-file <path>` for daemonised runs.
- Desktop honours `LANDROP_CONFIG_DIR`, so the production binary can be smoke
  tested in a sandbox without touching the real config.
- CI workflow: fmt + clippy + tests (Linux), tests (Windows, macOS), desktop
  `cargo check` (Linux, with Tauri deps), MSRV job pinned to the declared floor.
- `docs/INSTANCE_MODEL.md`: why `tauri-plugin-single-instance` is deliberately
  absent and how the build-aware instance logic works.
- Adversarial unit tests for the sanitizer and the address parser.

### Deferred, with reasons

- **Transfer resume (HTTP Range)**: the biggest win for flaky Wi-Fi, but it is a
  protocol change (offer/resume negotiation, partial-file naming, checksum
  over a window) and deserves its own round.
- **Cancel notice to the sender**: the receiver already drops the socket, which
  the sender fails on immediately; an explicit "cancelled" status only pays off
  once resume exists.
- **Transfer history in SQLite/JSON**: stays on the roadmap.
- **`cargo-fuzz` / proptest harness**: the adversarial unit tests cover the
  known-bad inputs; a fuzzing target is worth adding when the protocol settles.
- **Replacing the hand-rolled HTTP probe in `instance.rs` with reqwest**: reqwest
  would become a runtime dependency of the core binary (hyper + TLS) just to read
  one JSON field — the 15 lines stay, now with a bounded read.
