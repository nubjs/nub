//! Static metadata for every setting in `settings.toml`.
//!
//! The `SETTINGS` slice is emitted at build time by `build.rs` from
//! the crate-local `settings.toml`. Each entry is a `'static`
//! borrow so there's zero runtime allocation and the compiler
//! verifies the schema shape at compile time.
//!
//! Use cases:
//! - Future `aube settings` subcommand to render a CLI version of
//!   pnpm's settings reference page
//! - The docs generator that reads this and emits
//!   `docs/settings/*.md`

/// Substitute the namespace tokens a `default` string may carry with the
/// ACTIVE embedder's directory names.
///
/// `SettingMeta` is a build-time constant, so a default that names one of the
/// tool's own directories cannot spell it literally: the literal would be
/// baked as aube's name and then printed verbatim by whichever host embeds the
/// engine — `nub config list --all` rendering the cache default as
/// `$XDG_CACHE_HOME/aube`, a directory nub never uses. Resolving at display
/// time keeps one table correct for every embedder.
///
/// Identity for standalone aube, whose namespaces ARE `aube`, and for the
/// defaults that carry no token — which is all of them but the directory pair.
pub fn render_namespaces(raw: &'static str) -> std::borrow::Cow<'static, str> {
    if !raw.contains("{cache_namespace}") && !raw.contains("{data_namespace}") {
        return std::borrow::Cow::Borrowed(raw);
    }
    let id = aube_util::embedder();
    std::borrow::Cow::Owned(
        raw.replace("{cache_namespace}", id.cache_namespace)
            .replace("{data_namespace}", id.data_namespace),
    )
}

/// Metadata for one setting. Every field is `'static` because the
/// containing `SETTINGS` slice is a `const` built from string
/// literals at compile time.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)] // Fields are consumed by tests and future generators.
pub struct SettingMeta {
    /// pnpm-style canonical name (e.g. `preferFrozenLockfile`, `registries`)
    pub name: &'static str,
    /// One-line summary
    pub description: &'static str,
    /// TOML-ish type spec (e.g. `"bool"`, `"list<string>"`, `"\"highest\" | \"time-based\""`)
    pub type_: &'static str,
    /// Default value as a string (e.g. `"true"`, `"undefined"`).
    ///
    /// May carry a namespace TOKEN (`{cache_namespace}`, `{data_namespace}`)
    /// where the default names one of the tool's own directories. Display it
    /// through [`SettingMeta::rendered_default`], never raw — see that method
    /// for why the literal cannot live here.
    pub default: &'static str,
    /// Longer markdown docs body
    pub docs: &'static str,
    /// CLI flags that set this setting
    pub cli_flags: &'static [&'static str],
    /// Environment variables that set this setting
    pub env_vars: &'static [&'static str],
    /// `.npmrc` keys that set this setting (aliases listed together;
    /// the first entry is treated as the canonical spelling)
    pub npmrc_keys: &'static [&'static str],
    /// `pnpm-workspace.yaml` (and `aube-workspace.yaml`) keys that
    /// set this setting.
    pub workspace_yaml_keys: &'static [&'static str],
    /// Example invocations
    pub examples: &'static [&'static str],
    /// Escape hatch for the workspace-level accessor audit in
    /// `tests/accessor_audit.rs`. `true` means "this setting is
    /// honored but not via its generated `resolved::<name>` typed
    /// accessor" — e.g. read through `std::env::var` behind a
    /// hand-rolled `LazyLock`, looked up in `NpmConfig` by string
    /// key, or accepted for pnpm parity with no behavior wired.
    /// Default `false`; the audit fails CI when a setting with a
    /// supported scalar type has no `resolved::<name>` call site
    /// anywhere in the workspace and this flag is not set.
    pub typed_accessor_unused: bool,
    /// Marks a setting as part of the npm-shared `.npmrc` surface:
    /// npm (and yarn / pnpm) read it from `.npmrc` too, so
    /// `aube config set` writes it there to keep the multi-tool
    /// contract. Aube-specific / pnpm-only settings leave this
    /// `false` and route to aube's own config (`config.toml`).
    pub npm_shared: bool,
    /// Marks a setting that shapes how `node_modules` is physically
    /// ARRANGED — the linker strategy, hoisting, the modules and
    /// virtual-store directories — as opposed to which version resolves
    /// or how a module is found. The `read_layout_from_workspace_yaml`
    /// engine-context posture uses this to drop the workspace-YAML
    /// source for exactly these settings.
    pub layout: bool,
    /// Managed hardening policy for this setting. Empty means the setting
    /// cannot be enforced by managed config in v1.
    pub managed_policy: &'static str,
}

impl SettingMeta {
    /// This setting's default, with any namespace token resolved against the
    /// active embedder. Every surface that SHOWS a default to a user goes
    /// through here; reading [`SettingMeta::default`] raw prints the token.
    pub fn rendered_default(&self) -> std::borrow::Cow<'static, str> {
        render_namespaces(self.default)
    }
}


// Pulls in `pub const SETTINGS: &[SettingMeta] = &[...]` generated by build.rs.
include!(concat!(env!("OUT_DIR"), "/settings_meta_data.rs"));

/// Return the full slice of settings, alphabetically sorted by name.
pub fn all() -> &'static [SettingMeta] {
    SETTINGS
}

