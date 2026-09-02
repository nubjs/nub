//! Engine settings a `nub.jsonc` field ALSO supplies, and the refusal that
//! keeps a write out of the file that loses.
//!
//! Twelve engine settings can be supplied by a `nub.jsonc` field. The lowering
//! in [`super::lower_native_install_settings`] injects them at the engine's
//! `project_config` tier, which outranks every file source — `project_npmrc`
//! included — so once the owning field is set, an `.npmrc` line for the same
//! setting is read by nothing. `nub config set` reports success, the file gains
//! a line, and no resolve ever consults it: the same silent no-op
//! [`super::store_config_family::npmrc_first`] refuses elsewhere, reached from
//! the other side. There the key was never readable; here it is readable in
//! general and answered by a different file in THIS project.
//!
//! So the refusal is conditional on what the project actually sets, not on the
//! key alone. A project with no `install` block reads `.npmrc` `nodeLinker`
//! exactly as before and must keep working — refusing the key outright would
//! break a configuration that is correct today.
//!
//! **Why this refuses rather than rewriting the value into `nub.jsonc`.** A
//! redirect was built and rejected. The two surfaces do not share a value
//! grammar: `.npmrc` `minimumReleaseAge` is a bare integer in MINUTES while the
//! field is a duration string that deliberately rejects a unit-less number (the
//! days-vs-minutes trap in [`crate::project_config::parse_duration`]), `.npmrc`
//! lists are comma-separated where the fields are JSON arrays, and pnpm's
//! `install`/`prompt` verify-deps modes have no field spelling at all. Moving
//! the write silently restates the user's value in another grammar, and it
//! desynchronizes `get` from `set` unless `get` is rewritten to match — which
//! would reintroduce, on the read side, exactly the divergence this module
//! exists to remove. Naming the field the user should set has neither problem.

use anyhow::anyhow;

/// One engine setting a `nub.jsonc` field supplies.
struct DuplicateHome {
    /// The canonical engine setting name.
    setting: &'static str,
    /// The `nub.jsonc` address that outranks it, and the address the refusal
    /// tells the user to set instead.
    field: &'static str,
}

/// Every engine setting reachable from a `nub.jsonc` field, with the field that
/// owns it. Derived from [`super::lower_native_install_settings`] and
/// [`crate::verify_deps::resolve_policy`] — when either learns to supply
/// another setting, it belongs here too, and
/// `every_entry_names_a_real_setting_and_a_real_field` fails if a name here
/// stops being real.
const HOMES: &[DuplicateHome] = &[
    // `install.linker` lowers to a whole layout group: the strategy, where the
    // virtual store lives, and the hidden hoist tree that injected dependencies
    // need.
    DuplicateHome {
        setting: "nodeLinker",
        field: "install.linker",
    },
    DuplicateHome {
        setting: "enableGlobalVirtualStore",
        field: "install.linker",
    },
    DuplicateHome {
        setting: "hoist",
        field: "install.linker",
    },
    DuplicateHome {
        setting: "hoistPattern",
        field: "install.linker",
    },
    DuplicateHome {
        setting: "disableGlobalVirtualStoreForPackages",
        field: "install.linker",
    },
    DuplicateHome {
        setting: "diskMaterializePackages",
        field: "install.linker",
    },
    // Naming public-hoist patterns is always a narrowing, so the lowering also
    // forces the blanket flag off — both belong to the one field.
    DuplicateHome {
        setting: "publicHoistPattern",
        field: "install.publicHoist",
    },
    DuplicateHome {
        setting: "shamefullyHoist",
        field: "install.publicHoist",
    },
    // Release-age resolution. Admitted only under nub's own identity, which is
    // why the caller gates these on `native_mode` before consulting the table.
    DuplicateHome {
        setting: "minimumReleaseAge",
        field: "install.minimumReleaseAge",
    },
    DuplicateHome {
        setting: "minimumReleaseAgeStrict",
        field: "install.minimumReleaseAge",
    },
    DuplicateHome {
        setting: "minimumReleaseAgeExclude",
        field: "install.minimumReleaseAgeExclude",
    },
    // Read by `crate::verify_deps` rather than through the settings tier, but
    // the ordering is the same: an explicitly-set field wins over `.npmrc`.
    DuplicateHome {
        setting: "verifyDepsBeforeRun",
        field: "verifyDeps",
    },
];

