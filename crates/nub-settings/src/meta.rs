//! Static metadata for every setting in `settings.toml`.
//!
//! The `SETTINGS` slice is emitted at build time by `build.rs` from
//! the crate-local `settings.toml`. Each entry is a `'static`
//! borrow so there's zero runtime allocation and the compiler
//! verifies the schema shape at compile time.
//!
//! Use cases:
//! Read by `nub config` to describe, validate and ROUTE a setting, and by
//! the audits that compare nub's surface against pnpm's.

/// Substitute the namespace tokens a `default` string may carry with the
/// ACTIVE embedder's directory names.
///
/// `SettingMeta` is a build-time constant, so a default that names one of
/// nub's own directories cannot spell it literally — the table was written
/// for a tool with different directory names, and a baked literal would print
/// verbatim. Resolving at display time keeps one table correct.
///
/// Identity for the defaults that carry no token — which is all of them but
/// the directory pair.
pub fn render_namespaces(raw: &'static str) -> std::borrow::Cow<'static, str> {
    if !NAMESPACE_TOKENS.iter().any(|(tok, _)| raw.contains(tok)) {
        return std::borrow::Cow::Borrowed(raw);
    }
    let mut out = raw.to_string();
    for (token, resolved) in NAMESPACE_TOKENS {
        out = out.replace(token, resolved);
    }
    std::borrow::Cow::Owned(out)
}

/// Every token [`render_namespaces`] understands, paired with the embedder
/// field it resolves from.
///
/// Substitution and the audit that no default carries an UNKNOWN token both
/// read this one list, so a token can never be spelled in `settings.toml`
/// without something here to resolve it — a misspelling that would otherwise
/// sail through untouched and reach a user verbatim.
/// There is one host, so the tokens resolve to constants rather than through a
/// profile. `cache_namespace` is deeper than `data_namespace` because the
/// engine's cache is one of several things nub keeps under `~/.cache/nub`,
/// while the store owns its namespace outright.
const NAMESPACE_TOKENS: &[(&str, &str)] =
    &[("{cache_namespace}", "nub/pm"), ("{data_namespace}", "nub")];

/// The token-shaped `{identifier}` occurrences in `raw`, whether or not
/// [`render_namespaces`] knows them. A default may legitimately contain a
/// brace that is not a token (an inline table, `{ default = "..." }`), so only
/// a bare identifier between braces counts — that table is rejected on its
/// spaces, which is what makes the identifier rule safe to keep wide.
///
/// Wide deliberately: CASE is not part of the rule. `render_namespaces`
/// substitutes an exact lowercase match, so a token written `{Cache_Namespace}`
/// is left alone by it; if this scanner also skipped it, the two would share a
/// blind spot and the mis-cased token would print verbatim. Admitting it here
/// lets the audit reject it by name.
pub fn token_names(raw: &str) -> Vec<&str> {
    let mut found = Vec::new();
    let bytes = raw.as_bytes();
    let mut i = 0;
    while let Some(open) = raw[i..].find('{') {
        let start = i + open + 1;
        let Some(close) = raw[start..].find('}') else {
            break;
        };
        let end = start + close;
        let inner = &raw[start..end];
        if !inner.is_empty()
            && inner
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            found.push(inner);
        }
        i = end + 1;
        if i >= bytes.len() {
            break;
        }
    }
    found
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
    /// `pnpm-workspace.yaml` keys that
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
    /// `nub config set` writes it there to keep the multi-tool
    /// contract. A setting only one tool reads leaves this `false`
    /// and routes to that tool's own config instead.
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

/// Every setting the ACTIVE embedder consumes, alphabetically sorted by name.
///
/// Filtered, not the raw table: a host that does not route the command reading
/// a setting declares it in
/// [`crate::UNSUPPORTED_SETTINGS`], and this is where that declaration takes
/// effect for every enumerating surface at once — `config list`, `config get`,
/// `config find`.
///
/// Reach for [`all_unfiltered`] only to describe the TABLE rather than the
/// running tool — the docs generator, and the audits that must see every entry.
pub fn all() -> impl Iterator<Item = &'static SettingMeta> {
    SETTINGS.iter().filter(|meta| is_supported(meta.name))
}

/// The raw table, embedder filtering NOT applied. See [`all`].
pub fn all_unfiltered() -> &'static [SettingMeta] {
    SETTINGS
}

/// Whether the active embedder consumes `name`. False only for a setting the
/// host declared inert; see
/// [`crate::UNSUPPORTED_SETTINGS`].
pub fn is_supported(name: &str) -> bool {
    unsupported_advice(name).is_none()
}

/// The host's "use this instead" line for an unsupported setting, `None` when
/// the active embedder consumes it.
pub fn unsupported_advice(name: &str) -> Option<&'static str> {
    crate::UNSUPPORTED_SETTINGS
        .iter()
        .find(|(declared, _)| *declared == name)
        .map(|(_, advice)| *advice)
}

/// The unsupported setting `key` spells, under its canonical name or any
/// declared `.npmrc` / workspace-YAML alias.
///
/// The inverse of what [`find`] will tell you: `find` reports an unsupported
/// setting as simply absent, which is right for every RESOLUTION path but wrong
/// for a user who typed the name — `config set` needs to say "not a setting
/// here" rather than write the key through as free-form config.
pub fn unsupported_for_key(key: &str) -> Option<&'static SettingMeta> {
    SETTINGS.iter().find(|meta| {
        !is_supported(meta.name)
            && (meta.name == key
                || meta.npmrc_keys.contains(&key)
                || meta.workspace_yaml_keys.contains(&key))
    })
}

/// Whether an `.npmrc` key spells a `layout`-flagged setting, under any of
/// its aliases. Lets a caller filtering `.npmrc` entries by key — where no
/// [`SettingMeta`] is in hand — ask the layout question.
pub fn is_layout_npmrc_key(key: &str) -> bool {
    SETTINGS
        .iter()
        .any(|meta| meta.layout && meta.npmrc_keys.contains(&key) && is_supported(meta.name))
}

/// Look up a setting by its canonical pnpm name. `SETTINGS` is
/// generated sorted by name (build.rs guarantees this via
/// `BTreeMap` iteration), so a binary search drops the lookup from
/// O(N) to O(log N). With ~120 entries and dozens of lookups per
/// command, the saving compounds across every startup.
///
/// The sort invariant is asserted in debug builds. A future hand
/// edit of `SETTINGS` (or a regression in the build.rs sort) would
/// silently flip valid lookups to `None`; the assert catches that
/// in test runs without slowing release builds.
///
/// A setting the active embedder declared unsupported reports as ABSENT here,
/// which is what makes that declaration reach the resolver: every generated
/// `resolved::*` accessor opens with this lookup, so a miss falls the whole
/// source chain through to the default. Ask [`unsupported_for_key`] when the
/// caller needs to distinguish "not a setting" from "not a setting HERE".
pub fn find(name: &str) -> Option<&'static SettingMeta> {
    find_unfiltered(name).filter(|meta| is_supported(meta.name))
}

/// [`find`] without the embedder filter — the TABLE's answer rather than the
/// running tool's. For the docs generator and the audits only.
pub fn find_unfiltered(name: &str) -> Option<&'static SettingMeta> {
    debug_assert!(
        SETTINGS.windows(2).all(|w| w[0].name <= w[1].name),
        "SETTINGS must be sorted by name for find() to work"
    );
    SETTINGS
        .binary_search_by(|s| s.name.cmp(name))
        .ok()
        .map(|i| &SETTINGS[i])
}
