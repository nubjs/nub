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
//! `NUB_VERIFY_DEPS` env override); nub's default is `warn`. That is a
//! deliberate divergence from the vendored engine's `install` default, wired
//! through nub's OWN resolution so standalone aube's default is untouched
//! (fork-discipline). Under a pnpm-**11+** incumbent the key lives SOLELY in
//! `pnpm-workspace.yaml` — v11 dropped `.npmrc` support for it entirely — so
//! `resolve_policy` reads whichever home the detected incumbent major actually
//! uses (see its doc).

use std::collections::HashSet;
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
pub(crate) fn gate(cwd: &Path, compat_mode: bool) -> anyhow::Result<Option<i32>> {
    // `--node` / `NODE_COMPAT` is the zero-augmentation contract; a staleness
    // warning is nub being helpful, so compat mode skips it — and this keeps the
    // file-runner hot path free when `--node` is passed.
    if compat_mode {
        return Ok(None);
    }
    if DISABLED.load(Ordering::Relaxed) {
        return Ok(None);
    }
    // Cross-process re-entry guards — an ancestor already owns the decision:
    //  - a nub run/file/exec ancestor set our own inherited marker, OR
    //  - we're inside a running package script (`npm_lifecycle_event`, which the
    //    script child of a `nub run` carries, matching npm/pnpm).
    if std::env::var_os(CHECKED_MARKER).is_some()
        || std::env::var_os("npm_lifecycle_event").is_some()
    {
        return Ok(None);
    }
    // Once per process: latch BEFORE the (I/O-touching) resolution so a second
    // nested entrypoint is a cheap no-op.
    if CHECKED.swap(true, Ordering::Relaxed) {
        return Ok(None);
    }

    // No manifest here or above → nothing to verify (a bare `nub foo.ts` in a
    // non-project dir stays on the fast path).
    let Some(project) = nub_core::workspace::detect::detect_project(cwd) else {
        return Ok(None);
    };

    crate::phantom_scan::scan_and_warn(&project)?;

    let policy = resolve_policy(&project)?;
    if policy == Policy::Off {
        return Ok(None);
    }

    // Engine repair runs BEFORE the staleness walk: a tree built for another
    // Node major is stale in a way the walk cannot see (the wrong-ABI package
    // is present and version-satisfying), and the reinstall that fixes it also
    // settles whatever the walk would have reported.
    repair_engine_mismatch(&project, policy);

    let Some(reason) = needs_install_reason(&project) else {
        return Ok(None);
    };
    // Defense-in-depth brand pass: the reason strings are nub-native today, but
    // route them through the same rewrite all engine-adjacent output uses so no
    // future engine-sourced token could ever leak here.
    let reason = crate::pm_engine::present::rewrite(&reason);
    Ok(match policy {
        Policy::Warn => {
            eprintln!("nub: dependencies may be out of date ({reason}). Run `nub install`.");
            None
        }
        Policy::Error => {
            eprintln!("nub: dependencies are out of date ({reason}). Run `nub install`.");
            Some(1)
        }
        Policy::Off => None,
    })
}

/// Resolve the policy from nub's OWN surfaces: the config snapshot, then the
/// incumbent's real config home, else nub's `warn` default. Deliberately does
/// NOT call the engine's `resolve_verify_deps_before_run` — that carries the
/// engine's `install` default, and reusing it would either leak that default
/// under nub or force a fork-side edit.
///
/// The `NUB_VERIFY_DEPS` env override (and its pre-rename spelling) is NOT read
/// here: `cli::verify_deps_env_setting` parses both into the snapshot's
/// environment overlay, which the merge ranks above every file layer. A second
/// read below the snapshot branch would rank the variable BELOW a project
/// `nub.jsonc`, which is the precedence inversion this consolidation removes.
///
/// The incumbent's home is per-major (mirrors the pnpm-version-aware routing
/// `pm_engine::store_config_family` already established for scalar config, per
/// AGENTS.md's "Compat targets are PER-MAJOR-VERSION" position): a pnpm-**11+**
/// incumbent reads `verifyDepsBeforeRun` SOLELY from `pnpm-workspace.yaml` (v11
/// dropped `.npmrc` support for this key entirely, so a stale `.npmrc` leftover
/// from a pre-v11 migration must never shadow the yaml value); pnpm ≤10, the
/// unknown-pnpm-version default, and every non-pnpm incumbent keep reading the
/// neutral project `.npmrc` — unchanged from before this key was yaml-aware.
fn resolve_policy(project: &Project) -> anyhow::Result<Policy> {
    use crate::project_config::{ConfigKey, ConfigSourceKind};

    if let Some(config) = crate::project_config::effective_config()
        && let Some(source) = config.sources.get(&ConfigKey::VerifyDeps)
        && source.kind != ConfigSourceKind::Defaults
        && let Some(value) = config.values.verify_deps.as_ref()
    {
        return Ok(project_config_policy(value));
    }
    let workspace_root = project.workspace_root.as_deref().unwrap_or(&project.root);
    if let PnpmIncumbency::Major(major) = pnpm_incumbency(workspace_root)
        && major >= 11
    {
        return Ok(workspace_yaml_policy(workspace_root).unwrap_or(Policy::Warn));
    }
    if let Some(policy) = crate::pm_engine::unsupported_config::npmrc_scalar_value(
        &project.root,
        "verify-deps-before-run",
        true,
    )?
    .and_then(|value| parse_policy(&value))
    {
        return Ok(policy);
    }
    Ok(Policy::Warn)
}

