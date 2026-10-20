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
            .env("LANDROP_VERSION", "v0.2.0")
            .output()
            .expect("run installer --dry-run");
        assert!(
            dry.status.success(),
            "dry run failed: {}",
            String::from_utf8_lossy(&dry.stderr)
        );
        let text = String::from_utf8_lossy(&dry.stdout);
        assert!(text.contains("LanDrop installer (dry run)"));
        assert!(text.contains("v0.2.0"));
        assert!(text.contains(".local/bin/landrop"));

        let uninstall = Command::new("bash")
            .arg(&path)
            .args(["--uninstall", "--dry-run"])
            .output()
            .expect("run installer --uninstall --dry-run");
        assert!(uninstall.status.success());
        assert!(String::from_utf8_lossy(&uninstall.stdout).contains("LanDrop uninstalled."));
    }

    #[test]
    fn systemd_user_unit_matches_the_installer_targets() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let unit = std::fs::read_to_string(root.join("assets/landrop.service"))
            .expect("assets/landrop.service is missing");
        assert!(
            unit.contains("ExecStart=%h/.local/bin/landrop --hidden"),
            "the unit must start the user-local binary in the background"
        );
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("WantedBy=default.target"));

        let makefile = std::fs::read_to_string(root.join("Makefile")).expect("read Makefile");
        assert!(makefile.contains("install-service:"));
        assert!(makefile.contains("uninstall-service:"));
        assert!(makefile.contains("assets/landrop.service"));
    }
}
