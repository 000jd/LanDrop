#![no_main]
//! Whatever a peer sends, sanitising must never panic and must always produce a
//! name we are willing to write to disk.

use landrop::protocol::sanitize_filename;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &str| {
    let out = sanitize_filename(data);

    assert!(!out.is_empty(), "a name is always produced");
    assert!(!out.contains('/'), "no path separators: {out:?}");
    assert!(!out.contains('\\'), "no backslashes: {out:?}");
    assert!(!out.contains('\0'), "no NUL: {out:?}");
    assert!(
        !out.chars().any(char::is_control),
        "no control characters: {out:?}"
    );
    assert!(out.len() <= 1024, "bounded length: {} bytes", out.len());
    assert!(!out.ends_with(['.', ' ']), "no Windows-invalid ending: {out:?}");

    // Sanitising is idempotent: the result is already safe.
    assert_eq!(sanitize_filename(&out), out, "idempotent for {data:?}");
});
