//! Config-scoping policy — "mirror the active PM, never silently" (CP-3).
//!
//! Graph-shaping config fields (dependency pins via `overrides` /
//! `resolutions` / `pnpm.overrides`, catalogs, workspace membership) carry
//! different support across package managers. npm honors top-level
//! `overrides` but ignores `resolutions`; yarn is the mirror image; pnpm
//! honors `resolutions` and its own `pnpm.overrides` but ignores top-level
//! `overrides`; bun honors both. Applying *every* field universally —
//! which is what aube does by default — over-applies a pin where the
//! project's active PM would ignore it, and that breaks the lockfile
//! round-trip: `nub install` pins a dep the next `pnpm install` leaves
//! alone, so the two tools fight over the lockfile.
//!
//! The policy: nub applies exactly what the project's **active PM** would
//! apply, in that PM's dialect. When the active PM would *silently* ignore
//! a graph-shaping field the user wrote, nub applies nothing and emits one
//! dim warning so the ignore is never silent. The active PM is the [`Role`]
//! resolved from the `packageManager` declaration (preferred) or the
//! lockfile kind (fallback) — the same precedence identity resolution and
//! the lifecycle UA use.
//!
//! This module is pure policy: it gathers the manifest's override entries as
//! a neutral source-tagged list ([`gather_tagged_overrides`]), filters that
//! list down to the role-honored sources, folds the survivors back with the
//! native precedence ([`fold_tagged_overrides`]), and reports which fields it
//! dropped. The result is handed to the engine via the `EngineContext`
//! (`embedder_overrides`, `trusted_dependencies_honored`) — the engine's
//! `PackageJson::overrides_map` returns that scoped map verbatim. The tagging /
//! folding lives here in nub, not in aube: the upstream engine consumes only
//! the final map and assigns no policy to the sources, so the per-dialect
//! breakdown is nub's concern (it was previously exposed by aube-manifest's
//! `tagged_overrides` / `fold_tagged_overrides`, removed in the embedder
//! refactor when aube stopped needing it).

use aube_lockfile::LockfileKind;
use aube_manifest::PackageJson;
use std::collections::BTreeMap;

/// Which top-level / namespaced source a dependency-override entry came from.
/// Preserved un-merged so the per-dialect scoping policy can keep only the
/// active package manager's native field before folding back with precedence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OverrideSource {
    /// yarn-style top-level `resolutions`.
    Resolutions,
    /// Namespaced `pnpm.overrides` (the only branded namespace nub scopes;
    /// nub's own config home is the manifest root, not an `aube`/`nub` object).
    NamespacedOverrides,
    /// Top-level `overrides` (npm / pnpm / bun).
    Overrides,
}

/// One override entry tagged with the manifest source it came from. `key` is
/// the raw selector string, `value` the raw version/spec string. No merge,
/// precedence, or `$name` resolution is applied.
#[derive(Debug, Clone)]
pub(crate) struct TaggedOverride {
    pub(crate) source: OverrideSource,
    pub(crate) key: String,
    pub(crate) value: String,
}

/// Gather the manifest's override entries as a flat, source-tagged list — no
/// merge, no precedence, no `$name` resolution. Order within each source is
/// declaration order; order across sources is `resolutions`, then
/// `pnpm.overrides`, then top-level `overrides` (matching the engine's own
/// fold precedence). Malformed keys (empty) and non-string values are dropped.
/// Reads the manifest's raw `extra` map directly — the neutral seam the engine
/// used to expose as `PackageJson::tagged_overrides`.
pub(crate) fn gather_tagged_overrides(manifest: &PackageJson) -> Vec<TaggedOverride> {
    let mut out: Vec<TaggedOverride> = Vec::new();
    let push = |out: &mut Vec<TaggedOverride>,
                source: OverrideSource,
                obj: &serde_json::Map<String, serde_json::Value>| {
        for (k, v) in obj {
            if let Some(s) = v.as_str()
                && !k.is_empty()
            {
                out.push(TaggedOverride {
                    source,
                    key: k.clone(),
                    value: s.to_string(),
                });
            }
        }
    };

    // yarn `resolutions` (lowest priority).
    if let Some(obj) = manifest
        .extra
        .get("resolutions")
        .and_then(|v| v.as_object())
    {
        push(&mut out, OverrideSource::Resolutions, obj);
    }
    // Branded `pnpm.overrides`. Gathered so the scoping policy can DROP it
    // under a non-pnpm role (brand-symmetry); under pnpm it's honored.
    if let Some(obj) = manifest
        .extra
        .get("pnpm")
        .and_then(|v| v.as_object())
        .and_then(|p| p.get("overrides"))
        .and_then(|v| v.as_object())
    {
        push(&mut out, OverrideSource::NamespacedOverrides, obj);
    }
    // Top-level `overrides` (npm / pnpm / bun) — highest priority.
    if let Some(obj) = manifest.extra.get("overrides").and_then(|v| v.as_object()) {
        push(&mut out, OverrideSource::Overrides, obj);
    }
    out
}

