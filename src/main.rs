mod ui;
use anyhow::{Context, Result, bail, ensure};
use nox::{
    config::{self, Config},
    stdin::{self, StdinSpool},
};
use std::{ffi::OsString, path::PathBuf};

const USAGE: &str = "\
Nox — native log explorer

Usage: nox [--config-dir DIR] [FILE | - ...]
       nox --init-config DIR
       nox --migrate-config [DIR]
       nox --write-schemas DIR
       nox --check-config [--config-dir DIR]
       nox --version

NOX_CONFIG_DIR selects a configuration directory. Missing files/fields use built-in defaults.
--migrate-config adds missing config files/fields/keybindings and updates schemas in an existing
configuration directory (default: $NOX_CONFIG_DIR, ~/.config/nox on macOS/Linux, or
%APPDATA%/nox on Windows) without removing existing fields.
Piped or redirected standard input is opened automatically when no files are given,
or explicitly with -. Use -- before filenames starting with a dash.";

#[derive(Clone, Debug, PartialEq, Eq)]
enum CliInput {
    File(PathBuf),
    Stdin,
}

#[derive(Debug, PartialEq, Eq)]
enum CliAction {
    Help,
    Version,
    InitConfig(PathBuf),
    WriteSchemas(PathBuf),
    MigrateConfig {
        directory: Option<PathBuf>,
        explicit_config_dir: bool,
        migrate_dir: Option<PathBuf>,
    },
    CheckConfig {
        directory: Option<PathBuf>,
    },
    Run {
        directory: Option<PathBuf>,
        inputs: Vec<CliInput>,
    },
}

fn parse_cli_args(
    args: impl IntoIterator<Item = OsString>,
    env_config_dir: Option<OsString>,
) -> Result<CliAction> {
    let mut args = args.into_iter().peekable();
    let mut directory = env_config_dir
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let mut explicit_config_dir = false;
    let mut inputs = Vec::new();
    let mut validate = false;
    let mut migrate = false;
    let mut migrate_dir = None;

    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--help" | "-h") => return Ok(CliAction::Help),
            Some("--version" | "-V") => return Ok(CliAction::Version),
            Some("--config-dir") => {
                directory = Some(PathBuf::from(
                    args.next().context("--config-dir requires a directory")?,
                ));
                explicit_config_dir = true;
            }
            Some("--init-config") => {
                let path =
                    PathBuf::from(args.next().context("--init-config requires a directory")?);
                return Ok(CliAction::InitConfig(path));
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
                return Ok(CliAction::WriteSchemas(path));
            }
            Some("--check-config") => validate = true,
            Some("--") => {
                inputs.extend(args.map(|path| CliInput::File(PathBuf::from(path))));
                break;
            }
            Some("-") => {
                if !inputs.contains(&CliInput::Stdin) {
                    inputs.push(CliInput::Stdin);
                }
            }
            // macOS LaunchServices may pass `-psn_0_*` when opening a `.app` bundle.
            Some(option) if cfg!(target_os = "macos") && option.starts_with("-psn_") => {}
            Some(option) if option.starts_with('-') => {
                bail!("Unknown option: {option}. Use --help.")
            }
            _ => inputs.push(CliInput::File(PathBuf::from(arg))),
        }
    }

    if migrate {
        ensure!(
            !validate,
            "--migrate-config cannot be combined with --check-config"
        );
        ensure!(
            inputs.is_empty(),
            "--migrate-config accepts at most one directory"
        );
        return Ok(CliAction::MigrateConfig {
            directory,
            explicit_config_dir,
            migrate_dir,
        });
    }

    if validate {
        return Ok(CliAction::CheckConfig { directory });
    }

    Ok(CliAction::Run { directory, inputs })
}

fn resolve_launch_inputs(
    cli_inputs: Vec<CliInput>,
    has_piped_stdin: bool,
    startup_files: &[String],
) -> Vec<CliInput> {
    if !cli_inputs.is_empty() {
        return cli_inputs;
    }
    if has_piped_stdin {
        return vec![CliInput::Stdin];
    }
    startup_files
        .iter()
        .map(|path| CliInput::File(PathBuf::from(path)))
        .collect()
}

fn main() {
    if let Err(error) = start() {
        eprintln!("nox: {error:#}");
        std::process::exit(1);
    }
}

