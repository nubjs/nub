//! Bun-compatible pluggable security scanner.
//!
//! Loads and runs a [Bun Security Scanner](https://bun.sh/docs/pm/security-scanner-api)
//! module under `node` with the Bun runtime APIs the public
//! scanner ecosystem actually uses (`Bun.semver.satisfies`,
//! `Bun.env`, `Bun.file`, `import Bun from 'bun'`) shimmed in.
//! Drop-in compatible with the
//! [official template](https://github.com/oven-sh/security-scanner-template)
//! and SocketDev's `@socketsecurity/bun-security-scanner`.
//!
//! ## Architecture
//!
//! Each invocation drops three small `.mjs` files into a fresh
//! temp dir:
//!
//! - `bun_shim.mjs` — runtime values for `Bun.env` / `Bun.file` /
//!   `Bun.semver.satisfies` / `Bun.write`. `Bun.semver` tries to
//!   delegate to the project's `semver` npm package (near-universal
//!   transitive dep) and falls back to a naive comparator with a
//!   one-time stderr warning.
//! - `loader_hook.mjs` — Node module-loader hook registered via
//!   `module.register()`. Intercepts the `'bun'` specifier so
//!   `import Bun from 'bun'` in the scanner resolves to the shim.
//! - `runner.mjs` — the bridge entry. Installs the hook, eagerly
//!   loads the shim (so `globalThis.Bun` is populated for scanners
//!   that don't import explicitly), dynamic-imports the user's
//!   scanner module, reads `{packages}` on stdin, calls
//!   `scanner.scan()`, writes the `Advisory[]` on stdout.
//!
//! aube spawns:
//!
//! ```text
//! node --experimental-strip-types <runner.mjs>
//!   AUBE_SCANNER_SPEC=<spec>
//!   AUBE_BRIDGE_DIR=<temp>
//! ```
//!
//! `--experimental-strip-types` lets node load `.ts` scanner
//! entrypoints directly (Socket's package, for example, points
//! `exports` at `./src/index.ts` and ships no compile step).
//! Requires Node 22.6+.
//!
//! ## Fired from
//!
//! Post-resolve from `install::run` — once the resolver returns a
//! `LockfileGraph`, [`resolved_packages_for_scanner`] extracts the
//! full set of `(name, resolved_version)` pairs (root direct deps
//! plus every transitive) and [`run_scanner`] hands them to the
//! scanner before linking starts. Matches Bun, which also fires
//! the scanner against the resolved graph on both `bun add` and
//! `bun install`.
//!
//! `aube add` doesn't have its own scanner hook — it mutates
//! `package.json` and then runs the same install pipeline, so the
//! same post-resolve gate covers it. A `fatal` advisory on
//! `aube add` exits the install non-zero with `package.json`
//! still mutated (matches Bun); revert via `git checkout` if you
//! don't want to keep the change.
//!
//! Pre-resolve, the manifest-level OSV / downloads gates in
//! `add_supply_chain.rs` still run on the typed add args — those
//! are aube-only checks that don't need resolved versions.
//!
//! ## Contract differences vs. Bun
//!
//! - **`node` must be on PATH** (Bun runs the scanner in-process).
//!   `node` ≥ 22.6 for TypeScript entrypoints; ≥ 20 for compiled
//!   JS-only scanners.
//! - **Bun-runtime APIs outside the shim's scope** aren't
//!   available — calls like `Bun.spawn`, `Bun.password`, or
//!   `Bun.serve` will throw at runtime and the scanner subprocess
//!   exits non-zero. The bridge surfaces this as
//!   `ERR_AUBE_SECURITY_SCANNER_FAILED` and the install fails
//!   closed; a configured scanner that can't run is treated as a
//!   refusal. Set `securityScanner = ""` to disable the integration
//!   when bootstrapping or recovering from a broken scanner.