/// Fold a source-tagged list back into a precedence map: `resolutions`
/// (lowest), then `pnpm.overrides`, then top-level `overrides` (highest).
/// A stable sort by rank keeps within-source declaration order, then
/// later-wins on key collision reproduces a sequential per-source insert —
/// byte-identical to the engine's historical fold.
pub(crate) fn fold_tagged_overrides(tagged: Vec<TaggedOverride>) -> BTreeMap<String, String> {
    let rank = |s: OverrideSource| match s {
        OverrideSource::Resolutions => 0u8,
        OverrideSource::NamespacedOverrides => 1,
        OverrideSource::Overrides => 2,
    };
    let mut tagged = tagged;
    tagged.sort_by_key(|t| rank(t.source));
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for t in tagged {
        out.insert(t.key, t.value);
    }
    out
}

/// The active package manager whose config dialect nub mirrors. Resolved
/// declaration-first (the `packageManager`/`devEngines` pin names the
/// owner) then lockfile-kind, exactly like identity resolution. `Nub` is
/// nub's own brand-symmetric identity: it honors un-branded cross-tool
/// fields only, never another PM's branded config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    Npm,
    Pnpm,
    Yarn,
    Bun,
    Nub,
}

impl Role {
    /// The user-facing name nub prints in a scoping warning ("this project
    /// uses {pm}").
    pub(crate) fn display(self) -> &'static str {
        match self {
            Role::Npm => "npm",
            Role::Pnpm => "pnpm",
            Role::Yarn => "yarn",
            Role::Bun => "bun",
            Role::Nub => "nub",
        }
    }
}

/// Resolve the active-PM [`Role`] from the declared `packageManager` name
/// (if it names a PM nub recognizes) then the detected lockfile kind. This
/// is the shared role mapping; the lifecycle UA composer
/// (`compose_lifecycle_ua`) routes through it so the two never diverge.
///
/// `declared` is the raw `(name, version)` from `packageManager` /
/// `devEngines`; `kind` is the resolved lockfile kind. An unknown declared
/// name (vlt, deno, …) falls through to the lockfile kind, mirroring
/// identity resolution. Returns `None` only when neither a recognized
/// declaration nor a lockfile kind is present (a truly fresh project) — the
/// caller treats that as "fresh = nub identity" for scoping purposes.
pub(crate) fn role_of(declared: Option<&str>, kind: Option<LockfileKind>) -> Option<Role> {
    if let Some(name) = declared {
        match name {
            "npm" => return Some(Role::Npm),
            "pnpm" => return Some(Role::Pnpm),
            "yarn" => return Some(Role::Yarn),
            "bun" => return Some(Role::Bun),
            "nub" => return Some(Role::Nub),
            // Unknown declared tool: fall through to the lockfile kind.
            _ => {}
        }
    }
    kind.map(|k| match k {
        LockfileKind::Pnpm => Role::Pnpm,
        LockfileKind::Npm | LockfileKind::NpmShrinkwrap => Role::Npm,
        LockfileKind::Yarn | LockfileKind::YarnBerry => Role::Yarn,
        LockfileKind::Bun => Role::Bun,
        // The generic nub.lock (aube's `Aube` slot under nub's filename
        // toggle) is nub identity.
        LockfileKind::Aube => Role::Nub,
    })
}