/// Total, and infallible by construction: every layer that can reach the
/// snapshot — `nub.jsonc`, the `NUB_VERIFY_DEPS` overlay, the built-in defaults
/// — parses into [`crate::project_config::VerifyDeps`] first, and pnpm's
/// `install`/`prompt` are not representable there. Those two survive only on the
/// pnpm-mirroring surfaces, which resolve through [`parse_policy`] instead.
fn project_config_policy(value: &crate::project_config::VerifyDeps) -> Policy {
    use crate::project_config::VerifyDeps;
    match value {
        VerifyDeps::Enabled(false) => Policy::Off,
        VerifyDeps::Error => Policy::Error,
        VerifyDeps::Enabled(true) | VerifyDeps::Warn => Policy::Warn,
    }
}

/// pnpm incumbency + declared major at `workspace_root`, gating whether
/// `pnpm-workspace.yaml` may be read at all (the brand-boundary rule: a
/// pnpm-named file is never read unless pnpm is genuinely the incumbent —
/// AGENTS.md "pnpm-NAMED files ... NEVER read unless pnpm is the incumbent
/// PM"). Reuses the same declared-then-lockfile identity resolution
/// (`pm_engine::config_scope::role_of`) and the name-gated major extraction
/// `pm_engine::store_config_family::project_scalar_home` already use for this
/// exact per-major config-home question, rather than re-deriving it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PnpmIncumbency {
    /// Not pnpm (or unresolved) — `pnpm-workspace.yaml` must never be read.
    NotPnpm,
    /// pnpm is incumbent but no pin names a major — falls back to the
    /// `.npmrc` default (the dominant, safest target for an unpinned
    /// v9/v10-era project).
    UnknownVersion,
    Major(u64),
}

fn pnpm_incumbency(workspace_root: &Path) -> PnpmIncumbency {
    let declared = nub_core::pm::resolve::declared_pm_raw(workspace_root);
    let kind = aube_lockfile::detect_existing_lockfile_kind(workspace_root);
    let role =
        crate::pm_engine::config_scope::role_of(declared.as_ref().map(|(n, _)| n.as_str()), kind);
    if role != Some(crate::pm_engine::config_scope::Role::Pnpm) {
        return PnpmIncumbency::NotPnpm;
    }
    // Only trust the declared VERSION when the name is literally "pnpm" —
    // `role_of` maps an unrecognized declared tool through the lockfile
    // fallback too, and that tool's version string is not a pnpm major.
    let major = declared
        .as_ref()
        .and_then(|(name, v)| (name == "pnpm").then_some(v.as_deref()).flatten())
        .and_then(|v| crate::pm_engine::parse_major_minor(v).0);
    match major {
        Some(m) => PnpmIncumbency::Major(m),
        None => PnpmIncumbency::UnknownVersion,
    }
}

/// The `verifyDepsBeforeRun` value from `<workspace_root>/pnpm-workspace.yaml`.
/// `None` on a missing file, unparseable yaml, or an absent/unrecognized key —
/// callers fall through to the `warn` default, never to `.npmrc` (a real
/// pnpm-11 incumbent doesn't read `.npmrc` for this key either).
fn workspace_yaml_policy(workspace_root: &Path) -> Option<Policy> {
    let yaml = crate::pm_engine::use_nub::read_workspace_yaml(workspace_root)
        .ok()
        .flatten()?;
    yaml.get("verifyDepsBeforeRun").and_then(parse_policy_value)
}

