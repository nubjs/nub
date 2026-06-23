use std::sync::LazyLock;

/// Short user-facing version (e.g. `1.1.0` or `1.1.0-DEBUG`). Used by
/// the install progress header where the surrounding line is already
/// busy. Appends `-DEBUG` on non-release builds so a stray `cargo run`
/// binary on `$PATH` is obvious.
pub static VERSION: LazyLock<String> = LazyLock::new(|| {
    let mut v = aube_util::embedder().version.to_string();
    if cfg!(debug_assertions) {
        v.push_str("-DEBUG");
    }
    v
});

/// Long version line for `aube --version`: `<ver> <os>-<arch> (<date>)`.
/// Mirrors mise's format so users can disambiguate which binary they're
/// running across machines.
pub static VERSION_LONG: LazyLock<String> = LazyLock::new(|| {
    format!(
        "{} {}-{} ({})",
        *VERSION,
        std::env::consts::OS,
        arch_short(),
        env!("AUBE_BUILD_DATE"),
    )
});

fn arch_short() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    }
}
