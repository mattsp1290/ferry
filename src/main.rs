use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use ferry::cli::{Cli, Command, exit, resolve_config_path};
use ferry::config::{Config, ConfigError};
use ferry::git::askpass;

fn main() -> ExitCode {
    // Askpass mode comes first: git starts this binary with only a prompt
    // argument, so it must never reach clap, config loading, or telemetry.
    if askpass::requested() {
        let prompt = std::env::args().nth(1);
        return askpass::run(prompt.as_deref());
    }

    let cli = Cli::parse();
    match cli.command {
        Command::CheckConfig { config } => check_config(config),
        Command::Run { .. } | Command::Sync { .. } => {
            eprintln!("ferry: this subcommand is not implemented yet");
            ExitCode::from(exit::RUNTIME)
        }
    }
}

fn check_config(flag: Option<PathBuf>) -> ExitCode {
    let path = resolve_config_path(flag);
    match Config::load(&path).and_then(|config| config.validate().map(|()| config)) {
        Ok(config) => {
            println!(
                "config ok: {} ({} repos)",
                path.display(),
                config.repos.len()
            );
            ExitCode::from(exit::SUCCESS)
        }
        Err(error) => {
            report_config_error(&error);
            ExitCode::from(exit::CONFIG)
        }
    }
}

/// Prints one line per violation so that `check-config` shows all of them.
fn report_config_error(error: &ConfigError) {
    match error {
        ConfigError::Invalid(violations) => {
            for violation in violations {
                eprintln!("ferry: invalid config: {violation}");
            }
        }
        other => eprintln!("ferry: {other}"),
    }
}
