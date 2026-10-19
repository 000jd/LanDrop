//! Guards for the one-command installer: syntax, help text and the dry-run path
//! must work without touching the network or the user's home.

#[cfg(unix)]
mod unix {
    use std::path::PathBuf;
    use std::process::Command;

    fn script() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("install/install.sh")
    }

    fn bash_available() -> bool {
        Command::new("bash")
            .arg("--version")
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn installer_is_syntactically_valid_and_has_a_safe_cli_surface() {
        if !bash_available() {
            eprintln!("skipping installer test: bash is not available");
            return;
        }
        let path = script();
        assert!(path.is_file(), "{} is missing", path.display());

        let syntax = Command::new("bash")
            .arg("-n")
            .arg(&path)
            .output()
            .expect("run bash -n");
        assert!(
            syntax.status.success(),
            "bash -n failed: {}",
            String::from_utf8_lossy(&syntax.stderr)
        );

        let help = Command::new("bash")
            .arg(&path)
            .arg("--help")
            .output()
            .expect("run installer --help");
        assert!(help.status.success());
        assert!(String::from_utf8_lossy(&help.stdout).contains("Usage:"));

        let dry = Command::new("bash")
            .arg(&path)
            .arg("--dry-run")
            .env("LANDROP_VERSION", "v0.1.0")
            .output()
            .expect("run installer --dry-run");
        assert!(
            dry.status.success(),
            "dry run failed: {}",
            String::from_utf8_lossy(&dry.stderr)
        );
        let text = String::from_utf8_lossy(&dry.stdout);
        assert!(text.contains("LanDrop installer (dry run)"));
        assert!(text.contains("v0.1.0"));
        assert!(text.contains(".local/bin/landrop"));

        let uninstall = Command::new("bash")
            .arg(&path)
            .args(["--uninstall", "--dry-run"])
            .output()
            .expect("run installer --uninstall --dry-run");
        assert!(uninstall.status.success());
        assert!(String::from_utf8_lossy(&uninstall.stdout).contains("LanDrop uninstalled."));
    }
}
