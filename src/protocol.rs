//! Shared wire types and protocol helpers.
//!
//! Everything here is intentionally dependency-free on the rest of the crate so
//! it can be unit-tested in isolation.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Protocol version advertised via mDNS and checked between peers.
pub const PROTOCOL_VERSION: u32 = 1;
/// Header carrying the optional shared token on device-to-device requests.
pub const TOKEN_HEADER: &str = "X-LanDrop-Token";
/// Header carrying the one-time upload token.
pub const UPLOAD_TOKEN_HEADER: &str = "X-LanDrop-Upload-Token";

/// Maximum length of a sanitized filename, in characters.
const MAX_FILENAME_CHARS: usize = 160;
/// Maximum candidates tried when reserving a collision-free filename.
const MAX_NAME_ATTEMPTS: u32 = 10_000;

/// A transfer offer sent by the sender to the receiver.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Offer {
    pub sender_id: String,
    pub sender_name: String,
    pub filename: String,
    pub size: u64,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub note: String,
    /// Relative path inside the sender's selection, for folder sends
    /// (`photos/2024/img.jpg`). Optional and ignored by older peers, so a mixed
    /// pair simply saves the file flat.
    #[serde(default)]
    pub rel_path: Option<String>,
}

/// Streaming SHA-256 of a file (lower-case hex), used when a sender hands over a
/// path instead of uploading bytes through the webview.
pub fn sha256_file(path: &std::path::Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;

    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// How deep a sender-provided relative path may go.
const MAX_REL_DEPTH: usize = 16;

/// Split a sender-provided relative path into safe directory names.
///
/// Refuses anything that could climb out of the download folder (`..`, absolute
/// paths, drive letters), drops empty/`.` segments, sanitizes each name with the
/// same rules as filenames, and caps the depth. `None` means "save flat".
pub fn sanitize_rel_dirs(rel: &str) -> Option<Vec<String>> {
    let rel = rel.trim();
    // Absolute paths and UNC/drive forms are not "inside" anything: refuse them
    // rather than quietly reinterpreting them.
    if rel.starts_with('/') || rel.starts_with('\\') {
        return None;
    }
    let mut dirs = Vec::new();
    for raw in rel.replace('\\', "/").split('/') {
        let part = raw.trim();
        if part.is_empty() || part == "." {
            continue;
        }
        // A segment that tries to walk up, or carries a drive/UNC prefix, means
        // the path is not trustworthy at all.
        if part == ".." || part.contains(':') {
            return None;
        }
        let clean = sanitize_filename(part);
        if clean.is_empty() || clean.starts_with('.') {
            continue;
        }
        dirs.push(clean);
        if dirs.len() >= MAX_REL_DEPTH {
            break;
        }
    }
    if dirs.is_empty() {
        None
    } else {
        Some(dirs)
    }
}

/// Response to an accepted offer: where to upload and with which token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcceptInfo {
    pub status: String,
    pub upload_url: String,
    pub upload_token: String,
}

