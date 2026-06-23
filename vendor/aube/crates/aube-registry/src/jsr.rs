//! JSR (https://jsr.io) serves its packages through an npm-compatible
//! endpoint at <https://npm.jsr.io>. Packages are namespaced under the
//! `@jsr` scope with the JSR scope folded into the package name via a
//! `__` separator — so `jsr:@std/collections` is fetched as
//! `@jsr/std__collections` from the npm-compat registry.
//!
//! pnpm hides that translation from users: they write `jsr:@std/collections`
//! in package.json and pnpm rewrites internally. This module provides the
//! pure string helpers the CLI, resolver, and registry client share so the
//! translation lives in one place.

/// Default registry URL for JSR's npm-compat endpoint.
pub const JSR_DEFAULT_REGISTRY: &str = "https://npm.jsr.io/";

/// Scope that JSR packages live under on the npm-compat registry.
pub const JSR_NPM_SCOPE: &str = "@jsr";

/// Translate a JSR-style name (`@scope/name`) into the name the npm-compat
/// registry serves it under (`@jsr/scope__name`). Returns `None` if the
/// input isn't a scoped name. Pure string munging — we don't talk to JSR
/// to confirm the package exists.
pub fn jsr_to_npm_name(jsr_name: &str) -> Option<String> {
    let rest = jsr_name.strip_prefix('@')?;
    let (scope, name) = rest.split_once('/')?;
    if scope.is_empty() || name.is_empty() {
        return None;
    }
    Some(format!("@jsr/{scope}__{name}"))
}

/// Inverse of [`jsr_to_npm_name`]. `@jsr/std__collections` → `@std/collections`.
/// Returns `None` if the input isn't a `@jsr/<scope>__<name>` name.
pub fn npm_to_jsr_name(npm_name: &str) -> Option<String> {
    let rest = npm_name.strip_prefix("@jsr/")?;
    let (scope, name) = rest.split_once("__")?;
    if scope.is_empty() || name.is_empty() {
        return None;
    }
    Some(format!("@{scope}/{name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_basic_scoped_name() {
        assert_eq!(
            jsr_to_npm_name("@std/collections").as_deref(),
            Some("@jsr/std__collections"),
        );
    }

    #[test]
    fn translates_hyphenated_scope_and_name() {
        assert_eq!(
            jsr_to_npm_name("@foo-bar/baz-qux").as_deref(),
            Some("@jsr/foo-bar__baz-qux"),
        );
    }

    #[test]
    fn rejects_unscoped_and_malformed_inputs() {
        assert_eq!(jsr_to_npm_name("lodash"), None);
        assert_eq!(jsr_to_npm_name("@std"), None);
        assert_eq!(jsr_to_npm_name("@/collections"), None);
        assert_eq!(jsr_to_npm_name("@std/"), None);
    }

    #[test]
    fn round_trips_via_npm_to_jsr_name() {
        let jsr = "@std/collections";
        let npm = jsr_to_npm_name(jsr).unwrap();
        assert_eq!(npm_to_jsr_name(&npm).as_deref(), Some(jsr));
    }

    #[test]
    fn rejects_non_jsr_npm_names() {
        assert_eq!(npm_to_jsr_name("@scope/pkg"), None);
        assert_eq!(npm_to_jsr_name("lodash"), None);
        assert_eq!(npm_to_jsr_name("@jsr/no-separator"), None);
    }
}
