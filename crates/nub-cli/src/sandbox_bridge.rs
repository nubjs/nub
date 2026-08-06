//! The nub.jsonc → sandbox-engine bridge (nub-cli side). Maps a parsed
//! [`SandboxSetting`] to a compiled `nub_sandbox` policy, classifies the
//! `--sandbox` flag string into a shape, and selects the effective setting across
//! config positions by whole-value OVERRIDE (most-specific-present wins — NOT
//! per-axis inheritance). The engine (`nub-sandbox`) stays Boundary-B pure: it
//! only ever sees an already-parsed `Value` + `CompileCtx`, never a
//! `SandboxSetting`, a nub.jsonc, or a config position.
//!
//! Scope (P6, flag-only — DEC-2 = B): the `--sandbox <preset|file>` flag path is
//! WIRED (`setting_from_flag` + `resolve`, driven from `run_sandboxed`).
//! [`select_effective`] and the [`SandboxSource::ProjectFile`] source are built
//! and unit-tested but NOT wired to any auto-activation — a project `sandbox`
//! field never sandboxes a run.
//!
//! ⛔ THAT BOUNDARY HAS NO DEDICATED REGRESSION GUARD, and this comment claimed one until
//! 2026-08-06. It named `project_config_sandbox_inert.rs`, deleted in `c5651408f4` when the
//! per-package opt-out collapsed into the single global `install.buildJail` switch. The same
//! stale name also sat in `sandbox-conformance.yml`, where it failed the entire Linux job with
//! `error: no test target named project_config_sandbox_inert in nub-cli` — cargo never ran a
//! test at all, so the job was red on an argument rather than on behaviour.
//!
//! Whether the property still wants a guard is OPEN and deliberately not answered here: the
//! `--sandbox` flag path is wired and covered separately, but nothing today asserts that a
//! project-file `sandbox` field stays inert.

use crate::project_config::{SandboxSetting, classify_sandbox_string};
use anyhow::{Context, anyhow};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One resolved, ready-to-enforce sandbox request. `None` from [`resolve`] means
/// "no sandbox" (the `Disabled` shape) — the caller spawns the program plain.
pub(crate) struct ResolvedSandbox {
    pub policy: nub_sandbox::SandboxPolicy,
    pub warnings: Vec<nub_sandbox::CompileWarning>,
}

/// Where a setting was authored — drives `policy_files` self-exclusion (P5) and
/// relative file-ref anchoring. `Flag` (argv) is the only source WIRED in P6;
/// `ProjectFile` is built for a later nub.jsonc-activation frontend (DEC-2 = B
/// ships no auto-sourcing), hence never constructed outside tests today.
pub(crate) enum SandboxSource {
    /// Supplied inline on the CLI flag (`--sandbox true|build-jail|./p.jsonc`).
    Flag,
    /// Sourced from a nub.jsonc field. `path` = the nub.jsonc file (self-excluded
    /// from the sandbox); `root` = its dir (anchors a relative file-ref).
    #[allow(dead_code)]
    // built for a later activation frontend; only tests construct it in P6.
    ProjectFile { path: PathBuf, root: PathBuf },
}

/// The authored scope a sandbox setting came from — the seam that maps a scope to
/// its capability set. All three parsed positions are root-authored (`approved`)
/// today; the enum exists so a future dependency-controlled scope slots in as one
/// variant + one match arm rather than a special-case at the call site.
#[allow(dead_code)] // the selector/caps seam is built-not-activated in P6 (flag-only); tests exercise it.
pub(crate) enum SandboxScope {
    /// Top-level `sandbox` — root-authored.
    ProjectDefault,
    /// `dlx.sandbox` — root-authored.
    Dlx,
    // DEFERRED (not parsed yet — §7c): `ScriptsMeta` (root-authored → approved),
    // `DependenciesMeta` (dependency-authored → dependency() — no `$(…)`, no broker).
}

/// Map an authored scope to its capability set. Root-authored config gets the full
/// set (env `$(…)` substitution + credential brokering); a dependency-controlled
/// scope would get [`ScopeCapabilities::dependency`] — not reachable until such a
/// scope is parsed. Filesystem `$(…)` substitution is unconditional regardless.
#[allow(dead_code)] // built-not-activated in P6; exercised by unit tests only.
pub(crate) fn caps_for_scope(scope: SandboxScope) -> nub_sandbox::ScopeCapabilities {
    match scope {
        SandboxScope::ProjectDefault | SandboxScope::Dlx => {
            nub_sandbox::ScopeCapabilities::approved()
        }
    }
}

