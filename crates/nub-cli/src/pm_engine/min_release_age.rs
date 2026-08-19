//! Loose-mode `minimumReleaseAge` auto-persist (fixes #262).
//!
//! With `minimumReleaseAge` set, pnpm's default (loose) mode installs a version
//! younger than the cutoff when nothing older satisfies the range, and
//! co-writes that package to `minimumReleaseAgeExclude` so pnpm's own
//! install-time lockfile verifier — which a project's `pnpm <script>` triggers —
//! accepts the pin. nub's engine does the same lowest-satisfying fallback but
//! never recorded the exclude, so a `nub install`/`add`/`update` produced a
//! lockfile pnpm later rejected with `ERR_PNPM_MINIMUM_RELEASE_AGE_VIOLATION`
//! ([#262](https://github.com/nubjs/nub/issues/262)). This module closes that
//! gap: the resolver surfaces each immature fallback pick (armed via [`arm`]),
//! and after a successful mutating install nub records the packages in
//! `minimumReleaseAgeExclude` in the surface the project's config identity
//! dictates.
//!
//! ## Write target
//!
//! `minimumReleaseAgeExclude` is a pnpm *workspace-config-file* setting — pnpm
//! reads it ONLY from `pnpm-workspace.yaml`, never from `.npmrc` (it is in
//! pnpm's `configFileKey` set; verified against pnpm 11's config reader, where
//! an `.npmrc` `[]` value for a sibling policy key resolves to `undefined`). So
//! the target is chosen by who will re-read the lockfile:
//!
//! - **A pnpm-compat project** (pnpm is the incumbent — an existing
//!   `pnpm-workspace.yaml`, or a `pnpm-lock.yaml` nub round-trips) → the
//!   `pnpm-workspace.yaml` sequence, created if absent. This is the only place
//!   pnpm reads the setting and where pnpm's own auto-persist writes it. The
//!   choice is gated on the `read_branded_pnpm_config` posture, so a nub-identity
//!   or npm/yarn/bun project's stray `pnpm-workspace.yaml` is never chosen and no
//!   file is created for a truly-fresh project (nothing re-reads its lockfile as
//!   pnpm).
//! - **Otherwise** (nub identity, npm/yarn/bun compat, fresh) → the project
//!   `.npmrc`, comma-separated — the neutral surface nub itself reads
//!   `minimumReleaseAge*` from, so nub reads back exactly what it wrote. No pnpm
//!   re-reads these projects' lockfiles, so pnpm's `.npmrc` blind spot is moot.
//!   Brand-clean: `.npmrc` is the npm-shared config and the key is an unbranded
//!   setting name.
//!
//! Entries are written in pnpm's canonical per-package form (`name@v1 || v2`,
//! merged with what's already recorded), matching pnpm's own auto-persist output
//! and avoiding the pnpm edge where separate exact-version entries for one
//! package were not all honored.
//!
//! Strict mode (`minimumReleaseAgeStrict=true`) never reaches here: the resolver
//! hard-aborts on `AgeGated` before returning a fallback pick.

use super::output::OutputFlags;
use aube_manifest::workspace::{
    read_workspace_yaml_string_list, set_workspace_yaml_string_list, workspace_yaml_existing,
};
use std::path::{Path, PathBuf};

const EXCLUDE_KEY: &str = "minimumReleaseAgeExclude";

/// Arm the resolver's age-gate fallback sink for the upcoming resolve. Called
/// before an install/add/update engine run; [`persist`] drains it afterward.
pub(crate) fn arm() {
    aube_util::arm_age_gated_fallback_pick_collection();
}

/// Drain the resolver's immature fallback picks and, on a successful mutating
/// install, record them in `minimumReleaseAgeExclude`. Always drains the sink
/// (even on failure or when disarmed) so nothing leaks into a later in-process
/// command. `success` gates the write to a completed install; `output` gates
/// the notice on `--silent` (the write itself is not gated).
pub(crate) fn persist(cwd: &Path, success: bool, output: &OutputFlags) {
    let picks = aube_util::take_age_gated_fallback_picks();
    if !success || picks.is_empty() {
        return;
    }

    // `name@version` specs in first-seen order, deduped. The resolver only
    // records a pick that wasn't already age-exempt, so these are genuinely new.
    let mut new_specs: Vec<String> = Vec::with_capacity(picks.len());
    for (name, version) in &picks {
        let spec = format!("{name}@{version}");
        if !new_specs.contains(&spec) {
            new_specs.push(spec);
        }
    }

    match record(cwd, &new_specs) {
        Ok((target, changed)) => {
            if changed && !output.is_silent() {
                emit_notice(&target, &new_specs);
            }
        }
        Err(err) => {
            // The install itself succeeded; a config-write failure is surfaced
            // (not silently) with the manual remedy, but must not fail the run.
            super::present::warn(&format!(
                "warn: installed {n} package(s) newer than minimumReleaseAge but could not record \
                 them in {EXCLUDE_KEY} ({err}); add {list} manually, or pnpm may reject this \
                 lockfile.",
                n = new_specs.len(),
                list = new_specs.join(", "),
            ));
        }
    }
}

