fn main() {
    let hash = std::env::var("TGSRSSR_GIT_HASH")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| {
            std::process::Command::new("git")
                .args(["rev-parse", "--short", "HEAD"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_default()
        });
    // Validate the git hash: it is spliced into cargo:rustc-env and must not inject directives.
    let hash = if (7..=40).contains(&hash.len()) && hash.chars().all(|c| c.is_ascii_hexdigit()) {
        hash
    } else {
        String::new()
    };

    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let full = if hash.is_empty() {
        version
    } else {
        format!("{version}+{hash}")
    };
    println!("cargo:rustc-env=APP_VERSION={full}");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/logs/HEAD");
    println!("cargo:rerun-if-env-changed=TGSRSSR_GIT_HASH");
}