/// Whether an `.npmrc` key spells a `layout`-flagged setting, under any of
/// its aliases. Lets a caller filtering `.npmrc` entries by key — where no
/// [`SettingMeta`] is in hand — ask the layout question.
pub fn is_layout_npmrc_key(key: &str) -> bool {
    SETTINGS
        .iter()
        .any(|meta| meta.layout && meta.npmrc_keys.contains(&key))
}

/// Look up a setting by its canonical pnpm name. `SETTINGS` is
/// generated sorted by name (build.rs guarantees this via
/// `BTreeMap` iteration), so a binary search drops the lookup from
/// O(N) to O(log N). With ~120 entries and dozens of lookups per
/// command, the saving compounds across every `aube run` startup.
///
/// The sort invariant is asserted in debug builds. A future hand
/// edit of `SETTINGS` (or a regression in the build.rs sort) would
/// silently flip valid lookups to `None`; the assert catches that
/// in test runs without slowing release builds.
pub fn find(name: &str) -> Option<&'static SettingMeta> {
    debug_assert!(
        SETTINGS.windows(2).all(|w| w[0].name <= w[1].name),
        "SETTINGS must be sorted by name for find() to work"
    );
    SETTINGS
        .binary_search_by(|s| s.name.cmp(name))
        .ok()
        .map(|i| &SETTINGS[i])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_metadata_has_expected_count() {
        assert!(
            SETTINGS.len() >= 100,
            "expected ≥100 settings, got {}",
            SETTINGS.len()
        );
    }

    #[test]
    fn every_setting_has_required_fields() {
        for s in SETTINGS {
            assert!(!s.name.is_empty(), "empty name in entry");
            assert!(!s.description.is_empty(), "{}: empty description", s.name);
            assert!(!s.type_.is_empty(), "{}: empty type", s.name);
        }
    }

    #[test]
    fn known_settings_resolve() {
        assert!(find("registries").is_some());
        assert!(find("preferFrozenLockfile").is_some());
        assert!(find("aubeNoLock").is_some());
        assert!(find("totally-fake-setting").is_none());
    }

    #[test]
    fn find_matches_all() {
        for s in all() {
            assert_eq!(find(s.name).map(|x| x.name), Some(s.name));
        }
    }

    /// Every setting that advertises a `workspaceYaml` source must
    /// actually be readable from `pnpm-workspace.yaml` — i.e. the key
    /// has to deserialize onto `WorkspaceConfig` without falling into
    /// the `#[serde(flatten)] extra` catch-all. Guards against metadata
    /// rotting away from real workspace-config support.
    ///
    /// Lives here (in `aube-settings` — a crate that `aube-manifest`
    /// itself does not depend on) so it's a dev-only parity check; the
    /// dependency edge only exists for tests.
    #[test]
    fn workspace_yaml_keys_deserialize_onto_workspace_config() {
        // Field types on WorkspaceConfig vary, so there's no single
        // YAML value that parses for every field. Try a handful of
        // shapes in turn — if at least one parses AND the key doesn't
        // fall into `extra`, the field is wired up.
        let shapes = ["null", "true", "\"x\"", "0", "{}", "[]"];
        for s in SETTINGS {
            for key in s.workspace_yaml_keys {
                let mut recognized = false;
                let top_level_key = key.split('.').next().unwrap_or(key);
                for shape in shapes {
                    let yaml = format!("{top_level_key}: {shape}\n");
                    if let Ok(cfg) = yaml_serde::from_str::<aube_manifest::WorkspaceConfig>(&yaml)
                        && !cfg.extra.contains_key(top_level_key)
                    {
                        recognized = true;
                        break;
                    }
                }
                assert!(
                    recognized,
                    "{}: workspaceYaml key `{key}` is not a real field on WorkspaceConfig \
                     — it fell through to `extra` (or failed to deserialize entirely). \
                     Either add the field or remove the metadata.",
                    s.name
                );
            }
        }
    }

    /// Standalone aube must render EXACTLY what the literal used to say — the
    /// token exists for embedders, and buys nothing if it changes aube's own
    /// output. No embedder is registered in a unit-test process, so
    /// `embedder()` yields the AUBE profile and both namespaces are `aube`.
    #[test]
    fn tokens_render_to_aube_for_standalone_aube() {
        assert_eq!(
            render_namespaces("platform cache dir (`$XDG_CACHE_HOME/{cache_namespace}`)"),
            "platform cache dir (`$XDG_CACHE_HOME/aube`)"
        );
        assert_eq!(
            render_namespaces("$XDG_DATA_HOME/{data_namespace}/store/"),
            "$XDG_DATA_HOME/aube/store/"
        );
    }

    /// A default carrying no token is passed through untouched and unallocated.
    /// That is every setting but the two directory pairs, so the borrow is the
    /// case that matters for cost.
    #[test]
    fn untokenized_defaults_are_borrowed_unchanged() {
        let rendered = render_namespaces("true");
        assert_eq!(rendered, "true");
        assert!(matches!(rendered, std::borrow::Cow::Borrowed(_)));
    }

    /// Every tokenized default must resolve — a typo'd token would otherwise
    /// reach a user as a literal `{cache_namespace}` in `config list --all`.
    #[test]
    fn no_setting_default_leaves_an_unresolved_token() {
        for meta in all() {
            let rendered = meta.rendered_default();
            assert!(
                !rendered.contains('{') || !rendered.contains("_namespace}"),
                "{}: default still carries an unresolved token: {rendered}",
                meta.name
            );
        }
    }
}