/// The `nub.jsonc` field that would SHADOW an `.npmrc` write of `key`, or
/// `None` when nothing shadows it and the write is genuinely read back.
///
/// `supplied` is what the project's own `nub.jsonc` currently lowers to, so
/// this follows the real injection rather than predicting it from the field
/// names a second time. A key naming no setting, or a setting no field
/// supplies, routes exactly as it did before.
pub(super) fn shadowing_field(key: &str, supplied: &[String]) -> Option<&'static str> {
    // Matched by canonical name FIRST, then by every alias spelling, so
    // `hoist-pattern` resolves as well as `hoistPattern`. `find_unfiltered`
    // alone matches the canonical name only, which silently made every aliased
    // key unrecognized — and an unrecognized key is never refused, so the guard
    // failed open rather than loudly.
    let meta = aube_settings::meta::find_unfiltered(key).or_else(|| {
        aube_settings::meta::all_unfiltered().iter().find(|meta| {
            meta.npmrc_keys.contains(&key)
                || meta.workspace_yaml_keys.contains(&key)
                || meta.cli_flags.contains(&key)
        })
    })?;
    let home = HOMES.iter().find(|home| home.setting == meta.name)?;
    supplied
        .iter()
        .any(|name| name == home.setting)
        .then_some(home.field)
}

/// The refusal for an `.npmrc` write this project's `nub.jsonc` already answers.
pub(super) fn shadowed_error(key: &str, field: &str) -> anyhow::Error {
    anyhow!(
        "nub config set {key}: this project sets `{field}` in nub.jsonc, which outranks .npmrc, \
         so the value written here would never be read\n\
         \x20\x20set `{field}` instead, or remove it from nub.jsonc to configure this from .npmrc"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every setting named here is a real engine setting and every field a real
    /// `nub.jsonc` address. A typo in either makes the entry inert — the
    /// refusal never fires, and its advice names a command that does not work.
    #[test]
    fn every_entry_names_a_real_setting_and_a_real_field() {
        for home in HOMES {
            assert!(
                aube_settings::meta::find_unfiltered(home.setting).is_some(),
                "`{}` is not an engine setting",
                home.setting
            );
            assert!(
                crate::config_fields::field(home.field).is_some(),
                "`{}` is not a nub.jsonc field",
                home.field
            );
        }
    }

    /// The refusal is decided by what the project SUPPLIES, not by the key.
    ///
    /// Both directions matter and they pull opposite ways. A project that sets
    /// the field must not be able to write a dead `.npmrc` line; a project that
    /// does not set it reads `.npmrc` normally, and refusing there would break
    /// a configuration that works today.
    #[test]
    fn shadowing_follows_what_the_project_supplies() {
        let supplied = vec!["nodeLinker".to_string(), "hoistPattern".to_string()];
        // An alias spelling resolves to the same setting as the canonical one.
        assert_eq!(
            shadowing_field("hoist-pattern", &supplied),
            Some("install.linker")
        );
        assert_eq!(
            shadowing_field("nodeLinker", &supplied),
            Some("install.linker")
        );
        // Supplied by no field in this project, so the `.npmrc` write is read.
        assert_eq!(shadowing_field("minimumReleaseAge", &supplied), None);
        // Never a duplicate home at all.
        assert_eq!(shadowing_field("autoInstallPeers", &supplied), None);
        // An unknown key is free-form config and is not ours to refuse.
        assert_eq!(shadowing_field("some-custom-key", &supplied), None);
        // The ordinary project supplies nothing and nothing is refused.
        assert_eq!(shadowing_field("nodeLinker", &[]), None);
    }
}
