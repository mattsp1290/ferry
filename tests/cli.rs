//! The binary's top-level surface.

use std::process::Command;

fn ferry() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ferry"));
    command
        .env_remove("FERRY_ASKPASS")
        .env_remove("FERRY_CONFIG");
    command
}

#[test]
fn version_prints_crate_version_and_git_sha() {
    let output = ferry().arg("--version").output().expect("runs");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let expected_prefix = format!("ferry {} (", env!("CARGO_PKG_VERSION"));
    assert!(stdout.starts_with(&expected_prefix), "{stdout}");
    assert!(stdout.trim_end().ends_with(')'), "{stdout}");
    assert_eq!(stdout.trim_end(), format!("ferry {}", ferry::VERSION));
}

#[test]
fn usage_errors_exit_2() {
    let output = ferry().output().expect("runs");
    assert_eq!(output.status.code(), Some(2), "{output:?}");

    let output = ferry().arg("no-such-command").output().expect("runs");
    assert_eq!(output.status.code(), Some(2), "{output:?}");

    let output = ferry().arg("sync").output().expect("runs");
    assert_eq!(output.status.code(), Some(2), "{output:?}");
}
