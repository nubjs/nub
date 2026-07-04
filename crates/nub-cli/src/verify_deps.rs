//! Pre-run dependency-freshness gate (issue #252).
//!
//! Before nub runs a script, a file, or a bin, it checks whether the project's
//! installed `node_modules` looks stale relative to package.json, so a
//! missing/stale tree surfaces as a clear nub warning instead of a raw
//! `husky: command not found`. A single marker-free walk of the manifest's
//! direct dependencies against the installed tree handles every incumbent (npm,
//! pnpm, yarn-classic, bun, and nub's own installs) — no lockfile parse, so it's
//! immune to cross-PM lockfile churn.
//!
//! Two invariants govern the design:
//!
//! - **Never false-warn.** Every uncertain case — yarn-PnP, an unrecognized
//!   layout, a spec that isn't a semver range, a prerelease install, no manifest
//!   — degrades to a SILENT skip. A missed warning is cheap; a wrong one erodes
//!   trust (the maintainer's explicit concern).
//! - **Fire at most once per user command.** A process latch stops nested
//!   in-process entrypoints (`exec` → bin launch → file runner) from re-checking,
//!   and the `npm_lifecycle_event` re-entry guard stops the inner `node`s a
//!   running script spawns from re-checking (matching npm/pnpm).
//!
//! Policy lives in the neutral `.npmrc` key `verify-deps-before-run` (with the
//! `NUB_VERIFY_DEPS_BEFORE_RUN` env override); nub's default is `warn`. That is a
//! deliberate divergence from the vendored engine's `install` default, wired
//! through nub's OWN resolution so standalone aube's default is untouched
//! (fork-discipline).

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use nub_core::workspace::detect::Project;

/// Explicit `--no-check` / `--no-install` opt-out, set once during arg dispatch.
static DISABLED: AtomicBool = AtomicBool::new(false);
/// Run the check at most once per process — nested in-process entrypoints
/// (`exec` → bin launch → file runner) must not re-check the same tree.
static CHECKED: AtomicBool = AtomicBool::new(false);

/// Internal, inherited sentinel: once a nub run/file/exec entrypoint in this
/// process TREE has decided the dep-check, descendants must not re-decide it.
/// Set on the env of the child nub spawns (see [`should_propagate_marker`]);
/// [`gate`] skips when it's present. This is what keeps a `nub <file>` / `nub
/// exec` target that itself spawns `node` (test runners, workers) from
/// re-entering nub through the PATH shim and repeating the warning. (`nub run`
/// is already covered by `npm_lifecycle_event`, which its script child carries.)
pub(crate) const CHECKED_MARKER: &str = "__NUB_DEPS_CHECKED";

/// Disable the gate for this process (the `--no-check`/`--no-install` flag).
pub(crate) fn disable() {
    DISABLED.store(true, Ordering::Relaxed);
}