/// Classify a `--sandbox` flag string into a shape. `true`/`false` are the bool
/// shapes; every other string routes through the SAME classifier a nub.jsonc value
/// uses, so a bare identifier is a preset and a path-like string a file-ref.
pub(crate) fn setting_from_flag(s: &str) -> SandboxSetting {
    match s {
        "true" => SandboxSetting::Enabled,
        "false" => SandboxSetting::Disabled,
        other => classify_sandbox_string(other),
    }
}

/// Select the effective setting for an execution context by most-specific-present
/// OVERRIDE: the position's own setting wins WHOLLY when present, else the project
/// default, else `None` (no sandbox). This is override, NOT inheritance — no axis
/// is ever merged across positions (cross-list composition is expressible only via
/// an explicit `...:#/pointer` WITHIN one document, the engine's concern).
///
/// Built-not-activated in P6: no execution context sources a project field yet.
#[allow(dead_code)]
pub(crate) fn select_effective<'a>(
    project_default: Option<&'a SandboxSetting>,
    position_override: Option<&'a SandboxSetting>,
) -> Option<&'a SandboxSetting> {
    position_override.or(project_default)
}

/// Map a [`SandboxSetting`] to a compiled policy. `Ok(None)` is the `Disabled`
/// shape (spawn plain). `source` drives `policy_files` self-exclusion and relative
/// file-ref anchoring; `caps` is the scope's capability set.
pub(crate) fn resolve(
    setting: &SandboxSetting,
    source: &SandboxSource,
    cwd: &Path,
    homes: nub_sandbox::Homes,
    ambient_env: BTreeMap<String, String>,
    caps: nub_sandbox::ScopeCapabilities,
) -> anyhow::Result<Option<ResolvedSandbox>> {
    use serde_json::Value;

    // `block` is the surface handed to the compiler; `document` is the reuse root a
    // `...:#/pointer` resolves against (only a file-ref carries reuse — a bool/preset
    // expand to a fixed object with no pointer tokens, so `Null` is correct there).
    let (block, document, policy_files): (Value, Value, Vec<PathBuf>) = match setting {
        SandboxSetting::Disabled => return Ok(None),
        SandboxSetting::Enabled => (Value::Bool(true), Value::Null, config_source_file(source)?),
        SandboxSetting::Preset(name) => (
            Value::String(name.clone()),
            Value::Null,
            config_source_file(source)?,
        ),
        SandboxSetting::FileRef(p) => {
            let path = anchor(source, cwd, p);
            let text = std::fs::read_to_string(&path).with_context(|| {
                format!("reading external sandbox config file {}", path.display())
            })?;
            let value = crate::jsonc::parse_to_value(&text)
                .map_err(|e| anyhow!("parsing sandbox policy {}: {e}", path.display()))?
                .unwrap_or(Value::Null);
            // Accept either a bare surface block or a `{ "sandbox": … }` wrapper. The
            // WHOLE document (not the extracted block) is the reuse root, so a
            // `#/shared/*` sibling of the `sandbox` wrapper resolves.
            let block = value
                .get("sandbox")
                .cloned()
                .unwrap_or_else(|| value.clone());
            // Self-exclusion covers the WHOLE source chain, not just the document the rules
            // were read from: the config that NAMED this file confines the run just as much,
            // so a sandboxed process that can rewrite `sandbox: "./policy.jsonc"` to point at
            // a permissive file it also writes escapes on the next run.
            let mut files = config_source_file(source)?;
            files.push(std::fs::canonicalize(&path).with_context(|| {
                format!("canonicalizing sandbox policy path {}", path.display())
            })?);
            (block, value, files)
        }
    };

    let ctx = nub_sandbox::CompileCtx::new(homes, cwd.to_path_buf(), caps, ambient_env)
        .with_policy_files(policy_files)
        .with_document(document);
    let (policy, warnings) = nub_sandbox::compile_with_warnings(&block, &ctx)
        .map_err(|e| anyhow!("sandbox policy did not compile: {e}"))?;
    Ok(Some(ResolvedSandbox { policy, warnings }))
}

