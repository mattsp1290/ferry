//! Command-line surface.
//!
//! Askpass mode is deliberately not a subcommand: `main` enters it before
//! argument parsing when `FERRY_ASKPASS=1` is set (see `git::askpass`).

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

/// Env variable that names the config file when `--config` is absent.
pub const CONFIG_ENV: &str = "FERRY_CONFIG";
/// Config path used when neither `--config` nor `FERRY_CONFIG` is set.
pub const DEFAULT_CONFIG_PATH: &str = "/etc/ferry/ferry.toml";

/// Process exit codes. Clap reports usage errors with `2` on its own.
pub mod exit {
    pub const SUCCESS: u8 = 0;
    pub const RUNTIME: u8 = 1;
    pub const CONFIG: u8 = 2;
}

#[derive(Debug, Parser)]
#[command(name = "ferry", version = crate::VERSION, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum RefSide {
    Github,
    Forgejo,
}

impl From<RefSide> for crate::git::Side {
    fn from(side: RefSide) -> Self {
        match side {
            RefSide::Github => Self::Github,
            RefSide::Forgejo => Self::Forgejo,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// List the heads and tags of an allowlisted repository without changing it.
    #[command(hide = true)]
    Refs {
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long, value_enum)]
        side: RefSide,
        /// GitHub owner/name of the allowlist entry.
        repo: String,
    },
    /// Run the polling scheduler and the health server until terminated.
    Run {
        /// Config file. Defaults to $FERRY_CONFIG, then /etc/ferry/ferry.toml.
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Sync every entry, or the named entries, once and exit.
    Sync {
        /// Required: `sync` only supports a single pass.
        #[arg(long, required = true)]
        once: bool,
        /// Config file. Defaults to $FERRY_CONFIG, then /etc/ferry/ferry.toml.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Limit the pass to this GitHub `owner/name`. Repeatable.
        #[arg(long = "repo", value_name = "GITHUB_OWNER/NAME")]
        repos: Vec<String>,
    },
    /// Load and validate the config without any network I/O.
    CheckConfig {
        /// Config file. Defaults to $FERRY_CONFIG, then /etc/ferry/ferry.toml.
        #[arg(long)]
        config: Option<PathBuf>,
    },
}

/// Resolves the config path: the flag, then `FERRY_CONFIG`, then the default.
pub fn resolve_config_path(flag: Option<PathBuf>) -> PathBuf {
    resolve_config_path_from(flag, std::env::var_os(CONFIG_ENV).map(PathBuf::from))
}

fn resolve_config_path_from(flag: Option<PathBuf>, env: Option<PathBuf>) -> PathBuf {
    flag.or(env.filter(|path| !path.as_os_str().is_empty()))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_path_precedence_is_flag_env_default() {
        let flag = Some(PathBuf::from("/flag.toml"));
        let env = Some(PathBuf::from("/env.toml"));
        assert_eq!(
            resolve_config_path_from(flag, env.clone()),
            PathBuf::from("/flag.toml")
        );
        assert_eq!(
            resolve_config_path_from(None, env),
            PathBuf::from("/env.toml")
        );
        assert_eq!(
            resolve_config_path_from(None, Some(PathBuf::new())),
            PathBuf::from(DEFAULT_CONFIG_PATH)
        );
        assert_eq!(
            resolve_config_path_from(None, None),
            PathBuf::from(DEFAULT_CONFIG_PATH)
        );
    }

    #[test]
    fn sync_requires_once() {
        assert!(Cli::try_parse_from(["ferry", "sync"]).is_err());
        let cli =
            Cli::try_parse_from(["ferry", "sync", "--once", "--repo", "a/b", "--repo", "c/d"])
                .expect("parses");
        match cli.command {
            Command::Sync { once, repos, .. } => {
                assert!(once);
                assert_eq!(repos, ["a/b", "c/d"]);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }
}
