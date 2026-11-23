use std::process::Command;

#[test]
fn help_and_configuration_validation_do_not_require_a_window() {
    let help = Command::new(env!("CARGO_BIN_EXE_nox"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("--config-dir"));
    let check = Command::new(env!("CARGO_BIN_EXE_nox"))
        .env_remove("NOX_CONFIG_DIR")
        .arg("--check-config")
        .output()
        .unwrap();
    assert!(check.status.success());
    assert!(String::from_utf8_lossy(&check.stdout).contains("Configuration valid"));
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