/// Map a pnpm-mirroring surface's textual value (`.npmrc`, and the strings in
/// `pnpm-workspace.yaml`) to a policy. Unknown/empty → `None` (fall through to
/// the next source, ultimately the `warn` default).
///
/// `install`/`true` map to `warn`: nub deliberately does NOT auto-install before
/// a run — it will not reshape a tree another PM installed — so it warns
/// instead. `prompt` maps to `warn` too: nub has no interactive confirm step
/// here (real pnpm errors on `prompt` in a non-TTY context instead), and `warn`
/// is the safe, non-blocking approximation.
fn parse_policy(raw: &str) -> Option<Policy> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "off" | "false" | "0" | "no" | "none" | "skip" => Some(Policy::Off),
        "warn" | "true" | "install" | "prompt" => Some(Policy::Warn),
        "error" => Some(Policy::Error),
        _ => None,
    }
}

/// Map a `pnpm-workspace.yaml` scalar VALUE (already yaml-typed, not text) to a
/// policy. Real pnpm's `verifyDepsBeforeRun` type is `'install' | 'warn' |
/// 'error' | 'prompt' | false` — a literal yaml boolean for the off case, a
/// bare string otherwise; `true` isn't part of pnpm's own type but is accepted
/// here for symmetry with [`parse_policy`]'s textual `.npmrc`/env parsing.
fn parse_policy_value(value: &serde_json::Value) -> Option<Policy> {
    match value {
        serde_json::Value::Bool(false) => Some(Policy::Off),
        serde_json::Value::Bool(true) => Some(Policy::Warn),
        serde_json::Value::String(s) => parse_policy(s),
        _ => None,
    }
}

/// Reinstall a nub-installed tree that was built for a different Node engine.
///
/// The carve-out this makes to the no-auto-install posture documented on
/// [`parse_policy`] is deliberate and narrow. That posture says nub will not
/// reshape a tree ANOTHER package manager installed; the stamp is written only
/// by a successful nub install, so a mismatch here means nub is redoing nub's
/// own work. Every other kind of staleness still only warns.
///
/// The reinstall is a PLAIN `nub install` — the exact command the warning path
/// tells users to run — with default flags: the engine folds into the install's
/// own freshness/delta hash (`state.rs`, `finalize.rs`), so a present,
/// satisfying lockfile is not re-resolved (nub's no-churn write leaves it
/// untouched) while the native dependency is relinked from its per-engine store
/// build, or rebuilt when this engine was never built before. A frozen install
/// is deliberately NOT used — it forces a full lifecycle re-run even when the
/// store already holds the build, defeating the relink.
///
/// Best-effort throughout — a failed repair prints and lets the run continue,
/// because the script at hand may not touch the native dependency at all, and
/// aborting a run that works today would be a regression.
fn repair_engine_mismatch(project: &Project, policy: Policy) {
    // The install root, derived from the `project` the gate already walked —
    // NOT a fresh `install_engine::anchor()` call, which would re-run
    // `detect_project` (a whole manifest-tree walk, and a cache miss when the
    // run is invoked from a subdir) on every run of a nub project. Same value
    // `anchor()` computes (`workspace_root.unwrap_or(root)`), zero extra I/O.
    let anchor = project
        .workspace_root
        .clone()
        .unwrap_or_else(|| project.root.clone());
    // Anchored at the install root, matching how the installer resolved the
    // Node it built against (`pm_engine::apply_lifecycle_augmentation`). Only a
    // tree carrying nub's stamp reaches this closure (see `engine_repair_needed`),
    // so a foreign tree pays a single failed file read, never a Node resolution.
    // The spawn-free cached resolver runs first; its under-report (`None` on a
    // cold version cache) is unacceptable here — the first run after a switch is
    // the whole point — so it falls through to the full resolution the launch is
    // about to perform anyway.
    let resolve_current = || {
        let node = nub_core::node::discovery::discover_node_cached(&anchor)
            .or_else(|| nub_core::node::discovery::discover_node(&anchor).ok())?;
        Some(crate::install_engine::engine_name(
            &node.version.to_string(),
        ))
    };
    let Some((recorded, current)) = engine_repair_needed(policy, &anchor, resolve_current) else {
        return;
    };

    let (was, now) = crate::install_engine::describe(&recorded, &current);
    eprintln!(
        "nub: dependencies were installed for {was}, this run uses {now}. \
         Reinstalling — native addons are built per Node major."
    );
    let flags = crate::pm_engine::InstallFlags {
        dir: Some(anchor),
        ..Default::default()
    };
    // `-C <dir>` chdirs the process and never restores it (a PM verb exits right
    // after), but here the RUN still has to happen — from its own cwd, which the
    // caller may have set explicitly (a workspace member under `nub exec -r`).
    let restore = std::env::current_dir().ok();
    let outcome = crate::pm_engine::run_install(flags);
    if let Some(cwd) = restore {
        let _ = std::env::set_current_dir(cwd);
    }
    match outcome {
        Ok(0) => {}
        // A non-zero code already carried the engine's own diagnostic.
        Ok(_) => eprintln!("nub: reinstall failed; continuing with the tree as installed."),
        Err(e) => eprintln!("nub: reinstall failed: {e}"),
    }
}

