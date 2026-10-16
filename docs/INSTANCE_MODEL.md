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

Every node writes `instance.json` (pid + port) into its config dir. On startup
the app looks at that file, probes the port, and asks the node there for its
**build marker** (`GET /api/info` → `"build": "<unix seconds>"`, compiled in by
`build.rs`):

| Situation | Behaviour |
|---|---|
| No live instance | Start a node, open the window (this process owns the backend) |
| Live instance, **same** build | Attach: the window loads the running node's UI; this process owns nothing and exits without stopping the backend |
| Live instance, **different or missing** build marker | Do **not** attach and do **not** refuse to start: log a warning, start a second node on the next free port (`NodeOptions::allow_second_instance` skips the config-dir lock held by the old process), and show the new UI — with an in-app notice naming the port to free |
| Startup race lost (lock taken between probe and start) | Re-probe once and attach if it is the same build |

Because discovery advertises the actual bound port, a node on 8421 is just as
reachable as one on 8420 — nothing needs to be reconfigured. The only cost of
running both is that the older process keeps serving its older UI on its own
port until the user quits it (tray → **Quit**).

## Why not "kill the old instance"?

The app has no channel to ask a foreign process to exit, and killing a process
the user may be transferring files with is worse than showing a clear notice.
The log line and the in-app notice name the port, and the tray menu has **Quit**.

## Where the code lives

- `src/instance.rs` — `instance.json` lock, `live_instance_in`, `remote_build`
  (bounded 8 KiB read), `parse_build`.
- `src/app.rs` — `NodeOptions::allow_second_instance`, the lock usage.
- `desktop/src/lib.rs` — `existing_node` / `start_backend` (the table above),
  and the `?older_instance=<port>` query parameter.
- `ui/app.js` — the notice shown for `older_instance`.

## Testing this

```bash
# Build A: start a node with a temp config
./bin/landrop --config-dir /tmp/landrop-a --downloads /tmp/landrop-a/dl

# Build B: change anything in src/ or ui/ and rebuild, then start again with the
# same config dir — it must log the older instance and serve the new UI on a
# free port instead of attaching.
cargo build && ./target/release/landrop --config-dir /tmp/landrop-a
```

`tests/integration.rs::start_node_binds_and_serves` and
`settings_save_does_not_persist_run_only_overrides` cover the startup side of
this; `src/instance.rs` unit tests cover build-marker parsing for current,
older and unreachable nodes.