use aube_codes::errors::{ERR_AUBE_SECURITY_SCANNER_FAILED, ERR_AUBE_SECURITY_SCANNER_FATAL};
use aube_codes::warnings::WARN_AUBE_SECURITY_SCANNER_FINDING;
use miette::miette;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

/// Hard upper bound on how long the scanner may run. 30s mirrors
/// what npm and Bun use for similar install-time hooks.
const SCANNER_TIMEOUT: Duration = Duration::from_secs(30);

/// Bridge JS payloads, embedded at compile time so the install
/// pipeline has no on-disk runtime dependency on these files
/// living next to the binary.
const BUN_SHIM_SOURCE: &str = include_str!("security_scanner_js/bun_shim.mjs");
const LOADER_HOOK_SOURCE: &str = include_str!("security_scanner_js/loader_hook.mjs");
const RUNNER_SOURCE: &str = include_str!("security_scanner_js/runner.mjs");

/// One package the scanner will see. Field names match
/// `Bun.Security.Package`: `name` is the registry name (alias
/// entries report the real registry name, not the alias),
/// `version` is the *resolved* version string the resolver
/// picked (e.g. `"4.17.21"`, not `"^4.17.21"`).
#[derive(Debug, Clone, Serialize)]
pub struct ScannerPackage {
    pub name: String,
    pub version: String,
}

/// Collect the resolved registry packages from a `LockfileGraph`
/// into the scanner's input format. Mirrors Bun's contract: the
/// scanner sees the *full installation graph* (root manifest's
/// direct deps + every transitive resolved by the resolver) with
/// **resolved versions** (`"4.17.21"`) rather than user-typed
/// ranges (`"^4.17.21"`).
///
/// Skips entries with a `local_source` — those are `file:` /
/// `link:` / workspace links that resolve outside the public
/// registry. The scanner has no advisory data for them.
///
/// `registry_name()` is used so npm-aliased entries
/// (`{ "my-alias": "npm:real-pkg@^4" }`) report under the real
/// registry name `real-pkg`, not the alias.
///
/// Order is sorted-by-key (the `BTreeMap` iteration), deduped by
/// `(name, version)` so the scanner sees one entry per distinct
/// resolved package even when peer-context produced multiple
/// `dep_path` nodes that share the same `(name, version)` tuple.
pub fn resolved_packages_for_scanner(graph: &aube_lockfile::LockfileGraph) -> Vec<ScannerPackage> {
    let mut out: Vec<ScannerPackage> = graph
        .packages
        .values()
        .filter(|pkg| pkg.local_source.is_none())
        .map(|pkg| ScannerPackage {
            name: pkg.registry_name().to_string(),
            version: pkg.version.clone(),
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.version.cmp(&b.version)));
    out.dedup_by(|a, b| a.name == b.name && a.version == b.version);
    out
}

#[derive(Debug, Serialize)]
struct ScannerRequest<'a> {
    packages: &'a [ScannerPackage],
}

#[derive(Debug, Deserialize)]
struct Advisory {
    package: String,
    level: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    url: Option<String>,
}

/// Outcome categories used when classifying scanner advisories.
/// `Fatal` blocks the install; `Warn` emits a warning and
/// continues; `Other` is logged at debug level and otherwise
/// ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Severity {
    Fatal,
    Warn,
    Other,
}

fn classify(level: &str) -> Severity {
    match level.to_ascii_lowercase().as_str() {
        "fatal" => Severity::Fatal,
        "warn" | "warning" => Severity::Warn,
        _ => Severity::Other,
    }
}

