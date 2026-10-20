//! Guards for the embedded UI and packaged assets: a missing element id crashes
//! `init()` and leaves the top bar stuck on "…", and a broken icon makes the
//! installed app look blank, so both are checked as part of `cargo test`.

use std::collections::HashSet;
use std::path::PathBuf;

fn read(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("ui")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn repo_path(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative)
}

fn read_repo(relative: &str) -> String {
    let path = repo_path(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Validate the PNG signature and chunk structure, returning the IHDR size.
fn png_dimensions(relative: &str) -> (u32, u32) {
    let path = repo_path(relative);
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    assert!(
        bytes.len() >= 33,
        "{} is empty or too short to be a PNG",
        path.display()
    );
    assert_eq!(
        &bytes[..8],
        b"\x89PNG\r\n\x1a\n",
        "{} has no PNG signature",
        path.display()
    );

    let mut offset = 8;
    let mut dimensions = None;
    let mut saw_iend = false;
    while offset + 12 <= bytes.len() {
        let length = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
        let kind = &bytes[offset + 4..offset + 8];
        let data_start = offset + 8;
        let data_end = data_start + length;
        let chunk_end = data_end + 4;
        assert!(
            chunk_end <= bytes.len(),
            "{} has a truncated {:?} chunk",
            path.display(),
            String::from_utf8_lossy(kind)
        );
        if kind == b"IHDR" {
            assert!(length >= 8, "{} has a short IHDR", path.display());
            let width = u32::from_be_bytes(bytes[data_start..data_start + 4].try_into().unwrap());
            let height =
                u32::from_be_bytes(bytes[data_start + 4..data_start + 8].try_into().unwrap());
            dimensions = Some((width, height));
        }
        if kind == b"IEND" {
            saw_iend = true;
            break;
        }
        offset = chunk_end;
    }

    let (width, height) = dimensions.unwrap_or_else(|| panic!("{} has no IHDR", path.display()));
    assert!(
        width > 0 && height > 0,
        "{} reports an empty size {width}x{height}",
        path.display()
    );
    assert!(saw_iend, "{} has no IEND chunk", path.display());
    (width, height)
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
        "pasteTextBtn",
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

#[test]
fn android_launchers_use_vectors_and_declare_a_round_icon() {
    let manifest = read_repo("desktop/gen/android/app/src/main/AndroidManifest.xml");
    assert!(
        manifest.contains("android:icon=\"@mipmap/ic_launcher\""),
        "AndroidManifest.xml is missing android:icon"
    );
    assert!(
        manifest.contains("android:roundIcon=\"@mipmap/ic_launcher_round\""),
        "AndroidManifest.xml is missing android:roundIcon"
    );

    for name in ["ic_launcher.xml", "ic_launcher_round.xml"] {
        let relative = format!("desktop/gen/android/app/src/main/res/mipmap-anydpi-v26/{name}");
        let xml = read_repo(&relative);
        assert!(
            xml.contains("android:drawable=\"@drawable/ic_launcher_background\""),
            "{relative} does not use the vector background"
        );
        assert!(
            xml.contains("android:drawable=\"@drawable/ic_launcher_foreground\""),
            "{relative} does not use the vector foreground"
        );
        assert!(
            xml.contains("<monochrome android:drawable=\"@drawable/ic_launcher_foreground\""),
            "{relative} is missing the Android 13+ monochrome layer"
        );
        assert!(
            !xml.contains("@mipmap/ic_launcher_foreground"),
            "{relative} still uses the old bitmap foreground"
        );
    }

    let background =
        read_repo("desktop/gen/android/app/src/main/res/drawable/ic_launcher_background.xml");
    assert!(!background.to_lowercase().contains("<rect"));
    assert_eq!(background.matches("<path").count(), 1);
    assert!(background.contains("android:width=\"108dp\""));
    assert!(background.contains("android:height=\"108dp\""));
    assert!(background.contains("android:viewportWidth=\"108\""));
    assert!(background.contains("android:viewportHeight=\"108\""));
    assert!(background.contains("#2383E2"));
    assert!(background.contains("M0,0h108v108h-108z"));

    let foreground =
        read_repo("desktop/gen/android/app/src/main/res/drawable/ic_launcher_foreground.xml");
    assert!(
        !foreground.to_lowercase().contains("<rect"),
        "Android VectorDrawable does not support <rect>; use <path> data"
    );
    assert_eq!(
        foreground.matches("<path").count(),
        4,
        "the adaptive foreground must contain only the four inner squares"
    );
    for fill in ["#FFFFFF", "#E6F2FC", "#B8D8F7"] {
        assert!(foreground.contains(fill), "foreground is missing {fill}");
    }
    assert!(foreground.contains("android:width=\"108dp\""));
    assert!(foreground.contains("android:height=\"108dp\""));
    assert!(foreground.contains("android:viewportWidth=\"108\""));
    assert!(foreground.contains("android:viewportHeight=\"108\""));

    for path in foreground.split("<path").skip(1) {
        let data = path
            .split("android:pathData=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .expect("foreground <path> without pathData");
        assert!(data.starts_with('M'), "path does not start with M: {data}");
        assert!(data.contains('A'), "path has no rounded corners: {data}");
        assert!(data.ends_with('Z'), "path is not closed: {data}");
    }
}

#[test]
fn every_icon_asset_is_present_and_valid() {
    let expected = [
        ("assets/icon-32.png", 32u32, 32u32),
        ("assets/icon-128.png", 128, 128),
        ("assets/icon.png", 256, 256),
        ("assets/icon-512.png", 512, 512),
        ("assets/icon-soft.png", 128, 128),
        ("assets/icon-with-name.png", 512, 279),
        (
            "desktop/gen/android/app/src/main/res/mipmap-mdpi/ic_launcher.png",
            48,
            48,
        ),
        (
            "desktop/gen/android/app/src/main/res/mipmap-mdpi/ic_launcher_round.png",
            48,
            48,
        ),
        (
            "desktop/gen/android/app/src/main/res/mipmap-mdpi/ic_launcher_foreground.png",
            108,
            108,
        ),
        (
            "desktop/gen/android/app/src/main/res/mipmap-hdpi/ic_launcher.png",
            72,
            72,
        ),
        (
            "desktop/gen/android/app/src/main/res/mipmap-hdpi/ic_launcher_round.png",
            72,
            72,
        ),
        (
            "desktop/gen/android/app/src/main/res/mipmap-hdpi/ic_launcher_foreground.png",
            162,
            162,
        ),
        (
            "desktop/gen/android/app/src/main/res/mipmap-xhdpi/ic_launcher.png",
            96,
            96,
        ),
        (
            "desktop/gen/android/app/src/main/res/mipmap-xhdpi/ic_launcher_round.png",
            96,
            96,
        ),
        (
            "desktop/gen/android/app/src/main/res/mipmap-xhdpi/ic_launcher_foreground.png",
            216,
            216,
        ),
        (
            "desktop/gen/android/app/src/main/res/mipmap-xxhdpi/ic_launcher.png",
            144,
            144,
        ),
        (
            "desktop/gen/android/app/src/main/res/mipmap-xxhdpi/ic_launcher_round.png",
            144,
            144,
        ),
        (
            "desktop/gen/android/app/src/main/res/mipmap-xxhdpi/ic_launcher_foreground.png",
            324,
            324,
        ),
        (
            "desktop/gen/android/app/src/main/res/mipmap-xxxhdpi/ic_launcher.png",
            192,
            192,
        ),
        (
            "desktop/gen/android/app/src/main/res/mipmap-xxxhdpi/ic_launcher_round.png",
            192,
            192,
        ),
        (
            "desktop/gen/android/app/src/main/res/mipmap-xxxhdpi/ic_launcher_foreground.png",
            432,
            432,
        ),
    ];
    for (relative, width, height) in expected {
        assert_eq!(
            png_dimensions(relative),
            (width, height),
            "{relative} has the wrong size"
        );
    }

    // The tray and window icons are raw RGBA buffers.
    for (rgba, width, height) in [
        ("assets/icon-32.rgba", 32u32, 32u32),
        ("assets/icon-64.rgba", 64, 64),
    ] {
        let len = std::fs::metadata(repo_path(rgba))
            .unwrap_or_else(|e| panic!("read {rgba}: {e}"))
            .len();
        assert_eq!(
            len,
            u64::from(width) * u64::from(height) * 4,
            "{rgba} is not a {width}x{height} RGBA buffer"
        );
    }

    let ico = std::fs::read(repo_path("assets/icon.ico")).expect("read assets/icon.ico");
    assert!(
        ico.len() > 4 && ico[..4] == [0, 0, 1, 0],
        "icon.ico is invalid"
    );

    let icns = std::fs::read(repo_path("assets/icon.icns")).expect("read assets/icon.icns");
    assert!(
        icns.len() > 8 && &icns[..4] == b"icns",
        "icon.icns is invalid"
    );
    assert_eq!(
        u32::from_be_bytes(icns[4..8].try_into().unwrap()) as usize,
        icns.len(),
        "icon.icns length header does not match the file"
    );
}

#[test]
fn desktop_bundles_are_configured_for_every_icon_size() {
    let config = read_repo("desktop/tauri.conf.json");
    for icon in [
        "../assets/icon-32.png",
        "../assets/icon-128.png",
        "../assets/icon.png",
        "../assets/icon-512.png",
        "../assets/icon.ico",
        "../assets/icon.icns",
        "../assets/icon.svg",
    ] {
        assert!(config.contains(icon), "tauri.conf.json is missing {icon}");
    }
    assert!(
        config.contains("/usr/share/icons/hicolor/scalable/apps/landrop-desktop.svg"),
        "deb/rpm/AppImage must install the scalable SVG explicitly"
    );

    let entry = read_repo("assets/landrop.desktop");
    assert!(
        entry.contains("StartupWMClass=landrop"),
        "assets/landrop.desktop is missing StartupWMClass"
    );
    let makefile = read_repo("Makefile");
    assert!(
        makefile.contains("StartupWMClass=landrop"),
        "make install-desktop must set StartupWMClass"
    );

    let installer = read_repo("scripts/install-desktop-app.sh");
    assert!(installer.contains("StartupWMClass=landrop-desktop"));
    for size in ["32x32", "128x128", "256x256", "512x512"] {
        assert!(
            installer.contains(&format!("{size}/apps/landrop-desktop.png")),
            "user-local installer is missing the {size} icon"
        );
    }
    assert!(installer.contains("scalable/apps/landrop-desktop.svg"));
    assert!(installer.contains("gtk-update-icon-cache"));
    assert!(installer.contains("update-desktop-database"));

    let one_liner = read_repo("install/install.sh");
    assert!(one_liner.contains("for size in 32 128 256 512"));
    assert!(one_liner.contains("StartupWMClass=${DESKTOP_NAME}"));
    assert!(one_liner.contains("gtk-update-icon-cache"));
    assert!(one_liner.contains("update-desktop-database"));
}

#[test]
fn icon_generator_can_verify_android_resources() {
    let script = read_repo("scripts/generate_icons.py");
    assert!(script.contains("--verify-android"));
    assert!(script.contains("ic_launcher_background"));
    assert!(script.contains("ic_launcher_foreground"));
    assert!(script.contains("mipmap-anydpi-v26"));
}