/// Public view of a transfer, served to the local UI.
#[derive(Debug, Clone, Serialize)]
pub struct TransferView {
    pub id: String,
    pub direction: String,
    pub filename: String,
    pub size: u64,
    pub peer_id: String,
    pub peer_name: String,
    pub status: String,
    pub progress: f64,
    /// Bytes streamed so far.
    pub bytes_done: u64,
    /// Average transfer speed in bytes/second (0 when unknown).
    pub speed_bps: f64,
    /// Estimated seconds remaining (None when unknown).
    pub eta_secs: Option<u64>,
    pub saved_path: Option<String>,
    pub note: String,
    pub error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

// ---------------------------------------------------------------------------
// Filename handling
// ---------------------------------------------------------------------------

/// True for Unicode bidi-override characters, which are stripped so a
/// filename cannot visually misrepresent its extension.
fn is_bidi_override(c: char) -> bool {
    matches!(
        c,
        '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    )
}

/// Truncate to at most `max` characters, preserving the file extension when
/// one is present. Character-count based, so multi-byte names stay valid.
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    match s.rfind('.').filter(|i| *i > 0) {
        Some(i) => {
            let (stem, ext) = s.split_at(i);
            let ext_len = ext.chars().count();
            let keep = max.saturating_sub(ext_len);
            if keep == 0 {
                return s.chars().take(max).collect();
            }
            let mut out: String = stem.chars().take(keep).collect();
            out.push_str(ext);
            out
        }
        None => s.chars().take(max).collect(),
    }
}

/// Sanitize a filename from the wire:
/// - strip any path components (defeats path traversal),
/// - drop control characters (C0, DEL and C1) and bidi overrides,
/// - cap the length (keeping the extension), drop trailing dots/spaces,
/// - empty names get a placeholder.
pub fn sanitize_filename(raw: &str) -> String {
    // Take the last path component; also handle backslashes (Windows-style).
    let base = raw.replace('\\', "/");
    let name = base.rsplit('/').next().unwrap_or(raw);
    let cleaned: String = name
        .chars()
        .filter(|c| !c.is_control() && !is_bidi_override(*c))
        .collect();
    let cleaned = truncate_chars(cleaned.trim(), MAX_FILENAME_CHARS);
    let cleaned = cleaned.replace(['/', '\0'], "_");
    // Trailing dots/spaces are invalid on Windows and confusing elsewhere. Strip
    // them together with *any* trailing whitespace (the Unicode ones `trim`
    // already removes), otherwise a name ending in "\u{a0}." keeps the non-break
    // space on the first pass and loses it on the second — i.e. sanitising would
    // not be idempotent. Found by fuzz/fuzz_targets/sanitize_filename.rs.
    let cleaned = cleaned.trim_end_matches(|c: char| c == '.' || c.is_whitespace());
    if cleaned.is_empty() {
        "unnamed-file".to_string()
    } else {
        cleaned.to_string()
    }
}

/// Reserve a collision-free destination path by atomically creating the file.
///
/// Produces `name`, `name-1`, `name-2`, … and creates the final file with
/// `create_new`, so two concurrent transfers can never pick the same name.
/// The caller renames over the reserved (empty) placeholder to publish.
pub fn reserve_unique_path(dir: &Path, filename: &str) -> std::io::Result<PathBuf> {
    let base = Path::new(filename);
    let stem = base
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = base
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }

    for n in 0..MAX_NAME_ATTEMPTS {
        let candidate = if n == 0 {
            dir.join(format!("{stem}{ext}"))
        } else {
            dir.join(format!("{stem}-{n}{ext}"))
        };
        match opts.open(&candidate) {
            Ok(_) => return Ok(candidate),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!("no free filename for {filename}"),
    ))
}

// ---------------------------------------------------------------------------
// Checksums
// ---------------------------------------------------------------------------