/// Run `scanner_spec` against `packages`. Empty `scanner_spec` or
/// empty `packages` short-circuits to `Ok(())` without spawning
/// `node`.
pub async fn run_scanner(
    scanner_spec: &str,
    cwd: &Path,
    packages: &[ScannerPackage],
) -> miette::Result<()> {
    if scanner_spec.is_empty() || packages.is_empty() {
        return Ok(());
    }
    let advisories = match invoke(scanner_spec, cwd, packages).await {
        Ok(a) => a,
        Err(e) => {
            // Fail closed: a configured `securityScanner` that
            // can't run (node missing, module unresolvable,
            // scanner panicked, timeout, garbage output) is
            // treated as a refusal rather than a free pass.
            // Silently bypassing on failure would undermine the
            // exact intent of opting into the scanner. Operators
            // who need to bootstrap a project (scanner package
            // not yet installed) or recover from a broken scanner
            // can unset `securityScanner` and run the install,
            // then re-set it once the scanner is back.
            return Err(miette!(
                code = ERR_AUBE_SECURITY_SCANNER_FAILED,
                "securityScanner `{scanner_spec}` could not run: {e}\n\nSet `securityScanner = \"\"` to disable the integration temporarily."
            ));
        }
    };
    apply_advisories(scanner_spec, &advisories)
}

/// Materialize the bridge `.mjs` files into a fresh temp dir.
/// Each invocation gets its own dir so concurrent scanners don't
/// race on a shared temp file, and so a stale temp from a crash
/// can't leak state into the next run. The dir is cleaned up by
/// `tempfile::TempDir` going out of scope after the subprocess
/// finishes.
fn write_bridge_dir() -> Result<tempfile::TempDir, String> {
    let dir = tempfile::Builder::new()
        .prefix("aube-bun-scanner-")
        .tempdir()
        .map_err(|e| format!("failed to create bridge temp dir: {e}"))?;
    let write = |name: &str, body: &str| -> Result<(), String> {
        std::fs::write(dir.path().join(name), body)
            .map_err(|e| format!("failed to write bridge file {name}: {e}"))
    };
    write("bun_shim.mjs", BUN_SHIM_SOURCE)?;
    write("loader_hook.mjs", LOADER_HOOK_SOURCE)?;
    Ok(dir)
}

