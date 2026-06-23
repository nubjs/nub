//! Concurrency configuration helpers shared across aube crates.
//!
//! Today this module exposes one thing: a first-class env override that
//! lets users pin the tarball-fetch fan-out when the default 128
//! in-flight requests trigger 429/503 throttling on slow private
//! registries (Artifactory, Nexus). Read under the active embedder's
//! config-env brand via [`config_env`](crate::env::config_env) — so it's
//! `AUBE_CONCURRENCY` for standalone aube and `<BRAND>_CONCURRENCY` for an
//! embedder with its own `config_env_prefix`, and the branded `AUBE_*`
//! form is never read under such a host. The override is a knob, not a
//! probe — when AIMD ramping lands it will live alongside the semaphore in
//! `aube-registry::concurrency` (the layer that owns retry signals).
//!
//! Range-clamped to `[CONCURRENCY_FLOOR, CONCURRENCY_CEILING]` so a
//! hostile or typo'd value can't exhaust file descriptors on Windows
//! (default ulimit 8192).

/// Lower bound on the concurrency override. A degenerate slow link still
/// makes progress with 8 in-flight fetches.
pub const CONCURRENCY_FLOOR: u32 = 8;

/// Upper bound on the concurrency override. Picked so a pathological env
/// value cannot exhaust the Windows default fd ulimit.
pub const CONCURRENCY_CEILING: u32 = 256;

/// Read the `{config_env_prefix}_CONCURRENCY` override (`AUBE_CONCURRENCY`
/// under standalone aube) as a clamped integer.
/// Returns `None` when the variable is unset, missing, or outside
/// the range — callers fall back to the default (`network-concurrency`
/// npmrc / setting / hard-coded). Out-of-range and non-numeric values warn.
pub fn parse_concurrency_env() -> Option<u32> {
    let raw = crate::env::config_env("CONCURRENCY")?;
    if let Some(s) = raw.to_str()
        && let Ok(n) = s.parse::<u32>()
        && (CONCURRENCY_FLOOR..=CONCURRENCY_CEILING).contains(&n)
    {
        return Some(n);
    }
    tracing::warn!(
        code = aube_codes::warnings::WARN_AUBE_CONCURRENCY_ENV_INVALID,
        value = ?raw,
        floor = CONCURRENCY_FLOOR,
        ceiling = CONCURRENCY_CEILING,
        "concurrency override ignored: must be an integer in [floor, ceiling]; using default"
    );
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // `RUST_TEST_THREADS=1` in `.cargo/config.toml` runs every test
    // serially across the workspace, so these env-mutating tests
    // don't collide with concurrent setenv/getenv from other tests.
    // Preserve and restore the previous value so a test that leaves
    // `AUBE_CONCURRENCY` set in the environment doesn't bleed into
    // the next test in the suite.
    fn with_env<F: FnOnce()>(value: Option<&str>, f: F) {
        let prev = std::env::var_os("AUBE_CONCURRENCY");
        // SAFETY: tests run serially via RUST_TEST_THREADS=1; no
        // other thread touches this var concurrently.
        unsafe {
            match value {
                Some(v) => std::env::set_var("AUBE_CONCURRENCY", v),
                None => std::env::remove_var("AUBE_CONCURRENCY"),
            }
        }
        f();
        unsafe {
            match prev {
                Some(v) => std::env::set_var("AUBE_CONCURRENCY", v),
                None => std::env::remove_var("AUBE_CONCURRENCY"),
            }
        }
    }

    #[test]
    fn unset_returns_none() {
        with_env(None, || assert_eq!(parse_concurrency_env(), None));
    }

    #[test]
    fn in_range_returns_value() {
        with_env(Some("64"), || {
            assert_eq!(parse_concurrency_env(), Some(64));
        });
    }

    #[test]
    fn below_floor_warns_and_returns_none() {
        with_env(Some("1"), || assert_eq!(parse_concurrency_env(), None));
    }

    #[test]
    fn above_ceiling_warns_and_returns_none() {
        with_env(Some("99999"), || {
            assert_eq!(parse_concurrency_env(), None);
        });
    }

    #[test]
    fn non_numeric_warns_and_returns_none() {
        with_env(Some("garbage"), || {
            assert_eq!(parse_concurrency_env(), None);
        });
    }

    #[test]
    fn empty_warns_and_returns_none() {
        with_env(Some(""), || assert_eq!(parse_concurrency_env(), None));
    }

    #[test]
    fn floor_and_ceiling_inclusive() {
        with_env(Some("8"), || {
            assert_eq!(parse_concurrency_env(), Some(CONCURRENCY_FLOOR));
        });
        with_env(Some("256"), || {
            assert_eq!(parse_concurrency_env(), Some(CONCURRENCY_CEILING));
        });
    }
}
