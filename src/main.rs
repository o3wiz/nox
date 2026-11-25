mod ui;
use anyhow::{Context, Result, bail};
use nox::config::{self, Config};
use std::path::PathBuf;

const USAGE: &str = "\
Nox — native log explorer

Usage: nox [--config-dir DIR] [FILE ...]
       nox --init-config DIR
       nox --write-schemas DIR
       nox --check-config [--config-dir DIR]
       nox --version

NOX_CONFIG_DIR selects a configuration directory. Missing files/fields use built-in defaults.
Use -- before filenames starting with a dash.";

fn main() {
    if let Err(error) = start() {
        eprintln!("nox: {error:#}");
        std::process::exit(1);
    }
}

fn start() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let mut directory = std::env::var_os("NOX_CONFIG_DIR").map(PathBuf::from);
    let mut files = Vec::new();
    let mut validate = false;
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
                ))
            }
            Some("--init-config") => {
                let path =
                    PathBuf::from(args.next().context("--init-config requires a directory")?);
                config::write_defaults(&path)?;
                println!("Created configuration in {}", path.display());
                return Ok(());
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
