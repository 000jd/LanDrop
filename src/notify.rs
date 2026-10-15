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
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::process::Command;

use crate::transfer::TransferManager;

/// Best-effort path to the SVG (or SVG-derived) app icon for notify-send.
#[cfg(target_os = "linux")]
fn notification_icon() -> String {
    if let Ok(home) = std::env::var("HOME") {
        let svg = PathBuf::from(&home).join(".local/share/icons/hicolor/scalable/apps/landrop.svg");
        if svg.is_file() {
            return svg.to_string_lossy().into_owned();
        }
        let png = PathBuf::from(&home).join(".local/share/icons/hicolor/256x256/apps/landrop.png");
        if png.is_file() {
            return png.to_string_lossy().into_owned();
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
        "LanDrop".into(),
        replaces.to_string(),
        icon.to_string(),
        title.to_string(),
        body.to_string(),
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
                title,
                body,
            ]);
            let _ = crate::util::spawn_detached(&mut cmd);
        }
    } else if finished {
        notify(title, body);
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

/// Prompt the user to accept or reject an incoming file without opening the
/// full app. Runs asynchronously so the offer handler can return immediately.
pub fn prompt_incoming(mgr: TransferManager, id: String, sender: String, filename: String) {
    #[cfg(not(target_os = "linux"))]
    {
        // No system dialog on mobile: the in-app UI shows the request.
        let _ = (mgr, id, sender, filename);
    }
    #[cfg(target_os = "linux")]
    {
        notify(
            "LanDrop — incoming file",
            &format!("{sender} wants to send you '{filename}'"),
        );
        tokio::spawn(async move {
            let sender_c = sender.clone();
            let filename_c = filename.clone();
            let decision = tokio::task::spawn_blocking(move || ask_user(&sender_c, &filename_c))
                .await
                .unwrap_or(Decision::Ignored);

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
                            "{sender} wants to send you '{filename}'. Open LanDrop to accept or reject."
                        ),
                    );
                }
            }
        });
    }
}

#[cfg(target_os = "linux")]
fn ask_user(sender: &str, filename: &str) -> Decision {
    let body = format!("{sender} wants to send you '{filename}'");
    if let Some(d) = notify_send_actions(&body) {
        return d;
    }
    if let Some(d) = zenity_question(&body) {
        return d;
    }
    if let Some(d) = kdialog_yesno(&body) {
        return d;
    }
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
        if !out.status.success() && out.stdout.is_empty() {
            return None;
        }
        let s = String::from_utf8_lossy(&out.stdout)
            .trim()
            .to_ascii_lowercase();
        match s.as_str() {
            "accept" => Some(Decision::Accept),
            "reject" => Some(Decision::Reject),
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
fn zenity_question(body: &str) -> Option<Decision> {
    #[cfg(target_os = "linux")]
    {
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
        match status.code() {
            Some(0) => Some(Decision::Accept),
            Some(1) => Some(Decision::Reject),
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
        match status.code() {
            Some(0) => Some(Decision::Accept),
            Some(1) => Some(Decision::Reject),
            // 124 = timed out; 127 = kdialog not installed; 2 = dialog error.
            // Anything but an explicit "No" must not auto-reject the transfer.
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
