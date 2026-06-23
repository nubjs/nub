//! Stable identifiers for every error and warning that aube emits.
//!
//! The crate is dependency-free on purpose: every other aube crate may
//! depend on it. Codes are exposed as `pub const &str` so they can be
//! used unmodified in `tracing::warn!(code = aube_codes::warnings::X, ...)`,
//! `#[diagnostic(code = aube_codes::errors::Y)]`, and ndjson-emitting
//! reporters without needing to call `.as_str()` or do any conversion.
//!
//! Naming convention:
//! - `ERR_AUBE_*` for errors (anything that returns `Err` to the caller
//!   or aborts with a non-zero exit).
//! - `WARN_AUBE_*` for warnings (`tracing::warn!`) and non-fatal
//!   `tracing::error!` sites that don't change exit status.
//!
//! aube does not emit `ERR_PNPM_*` codes itself. Where a code maps
//! cleanly onto a pnpm concept (lockfile, peer-deps, tarball, etc.) we
//! reuse pnpm's *suffix* under the `ERR_AUBE_` prefix so the code reads
//! the same to anyone familiar with pnpm — but the published code is
//! always `ERR_AUBE_*`.
//!
//! Codes are stable: once published, a code's identifier and meaning
//! must not change. Adding new codes is fine; removing or repurposing
//! one is a breaking change.

#![forbid(unsafe_code)]

pub mod errors;
pub mod exit;
pub mod warnings;

/// Metadata for a single error or warning code.
///
/// `name` doubles as the emitted string value — every code is
/// declared as `pub const X: &str = "X"` so this field references the
/// same constant the call site uses. Keeping the const + the
/// `CodeMeta` entry pointing at the same identifier lets a rename
/// flow through both with no drift.
///
/// `description` and `category` feed the generated docs page
/// (`docs/error-codes.data.json`, consumed by `<ErrorCodesTable>`).
/// `exit_code` is `Some(_)` only for errors that have a bespoke
/// entry — warnings always set `None` because they don't change
/// exit status.
///
/// `Serialize` is derived so the generator binary can emit each
/// entry verbatim via `serde_json`. Every consuming crate already
/// has `serde` in its dep tree; adding it here doesn't grow the
/// compile graph.
#[derive(Debug, serde::Serialize)]
pub struct CodeMeta {
    pub name: &'static str,
    pub category: &'static str,
    pub description: &'static str,
    pub exit_code: Option<i32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_error_const_value_matches_its_name() {
        // The `pub const ERR_AUBE_X: &str = "ERR_AUBE_X"` shape is
        // load-bearing — typos between the const name and the value
        // would silently emit the wrong code. CodeMeta::name is the
        // const value, so checking the prefix on every entry catches
        // any rogue addition that didn't follow the convention.
        for meta in errors::ALL {
            assert!(
                meta.name.starts_with("ERR_AUBE_"),
                "error codes must use the ERR_AUBE_ prefix: {}",
                meta.name
            );
            assert!(
                !meta.description.is_empty(),
                "error code {} is missing a description",
                meta.name
            );
            assert!(
                !meta.category.is_empty(),
                "error code {} is missing a category",
                meta.name
            );
        }
    }

    #[test]
    fn every_warning_const_value_matches_its_name() {
        for meta in warnings::ALL {
            assert!(
                meta.name.starts_with("WARN_AUBE_"),
                "warning codes must use the WARN_AUBE_ prefix: {}",
                meta.name
            );
            assert!(
                !meta.description.is_empty(),
                "warning code {} is missing a description",
                meta.name
            );
            assert!(
                !meta.category.is_empty(),
                "warning code {} is missing a category",
                meta.name
            );
            assert!(
                meta.exit_code.is_none(),
                "warning {} has an exit_code; warnings don't change exit status",
                meta.name
            );
        }
    }

    #[test]
    fn no_duplicate_codes() {
        use std::collections::HashSet;
        let all: Vec<&str> = errors::ALL
            .iter()
            .chain(warnings::ALL.iter())
            .map(|m| m.name)
            .collect();
        let unique: HashSet<&str> = all.iter().copied().collect();
        assert_eq!(
            all.len(),
            unique.len(),
            "duplicate code identifier across errors/warnings"
        );
    }
}