fn start() -> Result<()> {
    match parse_cli_args(
        std::env::args_os().skip(1),
        std::env::var_os("NOX_CONFIG_DIR"),
    )? {
        CliAction::Help => {
            println!("{USAGE}");
            Ok(())
        }
        CliAction::Version => {
            println!("nox {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        CliAction::InitConfig(path) => {
            config::write_defaults(&path)?;
            println!("Created configuration in {}", path.display());
            Ok(())
        }
        CliAction::WriteSchemas(path) => {
            config::write_schemas(&path)?;
            println!("Generated schemas in {}", path.display());
            Ok(())
        }
        CliAction::MigrateConfig {
            directory,
            explicit_config_dir,
            migrate_dir,
        } => {
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
            Ok(())
        }
        CliAction::CheckConfig { directory } => {
            let config = Config::load(directory.as_deref())?;
            ui::keybindings(&config).context("Invalid keyboard.json")?;
            let warnings = config.warnings();
            for warning in &warnings {
                eprintln!("nox: warning: {warning}");
            }
            if warnings.is_empty() {
                println!("Configuration valid (7 sections)");
            } else {
                println!(
                    "Configuration valid (7 sections, {} warning(s))",
                    warnings.len()
                );
            }
            Ok(())
        }
        CliAction::Run { directory, inputs } => {
            let config = Config::load(directory.as_deref())?;
            ui::keybindings(&config).context("Invalid keyboard.json")?;
            for warning in config.warnings() {
                eprintln!("nox: warning: {warning}");
            }
            let has_piped_stdin = inputs.is_empty() && stdin::has_piped_input();
            let resolved = resolve_launch_inputs(inputs, has_piped_stdin, &config.files.startup);
            let mut sources = Vec::with_capacity(resolved.len());
            for input in resolved {
                match input {
                    CliInput::File(path) => sources.push(ui::InputSource::File(path)),
                    CliInput::Stdin => sources.push(ui::InputSource::Stdin(
                        StdinSpool::spawn_stdin().context("Cannot read piped standard input")?,
                    )),
                }
            }
            ui::run(config, sources)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn parses_dash_as_stdin_and_deduplicates_repeated_dashes() {
        assert_eq!(
            parse_cli_args(os(&["-"]), None).unwrap(),
            CliAction::Run {
                directory: None,
                inputs: vec![CliInput::Stdin],
            }
        );

        assert_eq!(
            parse_cli_args(os(&["-", "app.log", "-"]), None).unwrap(),
            CliAction::Run {
                directory: None,
                inputs: vec![CliInput::Stdin, CliInput::File(PathBuf::from("app.log"))],
            }
        );

        assert_eq!(
            parse_cli_args(os(&["a.log", "-", "b.log"]), None).unwrap(),
            CliAction::Run {
                directory: None,
                inputs: vec![
                    CliInput::File(PathBuf::from("a.log")),
                    CliInput::Stdin,
                    CliInput::File(PathBuf::from("b.log")),
                ],
            }
        );
    }

    #[test]
    fn treats_dash_after_double_dash_as_literal_filename() {
        assert_eq!(
            parse_cli_args(os(&["--", "-"]), None).unwrap(),
            CliAction::Run {
                directory: None,
                inputs: vec![CliInput::File(PathBuf::from("-"))],
            }
        );
    }

    #[test]
    fn resolves_piped_stdin_only_when_no_cli_inputs_are_provided() {
        let startup = vec!["startup.log".to_string()];

        assert_eq!(
            resolve_launch_inputs(vec![], true, &startup),
            vec![CliInput::Stdin]
        );
        assert_eq!(
            resolve_launch_inputs(vec![], false, &startup),
            vec![CliInput::File(PathBuf::from("startup.log"))]
        );
        assert_eq!(
            resolve_launch_inputs(
                vec![CliInput::File(PathBuf::from("cli.log"))],
                true,
                &startup
            ),
            vec![CliInput::File(PathBuf::from("cli.log"))]
        );
        assert_eq!(
            resolve_launch_inputs(
                vec![CliInput::Stdin, CliInput::File(PathBuf::from("cli.log"))],
                false,
                &startup,
            ),
            vec![CliInput::Stdin, CliInput::File(PathBuf::from("cli.log"))]
        );
    }

    #[test]
    fn rejects_dash_with_migrate_config() {
        assert!(parse_cli_args(os(&["--migrate-config", "-"]), None).is_err());
    }
}