/// Whether the child nub spawns should inherit [`CHECKED_MARKER`] so it skips
/// the check. True once this process has OWNED the decision — it ran the check
/// (`CHECKED`), was told to skip it (`--no-check`), or is itself a marked
/// descendant propagating the decision further down. Callers set the marker on
/// the spawned child's env at the file/exec launch sites.
pub(crate) fn should_propagate_marker() -> bool {
    CHECKED.load(Ordering::Relaxed)
        || DISABLED.load(Ordering::Relaxed)
        || std::env::var_os(CHECKED_MARKER).is_some()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Policy {
    Off,
    Warn,
    Error,
}

/// The gate. Call at each execution entrypoint with the invocation's cwd and its
/// compat bit. Returns `Some(exit_code)` when the run must ABORT (policy `error`
/// on a stale tree), or `None` to proceed — a fresh/uncertain tree, an opt-out,
/// or a non-fatal warning that has already been printed.
pub(crate) fn gate(cwd: &Path, compat_mode: bool) -> Option<i32> {
    // `--node` / `NODE_COMPAT` is the zero-augmentation contract; a staleness
    // warning is nub being helpful, so compat mode skips it — and this keeps the
    // file-runner hot path free when `--node` is passed.
    if compat_mode {
        return None;
    }
    if DISABLED.load(Ordering::Relaxed) {
        return None;
    }
    // Cross-process re-entry guards — an ancestor already owns the decision:
    //  - a nub run/file/exec ancestor set our own inherited marker, OR
    //  - we're inside a running package script (`npm_lifecycle_event`, which the
    //    script child of a `nub run` carries, matching npm/pnpm).
    if std::env::var_os(CHECKED_MARKER).is_some()
        || std::env::var_os("npm_lifecycle_event").is_some()
    {
        return None;
    }
    // Once per process: latch BEFORE the (I/O-touching) resolution so a second
    // nested entrypoint is a cheap no-op.
    if CHECKED.swap(true, Ordering::Relaxed) {
        return None;
    }

    // No manifest here or above → nothing to verify (a bare `nub foo.ts` in a
    // non-project dir stays on the fast path).
    let project = nub_core::workspace::detect::detect_project(cwd)?;

    // Static phantom-dependency nudge — warn-only, and independent of the
    // staleness policy below (a project can want phantom warnings without the
    // staleness gate, or vice-versa), sharing this entrypoint's compat/marker/
    // once-per-command guards. It never aborts the run.
    crate::phantom_scan::scan_and_warn(&project);

    let policy = resolve_policy(&project.root);
    if policy == Policy::Off {
        return None;
    }

    let reason = needs_install_reason(&project)?; // fresh / uncertain → proceed silently
    // Defense-in-depth brand pass: the reason strings are nub-native today, but
    // route them through the same rewrite all engine-adjacent output uses so no
    // future engine-sourced token could ever leak here.
    let reason = crate::pm_engine::present::rewrite(&reason);
    match policy {
        Policy::Warn => {
            eprintln!("nub: dependencies may be out of date ({reason}). Run `nub install`.");
            None
        }
        Policy::Error => {
            eprintln!("nub: dependencies are out of date ({reason}). Run `nub install`.");
            Some(1)
        }
        // Handled above; kept exhaustive.
        Policy::Off => None,
    }
}

/// Resolve the policy from nub's OWN surfaces: the `NUB_*` env override, then the
/// neutral `.npmrc` key, else nub's `warn` default. Deliberately does NOT call
/// the engine's `resolve_verify_deps_before_run` — that carries the engine's
/// `install` default, and reusing it would either leak that default under nub or
/// force a fork-side edit.
fn resolve_policy(project_root: &Path) -> Policy {
    if let Some(p) = std::env::var("NUB_VERIFY_DEPS_BEFORE_RUN")
        .ok()
        .and_then(|v| parse_policy(&v))
    {
        return p;
    }
    if let Some(p) = crate::pm_engine::unsupported_config::npmrc_scalar_value(
        project_root,
        "verify-deps-before-run",
        true,
    )
    .and_then(|v| parse_policy(&v))
    {
        return p;
    }
    Policy::Warn
}

/// Map a config/env value to a policy. Unknown/empty → `None` (fall through to
/// the next source, ultimately the `warn` default).
///
/// `install`/`true` map to `warn`: nub deliberately does NOT auto-install before
/// a run — it will not reshape a tree another PM installed — so it warns instead.
fn parse_policy(raw: &str) -> Option<Policy> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "off" | "false" | "0" | "no" | "none" | "skip" => Some(Policy::Off),
        "warn" | "true" | "install" => Some(Policy::Warn),
        "error" => Some(Policy::Error),
        _ => None,
    }
}

/// The staleness verdict for `project`: `Some(reason)` if the tree looks stale,
/// `None` if it's fresh (or freshness can't be determined with confidence).
///
/// A single marker-free walk covers every incumbent — npm, pnpm, yarn-classic,
/// bun, and nub's own installs alike. (An earlier design added a "Tier A" that
/// reused the engine's exact `check_needs_install` when nub was the installing
/// PM, but the engine resolves its state-marker path from install-time context
/// the run path doesn't set up, so it resolved the wrong path and silently
/// missed. The marker-free walk handles a nub-installed tree correctly on its
/// own — verified end-to-end — so it's the uniform path, which also keeps the
/// vendored engine untouched.)
fn needs_install_reason(project: &Project) -> Option<String> {
    // Yarn PnP has no `node_modules` — freshness would mean reconciling
    // `.pnp.cjs`/`.pnp.data.json` against the lockfile, which this walk does not
    // do. Degrade to a SILENT skip rather than false-warn "nothing installed".
    if nub_core::pnp::detect(&project.root).is_some() {
        return None;
    }
    installed_tree_reason(project)
}

/// One installed package's freshness-relevant fields.
struct InstalledPkg {
    /// `version` from the on-disk `package.json`, if it parsed.
    version: Option<String>,
}

/// The marker-free walk. Compares the manifest's DIRECT dependencies against
/// what's resolvable in the `node_modules` chain. Catches the fresh-clone case
/// (nothing installed), a missing production dependency, and a version that no
/// longer satisfies its declared range — without parsing any lockfile (so it's
/// immune to cross-PM lockfile churn) and without ever false-warning on a
/// `--prod` install.
fn installed_tree_reason(project: &Project) -> Option<String> {
    let deps = deps_map(&project.manifest, "dependencies");
    let dev_deps = deps_map(&project.manifest, "devDependencies");
    if deps.is_empty() && dev_deps.is_empty() {
        return None; // nothing declared → nothing to verify
    }

    let resolved = |name: &str| resolve_installed_manifest(&project.root, name);

    // "Nothing installed at all" — the fresh-clone case the issue reports. If
    // NONE of the declared direct deps resolve in the node_modules chain, an
    // install has not happened. This fires even for devDependency-only projects
    // (husky, tsc, …), which a per-set walk would otherwise miss.
    let any_present = deps
        .iter()
        .chain(dev_deps.iter())
        .any(|(name, _)| resolved(name).is_some());
    if !any_present {
        return Some("dependencies are not installed".to_string());
    }

    // An install HAS happened (something resolved). Require every production
    // dependency present + version-satisfying. For devDependencies, only flag a
    // present-but-mismatched version — a devDep that is ABSENT here is tolerated,
    // because a `--prod` / `--omit=dev` install legitimately omits them and
    // warning would be a false positive.
    for (name, spec) in &deps {
        match resolved(name) {
            None => return Some(format!("`{name}` is not installed")),
            Some(installed) => {
                if let Some(reason) = version_mismatch(name, spec, &installed) {
                    return Some(reason);
                }
            }
        }
    }
    for (name, spec) in &dev_deps {
        if let Some(installed) = resolved(name)
            && let Some(reason) = version_mismatch(name, spec, &installed)
        {
            return Some(reason);
        }
    }
    None
}