async fn invoke(
    scanner_spec: &str,
    cwd: &Path,
    packages: &[ScannerPackage],
) -> Result<Vec<Advisory>, String> {
    let request = ScannerRequest { packages };
    let body = serde_json::to_vec(&request)
        .map_err(|e| format!("failed to encode scanner request: {e}"))?;

    let bridge = write_bridge_dir()?;
    let mut cmd = tokio::process::Command::new(crate::runtime::node_program());
    cmd.current_dir(cwd)
        // `--experimental-strip-types` lets node import `.ts`
        // entrypoints (Socket's scanner package is the canonical
        // example — ships raw TS with `"exports": "./src/index.ts"`).
        // No-op on node ≥ 23.6 where it's default-on. Errors on
        // < 22.6 with a clear "unknown flag" message; the
        // fail-closed failure path then surfaces that error and
        // refuses the install rather than letting it through.
        .arg("--experimental-strip-types")
        .arg("--input-type=module")
        .arg("-e")
        .arg(RUNNER_SOURCE)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // `wait_with_output` consumes the `Child`; if `timeout`
        // fires and we drop the wait future, tokio does *not*
        // send SIGKILL by default. `.kill_on_drop(true)` is what
        // pnpmfile.rs uses for the same pattern — without it a
        // hung scanner survives its 30s timeout and keeps running.
        .kill_on_drop(true)
        // Pass the scanner spec and bridge dir via env (not argv)
        // so we sidestep node's `-e` argv handling quirks across
        // `--input-type` modes.
        .env("AUBE_SCANNER_SPEC", scanner_spec)
        .env("AUBE_BRIDGE_DIR", bridge.path())
        // Strip credentials before the scanner can read them via
        // `process.env`. The scanner has no legitimate reason to
        // talk to a registry or to GitHub — these are all
        // exfiltration vectors for a compromised scanner package.
        //
        // `AUBE_AUTH_TOKEN` matches what `aube-scripts` scrubs
        // from every lifecycle script. `NPM_TOKEN` /
        // `NODE_AUTH_TOKEN` stay scrubbed unconditionally for the
        // scanner (unlike in lifecycle scripts, where a
        // `postpublish` hook may genuinely need them — the
        // scanner never does). `GH_TOKEN` is the GitHub CLI's
        // PAT env var, commonly set alongside `GITHUB_TOKEN`.
        .env_remove("AUBE_AUTH_TOKEN")
        .env_remove("NPM_TOKEN")
        .env_remove("NODE_AUTH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .env_remove("GH_TOKEN");

    let mut child = cmd.spawn().map_err(|e| {
        // Most common cause: `node` isn't on PATH. The error
        // string from std::io::Error already includes
        // "No such file or directory" or the platform
        // equivalent, which is enough signal for the operator.
        format!("failed to spawn `node` for scanner bridge: {e}")
    })?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "internal pipe error: stdin not available".to_string())?;
    use tokio::io::AsyncWriteExt;
    stdin
        .write_all(&body)
        .await
        .map_err(|e| format!("failed to write request to scanner stdin: {e}"))?;
    drop(stdin);

    let wait = child.wait_with_output();
    let output = tokio::time::timeout(SCANNER_TIMEOUT, wait)
        .await
        .map_err(|_| {
            format!(
                "scanner exceeded {} second timeout",
                SCANNER_TIMEOUT.as_secs()
            )
        })?
        .map_err(|e| format!("failed to wait for scanner subprocess: {e}"))?;

    // Keep the bridge dir alive until after the child exits so
    // the runner can still readFile the shim during teardown.
    drop(bridge);

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let trimmed = stderr.trim();
        // Truncate by *character* count, not byte count — slicing
        // `&trimmed[..500]` directly panics when byte 500 lands in
        // the middle of a multi-byte UTF-8 sequence (localized
        // error messages, emoji, the `U+FFFD` replacement char
        // emitted by `from_utf8_lossy`).
        let snippet = if trimmed.chars().count() > 500 {
            let end = trimmed
                .char_indices()
                .nth(500)
                .map(|(i, _)| i)
                .unwrap_or(trimmed.len());
            format!("{}…", &trimmed[..end])
        } else {
            trimmed.to_string()
        };
        return Err(format!(
            "scanner exited with status {}; stderr: {snippet}",
            aube_scripts::exit_code_from_status(output.status)
        ));
    }

    serde_json::from_slice::<Vec<Advisory>>(&output.stdout)
        .map_err(|e| format!("scanner stdout was not a JSON advisory array: {e}"))
}