/// The self-exclusion path for the config that AUTHORED the setting, for every shape.
/// `Flag` has nothing to hide (the setting came from argv); a `ProjectFile` source
/// self-excludes the nub.jsonc itself. A file-ref appends the referenced policy on top.
fn config_source_file(source: &SandboxSource) -> anyhow::Result<Vec<PathBuf>> {
    match source {
        SandboxSource::Flag => Ok(Vec::new()),
        SandboxSource::ProjectFile { path, .. } => {
            let canon = std::fs::canonicalize(path)
                .with_context(|| format!("canonicalizing nub.jsonc path {}", path.display()))?;
            Ok(vec![canon])
        }
    }
}

/// Anchor a relative file-ref: a `--sandbox ./p.jsonc` flag resolves against the
/// process cwd; a nub.jsonc `sandbox: "./p.jsonc"` value resolves against the
/// nub.jsonc dir (the `ConfigSource` invariant — a relative value never falls back
/// to the ambient cwd). An absolute path is used verbatim.
fn anchor(source: &SandboxSource, cwd: &Path, p: &str) -> PathBuf {
    let raw = Path::new(p);
    if raw.is_absolute() {
        return raw.to_path_buf();
    }
    match source {
        SandboxSource::Flag => cwd.join(raw),
        SandboxSource::ProjectFile { root, .. } => root.join(raw),
    }
}

