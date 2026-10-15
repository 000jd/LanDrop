use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.svg");
    println!("cargo:rerun-if-changed=ui");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=desktop/src");
    println!("cargo:rerun-if-changed=build.rs");

    // Build marker: a unix timestamp that changes whenever any source or UI
    // file changes. It is reported by /api/info and shown in Settings, so a
    // stale binary (or a stale running instance) is immediately visible
    // instead of looking like a UI that "did not update".
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!("cargo:rustc-env=LANADROP_BUILD_ID={stamp}");

    // Keep the embedded UI logo in sync with the canonical SVG mark. Only
    // write when the content actually differs, so plain builds never dirty
    // the working tree.
    let src = Path::new("assets/icon.svg");
    let dest = Path::new("ui/logo.svg");
    match (std::fs::read(src), std::fs::read(dest)) {
        (Ok(new), Ok(old)) if new == old => {}
        (Ok(new), _) => {
            if let Err(e) = std::fs::write(dest, new) {
                println!("cargo:warning=failed to copy icon.svg to ui/logo.svg: {e}");
            }
        }
        (Err(e), _) => {
            println!("cargo:warning=assets/icon.svg is missing or unreadable: {e}");
        }
    }
}