/// Strip a `sha256:` prefix if present (mDNS TXT values often use it).
pub fn normalize_sha256(s: &str) -> Option<String> {
    let s = s.trim();
    let s = s.strip_prefix("sha256:").unwrap_or(s);
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// Simple canonical form used for checksum comparisons.
pub fn canonical_hex(s: &str) -> String {
    s.trim().to_ascii_lowercase()
}

/// True when the string is a full 64-character hex SHA-256 digest.
pub fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_paths_and_dots() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("a\\b\\c.txt"), "c.txt");
        assert_eq!(sanitize_filename(".."), "unnamed-file");
        assert_eq!(sanitize_filename("."), "unnamed-file");
        assert_eq!(sanitize_filename(""), "unnamed-file");
        assert_eq!(sanitize_filename("   "), "unnamed-file");
    }

    #[test]
    fn rel_dirs_are_sanitized_against_escape() {
        assert_eq!(
            sanitize_rel_dirs("photos/2024/vacation"),
            Some(vec!["photos".into(), "2024".into(), "vacation".into()])
        );
        assert_eq!(
            sanitize_rel_dirs("Ünïcode/ok"),
            Some(vec!["Ünïcode".into(), "ok".into()])
        );
        // Traversal, absolute paths and drive letters are refused outright.
        assert_eq!(sanitize_rel_dirs("../../etc"), None);
        assert_eq!(sanitize_rel_dirs("/etc/passwd"), None);
        assert_eq!(sanitize_rel_dirs("C:/Windows"), None);
        // Nothing usable left.
        assert_eq!(sanitize_rel_dirs(""), None);
        assert_eq!(sanitize_rel_dirs("././."), None);
        // Hidden segments are dropped, depth is capped.
        assert_eq!(sanitize_rel_dirs(".hidden/keep"), Some(vec!["keep".into()]));
        assert!(sanitize_rel_dirs(&"a/".repeat(40)).unwrap().len() <= 16);
    }

    #[test]
    fn sanitize_handles_adversarial_names() {
        // Bidi overrides are dropped (they can disguise the real extension),
        // control characters are stripped, trailing dots/spaces go away.
        assert_eq!(sanitize_filename("photo\u{202E}gpj.exe"), "photogpj.exe");
        assert_eq!(sanitize_filename("a\u{0}b.txt"), "ab.txt");
        assert_eq!(sanitize_filename("ok\u{7}.txt"), "ok.txt");
        assert_eq!(sanitize_filename("trailing...  "), "trailing");
        // A trailing dot after Unicode whitespace: both must go, and sanitising
        // the result again must change nothing (fuzzer counterexample).
        assert_eq!(sanitize_filename("Ϳ \u{a0}."), "Ϳ");
        assert_eq!(sanitize_filename("Ϳ \u{a0}"), "Ϳ");
        assert_eq!(sanitize_filename("/"), "unnamed-file");
        // Long names are capped without splitting a code point.
        let long = "é".repeat(MAX_FILENAME_CHARS * 2);
        let out = sanitize_filename(&long);
        assert_eq!(out.chars().count(), MAX_FILENAME_CHARS);
        assert!(out.chars().all(|c| c == 'é'));
    }

    #[test]
    fn sanitize_keeps_plain_names_and_unicode() {
        assert_eq!(sanitize_filename("report.pdf"), "report.pdf");
        assert_eq!(sanitize_filename("héllo wörld.txt"), "héllo wörld.txt");
        // Code points beyond latin-1 must survive (a byte-truncating cast
        // used to eat them).
        assert_eq!(sanitize_filename("Ā-notes.txt"), "Ā-notes.txt");
        assert_eq!(sanitize_filename("報告書.pdf"), "報告書.pdf");
    }

    #[test]
    fn sanitize_removes_control_chars() {
        assert_eq!(sanitize_filename("bad\x00name\x1f.txt"), "badname.txt");
        assert_eq!(sanitize_filename("c1\u{85}nel.txt"), "c1nel.txt");
        assert_eq!(sanitize_filename("evil\u{202e}gnp.exe"), "evilgnp.exe");
    }

    #[test]
    fn sanitize_caps_length_and_trailing_dots() {
        let long = format!("{}.txt", "a".repeat(500));
        let out = sanitize_filename(&long);
        assert!(out.chars().count() <= MAX_FILENAME_CHARS);
        assert!(out.ends_with(".txt"));
        assert_eq!(sanitize_filename("report.txt.."), "report.txt");
    }

    #[test]
    fn truncate_chars_preserves_extension() {
        assert_eq!(truncate_chars("short", 10), "short");
        assert_eq!(truncate_chars("aaaaaaaaaa.txt", 8), "aaaa.txt");
        let t = truncate_chars("報告書報告書.txt", 8);
        assert_eq!(t, "報告書報.txt");
        assert_eq!(t.chars().count(), 8);
    }

    #[test]
    fn reserve_unique_path_avoids_collisions() {
        let dir = tempfile::tempdir().unwrap();
        let p0 = reserve_unique_path(dir.path(), "a.txt").unwrap();
        let p1 = reserve_unique_path(dir.path(), "a.txt").unwrap();
        let p2 = reserve_unique_path(dir.path(), "a.txt").unwrap();
        assert_eq!(p0.file_name().unwrap(), "a.txt");
        assert_eq!(p1.file_name().unwrap(), "a-1.txt");
        assert_eq!(p2.file_name().unwrap(), "a-2.txt");
        // Reserved names exist on disk, so no second caller can pick them.
        assert!(p0.exists() && p1.exists() && p2.exists());
    }

    #[test]
    fn reserve_unique_path_handles_no_extension() {
        let dir = tempfile::tempdir().unwrap();
        let p0 = reserve_unique_path(dir.path(), "archive").unwrap();
        let p1 = reserve_unique_path(dir.path(), "archive").unwrap();
        assert_eq!(p0.file_name().unwrap(), "archive");
        assert_eq!(p1.file_name().unwrap(), "archive-1");
    }

    #[test]
    fn normalize_sha256_strips_prefix() {
        assert_eq!(normalize_sha256("sha256:abc"), Some("abc".into()));
        assert_eq!(normalize_sha256("  "), None);
        assert_eq!(normalize_sha256("abc"), Some("abc".into()));
    }

    #[test]
    fn canonical_hex_is_case_insensitive() {
        assert_eq!(canonical_hex("ABC123"), "abc123");
    }

    #[test]
    fn sha256_hex_validation() {
        assert!(is_sha256_hex(&"a".repeat(64)));
        assert!(!is_sha256_hex("abc"));
        assert!(!is_sha256_hex(&"z".repeat(64)));
    }
}