/// Spawn `program` with NO sandbox — the `Disabled` shape / no-setting path. Plain
/// inherited stdio, no compile and no backend apply (NOT `unjailed`-through-the-
/// engine, which would add enforcement overhead and diverge from a no-flag run).
/// Terminating signals are forwarded to the child (via the same helper the no-flag
/// run path uses) so `docker stop` / Ctrl-C / systemd reach the workload rather
/// than orphaning it — the divergence from a no-flag run stays minimal.
pub(crate) fn spawn_plain(program: &str, args: &[String], cwd: &Path) -> anyhow::Result<i32> {
    let mut command = std::process::Command::new(program);
    command.args(args).current_dir(cwd);
    let status = nub_core::node::spawn::status_forwarding_signals(&mut command)
        .with_context(|| format!("spawning `{program}` (sandbox disabled)"))?;
    Ok(nub_core::node::spawn::exit_code_from_status(&status))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_string_classifies_each_shape() {
        assert_eq!(setting_from_flag("true"), SandboxSetting::Enabled);
        assert_eq!(setting_from_flag("false"), SandboxSetting::Disabled);
        assert_eq!(
            setting_from_flag("build-jail"),
            SandboxSetting::Preset("build-jail".into())
        );
        assert_eq!(
            setting_from_flag("./p.jsonc"),
            SandboxSetting::FileRef("./p.jsonc".into())
        );
        // A non-bool string must classify identically to a nub.jsonc value.
        assert_eq!(
            setting_from_flag("/abs/p.json"),
            classify_sandbox_string("/abs/p.json")
        );
    }

    #[test]
    fn select_effective_is_whole_value_override_not_merge() {
        let default_preset = SandboxSetting::Enabled;
        let position_preset = SandboxSetting::Preset("build-jail".into());

        // Position present → wins WHOLLY (an `Enabled` default is replaced entirely,
        // no axis merge).
        assert_eq!(
            select_effective(Some(&default_preset), Some(&position_preset)),
            Some(&position_preset)
        );
        // Only the default present → the default applies.
        assert_eq!(
            select_effective(Some(&default_preset), None),
            Some(&default_preset)
        );
        // Only the position present → the position applies.
        assert_eq!(
            select_effective(None, Some(&position_preset)),
            Some(&position_preset)
        );
        // Neither → no sandbox.
        assert_eq!(select_effective(None, None), None);
    }

    #[test]
    fn every_root_scope_is_approved() {
        for scope in [SandboxScope::ProjectDefault, SandboxScope::Dlx] {
            let caps = caps_for_scope(scope);
            assert!(caps.env_substitution);
            assert!(caps.credential_broker);
        }
    }

    fn test_ctx_inputs(cwd: &Path) -> (nub_sandbox::Homes, BTreeMap<String, String>) {
        let homes = nub_sandbox::Homes {
            home: cwd.to_path_buf(),
            tmp: std::env::temp_dir(),
            cache: cwd.join(".cache"),
            project: cwd.to_path_buf(),
        };
        (homes, BTreeMap::new())
    }

    #[test]
    fn disabled_resolves_to_no_policy() {
        let cwd = std::env::current_dir().unwrap();
        let (homes, env) = test_ctx_inputs(&cwd);
        let resolved = resolve(
            &SandboxSetting::Disabled,
            &SandboxSource::Flag,
            &cwd,
            homes,
            env,
            nub_sandbox::ScopeCapabilities::approved(),
        )
        .unwrap();
        assert!(
            resolved.is_none(),
            "Disabled must resolve to no policy (plain spawn)"
        );
    }

    #[test]
    fn enabled_and_preset_compile_a_policy_from_the_flag() {
        let cwd = std::env::current_dir().unwrap();
        for setting in [
            SandboxSetting::Enabled,
            SandboxSetting::Preset("build-jail".into()),
        ] {
            let (homes, env) = test_ctx_inputs(&cwd);
            let resolved = resolve(
                &setting,
                &SandboxSource::Flag,
                &cwd,
                homes,
                env,
                nub_sandbox::ScopeCapabilities::approved(),
            )
            .unwrap_or_else(|e| panic!("{setting:?} must compile: {e}"));
            assert!(resolved.is_some(), "{setting:?} must produce a policy");
        }
    }

    #[test]
    fn file_ref_reads_the_referenced_document_and_compiles() {
        let dir = tempfile::tempdir().unwrap();
        // A wrapped `{ shared, sandbox }` document whose `sandbox` block reuses a
        // sibling via `...:#/pointer` — proving `document` is the whole file.
        std::fs::write(
            dir.path().join("policy.jsonc"),
            r#"{ "shared": { "fs": ["./src"] }, "sandbox": { "fs": ["...:#/shared/fs"] } }"#,
        )
        .unwrap();
        let (homes, env) = test_ctx_inputs(dir.path());
        let resolved = resolve(
            &SandboxSetting::FileRef("./policy.jsonc".into()),
            &SandboxSource::Flag,
            dir.path(),
            homes,
            env,
            nub_sandbox::ScopeCapabilities::approved(),
        )
        .expect("file-ref with reuse must compile")
        .expect("file-ref must produce a policy");
        // The reuse resolved against the whole document; a non-empty compile proves it.
        let _ = resolved.policy;
    }

    #[test]
    fn a_file_ref_self_excludes_both_the_referencing_config_and_the_referenced_policy() {
        // The confining policy is the WHOLE chain. Denying only the referenced file leaves
        // the nub.jsonc writable, and rewriting its `sandbox` value to name a permissive
        // file the process also writes escapes the sandbox on the next run.
        use nub_sandbox::matcher::PathMatcher;
        use nub_sandbox::policy::Effect;

        let dir = tempfile::tempdir().unwrap();
        let nub_jsonc = dir.path().join("nub.jsonc");
        let policy = dir.path().join("policy.jsonc");
        std::fs::write(&nub_jsonc, r#"{ "sandbox": "./policy.jsonc" }"#).unwrap();
        std::fs::write(&policy, r#"{ "fs": ["."] }"#).unwrap();

        let (homes, env) = test_ctx_inputs(dir.path());
        let resolved = resolve(
            &SandboxSetting::FileRef("./policy.jsonc".into()),
            &SandboxSource::ProjectFile {
                path: nub_jsonc.clone(),
                root: dir.path().to_path_buf(),
            },
            dir.path(),
            homes,
            env,
            nub_sandbox::ScopeCapabilities::approved(),
        )
        .expect("file-ref from a project config must compile")
        .expect("file-ref must produce a policy");

        let m = PathMatcher::new(&resolved.policy.fs.rules);
        for f in [&nub_jsonc, &policy] {
            assert_eq!(
                m.decide(f).effect,
                Effect::Deny,
                "{} must be denied under the `fs: [\".\"]` grant it authorizes",
                f.display()
            );
        }
        assert_eq!(
            m.decide(&dir.path().join("src/app.ts")).effect,
            Effect::Allow,
            "the self-exclusion is exact-path — a sibling under the same grant stays usable"
        );
    }
}
