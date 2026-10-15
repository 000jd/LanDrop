use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.svg");
    println!("cargo:rerun-if-changed=ui");
    println!("cargo:rerun-if-changed=build.rs");

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
