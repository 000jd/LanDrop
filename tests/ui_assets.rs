//! Guards for the embedded UI: a missing element id crashes `init()` and leaves
//! the top bar stuck on "…", so this is checked as part of `cargo test`.

use std::collections::HashSet;
use std::path::PathBuf;

fn read(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("ui")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Values following `needle` up to the next quote (`id="x"`, `.id = "x"`).
fn quoted_after(text: &str, needle: &str) -> HashSet<String> {
    let mut found = HashSet::new();
    let mut rest = text;
    while let Some(pos) = rest.find(needle) {
        rest = &rest[pos + needle.len()..];
        if let Some(end) = rest.find('"') {
            found.insert(rest[..end].to_string());
            rest = &rest[end..];
        }
    }
    found
}

#[test]
fn every_id_used_by_app_js_exists_in_index_html() {
    let app = read("app.js");
    let html = read("index.html");

    let ids_in_html = quoted_after(&html, "id=\"");
    let created = quoted_after(&app, ".id = \"");

    let mut missing: Vec<String> = quoted_after(&app, "$(\"")
        .into_iter()
        .filter(|id| !ids_in_html.contains(id) && !created.contains(id))
        .collect();
    missing.sort();

    assert!(
        missing.is_empty(),
        "app.js references ids that index.html does not define: {missing:?}"
    );
}

#[test]
fn the_send_panel_has_its_snippet_and_share_controls() {
    let html = read("index.html");
    for id in [
        "textToSend",
        "sendTextBtn",
        "shareLinkBtn",
        "showQrBtn",
        "settingsQrBtn",
        "sendTargetLabel",
        "sendBtn",
    ] {
        assert!(
            html.contains(&format!("id=\"{id}\"")),
            "index.html is missing #{id}"
        );
    }
}