/// Does the active PM honor a top-level `overrides` block?
///
/// Matrix: npm@8.3+ and bun honor it; pnpm, yarn, and npm<8.3 ignore it.
/// nub identity honors it (un-branded cross-tool field). `major` is the
/// declared major version (`None` when undeclared — assume a modern,
/// honoring npm, since an undeclared lockfile-only npm project is almost
/// always a current npm).
fn honors_overrides(role: Role, major: Option<u64>, minor: Option<u64>) -> bool {
    match role {
        Role::Npm => match (major, minor) {
            // npm gained top-level `overrides` in 8.3.0.
            (Some(8), Some(m)) => m >= 3,
            (Some(maj), _) => maj >= 8,
            // Undeclared npm: assume modern (≥8.3).
            (None, _) => true,
        },
        Role::Bun => true,
        Role::Nub => true,
        Role::Pnpm | Role::Yarn => false,
    }
}

/// Does the active PM honor top-level yarn-style `resolutions`?
///
/// Matrix: pnpm@5+, yarn (all), and bun honor it; npm (all) ignores it.
/// nub identity honors it. pnpm<5 predates `resolutions` support; an
/// undeclared pnpm is assumed modern.
fn honors_resolutions(role: Role, major: Option<u64>) -> bool {
    match role {
        Role::Pnpm => major.is_none_or(|m| m >= 5),
        Role::Yarn => true,
        Role::Bun => true,
        Role::Nub => true,
        Role::Npm => false,
    }
}

/// Does the active PM honor the branded `pnpm.overrides` namespace? Only
/// pnpm itself — and crucially NOT nub identity, which is brand-symmetric:
/// nub never adopts another PM's branded config.
fn honors_namespaced_overrides(role: Role) -> bool {
    role == Role::Pnpm
}

/// Does the active PM honor Bun's top-level `trustedDependencies` (the
/// build-script allowlist)? Only bun — at every major. Bun's current line
/// (1.3.x) still uses `trustedDependencies` for lifecycle-script approval
/// (bun.com/docs/install/lifecycle; bun source `install/lifecycle.zig` +
/// `lockfile.zig`), and ships no `onlyBuiltDependencies` / `approve-builds`
/// migration (zero hits in bun source). No version gate applies.
pub(crate) fn honors_trusted_dependencies(role: Role) -> bool {
    role == Role::Bun
}

/// One graph-shaping field nub dropped because the active PM ignores it,
/// with the fix nub recommends. Rendered as a single dim warning line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IgnoredField {
    /// The manifest field name, as written (`overrides`, `resolutions`).
    pub(crate) field: &'static str,
    /// The fix clause appended after the em-dash explanation.
    pub(crate) fix: String,
}

