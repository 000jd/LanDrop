//! Desktop notifications and Accept / Reject prompts.
//!
//! Incoming transfers can be handled without opening the full UI: a system
//! notification (and, if needed, a small zenity/kdialog dialog) asks the user
//! to accept or reject. While a transfer streams, a progress notification is
//! kept up to date in place (via the notification daemon when replace is
//! supported).
//!
//! Never blocks the HTTP server — work runs in the background. Failures are
//! ignored: notifications are best-effort.

#[cfg(target_os = "linux")]
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::process::Command;
#[cfg(target_os = "linux")]
use std::sync::OnceLock;

use crate::transfer::TransferManager;

/// Icon names installed by the different packaging paths: the CLI/browser
/// entry uses `landrop`, the Tauri desktop package uses `landrop-desktop`.
#[cfg(target_os = "linux")]
const ICON_NAMES: [&str; 2] = ["landrop", "landrop-desktop"];

#[cfg(target_os = "linux")]
fn push_theme_icons(out: &mut Vec<PathBuf>, root: &Path, name: &str) {
    out.push(root.join("scalable/apps").join(format!("{name}.svg")));
    for size in ["512x512", "256x256", "128x128", "64x64", "48x48", "32x32"] {
        out.push(root.join(size).join("apps").join(format!("{name}.png")));
    }
}

/// Candidate icon paths, most specific install first: user-local (`make
/// install-desktop` / `install-desktop-app.sh`), the AppImage mount, then the
/// system hicolor tree installed by the `.deb` and `.rpm`.
#[cfg(target_os = "linux")]
fn icon_candidates(home: Option<&Path>, appdir: Option<&Path>) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(home) = home {
        let hicolor = home.join(".local/share/icons/hicolor");
        for name in ICON_NAMES {
            push_theme_icons(&mut candidates, &hicolor, name);
        }
    }
    if let Some(appdir) = appdir {
        candidates.push(appdir.join("landrop-desktop.png"));
        candidates.push(appdir.join("LanDrop.png"));
        candidates.push(appdir.join(".DirIcon"));
    }
    let system = Path::new("/usr/share/icons/hicolor");
    for name in ICON_NAMES {
        push_theme_icons(&mut candidates, system, name);
    }
    for name in ICON_NAMES {
        candidates.push(Path::new("/usr/share/pixmaps").join(format!("{name}.png")));
    }
    candidates
}

/// Best-effort path to the SVG/PNG app icon for notify-send.
#[cfg(target_os = "linux")]
fn notification_icon() -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let appdir = std::env::var_os("APPDIR").map(PathBuf::from);
    for path in icon_candidates(home.as_deref(), appdir.as_deref()) {
        if path.is_file() {
            return path.to_string_lossy().into_owned();
        }
    }
    "landrop".into()
}

/// Cached `notify-send --help` text: spawning it per notification is wasteful.
#[cfg(target_os = "linux")]
fn notify_send_help() -> Option<&'static str> {
    static HELP: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    HELP.get_or_init(|| {
        let out = Command::new("notify-send").arg("--help").output().ok()?;
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        Some(text)
    })
    .as_deref()
}

