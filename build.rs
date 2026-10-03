//! Passes the build's git SHA to the crate. The SHA comes from the
//! environment so that the build never has to run `git`.

fn main() {
    println!("cargo:rerun-if-env-changed=FERRY_GIT_SHA");
    let sha = std::env::var("FERRY_GIT_SHA")
        .ok()
        .filter(|sha| !sha.trim().is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=FERRY_GIT_SHA={}", sha.trim());
}
