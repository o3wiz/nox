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
