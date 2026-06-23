//! Dependency stripping / filtering policy for `aube deploy`.
//!
//! Three consumers fan out from the CLI flag set (`--prod` / `--dev` /
//! `--no-prod` / `--no-optional`):
//!
//!   * the manifest rewriter, which physically removes excluded dep
//!     fields from the deployed `package.json` before install runs
//!     ([`StripFields`]),
//!   * install's resolved-graph filter
//!     ([`dep_selection_for_args`]),
//!   * the source-lockfile subsetter's keep predicate
//!     ([`keep_dep_for_args`]).
//!
//! All three derive from a single [`DepAxis`] truth table so the three
//! paths can't silently drift onto different formulas.
use super::DeployArgs;
use crate::commands::install;

/// Which dep fields `rewrite_local_refs` should physically remove from a
/// `package.json` before install runs.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct StripFields {
    pub(super) dependencies: bool,
    pub(super) dev_dependencies: bool,
    pub(super) optional_dependencies: bool,
}

impl StripFields {
    /// Stripping policy for the top-level deployed manifest. Honors the
    /// CLI flags: `--prod` (default), `--dev`, `--no-prod`,
    /// `--no-optional`.
    pub(super) fn for_args(args: &DeployArgs) -> Self {
        let DepAxis {
            prod,
            dev,
            optional,
        } = DepAxis::for_args(args);
        Self {
            dependencies: !prod,
            dev_dependencies: !dev,
            optional_dependencies: !optional,
        }
    }

    /// Stripping policy for bundled siblings. Bundled siblings exist
    /// only to satisfy the deployed package's runtime tree, so their
    /// devDependencies are always dropped — the deploy isn't a dev
    /// environment for siblings. Optional sibling deps mirror the
    /// top-level `--no-optional` choice so a sibling's optional sub-dep
    /// doesn't sneak past the user's filter.
    pub(super) fn for_bundled_sibling(args: &DeployArgs) -> Self {
        Self {
            dependencies: false,
            dev_dependencies: true,
            optional_dependencies: args.no_optional,
        }
    }
}

/// Single source of truth for which dep types a deploy keeps, given the
/// CLI flag combination. Every consumer (manifest strip, install
/// `DepSelection`, lockfile-subset keep predicate) derives from this,
/// so the three paths can't silently drift onto different formulas.
/// Booleans are "keep this dep type", not "the flag is set".
#[derive(Debug, Clone, Copy)]
struct DepAxis {
    prod: bool,
    dev: bool,
    optional: bool,
}

impl DepAxis {
    fn for_args(args: &DeployArgs) -> Self {
        // clap enforces `--prod`, `--dev`, `--no-prod` mutually
        // exclusive on the deploy surface, so the cases collapse:
        //   default / --prod  -> prod + optional
        //   --dev             -> dev only
        //   --no-prod         -> prod + dev + optional
        // `--no-optional` is independent and only suppresses optionals.
        Self {
            prod: !args.dev,
            dev: args.dev || args.no_prod,
            optional: !args.dev && !args.no_optional,
        }
    }
}

/// Install-side dep selection. Intentionally NOT derived from `DepAxis`:
/// the manifest strip and lockfile subset operate on *direct* dep fields
/// (under `--dev` they drop the top-level `optionalDependencies` field,
/// matching pnpm), but install's `DepSelection` filters the *resolved*
/// dependency graph — folding `--dev` into `no_optional` here would also
/// strip transitive optional sub-deps of devDependencies (e.g. an
/// optional sub-dep of `jest`), breaking dev tooling at runtime. Only
/// the explicit `--no-optional` flag gates the install-side optional
/// axis; the direct `optionalDependencies` field is already gone from
/// the deployed manifest before install runs.
pub(super) fn dep_selection_for_args(args: &DeployArgs) -> install::DepSelection {
    let prod = !args.dev && !args.no_prod;
    let dev = args.dev;
    install::DepSelection::from_flags(prod, dev, args.no_optional)
}

