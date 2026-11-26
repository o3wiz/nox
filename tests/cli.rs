use std::process::Command;

#[test]
fn help_version_and_configuration_validation_do_not_require_a_window() {
    let help = Command::new(env!("CARGO_BIN_EXE_nox"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(help.status.success());
    let help_stdout = String::from_utf8_lossy(&help.stdout);
    assert!(help_stdout.contains("--config-dir"));
    assert!(help_stdout.contains("--migrate-config"));
    assert!(help_stdout.contains("--version"));

    for flag in ["--version", "-V"] {
        let version = Command::new(env!("CARGO_BIN_EXE_nox"))
            .arg(flag)
            .output()
            .unwrap();
        assert!(version.status.success(), "{flag} failed");
        assert_eq!(
            String::from_utf8_lossy(&version.stdout).trim(),
            format!("nox {}", env!("CARGO_PKG_VERSION"))
        );
    }

    let check = Command::new(env!("CARGO_BIN_EXE_nox"))
        .env_remove("NOX_CONFIG_DIR")
        .arg("--check-config")
        .output()
        .unwrap();
    assert!(check.status.success());
    assert!(String::from_utf8_lossy(&check.stdout).contains("Configuration valid"));
}

#[test]
fn init_config_and_write_schemas_round_trip_through_cli() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_dir = dir.path().join("cfg");
    let init = Command::new(env!("CARGO_BIN_EXE_nox"))
        .arg("--init-config")
        .arg(&cfg_dir)
        .output()
        .unwrap();
    assert!(init.status.success());
    assert!(cfg_dir.join("window.json").is_file());
    assert!(cfg_dir.join("schemas/session.schema.json").is_file());

    let check = Command::new(env!("CARGO_BIN_EXE_nox"))
        .args(["--check-config", "--config-dir"])
        .arg(&cfg_dir)
        .output()
        .unwrap();
    assert!(check.status.success());

    let schema_dir = dir.path().join("schemas");
    let schemas = Command::new(env!("CARGO_BIN_EXE_nox"))
        .arg("--write-schemas")
        .arg(&schema_dir)
        .output()
        .unwrap();
    assert!(schemas.status.success());
    assert!(schema_dir.join("window.schema.json").is_file());
    assert!(schema_dir.join("session.schema.json").is_file());
}

#[test]
fn invalid_cli_or_keyboard_config_exits_with_actionable_errors() {
    let invalid = Command::new(env!("CARGO_BIN_EXE_nox"))
        .arg("--unknown")
        .output()
        .unwrap();
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("Unknown option"));

    let dir = tempfile::tempdir().unwrap();
    let missing = Command::new(env!("CARGO_BIN_EXE_nox"))
        .args(["--check-config", "--config-dir"])
        .arg(dir.path().join("does-not-exist"))
        .output()
        .unwrap();
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("does not exist"));

    std::fs::write(
        dir.path().join("keyboard.json"),
        r#"{"bindings":[{"keys":"invalidmodifier-x","context":"Log","action":"open"}]}"#,
    )
    .unwrap();
    let invalid = Command::new(env!("CARGO_BIN_EXE_nox"))
        .args(["--check-config", "--config-dir"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("Invalid keyboard.json"));
}

