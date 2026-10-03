//! Askpass mode: git runs the ferry binary itself to obtain credentials.

use std::process::ExitCode;

/// Env variable that switches the binary into askpass mode when set to `1`.
pub const ASKPASS_ENV: &str = "FERRY_ASKPASS";

/// Whether the process was started by git as its askpass helper.
pub fn requested() -> bool {
    std::env::var(ASKPASS_ENV).is_ok_and(|value| value == "1")
}

/// Runs askpass mode. `prompt` is git's prompt, taken from `argv[1]`.
pub fn run(_prompt: Option<&str>) -> ExitCode {
    ExitCode::from(1)
}