/// Where the exclude entries were persisted, for the notice.
enum Target {
    WorkspaceYaml(String),
    Npmrc,
}

impl Target {
    fn display(&self) -> &str {
        match self {
            Target::WorkspaceYaml(name) => name,
            Target::Npmrc => ".npmrc",
        }
    }
}

/// Route to the config surface, merge `new_specs` with what's already recorded
/// into pnpm's canonical per-package form, and write the result. Returns the
/// target and whether the file changed.
fn record(cwd: &Path, new_specs: &[String]) -> anyhow::Result<(Target, bool)> {
    match resolve_target(cwd) {
        WriteTarget::WorkspaceYaml(path) => {
            let existing = read_workspace_yaml_string_list(&path, EXCLUDE_KEY);
            let merged = merge_package_version_specs(&existing, new_specs);
            let changed = merged != merge_package_version_specs(&existing, &[]);
            if changed {
                set_workspace_yaml_string_list(&path, EXCLUDE_KEY, &merged)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
            }
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("pnpm-workspace.yaml")
                .to_string();
            Ok((Target::WorkspaceYaml(name), changed))
        }
        WriteTarget::Npmrc => {
            let changed = write_npmrc(cwd, new_specs)?;
            Ok((Target::Npmrc, changed))
        }
    }
}

enum WriteTarget {
    WorkspaceYaml(PathBuf),
    Npmrc,
}

/// Pick the config file to write. See the module docs for the rule.
fn resolve_target(cwd: &Path) -> WriteTarget {
    if let Some(path) = workspace_yaml_existing(cwd) {
        return WriteTarget::WorkspaceYaml(path);
    }
    // pnpm is the incumbent (nub round-trips its lockfile) but keeps no
    // workspace yaml yet: create one, the only surface pnpm reads the setting
    // from. Gated on the pnpm-compat posture so a nub-identity / npm / yarn /
    // bun project (or a fresh one) is never handed a pnpm-workspace.yaml.
    if aube_util::engine_context().read_branded_pnpm_config && cwd.join("pnpm-lock.yaml").is_file()
    {
        return WriteTarget::WorkspaceYaml(cwd.join("pnpm-workspace.yaml"));
    }
    WriteTarget::Npmrc
}

/// Merge the project `.npmrc`'s comma-separated `minimumReleaseAgeExclude` with
/// `new_specs` into the canonical per-package form, preserving the rest of the
/// file. Returns whether the file changed.
fn write_npmrc(cwd: &Path, new_specs: &[String]) -> anyhow::Result<bool> {
    use aube::commands::npmrc::NpmrcEdit;
    let path = cwd.join(".npmrc");
    let mut edit = NpmrcEdit::load(&path).map_err(|e| anyhow::anyhow!("{e}"))?;

    // Existing value is last-write-wins at read time; merge into the last
    // occurrence and rewrite a single canonical key.
    let existing: Vec<String> = edit
        .entries()
        .into_iter()
        .rfind(|(k, _)| k == EXCLUDE_KEY)
        .map(|(_, v)| parse_comma_list(&v))
        .unwrap_or_default();
    let merged = merge_package_version_specs(&existing, new_specs);
    if merged == merge_package_version_specs(&existing, &[]) {
        return Ok(false);
    }
    edit.set(EXCLUDE_KEY, &merged.join(","));
    edit.save(&path).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(true)
}

