fn main() {
    // Windows defaults the main-thread stack to 1 MB. The `async_main`
    // dispatcher's future state machine — which holds locals across every
    // await in every CLI command arm — exceeds that on debug builds and
    // crashes startup with `thread 'main' has overflowed its stack`.
    // Bump the reserve to 8 MB (matching Linux/macOS) via the MSVC
    // linker's `/STACK:` flag. No-op on every other target.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        println!("cargo:rustc-link-arg-bins=/STACK:8388608");
    }
    println!("cargo:rustc-env=AUBE_BUILD_DATE={}", build_date());
    println!("cargo:rerun-if-changed=build.rs");
}

/// Capture the build host's UTC date as `YYYY-MM-DD` for the `aube
/// --version` line. Shell-out keeps it dep-free; falls back to
/// `unknown` if the host's `date` / `Get-Date` isn't reachable.
///
/// `Get-Date -UFormat` only controls the *format-specifier style*, not
/// the timezone — so the Windows path explicitly converts to UTC via
/// `.ToUniversalTime()` so build dates stay consistent with the
/// Unix `date -u` path on either side of midnight.
fn build_date() -> String {
    let (cmd, args): (&str, &[&str]) = if cfg!(windows) {
        (
            "powershell",
            &[
                "-NoProfile",
                "-Command",
                "(Get-Date).ToUniversalTime().ToString('yyyy-MM-dd')",
            ],
        )
    } else {
        ("date", &["-u", "+%Y-%m-%d"])
    };
    std::process::Command::new(cmd)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}