#[test]
fn migrate_config_cli_updates_explicit_and_default_directories_and_skips_missing_default() {
    let dir = tempfile::tempdir().unwrap();

    // Explicit directory migration.
    let explicit = dir.path().join("explicit");
    std::fs::create_dir_all(&explicit).unwrap();
    std::fs::write(
        explicit.join("window.json"),
        r#"{"width":1500,"deprecated_opt":"keep"}"#,
    )
    .unwrap();
    std::fs::create_dir_all(explicit.join("schemas")).unwrap();
    std::fs::write(explicit.join("schemas/window.schema.json"), r#"{"old":1}"#).unwrap();

    let first = Command::new(env!("CARGO_BIN_EXE_nox"))
        .env_remove("NOX_CONFIG_DIR")
        .arg("--migrate-config")
        .arg(&explicit)
        .output()
        .unwrap();
    assert!(
        first.status.success(),
        "migrate failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(String::from_utf8_lossy(&first.stdout).contains("Migrated configuration"));
    assert!(explicit.join("files.json").is_file());
    assert!(explicit.join("schemas/session.schema.json").is_file());

    let window: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(explicit.join("window.json")).unwrap())
            .unwrap();
    assert_eq!(window["width"], 1500);
    assert_eq!(window["deprecated_opt"], "keep");
    assert_eq!(window["$schema"], "schemas/window.schema.json");
    assert_eq!(window["show_status_bar"], true);

    // Second run is idempotent and also works with `--config-dir`.
    let second = Command::new(env!("CARGO_BIN_EXE_nox"))
        .env_remove("NOX_CONFIG_DIR")
        .args(["--migrate-config", "--config-dir"])
        .arg(&explicit)
        .output()
        .unwrap();
    assert!(second.status.success());
    assert!(String::from_utf8_lossy(&second.stdout).contains("up to date"));

    // Explicit missing directory errors cleanly.
    let missing_explicit = Command::new(env!("CARGO_BIN_EXE_nox"))
        .env_remove("NOX_CONFIG_DIR")
        .arg("--migrate-config")
        .arg(dir.path().join("does-not-exist"))
        .output()
        .unwrap();
    assert!(!missing_explicit.status.success());
    assert!(String::from_utf8_lossy(&missing_explicit.stderr).contains("does not exist"));

    // Default resolution via NOX_CONFIG_DIR skips when the directory does not exist.
    let missing_env_dir = dir.path().join("env-missing");
    let skipped = Command::new(env!("CARGO_BIN_EXE_nox"))
        .env("NOX_CONFIG_DIR", &missing_env_dir)
        .arg("--migrate-config")
        .output()
        .unwrap();
    assert!(skipped.status.success());
    assert!(String::from_utf8_lossy(&skipped.stdout).contains("skipping migration"));
    assert!(!missing_env_dir.exists());

    // Default resolution via NOX_CONFIG_DIR migrates when the directory exists.
    let env_dir = dir.path().join("env-existing");
    std::fs::create_dir_all(&env_dir).unwrap();
    std::fs::write(env_dir.join("viewer.json"), r#"{"tab_width":2}"#).unwrap();
    let env_migrated = Command::new(env!("CARGO_BIN_EXE_nox"))
        .env("NOX_CONFIG_DIR", &env_dir)
        .arg("--migrate-config")
        .output()
        .unwrap();
    assert!(env_migrated.status.success());
    assert!(String::from_utf8_lossy(&env_migrated.stdout).contains("Migrated configuration"));
    let viewer: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(env_dir.join("viewer.json")).unwrap())
            .unwrap();
    assert_eq!(viewer["tab_width"], 2);
    assert_eq!(viewer["cursor_surrounding_lines"], 0);
    assert!(env_dir.join("schemas/viewer.schema.json").is_file());

    // Default platform directory (without NOX_CONFIG_DIR) skips when missing and migrates when present.
    let fake_home = dir.path().join("home");
    std::fs::create_dir_all(&fake_home).unwrap();
    #[cfg(target_os = "windows")]
    let platform_default = fake_home.join("nox");
    #[cfg(not(target_os = "windows"))]
    let platform_default = fake_home.join(".config/nox");

    let mut skip_platform = Command::new(env!("CARGO_BIN_EXE_nox"));
    skip_platform
        .env_remove("NOX_CONFIG_DIR")
        .env_remove("XDG_CONFIG_HOME")
        .env("HOME", &fake_home)
        .env("APPDATA", &fake_home)
        .arg("--migrate-config");
    let skip_platform_out = skip_platform.output().unwrap();
    assert!(skip_platform_out.status.success());
    assert!(String::from_utf8_lossy(&skip_platform_out.stdout).contains("skipping migration"));
    assert!(!platform_default.exists());

    std::fs::create_dir_all(&platform_default).unwrap();
    std::fs::write(platform_default.join("window.json"), r#"{"width":1400}"#).unwrap();
    let mut migrate_platform = Command::new(env!("CARGO_BIN_EXE_nox"));
    migrate_platform
        .env_remove("NOX_CONFIG_DIR")
        .env_remove("XDG_CONFIG_HOME")
        .env("HOME", &fake_home)
        .env("APPDATA", &fake_home)
        .arg("--migrate-config");
    let migrate_platform_out = migrate_platform.output().unwrap();
    assert!(migrate_platform_out.status.success());
    assert!(
        String::from_utf8_lossy(&migrate_platform_out.stdout).contains("Migrated configuration")
    );
    assert!(
        platform_default
            .join("schemas/window.schema.json")
            .is_file()
    );
    let check_migrated = Command::new(env!("CARGO_BIN_EXE_nox"))
        .env_remove("NOX_CONFIG_DIR")
        .args(["--check-config", "--config-dir"])
        .arg(&platform_default)
        .output()
        .unwrap();
    assert!(check_migrated.status.success());

    // Conflicting or invalid CLI combinations fail cleanly.
    for args in [
        vec!["--migrate-config", "a", "b"],
        vec!["--migrate-config", "a", "--config-dir", "b"],
        vec!["--migrate-config", "--check-config"],
        vec!["--migrate-config", "--unknown"],
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_nox"))
            .env_remove("NOX_CONFIG_DIR")
            .args(&args)
            .output()
            .unwrap();
        assert!(!out.status.success(), "unexpectedly succeeded for {args:?}");
    }
}

#[test]
fn check_config_warns_when_preset_select_text_is_missing_from_pattern() {
    let dir = tempfile::tempdir().unwrap();
    let search_path = dir.path().join("search.json");
    std::fs::write(
        &search_path,
        r#"{
            "presets": [
                {
                    "name": "Port request",
                    "pattern": "GET .* port 8080",
                    "mode": "regex",
                    "select_text": "PORT_NUMBER"
                }
            ]
        }"#,
    )
    .unwrap();

    let warned = Command::new(env!("CARGO_BIN_EXE_nox"))
        .env_remove("NOX_CONFIG_DIR")
        .args(["--check-config", "--config-dir"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(warned.status.success());
    let stdout = String::from_utf8_lossy(&warned.stdout);
    let stderr = String::from_utf8_lossy(&warned.stderr);
    assert!(stdout.contains("Configuration valid"));
    assert!(stdout.contains("1 warning(s)"));
    assert!(stderr.contains("warning:"));
    assert!(stderr.contains("Port request"));
    assert!(stderr.contains("PORT_NUMBER"));

    std::fs::write(
        &search_path,
        r#"{
            "presets": [
                {
                    "name": "Port request",
                    "pattern": "GET .* port PORT_NUMBER",
                    "mode": "regex",
                    "select_text": "PORT_NUMBER"
                }
            ]
        }"#,
    )
    .unwrap();

    let clean = Command::new(env!("CARGO_BIN_EXE_nox"))
        .env_remove("NOX_CONFIG_DIR")
        .args(["--check-config", "--config-dir"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(clean.status.success());
    assert!(String::from_utf8_lossy(&clean.stdout).contains("Configuration valid (7 sections)"));
    assert!(String::from_utf8_lossy(&clean.stderr).is_empty());
}

#[cfg(target_os = "macos")]
#[test]
fn ignores_macos_launch_services_psn_argument() {
    let check = Command::new(env!("CARGO_BIN_EXE_nox"))
        .env_remove("NOX_CONFIG_DIR")
        .args(["-psn_0_123456", "--check-config"])
        .output()
        .unwrap();
    assert!(check.status.success());
    assert!(String::from_utf8_lossy(&check.stdout).contains("Configuration valid"));
}