/// Merge `existing` exclude specs with `additions` into pnpm's canonical
/// per-package form: one entry per package; a bare name (no version) excludes
/// every version and absorbs any version-specific specs; otherwise the exact
/// versions are deduped and joined with ` || `. Packages keep first-seen order.
/// A faithful port of pnpm's `mergePackageVersionSpecs`
/// (`config/version-policy`), minus the semver sort (a union is order-agnostic
/// for matching).
fn merge_package_version_specs(existing: &[String], additions: &[String]) -> Vec<String> {
    // `None` for a package means "bare name" (all versions excluded).
    let mut order: Vec<String> = Vec::new();
    let mut by_pkg: std::collections::HashMap<String, Option<Vec<String>>> =
        std::collections::HashMap::new();

    for spec in existing.iter().chain(additions.iter()) {
        let (name, versions) = parse_spec(spec);
        match by_pkg.get_mut(&name) {
            None => {
                order.push(name.clone());
                by_pkg.insert(
                    name,
                    if versions.is_empty() {
                        None
                    } else {
                        Some(versions)
                    },
                );
            }
            Some(slot) => {
                if versions.is_empty() {
                    // Bare name excludes every version — absorbs the rest.
                    *slot = None;
                } else if let Some(vers) = slot.as_mut() {
                    for v in versions {
                        if !vers.contains(&v) {
                            vers.push(v);
                        }
                    }
                }
                // else the slot is already `None` (all versions): nothing to do.
            }
        }
    }

    order
        .into_iter()
        .map(|name| match by_pkg.remove(&name).flatten() {
            None => name,
            Some(vers) => format!("{name}@{}", vers.join(" || ")),
        })
        .collect()
}

/// Split a `name@version-union` spec into `(package_name, versions)`. A spec
/// with no version tail (a bare name or `*` glob) yields an empty version list.
/// Scoped names (`@scope/name`) keep their leading `@`.
fn parse_spec(spec: &str) -> (String, Vec<String>) {
    let at = if let Some(stripped) = spec.strip_prefix('@') {
        stripped.find('@').map(|i| i + 1)
    } else {
        spec.find('@')
    };
    match at {
        None => (spec.to_string(), Vec::new()),
        Some(i) => {
            let versions = spec[i + 1..]
                .split("||")
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            (spec[..i].to_string(), versions)
        }
    }
}

/// Split a `.npmrc` list value on commas, trimming and dropping empties.
/// Mirrors how the engine reads a `list<string>` setting from `.npmrc`.
fn parse_comma_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Factual notice naming what was recorded and where, plus the strict opt-out.
/// Stderr (engine convention: stdout is data), suppressed under `--silent` by
/// the caller.
fn emit_notice(target: &Target, recorded: &[String]) {
    let lead = if recorded.len() == 1 {
        "1 version newer than minimumReleaseAge was installed".to_string()
    } else {
        format!(
            "{} versions newer than minimumReleaseAge were installed",
            recorded.len()
        )
    };
    super::present::info(&format!(
        "{lead} and recorded in {EXCLUDE_KEY} ({}):\n  {}\n\
         Set minimumReleaseAgeStrict=true to fail on these instead.",
        target.display(),
        recorded.join("\n  "),
    ));
}

/// Engine minutes for a `--minimum-release-age` value, accepting BOTH surfaces
/// this setting already has:
///
/// - `<integer><unit>` (`s|m|h|d|w`) — the exact grammar `nub.jsonc`'s
///   `install.minimumReleaseAge` accepts, parsed by the same
///   [`crate::project_config::parse_duration`] so a value that works in the file
///   works on the flag and the two cannot drift.
/// - a bare integer — MINUTES, mirroring pnpm's CLI, whose type map declares
///   `minimum-release-age` as a Number.
///
/// The file deliberately REJECTS the bare form (npm counts days, pnpm counts
/// minutes, so an unqualified number in a config file is a trap). On the CLI the
/// ambiguity is already settled by the tool nub mirrors, so accepting it is
/// compatibility rather than a hazard.
///
/// Sub-minute values round UP, never down: the engine setting is whole minutes,
/// and `30s` collapsing to `0` would SILENTLY DISABLE the gate instead of
/// tightening it. Same rule, and the same reason, as the bunfig seconds→minutes
/// conversion in [`super::bun_config`]. A literal `0` still means zero — that is
/// the documented "turn it off" value, not a rounding artifact.
fn parse_release_age_minutes(raw: &str) -> Result<u64, String> {
    let s = raw.trim();
    if let Ok(minutes) = s.parse::<u64>() {
        return Ok(minutes);
    }
    // Take the config parser's VERDICT but not its wording: its `Display`
    // attributes the failure to `nub.jsonc`, which is a lie on a command line.
    // The grammar stays in one place; only the sentence differs.
    let dur = crate::project_config::parse_duration(s, "--minimum-release-age").map_err(|_| {
        format!(
            "expected minutes (e.g. `1440`) or a duration with a unit \
             s|m|h|d|w (e.g. `3d`), got `{raw}`"
        )
    })?;
    Ok(dur.as_secs().div_ceil(60))
}

