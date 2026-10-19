//! Small shared helpers used across the crate.

use std::io;
use std::path::Path;
use std::process::Command;

/// Spawn a helper process and reap it on a background thread so it never
/// becomes a zombie.
pub fn spawn_detached(cmd: &mut Command) -> io::Result<()> {
    let mut child = cmd.spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Free bytes available on the filesystem containing `path` (0 when unknown).
///
/// `saturating_mul`: on a 32-bit target a huge filesystem can make the two
/// factors overflow `u64` in theory, and a saturated number is a much better
/// answer than a wrapped one when we use it as a "is there room?" guard.
#[cfg(unix)]
#[allow(clippy::unnecessary_cast)] // u64 on Linux, u32 on macOS and BSD
pub fn free_space(path: &Path) -> u64 {
    nix::sys::statvfs::statvfs(path)
        .map(|s| (s.blocks_available() as u64).saturating_mul(s.fragment_size() as u64))
        .unwrap_or(0)
}

#[cfg(windows)]
pub fn free_space(path: &Path) -> u64 {
    disk_usage(path).map(|(free, _)| free).unwrap_or(0)
}

#[cfg(not(any(unix, windows)))]
pub fn free_space(_path: &Path) -> u64 {
    0
}

/// `(free, total)` bytes of the filesystem containing `path` (None when unknown).
#[cfg(unix)]
#[allow(clippy::unnecessary_cast)] // u64 on Linux, u32 on macOS and BSD
pub fn disk_usage(path: &Path) -> Option<(u64, u64)> {
    nix::sys::statvfs::statvfs(path).ok().map(|s| {
        let unit = s.fragment_size() as u64;
        (
            (s.blocks_available() as u64).saturating_mul(unit),
            (s.blocks() as u64).saturating_mul(unit),
        )
    })
}

#[cfg(windows)]
pub fn disk_usage(path: &Path) -> Option<(u64, u64)> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    let (mut free, mut total, mut total_free) = (0u64, 0u64, 0u64);
    // SAFETY: `wide` is NUL-terminated and outlives the call; the out pointers
    // are valid for the duration of the call.
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, &mut total, &mut total_free) };
    (ok != 0).then_some((free, total))
}

#[cfg(not(any(unix, windows)))]
pub fn disk_usage(_path: &Path) -> Option<(u64, u64)> {
    None
}

/// Open a file, folder or URL with the platform's default handler (shared by the
/// local API, the desktop wrapper and the CLI).
pub fn open_target(target: &str) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let mut cmd = Command::new("xdg-open");
        cmd.arg(target);
        spawn_detached(&mut cmd)
    }
    #[cfg(target_os = "macos")]
    {
        let mut cmd = Command::new("open");
        cmd.arg(target);
        spawn_detached(&mut cmd)
    }
    #[cfg(target_os = "windows")]
    {
        let mut cmd = Command::new("cmd");
        cmd.args(["/c", "start", "", target]);
        spawn_detached(&mut cmd)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = target;
        Ok(())
    }
}

/// Compact human size: `812 B`, `1.9 MB`, `2.4 GB`.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [(&str, u64); 4] = [
        ("GB", 1024 * 1024 * 1024),
        ("MB", 1024 * 1024),
        ("KB", 1024),
        ("B", 1),
    ];
    for (unit, scale) in UNITS {
        if bytes >= scale {
            let value = bytes as f64 / scale as f64;
            return if scale == 1 || value >= 10.0 {
                format!("{} {unit}", value.round() as u64)
            } else {
                format!("{value:.1} {unit}")
            };
        }
    }
    "0 B".to_string()
}

/// Compact human duration: `42 s`, `3 min 5 s`, `2 h 4 min`.
pub fn human_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{secs} s")
    } else if secs < 3600 {
        format!("{} min {} s", secs / 60, secs % 60)
    } else {
        format!("{} h {} min", secs / 3600, (secs % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_are_compact() {
        assert_eq!(human_duration(0), "0 s");
        assert_eq!(human_duration(42), "42 s");
        assert_eq!(human_duration(65), "1 min 5 s");
        assert_eq!(human_duration(3600), "1 h 0 min");
        assert_eq!(human_duration(7860), "2 h 11 min");
    }

    #[cfg(unix)]
    #[test]
    fn disk_usage_reports_a_sane_filesystem() {
        let (free, total) = disk_usage(Path::new("/")).expect("root filesystem is readable");
        assert!(total > 0);
        assert!(free <= total);
    }

    #[test]
    fn sizes_are_compact() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(812), "812 B");
        assert_eq!(human_size(2048), "2.0 KB");
        assert_eq!(human_size(1_990_000), "1.9 MB");
        assert_eq!(human_size(2_400_000_000), "2.2 GB");
    }

    /// The guarded arithmetic must never wrap, whatever the OS reports.
    #[test]
    fn space_maths_saturates_instead_of_wrapping() {
        let unit = 4096u64;
        assert_eq!(u64::MAX.saturating_mul(unit), u64::MAX);
        assert_eq!(0u64.saturating_mul(unit), 0);
    }
}