/// `subset_to_importer` keep predicate: shares `DepAxis::for_args` with
/// `StripFields::for_args` so the source lockfile subset and the
/// rewritten target manifest agree on which dep types survive.
pub(super) fn keep_dep_for_args(
    args: &DeployArgs,
) -> impl Fn(&aube_lockfile::DirectDep) -> bool + use<> {
    let DepAxis {
        prod,
        dev,
        optional,
    } = DepAxis::for_args(args);
    move |d: &aube_lockfile::DirectDep| match d.dep_type {
        aube_lockfile::DepType::Production => prod,
        aube_lockfile::DepType::Dev => dev,
        aube_lockfile::DepType::Optional => optional,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deploy_args() -> DeployArgs {
        DeployArgs {
            target: std::path::PathBuf::from("/tmp/unused"),
            dev: false,
            no_optional: false,
            prod: false,
            no_prod: false,
            offline: false,
            prefer_offline: false,
            lockfile: crate::cli_args::LockfileArgs::default(),
            network: crate::cli_args::NetworkArgs::default(),
            virtual_store: crate::cli_args::VirtualStoreArgs::default(),
        }
    }

    #[test]
    fn dep_selection_default_is_prod() {
        let a = deploy_args();
        assert_eq!(dep_selection_for_args(&a), install::DepSelection::Prod);
    }

    #[test]
    fn dep_selection_no_prod_is_all() {
        let a = DeployArgs {
            no_prod: true,
            ..deploy_args()
        };
        assert_eq!(dep_selection_for_args(&a), install::DepSelection::All);
    }

    #[test]
    fn dep_selection_no_prod_and_no_optional_is_no_optional() {
        let a = DeployArgs {
            no_prod: true,
            no_optional: true,
            ..deploy_args()
        };
        assert_eq!(
            dep_selection_for_args(&a),
            install::DepSelection::NoOptional
        );
    }

    #[test]
    fn dep_selection_covers_every_flag_combo() {
        // Lock the (dev, no_prod, no_optional) -> DepSelection table so
        // a future tweak to dep_selection_for_args can't silently drift.
        // Note `--dev` alone maps to `Dev`, not `DevNoOptional`: the
        // direct `optionalDependencies` field is stripped by
        // `StripFields`, but install's optional axis must stay open so
        // transitive optional sub-deps of devDependencies still resolve
        // (see comment on `dep_selection_for_args`).
        let cases: &[(bool, bool, bool, install::DepSelection)] = &[
            (false, false, false, install::DepSelection::Prod),
            (false, false, true, install::DepSelection::ProdNoOptional),
            (false, true, false, install::DepSelection::All),
            (false, true, true, install::DepSelection::NoOptional),
            (true, false, false, install::DepSelection::Dev),
            (true, false, true, install::DepSelection::DevNoOptional),
        ];
        for &(dev, no_prod, no_optional, want) in cases {
            let a = DeployArgs {
                dev,
                no_prod,
                no_optional,
                ..deploy_args()
            };
            assert_eq!(
                dep_selection_for_args(&a),
                want,
                "dev={dev} no_prod={no_prod} no_optional={no_optional}"
            );
        }
    }

    #[test]
    fn strip_default_drops_dev_keeps_prod_and_optional() {
        let s = StripFields::for_args(&deploy_args());
        assert!(!s.dependencies);
        assert!(s.dev_dependencies);
        assert!(!s.optional_dependencies);
    }

    #[test]
    fn strip_no_prod_keeps_everything() {
        let a = DeployArgs {
            no_prod: true,
            ..deploy_args()
        };
        let s = StripFields::for_args(&a);
        assert!(!s.dependencies);
        assert!(!s.dev_dependencies);
        assert!(!s.optional_dependencies);
    }

    #[test]
    fn strip_dev_only_drops_prod_and_optional() {
        let a = DeployArgs {
            dev: true,
            ..deploy_args()
        };
        let s = StripFields::for_args(&a);
        assert!(s.dependencies);
        assert!(!s.dev_dependencies);
        assert!(s.optional_dependencies);
    }

    #[test]
    fn strip_for_bundled_sibling_always_drops_dev() {
        // Bundled siblings exist only to satisfy the runtime tree. The
        // top-level flag set must not flip dev back on for siblings.
        let s = StripFields::for_bundled_sibling(&DeployArgs {
            no_prod: true,
            ..deploy_args()
        });
        assert!(s.dev_dependencies);
        assert!(!s.dependencies);
        assert!(!s.optional_dependencies);
    }
}