/// Scope a source-tagged override list to exactly the fields the active
/// `role` (at `major`/`minor`) honors, folding the survivors with aube's
/// native precedence. Returns the effective map and the list of
/// graph-shaping fields that were present in the manifest but ignored
/// under this role.
///
/// "Present but ignored" is per *source field*, not per entry: if a role
/// ignores top-level `overrides`, that's one warning regardless of how many
/// pins the block held. The fix clause names the role-native field the user
/// should move the pins to.
///
/// Precedence: the survivors keep the role-native ordering via
/// [`fold_tagged_overrides`]. For a single
/// surviving source that's a straight key→value map; for bun (both
/// `overrides` and `resolutions` honored) top-level `overrides` wins, which
/// is exactly bun's documented precedence and aube's fold rank.
pub(crate) fn scope_overrides(
    role: Role,
    major: Option<u64>,
    minor: Option<u64>,
    tagged: &[TaggedOverride],
) -> (BTreeMap<String, String>, Vec<IgnoredField>) {
    let keep = |src: OverrideSource| match src {
        OverrideSource::Overrides => honors_overrides(role, major, minor),
        OverrideSource::Resolutions => honors_resolutions(role, major),
        OverrideSource::NamespacedOverrides => honors_namespaced_overrides(role),
    };

    let kept: Vec<TaggedOverride> = tagged.iter().filter(|t| keep(t.source)).cloned().collect();
    let effective = fold_tagged_overrides(kept);

    // Which graph-shaping *fields* were present but dropped. Branded
    // `pnpm.overrides` under a non-pnpm role is the user's own pnpm-tutorial
    // residue, not a cross-tool pin to mirror — we never warn nub *into*
    // applying another PM's branded namespace, so it's excluded from the
    // warning surface (dropping it silently is correct: a non-pnpm PM
    // ignores it too).
    let present = |src: OverrideSource| tagged.iter().any(|t| t.source == src);
    let mut ignored = Vec::new();

    if present(OverrideSource::Overrides) && !honors_overrides(role, major, minor) {
        ignored.push(IgnoredField {
            field: "overrides",
            // pnpm/yarn want the pins in `resolutions` (or, for pnpm, its
            // branded `pnpm.overrides`); both honor `resolutions`.
            fix: "move these pins to `resolutions`".to_string(),
        });
    }
    if present(OverrideSource::Resolutions) && !honors_resolutions(role, major) {
        // Only npm ignores `resolutions`; npm wants `overrides`.
        ignored.push(IgnoredField {
            field: "resolutions",
            fix: "move these pins to `overrides`".to_string(),
        });
    }

    // Critical "not annoying" guard: suppress a warning when a HONORED
    // field already carries the same pin (a portable repo declaring the pin
    // in both `overrides` and `resolutions` for cross-PM compatibility must
    // stay SILENT — the ignore changes nothing). A field's warning is
    // suppressed when every pin it holds is already present, with the same
    // value, in the effective (honored) map.
    ignored.retain(|f| {
        let src = match f.field {
            "overrides" => OverrideSource::Overrides,
            "resolutions" => OverrideSource::Resolutions,
            _ => return true,
        };
        let entries: Vec<&TaggedOverride> = tagged.iter().filter(|t| t.source == src).collect();
        // Keep the warning only if at least one pin in the ignored field is
        // NOT already satisfied by the honored map.
        entries
            .iter()
            .any(|t| effective.get(&t.key) != Some(&t.value))
    });

    (effective, ignored)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(source: OverrideSource, key: &str, value: &str) -> TaggedOverride {
        TaggedOverride {
            source,
            key: key.to_string(),
            value: value.to_string(),
        }
    }

    #[test]
    fn role_of_prefers_declaration_then_lockfile_kind() {
        assert_eq!(
            role_of(Some("pnpm"), Some(LockfileKind::Npm)),
            Some(Role::Pnpm)
        );
        assert_eq!(
            role_of(Some("vlt"), Some(LockfileKind::Bun)),
            Some(Role::Bun)
        );
        assert_eq!(role_of(None, Some(LockfileKind::Aube)), Some(Role::Nub));
        assert_eq!(role_of(None, None), None);
    }

    #[test]
    fn overrides_ignored_under_pnpm_is_warned_and_dropped() {
        let tagged = vec![tag(OverrideSource::Overrides, "lodash", "4.17.21")];
        let (eff, ignored) = scope_overrides(Role::Pnpm, Some(9), None, &tagged);
        assert!(eff.is_empty(), "pnpm must not apply top-level overrides");
        assert_eq!(ignored.len(), 1);
        assert_eq!(ignored[0].field, "overrides");
    }

    #[test]
    fn resolutions_ignored_under_npm_is_warned_and_dropped() {
        let tagged = vec![tag(OverrideSource::Resolutions, "lodash", "4.17.21")];
        let (eff, ignored) = scope_overrides(Role::Npm, Some(10), None, &tagged);
        assert!(eff.is_empty(), "npm must not apply resolutions");
        assert_eq!(ignored.len(), 1);
        assert_eq!(ignored[0].field, "resolutions");
    }

    #[test]
    fn both_honored_under_bun_overrides_wins_no_warning() {
        let tagged = vec![
            tag(OverrideSource::Resolutions, "lodash", "1.0.0"),
            tag(OverrideSource::Overrides, "lodash", "2.0.0"),
        ];
        let (eff, ignored) = scope_overrides(Role::Bun, None, None, &tagged);
        assert_eq!(eff.get("lodash").unwrap(), "2.0.0", "bun: overrides wins");
        assert!(ignored.is_empty(), "bun honors both, nothing ignored");
    }

    #[test]
    fn portable_repo_same_pins_stays_silent() {
        // A repo declaring the same pin in both fields for cross-PM
        // portability: under npm, `resolutions` is ignored — but `overrides`
        // carries the identical pin, so the ignore changes nothing. Silent.
        let tagged = vec![
            tag(OverrideSource::Overrides, "lodash", "4.17.21"),
            tag(OverrideSource::Resolutions, "lodash", "4.17.21"),
        ];
        let (eff, ignored) = scope_overrides(Role::Npm, Some(10), None, &tagged);
        assert_eq!(eff.get("lodash").unwrap(), "4.17.21");
        assert!(
            ignored.is_empty(),
            "honored field carries the same pin — must stay silent"
        );
    }

    #[test]
    fn portable_repo_divergent_pins_still_warns() {
        // Same field shape, but the pins DIFFER — the ignore now changes the
        // resolved version, so the warning must fire.
        let tagged = vec![
            tag(OverrideSource::Overrides, "lodash", "4.0.0"),
            tag(OverrideSource::Resolutions, "lodash", "5.0.0"),
        ];
        let (eff, ignored) = scope_overrides(Role::Npm, Some(10), None, &tagged);
        assert_eq!(eff.get("lodash").unwrap(), "4.0.0");
        assert_eq!(ignored.len(), 1, "divergent ignored pin must warn");
    }

    #[test]
    fn nub_identity_ignores_branded_pnpm_overrides() {
        // nub is brand-symmetric: it honors un-branded `overrides` /
        // `resolutions` but never another PM's branded `pnpm.overrides`.
        let tagged = vec![
            tag(OverrideSource::NamespacedOverrides, "lodash", "2.0.0"),
            tag(OverrideSource::Overrides, "left-pad", "1.3.0"),
        ];
        let (eff, ignored) = scope_overrides(Role::Nub, None, None, &tagged);
        assert_eq!(eff.get("left-pad").unwrap(), "1.3.0");
        assert!(
            !eff.contains_key("lodash"),
            "nub must not apply branded pnpm.overrides"
        );
        // Branded-namespace drops are silent (no cross-tool field to mirror).
        assert!(ignored.is_empty());
    }

    #[test]
    fn pnpm_honors_its_branded_overrides_and_resolutions() {
        let tagged = vec![
            tag(OverrideSource::Resolutions, "a", "1.0.0"),
            tag(OverrideSource::NamespacedOverrides, "b", "2.0.0"),
            tag(OverrideSource::Overrides, "c", "3.0.0"),
        ];
        let (eff, ignored) = scope_overrides(Role::Pnpm, Some(9), None, &tagged);
        assert_eq!(eff.get("a").unwrap(), "1.0.0");
        assert_eq!(eff.get("b").unwrap(), "2.0.0");
        assert!(!eff.contains_key("c"), "pnpm ignores top-level overrides");
        // Only the top-level `overrides` field is ignored → one warning.
        assert_eq!(ignored.len(), 1);
        assert_eq!(ignored[0].field, "overrides");
    }

    #[test]
    fn npm_below_8_3_does_not_honor_overrides() {
        assert!(!honors_overrides(Role::Npm, Some(8), Some(2)));
        assert!(honors_overrides(Role::Npm, Some(8), Some(3)));
        assert!(honors_overrides(Role::Npm, Some(9), None));
        assert!(!honors_overrides(Role::Npm, Some(7), None));
    }

    #[test]
    fn only_bun_honors_trusted_dependencies() {
        // Bun honors `trustedDependencies` at every major (no version drop —
        // bun 1.3.x still uses it; see `honors_trusted_dependencies`).
        assert!(honors_trusted_dependencies(Role::Bun));
        assert!(!honors_trusted_dependencies(Role::Pnpm));
        assert!(!honors_trusted_dependencies(Role::Npm));
        assert!(!honors_trusted_dependencies(Role::Yarn));
        assert!(!honors_trusted_dependencies(Role::Nub));
    }
}
