use std::process::Command;

fn main() {
    // Embed git commit hash (short) at compile time.
    let hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default();
    println!("cargo:rustc-env=GIT_HASH={}", hash.trim());

    // Embed build date (YYYYMMDD).
    let date = Command::new("date")
        .arg("+%Y%m%d")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default();
    println!("cargo:rustc-env=BUILD_DATE={}", date.trim());

    // Only re-run if git HEAD changes or build.rs itself changes.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=build.rs");
}