/// `(engine the tree was built for, engine this run uses)` when they differ.
///
/// The stamp read comes FIRST and `current` is a closure, so the run path's cost
/// on a tree nub did not install is a single failed file read — no Node
/// resolution at all. Pure over the on-disk stamp, so the three cases that
/// matter — a real mismatch, a matching engine (never repair on the happy path),
/// and the `off` opt-out — are testable without switching Node.
fn engine_repair_needed(
    policy: Policy,
    anchor: &Path,
    current: impl FnOnce() -> Option<String>,
) -> Option<(String, String)> {
    if policy == Policy::Off {
        return None;
    }
    let recorded = crate::install_engine::recorded(anchor)?;
    let current = current()?;
    (recorded != current).then_some((recorded, current))
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
/// `--prod` install or on a dependency an override deliberately pins outside
/// its declared range (see [`override_pinned_names`]).
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
    //
    // The override set is materialized LAZILY — only a dependency that already
    // looks mismatched can be suppressed by it, and that is the rare case, so
    // the happy path never pays for the gather (which in a workspace reads the
    // root manifest).
    let mut pinned: Option<HashSet<String>> = None;
    let mut is_override_pinned = |name: &String| {
        pinned
            .get_or_insert_with(|| override_pinned_names(project))
            .contains(name)
    };
    for (name, spec) in &deps {
        let Some(installed) = resolved(name) else {
            return Some(format!("`{name}` is not installed"));
        };
        if let Some(reason) = version_mismatch(name, spec, &installed)
            && !is_override_pinned(name)
        {
            return Some(reason);
        }
    }
    for (name, spec) in &dev_deps {
        if let Some(installed) = resolved(name)
            && let Some(reason) = version_mismatch(name, spec, &installed)
            && !is_override_pinned(name)
        {
            return Some(reason);
        }
    }
    None
}

/// Package names an *effective* override pins, gathered from the manifest the
/// walk already parsed and — in a workspace, where the pins live at the root
/// while the deps are the member's — the workspace-root manifest.
///
/// A dependency pinned this way is installed OUTSIDE its declared range BY
/// DESIGN, so comparing the two produces a warning that is both wrong and
/// permanent: `nub install` honors the same pin, so the remedy the warning
/// prints can never clear it. Suppression is by NAME, wholesale — the installed
/// version is deliberately not re-checked against the override's own value,
/// because override specs have forms that are not plain semver ranges (`$name`
/// references, nested per-parent objects, `npm:` aliases) and mis-evaluating one
/// reintroduces exactly the false-warn class this removes. Only DIRECT deps are
/// ever version-checked, so keying on the target name is sufficient.
fn override_pinned_names(project: &Project) -> HashSet<String> {
    let mut names = HashSet::new();

    let workspace_manifest = project
        .workspace_root
        .as_deref()
        .filter(|ws| *ws != project.root.as_path())
        .and_then(|ws| std::fs::read_to_string(ws.join("package.json")).ok())
        .and_then(|text| {
            serde_json::from_str::<serde_json::Value>(nub_core::strip_utf8_bom(&text)).ok()
        });
    // `pnpm.overrides` is a pnpm-BRANDED field, so it is honored only when pnpm
    // is genuinely the incumbent (AGENTS.md's pnpm-named gate). The free presence
    // probe short-circuits first, so a project without the field never pays for
    // resolving incumbency.
    let anchor = project.workspace_root.as_deref().unwrap_or(&project.root);
    let mut pnpm_honored: Option<bool> = None;
    for manifest in std::iter::once(&project.manifest).chain(workspace_manifest.as_ref()) {
        for field in ["overrides", "resolutions"] {
            if let Some(obj) = manifest.get(field).and_then(|v| v.as_object()) {
                collect_override_names(obj, &mut names);
            }
        }
        if let Some(obj) = pnpm_overrides(manifest)
            && *pnpm_honored
                .get_or_insert_with(|| pnpm_incumbency(anchor) != PnpmIncumbency::NotPnpm)
        {
            collect_override_names(obj, &mut names);
        }
    }

    names
}

