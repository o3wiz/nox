mod ui;
use anyhow::{Context, Result, bail, ensure};
use nox::config::{self, Config};
use std::path::PathBuf;

const USAGE: &str = "\
Nox — native log explorer

Usage: nox [--config-dir DIR] [FILE ...]
       nox --init-config DIR
       nox --migrate-config [DIR]
       nox --write-schemas DIR
       nox --check-config [--config-dir DIR]
       nox --version

NOX_CONFIG_DIR selects a configuration directory. Missing files/fields use built-in defaults.
--migrate-config adds missing config files/fields/keybindings and updates schemas in an existing
configuration directory (default: $NOX_CONFIG_DIR, ~/.config/nox on macOS/Linux, or
%APPDATA%/nox on Windows) without removing existing fields.
Use -- before filenames starting with a dash.";

fn main() {
    if let Err(error) = start() {
        eprintln!("nox: {error:#}");
        std::process::exit(1);
    }
}

fn start() -> Result<()> {
    let mut args = std::env::args_os().skip(1).peekable();
    let mut directory = std::env::var_os("NOX_CONFIG_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let mut explicit_config_dir = false;
    let mut files = Vec::new();
    let mut validate = false;
    let mut migrate = false;
    let mut migrate_dir = None;
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--help" | "-h") => {
                println!("{USAGE}");
                return Ok(());
            }
            Some("--version" | "-V") => {
                println!("nox {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            Some("--config-dir") => {
                directory = Some(PathBuf::from(
                    args.next().context("--config-dir requires a directory")?,
                ));
                explicit_config_dir = true;
            }
            Some("--init-config") => {
                let path =
                    PathBuf::from(args.next().context("--init-config requires a directory")?);
                config::write_defaults(&path)?;
                println!("Created configuration in {}", path.display());
                return Ok(());
            }
            Some("--migrate-config") => {
                migrate = true;
                if let Some(next) = args.peek()
                    && !next.to_str().is_some_and(|option| option.starts_with('-'))
                {
                    migrate_dir = Some(PathBuf::from(args.next().unwrap()));
                }
            }
            Some("--write-schemas") => {
                let path = PathBuf::from(
                    args.next()
                        .context("--write-schemas requires a directory")?,
                );
                config::write_schemas(&path)?;
                println!("Generated schemas in {}", path.display());
                return Ok(());
            }
            Some("--check-config") => validate = true,
            Some("--") => {
                files.extend(args.map(PathBuf::from));
                break;
            }
            // macOS LaunchServices may pass `-psn_0_*` when opening a `.app` bundle.
            Some(option) if cfg!(target_os = "macos") && option.starts_with("-psn_") => {}
            Some(option) if option.starts_with('-') => {
                bail!("Unknown option: {option}. Use --help.")
            }
            _ => files.push(PathBuf::from(arg)),
        }
    }
    if migrate {
        ensure!(
            !validate,
            "--migrate-config cannot be combined with --check-config"
        );
        ensure!(
            files.is_empty(),
            "--migrate-config accepts at most one directory"
        );
        if let Some(path) = migrate_dir {
            ensure!(
                !explicit_config_dir,
                "Specify either --migrate-config DIR or --config-dir DIR, not both"
            );
            let report = config::migrate_config(&path)?;
            println!("{}", report.summary(&path));
            return Ok(());
        }
        if explicit_config_dir {
            let path = directory.expect("explicit --config-dir sets directory");
            let report = config::migrate_config(&path)?;
            println!("{}", report.summary(&path));
            return Ok(());
        }
        let path = config::default_config_dir().context(
            "Unable to determine default configuration directory; pass --migrate-config DIR or set NOX_CONFIG_DIR",
        )?;
        if !path.exists() {
            println!(
                "No existing configuration in {}; skipping migration",
                path.display()
            );
            return Ok(());
        }
        let report = config::migrate_config(&path)?;
        println!("{}", report.summary(&path));
        return Ok(());
    }
    let config = Config::load(directory.as_deref())?;
    ui::keybindings(&config).context("Invalid keyboard.json")?;
    if validate {
        println!("Configuration valid (7 sections)");
        return Ok(());
    }
    if files.is_empty() {
        files.extend(config.files.startup.iter().map(PathBuf::from));
    }
    ui::run(config, files)
}