#[cfg(target_os = "linux")]
fn notify_send_has_actions() -> bool {
    notify_send_help()
        .map(|t| t.contains("--action") || t.contains("-A,"))
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn notify_send_has_hints() -> bool {
    notify_send_help()
        .map(|t| t.contains("--hint"))
        .unwrap_or(false)
}

/// Show a desktop notification with the given title and body.
pub fn notify(title: &str, body: &str) {
    #[cfg(target_os = "linux")]
    {
        let icon = notification_icon();
        let mut cmd = Command::new("notify-send");
        cmd.arg("-a")
            .arg("LanDrop")
            .arg("-i")
            .arg(&icon)
            .arg(title)
            .arg(body);
        let _ = crate::util::spawn_detached(&mut cmd);
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (title, body);
    }
}

// ---------------------------------------------------------------------------
// Progress notifications
// ---------------------------------------------------------------------------

/// Strip control characters from text handed to external tools: they carry no
/// meaning in a notification or dialog, and a newline or NUL can break or
/// truncate the call.
#[cfg(target_os = "linux")]
fn clean_text(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// Say it once instead of silently dropping every notification.
#[cfg(target_os = "linux")]
fn warn_missing_notify_tool() {
    static WARNED: OnceLock<()> = OnceLock::new();
    if WARNED.set(()).is_ok() {
        tracing::warn!(
            "no progress-capable notification tool found (gdbus / notify-send with hints); \
             falling back to plain notifications, if any tool exists at all"
        );
    }
}

/// Same for the accept/reject dialog tools.
#[cfg(target_os = "linux")]
fn warn_missing_dialog_tool() {
    static WARNED: OnceLock<()> = OnceLock::new();
    if WARNED.set(()).is_ok() {
        tracing::warn!(
            "no dialog tool found (zenity / kdialog); incoming offers are shown in the app UI only"
        );
    }
}

/// Arguments for the `gdbus` Notify call that shows a progress bar.
#[cfg(target_os = "linux")]
fn gdbus_notify_args(
    replaces: u32,
    icon: &str,
    title: &str,
    body: &str,
    percent: u8,
    timeout_ms: u32,
) -> Vec<String> {
    vec![
        "call".into(),
        "--session".into(),
        "--dest".into(),
        "org.freedesktop.Notifications".into(),
        "--object-path".into(),
        "/org/freedesktop/Notifications".into(),
        "--method".into(),
        "org.freedesktop.Notifications.Notify".into(),
        // `--` ends option parsing, so a title or body that begins with `-`
        // cannot be mistaken for a gdbus flag.
        "--".into(),
        "LanDrop".into(),
        replaces.to_string(),
        icon.to_string(),
        clean_text(title),
        clean_text(body),
        "[]".into(),                               // actions
        format!("{{'value': <int32 {percent}>}}"), // progress hint
        timeout_ms.to_string(),
    ]
}

/// Show or update the progress notification for one transfer.
///
/// `finished` makes this the final update (it auto-expires afterwards);
/// otherwise the notification stays visible and is replaced in place on every
/// call. Best-effort on every platform.
#[cfg(target_os = "linux")]
pub fn progress(transfer_id: &str, title: &str, body: &str, percent: u8, finished: bool) {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    /// Notification ids returned by the daemon, per transfer.
    fn ids() -> &'static Mutex<HashMap<String, u32>> {
        static IDS: OnceLock<Mutex<HashMap<String, u32>>> = OnceLock::new();
        IDS.get_or_init(|| Mutex::new(HashMap::new()))
    }
    fn have_gdbus() -> bool {
        static HAVE: OnceLock<bool> = OnceLock::new();
        *HAVE.get_or_init(|| Command::new("gdbus").arg("--version").output().is_ok())
    }

    let icon = notification_icon();
    if have_gdbus() {
        // Chain the id the daemon returned, so updates replace in place.
        let replaces = ids()
            .lock()
            .map(|m| m.get(transfer_id).copied().unwrap_or(0))
            .unwrap_or(0);
        let timeout = if finished { 5_000 } else { 0 };
        let args = gdbus_notify_args(replaces, &icon, title, body, percent, timeout);
        let key = transfer_id.to_string();
        std::thread::spawn(move || {
            let Ok(out) = Command::new("gdbus").args(&args).output() else {
                return;
            };
            if !out.status.success() {
                return;
            }
            let digits: String = String::from_utf8_lossy(&out.stdout)
                .chars()
                .filter(|c| c.is_ascii_digit())
                .collect();
            let new_id = digits.parse::<u32>().ok();
            match (finished, new_id) {
                (false, Some(id)) => {
                    if let Ok(mut m) = ids().lock() {
                        m.insert(key, id);
                    }
                }
                (true, _) => {
                    if let Ok(mut m) = ids().lock() {
                        m.remove(&key);
                    }
                }
                _ => {}
            }
        });
    } else if notify_send_has_hints() {
        // No replace support: only show the start and the final state, so the
        // notification list does not fill with stale percentages.
        if percent == 0 || finished {
            let title = clean_text(title);
            let body = clean_text(body);
            let mut cmd = Command::new("notify-send");
            cmd.args([
                "-a",
                "LanDrop",
                "-i",
                &icon,
                "-h",
                &format!("int:value:{percent}"),
                "-t",
                if finished { "5000" } else { "0" },
                "--",
                &title,
                &body,
            ]);
            let _ = crate::util::spawn_detached(&mut cmd);
        }
    } else {
        // Neither gdbus nor a hint-capable notify-send: progress cannot be
        // shown live, so only the plain fallback is left.
        warn_missing_notify_tool();
        if finished {
            notify(title, body);
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub fn progress(_transfer_id: &str, _title: &str, _body: &str, _percent: u8, _finished: bool) {}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    Accept,
    Reject,
    Ignored,
}

/// One prompt for a whole batch (a folder or multi-file selection).
///
/// The files arrive as separate offers, so we wait until the batch stops growing
/// before asking — otherwise a 40-file folder would mean 40 dialogs. Accepting
/// releases every waiting member; rejecting rejects them all. Best-effort: files
/// that disappeared meanwhile are skipped.
pub fn prompt_batch(
    mgr: TransferManager,
    batch_id: String,
    batch_name: String,
    sender: String,
    pin: String,
) {
    #[cfg(not(target_os = "linux"))]
    {
        // Mobile has no system dialog: the in-app UI shows the batch.
        let _ = (mgr, batch_id, batch_name, sender, pin);
    }
    #[cfg(target_os = "linux")]
    {
        tokio::spawn(async move {
            // Wait for the batch to settle: three quiet half-second ticks.
            let mut last = usize::MAX;
            let mut quiet = 0u32;
            for _ in 0..240 {
                let waiting = mgr.waiting_in_batch(&batch_id).await;
                if waiting.is_empty() {
                    mgr.end_batch_prompt(&batch_id);
                    return;
                }
                if waiting.len() == last {
                    quiet += 1;
                    if quiet >= 3 {
                        break;
                    }
                } else {
                    quiet = 0;
                    last = waiting.len();
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }

            let members = mgr.waiting_in_batch(&batch_id).await;
            if members.is_empty() {
                mgr.end_batch_prompt(&batch_id);
                return;
            }
            let (files, bytes) = mgr.batch_totals(&members).await;
            let body = format!(
                "{sender} wants to send you \"{batch_name}\" — {files} files ({}) [PIN {pin}]",
                crate::util::human_size(bytes)
            );
            notify("LanDrop — incoming folder", &body);

            let body_c = body.clone();
            let decision = tokio::task::spawn_blocking(move || ask_user(&body_c))
                .await
                .unwrap_or(Decision::Ignored);
            tracing::info!("batch {batch_id}: {files} files decided {decision:?}");
            // A later batch with the same id may prompt again.
            mgr.end_batch_prompt(&batch_id);

            match decision {
                Decision::Accept => {
                    for id in members {
                        if let Err(e) = mgr.accept(&id, None).await {
                            tracing::warn!("batch accept failed for {id}: {e:#}");
                        }
                    }
                }
                Decision::Reject => {
                    for id in members {
                        if let Err(e) = mgr.reject(&id).await {
                            tracing::warn!("batch reject failed for {id}: {e:#}");
                        }
                    }
                }
                Decision::Ignored => {
                    // Left to the app UI.
                }
            }
        });
    }
}

/// Prompt the user to accept or reject an incoming file without opening the
/// full app. Runs asynchronously so the offer handler can return immediately.
pub fn prompt_incoming(
    mgr: TransferManager,
    id: String,
    sender: String,
    filename: String,
    pin: String,
) {
    #[cfg(not(target_os = "linux"))]
    {
        // No system dialog on mobile: the in-app UI shows the request.
        let _ = (mgr, id, sender, filename, pin);
    }
    #[cfg(target_os = "linux")]
    {
        let body = format!("{sender} wants to send you '{filename}' [PIN {pin}]");
        notify("LanDrop — incoming file", &body);
        tokio::spawn(async move {
            let body_c = body.clone();
            let decision = tokio::task::spawn_blocking(move || ask_user(&body_c))
                .await
                .unwrap_or(Decision::Ignored);
            tracing::info!(
                "offer {id}: prompt decided {decision:?} (notification / zenity / kdialog, \
                 whichever answered first)"
            );

            match decision {
                Decision::Accept => {
                    if let Err(e) = mgr.accept(&id, None).await {
                        tracing::warn!("notification accept failed: {e:#}");
                    }
                }
                Decision::Reject => {
                    if let Err(e) = mgr.reject(&id).await {
                        tracing::warn!("notification reject failed: {e:#}");
                    }
                }
                Decision::Ignored => {
                    notify(
                        "LanDrop",
                        &format!(
                            "{sender} wants to send you '{filename}' [PIN {pin}]. \
                             Open LanDrop to accept or reject."
                        ),
                    );
                }
            }
        });
    }
}

#[cfg(target_os = "linux")]
/// Ask the user with a finished message body; callers format it themselves, so a
/// batch message is never wrapped twice.
fn ask_user(body: &str) -> Decision {
    if let Some(d) = notify_send_actions(body) {
        return d;
    }
    if let Some(d) = zenity_question(body) {
        return d;
    }
    if let Some(d) = kdialog_yesno(body) {
        return d;
    }
    warn_missing_dialog_tool();
    Decision::Ignored
}

/// libnotify `notify-send --action` (GNOME and others). Returns `None` when
/// the binary is missing or does not support actions.
#[cfg(target_os = "linux")]
fn notify_send_actions(body: &str) -> Option<Decision> {
    #[cfg(target_os = "linux")]
    {
        if !notify_send_has_actions() {
            return None;
        }
        let icon = notification_icon();
        let started = std::time::Instant::now();
        let out = Command::new("notify-send")
            .args([
                "-a",
                "LanDrop",
                "-i",
                &icon,
                "-u",
                "critical",
                "-t",
                "110000",
                "-w",
                "-A",
                "accept=Accept",
                "-A",
                "reject=Reject",
                "LanDrop — incoming file",
                body,
            ])
            .output()
            .ok()?;
        let elapsed = started.elapsed();
        if !out.status.success() && out.stdout.is_empty() {
            return None;
        }
        let s = String::from_utf8_lossy(&out.stdout)
            .trim()
            .to_ascii_lowercase();
        // A person cannot click within a few hundred milliseconds: an immediate
        // answer means the daemon echoed a default (some do when actions are not
        // supported). Never accept a file on that basis.
        let deliberate = elapsed >= std::time::Duration::from_millis(300);
        match s.as_str() {
            "accept" if deliberate => Some(Decision::Accept),
            "reject" if deliberate => Some(Decision::Reject),
            _ => {
                tracing::info!(
                    "notification prompt answered {s:?} after {elapsed:?}; treating as not accepted"
                );
                Some(Decision::Ignored)
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = body;
        None
    }
}

#[cfg(target_os = "linux")]
fn zenity_question(body: &str) -> Option<Decision> {
    #[cfg(target_os = "linux")]
    {
        let started = std::time::Instant::now();
        let status = Command::new("zenity")
            .args([
                "--question",
                "--title=LanDrop",
                "--ok-label=Accept",
                "--cancel-label=Reject",
                "--timeout=110",
                "--width=380",
                &format!("--text={body}"),
            ])
            .status()
            .ok()?;
        let elapsed = started.elapsed();
        let deliberate = elapsed >= std::time::Duration::from_millis(300);
        match status.code() {
            Some(0) if deliberate => Some(Decision::Accept),
            Some(1) if deliberate => Some(Decision::Reject),
            // Timeout (5), a broken dialog, or an answer too fast to be a click.
            _ => Some(Decision::Ignored),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = body;
        None
    }
}

#[cfg(target_os = "linux")]
fn kdialog_yesno(body: &str) -> Option<Decision> {
    #[cfg(target_os = "linux")]
    {
        let started = std::time::Instant::now();
        let status = Command::new("timeout")
            .args([
                "110",
                "kdialog",
                "--title",
                "LanDrop",
                "--yesno",
                body,
                "--yes-label",
                "Accept",
                "--no-label",
                "Reject",
            ])
            .status()
            .ok()?;
        let elapsed = started.elapsed();
        let deliberate = elapsed >= std::time::Duration::from_millis(300);
        match status.code() {
            Some(0) if deliberate => Some(Decision::Accept),
            Some(1) if deliberate => Some(Decision::Reject),
            // 124 = timed out; 127 = kdialog not installed; 2 = dialog error.
            // Anything but an explicit answer must not decide the transfer, and
            // an answer that arrives as fast as the process starts was not a
            // click either.
            _ => Some(Decision::Ignored),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = body;
        None
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn notification_icon_candidates_cover_every_install_layout() {
        let home = Path::new("/home/tester");
        let appdir = Path::new("/tmp/.mount_LanDrop");
        let paths: Vec<String> = icon_candidates(Some(home), Some(appdir))
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        for expected in [
            "/home/tester/.local/share/icons/hicolor/scalable/apps/landrop.svg",
            "/home/tester/.local/share/icons/hicolor/256x256/apps/landrop.png",
            "/home/tester/.local/share/icons/hicolor/scalable/apps/landrop-desktop.svg",
            "/home/tester/.local/share/icons/hicolor/512x512/apps/landrop-desktop.png",
            "/tmp/.mount_LanDrop/landrop-desktop.png",
            "/usr/share/icons/hicolor/scalable/apps/landrop.svg",
            "/usr/share/icons/hicolor/256x256/apps/landrop-desktop.png",
            "/usr/share/pixmaps/landrop.png",
        ] {
            assert!(
                paths.iter().any(|p| p == expected),
                "missing icon candidate {expected}: {paths:?}"
            );
        }
    }

    #[test]
    fn gdbus_args_carry_replace_id_and_progress_hint() {
        let args = gdbus_notify_args(
            77,
            "/tmp/icon.svg",
            "LanDrop — receiving",
            "file.bin — 42% · ~18 s left",
            42,
            0,
        );
        assert!(args
            .iter()
            .any(|a| a == "org.freedesktop.Notifications.Notify"));
        assert!(
            args.iter().any(|a| a == "77"),
            "replace id is passed: {args:?}"
        );
        assert!(
            args.iter().any(|a| a == "{'value': <int32 42>}"),
            "progress hint is passed: {args:?}"
        );
        assert!(args.iter().any(|a| a == "/tmp/icon.svg"));
        assert_eq!(
            args.last().map(String::as_str),
            Some("0"),
            "never expire while running"
        );
    }
}