fn pnpm_overrides(
    manifest: &serde_json::Value,
) -> Option<&serde_json::Map<String, serde_json::Value>> {
    manifest.get("pnpm")?.get("overrides")?.as_object()
}

/// Every package name an override map targets, recursing into npm's nested
/// per-parent blocks (`{"parent": {"child": "1.0.0"}}`). Deliberately
/// OVER-collects — a name appearing anywhere in a selector, including as the
/// parent half of pnpm's `parent>child`, suppresses its own check too. Erring
/// wide can at worst miss a genuine staleness; erring narrow false-warns.
fn collect_override_names(
    obj: &serde_json::Map<String, serde_json::Value>,
    out: &mut HashSet<String>,
) {
    for (key, value) in obj {
        for segment in key.split('>') {
            if let Some(name) = selector_package_name(segment) {
                out.insert(name.to_string());
            }
        }
        if let Some(nested) = value.as_object() {
            collect_override_names(nested, out);
        }
    }
}

/// The package name in one override selector segment: `foo`, `foo@^1.2`,
/// `@scope/pkg`, `@scope/pkg@2`. `None` for a segment carrying no name — npm's
/// `"."` self-selector, or the tail of a range that itself contained a `>`.
fn selector_package_name(segment: &str) -> Option<&str> {
    let segment = segment.trim();
    // A scoped name's leading `@` belongs to the name; the version separator is
    // the NEXT `@`.
    let separator = match segment.strip_prefix('@') {
        Some(rest) => rest.find('@').map(|i| i + 1),
        None => segment.find('@'),
    };
    let name = separator.map_or(segment, |i| &segment[..i]);
    (!name.is_empty() && name != ".").then_some(name)
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
        // `install`/`prompt` are recognized but mapped to `warn`: nub does not
        // auto-install, nor does it have an interactive confirm step.
        assert_eq!(parse_policy("install"), Some(Policy::Warn));
        assert_eq!(parse_policy("prompt"), Some(Policy::Warn));
        // Case/whitespace-insensitive; unknown falls through to the default.
        assert_eq!(parse_policy("  ERROR "), Some(Policy::Error));
        assert_eq!(parse_policy("nonsense"), None);
        assert_eq!(parse_policy(""), None);
    }

    #[test]
    fn maps_the_typed_project_policy_without_reparsing() {
        use crate::project_config::VerifyDeps;
        assert_eq!(
            project_config_policy(&VerifyDeps::Enabled(false)),
            Policy::Off
        );
        assert_eq!(project_config_policy(&VerifyDeps::Error), Policy::Error);
        for value in [VerifyDeps::Enabled(true), VerifyDeps::Warn] {
            assert_eq!(project_config_policy(&value), Policy::Warn);
        }
    }

    #[test]
    fn parses_the_yaml_typed_verify_deps_before_run_values() {
        use serde_json::Value;
        // The off case is a literal yaml boolean, not a string.
        assert_eq!(parse_policy_value(&Value::Bool(false)), Some(Policy::Off));
        // `true` isn't part of pnpm's own type but is accepted for symmetry
        // with `parse_policy`'s textual `.npmrc`/env handling.
        assert_eq!(parse_policy_value(&Value::Bool(true)), Some(Policy::Warn));
        assert_eq!(
            parse_policy_value(&Value::String("error".to_string())),
            Some(Policy::Error)
        );
        assert_eq!(parse_policy_value(&Value::Null), None);
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

    fn project_at(root: &Path, manifest: serde_json::Value) -> Project {
        Project {
            root: root.to_path_buf(),
            workspace_root: None,
            manifest,
        }
    }

    /// Every neutral override dialect and selector shape a manifest can pin a
    /// direct dependency with must reach the suppression set — that set is the
    /// only thing standing between a deliberate pin and a permanent warning.
    #[test]
    fn override_targets_are_collected_from_every_neutral_source_and_selector_shape() {
        let dir = tempfile::tempdir().unwrap();
        let names = override_pinned_names(&project_at(
            dir.path(),
            serde_json::json!({
                "overrides": {
                    "typescript": "~5.8.3",
                    "esbuild@<0.25": "0.25.0",
                    "@scope/pkg@^1": "1.2.3",
                    "vite>rollup": "4.0.0",
                    "webpack": { ".": "5.0.0", "terser": "5.31.0" }
                },
                "resolutions": { "lodash": "4.17.21" }
            }),
        ));
        let mut got: Vec<&str> = names.iter().map(String::as_str).collect();
        got.sort_unstable();
        assert_eq!(
            got,
            [
                "@scope/pkg",
                "esbuild",
                "lodash",
                "rollup",
                "terser",
                "typescript",
                "vite",
                "webpack"
            ]
        );
    }

    /// `pnpm.overrides` is a pnpm-BRANDED field, so the suppression it grants is
    /// gated on pnpm genuinely being the incumbent — the same name-gate the rest
    /// of nub applies to pnpm-named config.
    #[test]
    fn pnpm_overrides_suppress_only_under_a_pnpm_incumbent() {
        let manifest = serde_json::json!({
            "devDependencies": { "typescript": "^5.9.2" },
            "pnpm": { "overrides": { "typescript": "~5.8.3" } }
        });
        let suppresses = |pnpm_lockfile: bool| {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("package.json"), "{}").unwrap();
            if pnpm_lockfile {
                std::fs::write(
                    dir.path().join("pnpm-lock.yaml"),
                    "lockfileVersion: '9.0'\n",
                )
                .unwrap();
            }
            override_pinned_names(&project_at(dir.path(), manifest.clone())).contains("typescript")
        };
        assert!(
            suppresses(true),
            "under a pnpm-lock.yaml incumbent the `pnpm.overrides` pin is real config and must suppress the check"
        );
        assert!(
            !suppresses(false),
            "with no pnpm incumbent the branded field must not be read at all"
        );
    }

    /// The end-to-end contract this suppression exists for: a dependency the
    /// manifest deliberately pins outside its declared range is not stale, while
    /// the identical tree without the pin still is.
    #[test]
    fn an_override_pinned_dependency_is_not_reported_stale() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("node_modules").join("typescript");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            r#"{"name":"typescript","version":"5.8.3"}"#,
        )
        .unwrap();

        let declared = serde_json::json!({ "devDependencies": { "typescript": "^5.9.2" } });
        assert_eq!(
            installed_tree_reason(&project_at(dir.path(), declared.clone())).as_deref(),
            Some("`typescript@5.8.3` does not satisfy `^5.9.2`"),
            "an unpinned dependency outside its declared range is genuinely stale"
        );

        let mut pinned = declared;
        pinned["overrides"] = serde_json::json!({ "typescript": "~5.8.3" });
        assert_eq!(
            installed_tree_reason(&project_at(dir.path(), pinned)).as_deref(),
            None,
            "the override pins typescript to the installed version, so `nub install` could never clear this warning"
        );
    }

    /// The whole engine-repair contract, at the decision boundary: a tree nub
    /// built for another Node major is repaired; a matching one never is (a
    /// false positive here would reinstall on EVERY run); a tree nub did not
    /// install is untouched; and `off` opts out of all of it.
    #[test]
    fn engine_repair_fires_only_on_a_nub_tree_built_for_another_engine() {
        let dir = tempfile::tempdir().unwrap();
        let anchor = dir.path();
        let stamp = |engine: &str| {
            std::fs::create_dir_all(anchor.join("node_modules")).unwrap();
            std::fs::write(anchor.join("node_modules/.nub-engine"), engine).unwrap();
        };
        let node26 = crate::install_engine::engine_name("26.5.0");
        let node22 = crate::install_engine::engine_name("22.15.0");
        let running_26 = || Some(node26.clone());
        let moved = || Some((node22.clone(), node26.clone()));

        // No stamp: another PM's tree (or a pre-stamp nub install). The current
        // engine is never even resolved.
        assert_eq!(
            engine_repair_needed(Policy::Warn, anchor, || panic!(
                "a tree nub did not install must not cost a Node resolution"
            )),
            None
        );

        stamp(&node22);
        assert_eq!(
            engine_repair_needed(Policy::Warn, anchor, running_26),
            moved()
        );
        assert_eq!(
            engine_repair_needed(Policy::Error, anchor, running_26),
            moved(),
            "`error` is a stricter policy, not an opt-out of the repair"
        );
        assert_eq!(engine_repair_needed(Policy::Off, anchor, running_26), None);

        stamp(&node26);
        assert_eq!(
            engine_repair_needed(Policy::Warn, anchor, running_26),
            None,
            "the engine that built the tree must never trigger a reinstall"
        );
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