fn apply_advisories(scanner_spec: &str, advisories: &[Advisory]) -> miette::Result<()> {
    let mut fatal: Vec<&Advisory> = Vec::new();
    for adv in advisories {
        match classify(&adv.level) {
            Severity::Fatal => fatal.push(adv),
            Severity::Warn => {
                let url_suffix = adv
                    .url
                    .as_deref()
                    .map(|u| format!(" ({u})"))
                    .unwrap_or_default();
                tracing::warn!(
                    code = WARN_AUBE_SECURITY_SCANNER_FINDING,
                    "{}: {}{}",
                    adv.package,
                    if adv.description.is_empty() {
                        "flagged by securityScanner"
                    } else {
                        adv.description.as_str()
                    },
                    url_suffix
                );
            }
            Severity::Other => {
                tracing::debug!(
                    "securityScanner reported level={} for {}: {}",
                    adv.level,
                    adv.package,
                    adv.description
                );
            }
        }
    }
    if fatal.is_empty() {
        return Ok(());
    }
    let mut lines = vec![format!(
        "refusing to install package(s) flagged by `securityScanner = {scanner_spec}`:"
    )];
    for adv in &fatal {
        let url_suffix = adv
            .url
            .as_deref()
            .map(|u| format!(" — {u}"))
            .unwrap_or_default();
        let body = if adv.description.is_empty() {
            "(no description)".to_string()
        } else {
            adv.description.clone()
        };
        lines.push(format!("  - {}: {}{url_suffix}", adv.package, body));
    }
    Err(miette!(
        code = ERR_AUBE_SECURITY_SCANNER_FATAL,
        "{}",
        lines.join("\n")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adv(package: &str, level: &str) -> Advisory {
        Advisory {
            package: package.to_string(),
            level: level.to_string(),
            description: String::new(),
            url: None,
        }
    }

    #[test]
    fn classify_is_case_insensitive() {
        assert_eq!(classify("FATAL"), Severity::Fatal);
        assert_eq!(classify("fatal"), Severity::Fatal);
        assert_eq!(classify("Warning"), Severity::Warn);
        assert_eq!(classify("warn"), Severity::Warn);
        assert_eq!(classify("info"), Severity::Other);
        assert_eq!(classify(""), Severity::Other);
    }

    #[test]
    fn apply_advisories_empty_is_ok() {
        assert!(apply_advisories("/some/scanner", &[]).is_ok());
    }

    #[test]
    fn apply_advisories_warn_only_does_not_block() {
        let advs = vec![adv("pkg-a", "warn"), adv("pkg-b", "warning")];
        assert!(apply_advisories("scanner", &advs).is_ok());
    }

    #[test]
    fn apply_advisories_fatal_blocks() {
        let advs = vec![adv("pkg-a", "warn"), adv("evil", "fatal")];
        let err = apply_advisories("scanner", &advs).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("evil"), "missing package name: {msg}");
        assert!(msg.contains("scanner"), "missing scanner ref: {msg}");
    }

    #[test]
    fn unknown_severity_falls_through() {
        let advs = vec![adv("pkg-a", "info"), adv("pkg-b", "trace")];
        assert!(apply_advisories("scanner", &advs).is_ok());
    }

    /// Build a minimal `LockedPackage` for graph-fixture tests.
    /// `aliased_to=Some(real)` simulates an npm-alias entry where
    /// the manifest key is `name` but the underlying registry
    /// package is `real`.
    fn locked(name: &str, version: &str, aliased_to: Option<&str>) -> aube_lockfile::LockedPackage {
        aube_lockfile::LockedPackage {
            name: name.to_string(),
            version: version.to_string(),
            alias_of: aliased_to.map(str::to_string),
            ..Default::default()
        }
    }

    /// Local (`file:` / `link:`) entries set `local_source` —
    /// the scanner should skip them entirely.
    fn locked_local(name: &str, version: &str) -> aube_lockfile::LockedPackage {
        let mut pkg = locked(name, version, None);
        pkg.local_source = Some(aube_lockfile::LocalSource::Link("./somewhere".into()));
        pkg
    }

    #[test]
    fn resolved_packages_uses_registry_name_and_skips_local() {
        // Graph-fixture: one normal entry, one npm-alias entry,
        // one `file:` entry. The scanner should see the normal +
        // the alias-resolved real name with their resolved versions,
        // and skip the local entry.
        let mut graph = aube_lockfile::LockfileGraph::default();
        graph.packages.insert(
            "lodash@4.17.21".to_string(),
            locked("lodash", "4.17.21", None),
        );
        graph.packages.insert(
            "my-alias@1.2.3".to_string(),
            locked("my-alias", "1.2.3", Some("real-pkg")),
        );
        graph.packages.insert(
            "local-thing@0.0.0".to_string(),
            locked_local("local-thing", "0.0.0"),
        );

        let packages = resolved_packages_for_scanner(&graph);
        let view: Vec<(&str, &str)> = packages
            .iter()
            .map(|p| (p.name.as_str(), p.version.as_str()))
            .collect();
        assert_eq!(
            view,
            vec![("lodash", "4.17.21"), ("real-pkg", "1.2.3")],
            "alias should report `real-pkg`, local entry should be filtered out",
        );
    }

    #[test]
    fn resolved_packages_dedupes_peer_context_duplicates() {
        // Peer-context produces multiple `dep_path` nodes that share
        // the same `(name, version)` tuple — e.g. styled-components
        // appears once under `react@18.2.0` peer-suffix and once
        // under `react@19.0.0`. The scanner only cares about the
        // (name, version) pair, so the output should be deduped.
        let mut graph = aube_lockfile::LockfileGraph::default();
        graph.packages.insert(
            "styled-components@6.1.0(react@18.2.0)".to_string(),
            locked("styled-components", "6.1.0", None),
        );
        graph.packages.insert(
            "styled-components@6.1.0(react@19.0.0)".to_string(),
            locked("styled-components", "6.1.0", None),
        );

        let packages = resolved_packages_for_scanner(&graph);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "styled-components");
        assert_eq!(packages[0].version, "6.1.0");
    }

    /// Returns true iff `node --version` exits 0. e2e tests gate
    /// on this — CI runners without node skip rather than fail.
    fn node_available() -> bool {
        std::process::Command::new(crate::runtime::node_program())
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Write a minimal Bun-shape scanner module that matches by
    /// name and emits one advisory of the given level. Mirrors
    /// the simplest realistic scanner — what the oven-sh template
    /// degenerates to once you strip the type annotations.
    fn write_simple_scanner(path: &Path, target_name: &str, level: &str) {
        let body = format!(
            r#"export const scanner = {{
  version: '1',
  async scan({{ packages }}) {{
    const hits = [];
    for (const p of packages) {{
      if (p.name === {target:?}) {{
        hits.push({{
          level: {level:?},
          package: p.name,
          description: 'mock',
          url: 'https://example.org/mock',
        }});
      }}
    }}
    return hits;
  }},
}};
"#,
            target = target_name,
            level = level,
        );
        std::fs::write(path, body).unwrap();
    }

    /// End-to-end: drop a real `.mjs` scanner on disk, run the
    /// bridge, verify the fatal path surfaces with the expected
    /// package and description. Exercises temp-dir bridge file
    /// extraction, module-loader hook registration, stdin/stdout
    /// JSON plumbing, and the policy layer end-to-end.
    #[tokio::test]
    async fn end_to_end_blocks_on_fatal_advisory() {
        if !node_available() {
            eprintln!("skipping: `node` not on PATH");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let scanner_path = tmp.path().join("scanner.mjs");
        write_simple_scanner(&scanner_path, "evil", "fatal");

        let pkgs = vec![ScannerPackage {
            name: "evil".to_string(),
            version: "latest".to_string(),
        }];
        let err = run_scanner(scanner_path.to_str().unwrap(), tmp.path(), &pkgs)
            .await
            .unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("evil"), "missing pkg in error: {msg}");
        assert!(msg.contains("mock"), "missing description in error: {msg}");
    }

    /// Companion: `warn`-only output lets the install through.
    #[tokio::test]
    async fn end_to_end_passes_on_warn_only() {
        if !node_available() {
            eprintln!("skipping: `node` not on PATH");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let scanner_path = tmp.path().join("scanner.mjs");
        write_simple_scanner(&scanner_path, "meh", "warn");

        let pkgs = vec![ScannerPackage {
            name: "meh".to_string(),
            version: "1.0.0".to_string(),
        }];
        assert!(
            run_scanner(scanner_path.to_str().unwrap(), tmp.path(), &pkgs)
                .await
                .is_ok()
        );
    }

    /// Fail-closed contract: a missing scanner module surfaces as
    /// `ERR_AUBE_SECURITY_SCANNER_FAILED` and blocks the install.
    /// A configured scanner that can't run is treated as a refusal,
    /// not a free pass — silent bypass would defeat the point of
    /// opting into the scanner. The error message points at the
    /// `securityScanner = ""` escape hatch so operators bootstrapping
    /// a project know how to unblock themselves.
    #[tokio::test]
    async fn missing_scanner_fails_closed() {
        if !node_available() {
            eprintln!("skipping: `node` not on PATH");
            return;
        }
        let pkgs = vec![ScannerPackage {
            name: "lodash".to_string(),
            version: "^4".to_string(),
        }];
        let err = run_scanner(
            "/definitely/not/a/real/path/to/a/scanner.mjs",
            std::path::Path::new("."),
            &pkgs,
        )
        .await
        .unwrap_err();
        // `Debug` for `miette::Report` line-wraps the message,
        // breaking simple substring assertions on multi-word phrases.
        // Match against the structured code instead, and probe the
        // wrapped body via a handful of unique tokens.
        let chain = format!("{err:?}");
        assert!(
            chain.contains("ERR_AUBE_SECURITY_SCANNER_FAILED"),
            "wrong code: {chain}"
        );
        assert!(
            chain.contains("scanner.mjs"),
            "missing scanner spec in error: {chain}"
        );
        assert!(chain.contains("disable"), "missing bootstrap hint: {chain}");
    }

    /// `{ advisories: [...] }` response shape is accepted in
    /// addition to the canonical `Advisory[]`.
    #[tokio::test]
    async fn accepts_wrapped_advisories_response() {
        if !node_available() {
            eprintln!("skipping: `node` not on PATH");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let scanner_path = tmp.path().join("scanner.mjs");
        std::fs::write(
            &scanner_path,
            r#"export const scanner = {
  version: '1',
  async scan({ packages }) {
    return { advisories: packages.map(p => ({
      level: 'fatal',
      package: p.name,
      description: 'wrapped',
    })) };
  },
};
"#,
        )
        .unwrap();

        let pkgs = vec![ScannerPackage {
            name: "any".to_string(),
            version: "1".to_string(),
        }];
        let advisories = invoke(scanner_path.to_str().unwrap(), tmp.path(), &pkgs)
            .await
            .unwrap();
        assert_eq!(advisories.len(), 1);
        assert_eq!(advisories[0].package, "any");
        assert_eq!(advisories[0].description, "wrapped");
    }

    /// Bun-compat test: a scanner that does `import Bun from 'bun'`
    /// and calls `Bun.semver.satisfies` + `Bun.env` works
    /// unchanged. Mirrors the shape of the oven-sh template
    /// (semver) and the SocketDev scanner (env). Uses the naive
    /// `Bun.semver` fallback path since the test temp project has
    /// no `semver` package installed; that fallback handles
    /// `version === "1.0.0", range === "1.0.0"` correctly via
    /// exact equality. The shim emits a one-time stderr warning
    /// when it falls back — we don't assert on it here since
    /// stderr also carries unrelated node bootstrap chatter.
    #[tokio::test]
    async fn bun_shim_exposes_env_and_semver() {
        if !node_available() {
            eprintln!("skipping: `node` not on PATH");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let scanner_path = tmp.path().join("scanner.mjs");
        std::fs::write(
            &scanner_path,
            r#"import Bun from 'bun';
export const scanner = {
  version: '1',
  async scan({ packages }) {
    const hits = [];
    for (const p of packages) {
      // Use Bun.semver.satisfies (the oven-sh template pattern).
      // With the naive fallback the exact-equality branch fires;
      // both `"1.0.0"` and `"1.0.0"` match.
      if (Bun.semver.satisfies(p.version, '1.0.0')) {
        // Touch Bun.env to ensure the env shim is wired.
        const tag = Bun.env.AUBE_TEST_TAG ?? 'no-tag';
        hits.push({
          level: 'fatal',
          package: p.name,
          description: `matched via Bun.semver; tag=${tag}`,
        });
      }
    }
    return hits;
  },
};
"#,
        )
        .unwrap();

        // SAFETY: `set_var` is safe on a single-threaded test
        // body; the env is read by the *child* process via env we
        // explicitly pass on the Command, not the parent's env at
        // read time. Use a Command-level env override instead of
        // process-wide mutation to keep the test thread-safe.
        let pkgs = vec![ScannerPackage {
            name: "target".to_string(),
            version: "1.0.0".to_string(),
        }];

        // The shim reads `Bun.env.AUBE_TEST_TAG` from
        // `process.env`, which the child inherits from this
        // process unless we override. We don't override here:
        // `Bun.env.AUBE_TEST_TAG` will be `undefined` and the
        // scanner falls back to `'no-tag'`, which is enough to
        // confirm `Bun.env` is a live object.
        let err = run_scanner(scanner_path.to_str().unwrap(), tmp.path(), &pkgs)
            .await
            .unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("matched via Bun.semver"),
            "Bun.semver.satisfies didn't fire: {msg}"
        );
        assert!(
            msg.contains("tag=no-tag"),
            "Bun.env wasn't a live object: {msg}"
        );
    }

    /// Bun-compat test: scanner uses `Bun.file()` to read a
    /// fixture and incorporates the content into its advisory.
    /// Mirrors what the SocketDev scanner does with its settings
    /// file lookup.
    #[tokio::test]
    async fn bun_shim_file_reads_local_fixture() {
        if !node_available() {
            eprintln!("skipping: `node` not on PATH");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("policy.json"), r#"{"badName":"evil"}"#).unwrap();
        let scanner_path = tmp.path().join("scanner.mjs");
        std::fs::write(
            &scanner_path,
            r#"import Bun from 'bun';
export const scanner = {
  version: '1',
  async scan({ packages }) {
    const policy = await Bun.file('policy.json').json();
    return packages
      .filter(p => p.name === policy.badName)
      .map(p => ({ level: 'fatal', package: p.name, description: 'matched policy' }));
  },
};
"#,
        )
        .unwrap();

        let pkgs = vec![ScannerPackage {
            name: "evil".to_string(),
            version: "1".to_string(),
        }];
        let err = run_scanner(scanner_path.to_str().unwrap(), tmp.path(), &pkgs)
            .await
            .unwrap_err();
        assert!(format!("{err:?}").contains("matched policy"));
    }

    /// Bun-compat test: a `.ts` scanner entrypoint (Socket's
    /// distribution shape — `"exports": "./src/index.ts"`) loads
    /// via `--experimental-strip-types`. Gates on the node binary
    /// supporting the flag — older nodes will exit with "unknown
    /// flag" and the test reads that exit-code 1 as a skip rather
    /// than a failure.
    #[tokio::test]
    async fn bun_shim_loads_typescript_entrypoint() {
        if !node_available() {
            eprintln!("skipping: `node` not on PATH");
            return;
        }
        // Detect whether the installed node supports
        // --experimental-strip-types. Cheap probe: `node
        // --experimental-strip-types -e ''` is a no-op on
        // supported versions and exits non-zero on unsupported.
        let probe = std::process::Command::new(crate::runtime::node_program())
            .arg("--experimental-strip-types")
            .arg("-e")
            .arg("''")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if !probe.is_ok_and(|s| s.success()) {
            eprintln!("skipping: node lacks --experimental-strip-types (< 22.6)");
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        let scanner_path = tmp.path().join("scanner.ts");
        // TS-only construct (type annotation on the destructured
        // parameter) — must be stripped before evaluation. If
        // strip-types is mis-wired this fails to parse.
        std::fs::write(
            &scanner_path,
            r#"export const scanner = {
  version: '1' as const,
  async scan({ packages }: { packages: Array<{ name: string; version: string }> }) {
    const hits: Array<{ level: string; package: string; description: string }> = [];
    for (const p of packages) {
      if (p.name === 'evil') {
        hits.push({ level: 'fatal', package: p.name, description: 'ts ok' });
      }
    }
    return hits;
  },
};
"#,
        )
        .unwrap();

        let pkgs = vec![ScannerPackage {
            name: "evil".to_string(),
            version: "1".to_string(),
        }];
        let err = run_scanner(scanner_path.to_str().unwrap(), tmp.path(), &pkgs)
            .await
            .unwrap_err();
        assert!(format!("{err:?}").contains("ts ok"));
    }
}