// The per-invocation age-gate flags, flattened into every nub surface that
// resolves from the registry (`install`/`ci`, the engine verbs via
// `EngineGlobals`, and `nubx`).
//
// Each flag mirrors BOTH of the surfaces this setting already has: the pnpm
// spelling (its CLI type map declares `minimum-release-age` and
// `minimum-release-age-exclude`) and the value grammar of the matching
// `nub.jsonc` field (`install.minimumReleaseAge`,
// `install.minimumReleaseAgeExclude`).
//
// NO STRICTNESS FLAG, deliberately. Under NUB IDENTITY the gate defaults to
// strict, and the way to opt out is `--minimum-release-age=0` — turn the window
// OFF, rather than keep a window and quietly install versions that fail it. So
// there is no `--[no-]minimum-release-age-strict`, and no
// `install.minimumReleaseAgeStrict` in `nub.jsonc` either — one axis (how
// long), not two. A COMPAT project is a different question, not a different
// flag surface: it never receives nub's strict pin at all (see
// `super::nub_setting_defaults`), so it runs the engine's non-strict default
// and this module's loose-mode auto-persist below is its ordinary path.
//
// (Plain `//`, not rustdoc: a `///` comment on a clap `Args` struct becomes the
// augmented command's `--help` about-text and clobbers the verb's own, the same
// hazard `EngineGlobals` documents. This one leaked onto `nub add --help`.)
#[derive(Debug, Default, Clone, clap::Args)]
pub struct AgeGateFlags {
    /// How old a version must be before it can be installed: a duration with a
    /// unit (`30s`, `5m`, `2h`, `3d`, `1w`), or a bare number meaning minutes.
    /// `0` turns the age gate off for this run. Overrides `minimumReleaseAge`
    /// from config.
    #[arg(long, value_name = "DURATION", value_parser = parse_release_age_minutes)]
    pub minimum_release_age: Option<u64>,

    /// Exempt packages from the age gate for this run (repeatable). Entry
    /// grammar matches the config field: a bare name, a `*` name glob, or a
    /// name with a version range. REPLACES any configured
    /// `minimumReleaseAgeExclude` rather than adding to it, so pass every
    /// package you still need exempt.
    #[arg(long, value_name = "PKG")]
    pub minimum_release_age_exclude: Vec<String>,
}

impl AgeGateFlags {
    /// The `(setting, value)` pairs for [`aube_settings::set_global_cli_overrides`].
    /// Keys are the canonical setting names — `cli_key_matches` compares in
    /// kebab-case, so these reach `minimumReleaseAge` / `minimumReleaseAgeExclude`
    /// without needing a `sources.cli` alias declared in `settings.toml`.
    fn cli_overrides(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        if let Some(minutes) = self.minimum_release_age {
            out.push(("minimumReleaseAge".to_string(), minutes.to_string()));
        }
        if !self.minimum_release_age_exclude.is_empty() {
            // The settings reader takes the LAST matching CLI entry and parses it
            // as one list, so repeated `--minimum-release-age-exclude` flags have
            // to arrive joined rather than as separate entries.
            out.push((
                "minimumReleaseAgeExclude".to_string(),
                self.minimum_release_age_exclude.join(","),
            ));
        }
        out
    }

