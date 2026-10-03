use std::process::ExitCode;

use clap::Parser;

use ferry::app;
use ferry::cli::{Cli, Command, exit, resolve_config_path};
use ferry::git::askpass;

fn main() -> ExitCode {
    // Askpass mode comes first: git starts this binary with only a prompt
    // argument, so it must never reach clap, config loading, or telemetry.
    if askpass::requested() {
        let prompt = std::env::args().nth(1);
        return askpass::run(prompt.as_deref());
    }

    let code = match Cli::parse().command {
        Command::Run { config } => app::run(&resolve_config_path(config)),
        Command::Sync { config, repos, .. } => app::sync_once(&resolve_config_path(config), &repos),
        Command::CheckConfig { config } => {
            let path = resolve_config_path(config);
            match app::load_config(&path) {
                Ok(config) => {
                    println!(
                        "config ok: {} ({} repos)",
                        path.display(),
                        config.repos.len()
                    );
                    exit::SUCCESS
                }
                Err(code) => code,
            }
        }
    };
    ExitCode::from(code)
}
