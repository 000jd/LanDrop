//! Login autostart so this device stays visible on the LAN without opening
//! the full window. Linux: `~/.config/autostart/landrop.desktop`.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};

fn autostart_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("autostart").join("landrop.desktop"))
}

/// Quote a path for a desktop-entry `Exec=` line.
///
/// Inside double quotes the Desktop Entry spec requires `"`, `` ` ``, `$` and
/// `\` to be backslash-escaped; a path containing them would otherwise produce
/// a malformed (or differently-meant) command line.
fn quote_exec_arg(path: &str) -> String {
    let mut out = String::with_capacity(path.len() + 2);
    out.push('"');
    for c in path.chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// Command used when the session starts (quoted path + `--hidden`).
fn launch_command() -> Result<String> {
    if let Ok(appimage) = std::env::var("APPIMAGE") {
        if !appimage.is_empty() {
            return Ok(format!("{} --hidden", quote_exec_arg(&appimage)));
        }
    }
    let exe = std::env::current_exe().context("current executable path")?;
    Ok(format!(
        "{} --hidden",
        quote_exec_arg(&exe.display().to_string())
    ))
}

/// Whether an autostart entry is present.
pub fn is_enabled() -> bool {
    autostart_path().map(|p| p.exists()).unwrap_or(false)
}

/// Create or remove the session autostart entry.
pub fn set_enabled(on: bool) -> Result<()> {
    let path = autostart_path().context("could not determine autostart directory")?;
    if !on {
        if path.exists() {
            fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
        }
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let exec = launch_command()?;
    let body = format!(
        "\
[Desktop Entry]\n\
Type=Application\n\
Name=LanDrop\n\
Comment=Background LAN file sharing (stays available to other devices)\n\
Exec={exec}\n\
Icon=landrop\n\
Terminal=false\n\
X-GNOME-Autostart-enabled=true\n\
StartupNotify=false\n"
    );
    fs::write(&path, body).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::quote_exec_arg;

    #[test]
    fn exec_paths_are_escaped_for_desktop_entries() {
        assert_eq!(quote_exec_arg("/usr/bin/landrop"), "\"/usr/bin/landrop\"");
        assert_eq!(
            quote_exec_arg("/home/me/My \"Apps\"/landrop"),
            "\"/home/me/My \\\"Apps\\\"/landrop\""
        );
        // `$`, backticks and backslashes must survive a desktop-entry parser.
        assert_eq!(
            quote_exec_arg("/a/$HOME/`x`/\\b"),
            "\"/a/\\$HOME/\\`x\\`/\\\\b\""
        );
    }
}
