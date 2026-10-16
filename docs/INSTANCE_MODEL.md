# Instance model (why there is no single-instance plugin)

LanDrop runs one backend node per config dir, but **two builds may run side by
side**. This document records how that works and why the obvious shortcut —
Tauri's `tauri-plugin-single-instance` — is deliberately *not* used.

## What the plugin would do

`tauri-plugin-single-instance` keys on the app's bundle identifier. The second
launch of *any* build of the app forwards its arguments to the first process and
then exits silently (status 0, no output). That is fine for "focus the window
that is already open" — and it is wrong for an app whose UI is compiled into the
binary:

1. A user installs a new `.deb` while the old app is still running.
2. They launch the new build.
3. The plugin sees an instance with the same bundle identifier, hands the launch
   over, and exits. The **old window with the old UI** comes to the front.
4. The user concludes the update did nothing.

That is exactly the bug that was reported against this project, and the plugin
was removed in `84dde25` after reproducing it.

## What happens instead

Every node writes `instance.json` (pid + port) into its config dir and reports a
**build marker** (`GET /api/info` → `"build": "<unix seconds>"`, compiled in by
`build.rs`).

**One instance only.** A second launch never opens a second window:

| Situation | Behaviour |
|---|---|
| No live instance | Start a node and open the window (this process owns the backend) |
| Live instance, **same** build | `POST /api/window/focus` on the running node, then exit quietly — the window that is already open comes to the front |
| Live instance, **different** build, idle | Ask it to quit through its local-only `POST /api/quit`, wait (bounded, ≤5 s) for the port to free up, then start this build and open the window — an installed update takes over cleanly |
| Live instance, **different** build, transferring | Leave it alone (a transfer is never interrupted), show a notification — *"An older copy is transferring files on port 8420. Quit it from its tray menu, then open LanDrop again."* — and exit |
| Live instance too old to understand `/api/quit` | Same as above: notification and exit. No second window, and no silently showing the older UI |
| Startup race lost (lock taken between probe and start) | Re-probe once and hand over if it is the same build |

The two local-only endpoints that make this work:

| Endpoint | Who calls it | Effect |
|---|---|---|
| `POST /api/quit` | a newer build replacing an older one | graceful shutdown of that node (not a kill: discovery stops, the server finishes, the lock is released) |
| `POST /api/window/focus` | a second launch of the same build | un-minimize + raise + focus the existing window |

Both refuse anything that does not come from this machine (`403`) and are
unreachable from a browser page thanks to the global cross-origin/`Host` guard.

## Why not a plugin, and not "kill the old instance"?

A plugin keyed on the app identity cannot see *which* build owns the window, which
is the whole problem. A hard kill would work, but it can interrupt a transfer the
user is watching and leaves no opportunity for the older process to release its
instance lock — so the newer build asks politely, and only when the older node
reports no transfer in flight (`GET /api/transfers`; unknown answers count as
busy).

## Where the code lives

- `src/instance.rs` — `instance.json` lock, `live_instance_in`, `remote_build`,
  `request_quit`, `request_focus`, `has_active_transfers` (bounded 8 KiB reads).
- `src/app.rs` — `NodeOptions::{allow_second_instance, on_focus}`, the lock usage.
- `src/server/handlers.rs` — `/api/quit` and `/api/window/focus` (both gated by
  `is_local_peer`).
- `desktop/src/lib.rs` — `existing_node` / `start_backend` (the table above), the
  focus hook, and `wait_for_port_free`.
- `ui/app.js` — nothing instance-specific any more; the UI is the same in both modes.

## Testing this

```bash
# Node A: start with a temp config
./bin/landrop --config-dir /tmp/landrop-a --downloads /tmp/landrop-a/dl

# Node B: change anything in src/ or ui/ and rebuild, then start again with the
# same config dir — it must retire the idle older node and take over the port.
cargo build && ./target/release/landrop --config-dir /tmp/landrop-a
```

Automated coverage: `tests/integration.rs::focus_and_quit_endpoints_drive_the_window_and_the_node`
(the hook runs; `/api/quit` stops the node), plus the build-marker tests in
`src/instance.rs`. `docs/security-tests.md` maps every rule to its test.