    /// Publish the flags into the engine's process-global CLI-override bag,
    /// which `aube_settings`' typed accessors consult ahead of every other
    /// source. One call covers the whole run: the resolver, the default-trust
    /// floor, and any chained install all read through the same accessors, so
    /// nothing needs the value threaded down to it.
    ///
    /// A managed (org-policy) config still wins — `minimumReleaseAge` carries
    /// `managedPolicy = "max"`, applied by the generated accessor's finalizer
    /// after this bag is consulted, so a CLI flag can raise the floor but never
    /// lower one an administrator set.
    ///
    /// The bag is a `OnceLock`, so skip the call when nothing was passed rather
    /// than latching an empty vec.
    pub fn apply(&self) {
        let overrides = self.cli_overrides();
        if !overrides.is_empty() {
            aube_settings::set_global_cli_overrides(overrides);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_spec_handles_bare_scoped_and_union() {
        assert_eq!(parse_spec("react"), ("react".into(), vec![]));
        assert_eq!(parse_spec("@myorg/*"), ("@myorg/*".into(), vec![]));
        assert_eq!(
            parse_spec("vite@6.0.0"),
            ("vite".into(), vec!["6.0.0".to_string()])
        );
        assert_eq!(
            parse_spec("@scope/pkg@1.0.0 || 2.0.0"),
            (
                "@scope/pkg".into(),
                vec!["1.0.0".to_string(), "2.0.0".to_string()]
            )
        );
    }

    #[test]
    fn merge_produces_canonical_per_package_union() {
        // Same package across existing + additions collapses to one `||` entry;
        // a bare name absorbs versions; order is first-seen.
        let merged = merge_package_version_specs(
            &["caniuse-lite@1.0.0".to_string(), "left-pad".to_string()],
            &[
                "caniuse-lite@2.0.0".to_string(),
                "vite@6.0.0".to_string(),
                "left-pad@9.9.9".to_string(),
            ],
        );
        assert_eq!(
            merged,
            vec!["caniuse-lite@1.0.0 || 2.0.0", "left-pad", "vite@6.0.0"]
        );
        // Idempotent: re-merging the same additions changes nothing.
        assert_eq!(merge_package_version_specs(&merged, &[]), merged);
    }

    #[test]
    fn parse_comma_list_trims_and_drops_empties() {
        assert_eq!(
            parse_comma_list("a@1.0.0, b@2.0.0 ,, c@3.0.0"),
            vec!["a@1.0.0", "b@2.0.0", "c@3.0.0"]
        );
        assert!(parse_comma_list("").is_empty());
    }

    #[test]
    fn write_npmrc_creates_merges_and_canonicalizes() {
        let dir = tempfile::tempdir().unwrap();
        let npmrc = dir.path().join(".npmrc");
        std::fs::write(&npmrc, "registry=https://example.test/\n").unwrap();

        let changed = write_npmrc(dir.path(), &["caniuse-lite@1.0.0".to_string()]).unwrap();
        assert!(changed);
        let content = std::fs::read_to_string(&npmrc).unwrap();
        assert!(content.contains("registry=https://example.test/"));
        assert!(
            content.contains("minimumReleaseAgeExclude=caniuse-lite@1.0.0"),
            "unexpected .npmrc:\n{content}"
        );

        // A second immature version of the same package collapses to `||`, in
        // one canonical key (not a duplicate line).
        let changed = write_npmrc(dir.path(), &["caniuse-lite@2.0.0".to_string()]).unwrap();
        assert!(changed);
        let content = std::fs::read_to_string(&npmrc).unwrap();
        assert_eq!(content.matches("minimumReleaseAgeExclude").count(), 1);
        assert!(
            content.contains("minimumReleaseAgeExclude=caniuse-lite@1.0.0 || 2.0.0"),
            "not canonicalized:\n{content}"
        );
    }

    #[test]
    fn resolve_target_npmrc_without_pnpm_signal() {
        // No workspace yaml, no pnpm-lock.yaml → the neutral .npmrc.
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(resolve_target(dir.path()), WriteTarget::Npmrc));
    }

    #[test]
    fn resolve_target_creates_workspace_yaml_for_pnpm_incumbent() {
        // `workspace_yaml_existing` and the pnpm-lock signal both read the
        // process-global EngineContext posture; serialize + set it.
        let _guard = super::super::ENGINE_GLOBAL_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        aube_util::update_engine_context(|c| c.read_branded_pnpm_config = true);

        // Existing pnpm-workspace.yaml wins.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pnpm-workspace.yaml"), "packages: []\n").unwrap();
        assert!(matches!(
            resolve_target(dir.path()),
            WriteTarget::WorkspaceYaml(_)
        ));

        // No yaml but a pnpm-lock.yaml (pnpm incumbent) → target the yaml (created).
        let dir2 = tempfile::tempdir().unwrap();
        std::fs::write(
            dir2.path().join("pnpm-lock.yaml"),
            "lockfileVersion: '9.0'\n",
        )
        .unwrap();
        match resolve_target(dir2.path()) {
            WriteTarget::WorkspaceYaml(p) => {
                assert_eq!(p.file_name().unwrap(), "pnpm-workspace.yaml")
            }
            WriteTarget::Npmrc => panic!("pnpm incumbent should target the workspace yaml"),
        }
    }

    #[test]
    fn record_writes_yaml_sequence_for_pnpm_project() {
        let _guard = super::super::ENGINE_GLOBAL_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        aube_util::update_engine_context(|c| c.read_branded_pnpm_config = true);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pnpm-workspace.yaml"), "packages: []\n").unwrap();

        let (target, changed) = record(dir.path(), &["vite@6.0.0".to_string()]).unwrap();
        assert!(changed);
        assert!(matches!(target, Target::WorkspaceYaml(_)));
        assert!(!dir.path().join(".npmrc").exists());
        assert_eq!(
            read_workspace_yaml_string_list(&dir.path().join("pnpm-workspace.yaml"), EXCLUDE_KEY),
            vec!["vite@6.0.0"]
        );
    }

    /// Zero must reach the engine as a literal `0`, because `0` is the ONLY way
    /// to turn the gate off — nub ships no strictness flag, so a value that
    /// arrived as anything else (or was dropped as "unset") would leave a user
    /// who asked for no window still gated. `resolve_minimum_release_age`
    /// short-circuits to `None` on exactly `0`.
    #[test]
    fn zero_reaches_the_engine_as_the_off_switch() {
        let flags = AgeGateFlags {
            minimum_release_age: Some(0),
            ..Default::default()
        };
        assert_eq!(
            flags.cli_overrides(),
            vec![("minimumReleaseAge".to_string(), "0".to_string())],
            "0 must be published verbatim, not elided as a falsy/default value"
        );
        // Every spelling of "no window" collapses to the same 0.
        assert_eq!(parse_release_age_minutes("0"), Ok(0));
        assert_eq!(parse_release_age_minutes("0s"), Ok(0));
        assert_eq!(parse_release_age_minutes("0d"), Ok(0));
    }

    /// The flag takes both surfaces the setting already has: `nub.jsonc`'s
    /// unit grammar, and pnpm's bare-number-means-minutes.
    #[test]
    fn release_age_accepts_the_config_units_and_pnpms_bare_minutes() {
        // Bare number = minutes, matching pnpm's CLI type map.
        assert_eq!(parse_release_age_minutes("1440"), Ok(1440));
        assert_eq!(parse_release_age_minutes("0"), Ok(0));
        // Every unit `nub.jsonc`'s install.minimumReleaseAge accepts.
        assert_eq!(parse_release_age_minutes("5m"), Ok(5));
        assert_eq!(parse_release_age_minutes("2h"), Ok(120));
        assert_eq!(parse_release_age_minutes("3d"), Ok(4320));
        assert_eq!(parse_release_age_minutes("1w"), Ok(10_080));
        // `1d` is the default window, so this is the identity a user checks.
        assert_eq!(
            parse_release_age_minutes("1d"),
            parse_release_age_minutes("1440")
        );
    }

    /// Sub-minute values must round UP. Truncating `30s` to `0` would turn a
    /// request to TIGHTEN the gate into silently disabling it — the same trap
    /// the bunfig seconds→minutes conversion guards against.
    #[test]
    fn sub_minute_release_age_rounds_up_so_it_never_disables_the_gate() {
        assert_eq!(parse_release_age_minutes("30s"), Ok(1));
        assert_eq!(parse_release_age_minutes("1s"), Ok(1));
        assert_eq!(parse_release_age_minutes("90s"), Ok(2));
        // An explicit zero is the documented "off" value, not a rounding artifact.
        assert_eq!(parse_release_age_minutes("0s"), Ok(0));
    }

    /// The flag inherits the config grammar's rejections, so a value the file
    /// refuses cannot sneak in through the CLI.
    #[test]
    fn release_age_rejects_what_the_config_grammar_rejects() {
        for bad in ["3y", "-1d", "3 d", "d", "3_0d", ""] {
            assert!(
                parse_release_age_minutes(bad).is_err(),
                "{bad:?} must be rejected on the flag, as it is in nub.jsonc"
            );
        }
    }

    /// Repeated `--minimum-release-age-exclude` must arrive as ONE joined entry:
    /// the settings reader takes the last matching CLI key and parses it as a
    /// single list, so separate entries would drop all but the final package.
    #[test]
    fn repeated_excludes_are_joined_into_one_cli_entry() {
        let flags = AgeGateFlags {
            minimum_release_age_exclude: vec!["react".into(), "@myorg/*".into()],
            ..Default::default()
        };
        let overrides = flags.cli_overrides();
        let exclude: Vec<_> = overrides
            .iter()
            .filter(|(k, _)| k == "minimumReleaseAgeExclude")
            .collect();
        assert_eq!(exclude.len(), 1, "must be one entry, got {overrides:?}");
        assert_eq!(exclude[0].1, "react,@myorg/*");
    }
}