/// Direct-dependency `(name, spec)` pairs from one manifest map. Non-string
/// values are skipped — a malformed manifest is not something to guess about.
fn deps_map(manifest: &serde_json::Value, key: &str) -> Vec<(String, String)> {
    manifest
        .get(key)
        .and_then(|v| v.as_object())
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// Resolve `<name>`'s INSTALLED package.json by walking the `node_modules` chain
/// up from `start` — Node's own resolution — so a workspace member whose deps are
/// hoisted to the root still resolves, and a pnpm symlink into `.pnpm/<name>@<v>`
/// is followed transparently. `None` only when the package is absent from the
/// whole chain; a present-but-unparseable manifest resolves to a version of
/// `None` (its version check is skipped, never treated as missing).
fn resolve_installed_manifest(start: &Path, name: &str) -> Option<InstalledPkg> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        let candidate = d.join("node_modules").join(name).join("package.json");
        if candidate.is_file() {
            let version = std::fs::read_to_string(&candidate)
                .ok()
                .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                .and_then(|j| j.get("version").and_then(|v| v.as_str()).map(String::from));
            return Some(InstalledPkg { version });
        }
        dir = d.parent();
    }
    None
}

/// Flag a present dependency whose installed version doesn't satisfy its declared
/// range. Everything uncertain resolves to `None` (no warn):
///
/// - a spec that isn't a semver range (`workspace:`, `file:`, `link:`, `git:`, a
///   URL, an `npm:` alias, a dist-tag) fails to parse as a range;
/// - a prerelease install is intentional, and npm range semantics would
///   spuriously reject it;
/// - an unreadable installed version.
fn version_mismatch(name: &str, spec: &str, installed: &InstalledPkg) -> Option<String> {
    let range = node_semver::Range::parse(spec).ok()?;
    let installed_ver = installed.version.as_deref()?;
    let version = node_semver::Version::parse(installed_ver).ok()?;
    if version.is_prerelease() {
        return None;
    }
    if !version.satisfies(&range) {
        return Some(format!(
            "`{name}@{installed_ver}` does not satisfy `{spec}`"
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_canonical_policy_values() {
        assert_eq!(parse_policy("warn"), Some(Policy::Warn));
        assert_eq!(parse_policy("error"), Some(Policy::Error));
        assert_eq!(parse_policy("off"), Some(Policy::Off));
        assert_eq!(parse_policy("false"), Some(Policy::Off));
        // `install` is recognized but mapped to `warn`: nub does not auto-install.
        assert_eq!(parse_policy("install"), Some(Policy::Warn));
        // Case/whitespace-insensitive; unknown falls through to the default.
        assert_eq!(parse_policy("  ERROR "), Some(Policy::Error));
        assert_eq!(parse_policy("nonsense"), None);
        assert_eq!(parse_policy(""), None);
    }

    #[test]
    fn version_mismatch_only_flags_a_clear_range_violation() {
        let at = |v: &str| InstalledPkg {
            version: Some(v.to_string()),
        };
        // Installed satisfies the declared range → no warning.
        assert!(version_mismatch("foo", "^1.0.0", &at("1.4.2")).is_none());
        // Installed violates a bumped range → warning (the manifest-ahead case).
        assert!(version_mismatch("foo", "^2.0.0", &at("1.4.2")).is_some());
        // A non-range protocol spec is never a version finding (presence-only).
        assert!(version_mismatch("foo", "workspace:*", &at("1.0.0")).is_none());
        assert!(version_mismatch("foo", "npm:bar@^1", &at("1.0.0")).is_none());
        // A prerelease install is intentional — never flagged.
        assert!(version_mismatch("foo", "^2.0.0", &at("1.0.0-beta.1")).is_none());
        // No readable installed version → skip.
        assert!(version_mismatch("foo", "^2.0.0", &InstalledPkg { version: None }).is_none());
    }

    #[test]
    fn deps_map_skips_non_string_values() {
        let manifest = serde_json::json!({
            "dependencies": { "a": "^1.0.0", "b": { "nope": true }, "c": "2.0.0" }
        });
        let mut got = deps_map(&manifest, "dependencies");
        got.sort();
        assert_eq!(
            got,
            vec![
                ("a".to_string(), "^1.0.0".to_string()),
                ("c".to_string(), "2.0.0".to_string()),
            ]
        );
        assert!(deps_map(&manifest, "devDependencies").is_empty());
    }
}
