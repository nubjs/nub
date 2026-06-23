//! pnpmfile.js hook support.
//!
//! Shells out to `node` to run hooks from a project's `.pnpmfile.mjs`
//! or `.pnpmfile.cjs`,
//! matching pnpm's hook interface closely enough that existing
//! pnpmfiles that use `hooks.afterAllResolved` work unchanged. We
//! pipe a JSON representation of the resolved lockfile to a small
//! node shim, invoke the hook, and apply the narrow set of
//! mutations we understand back to the `LockfileGraph`.
//!
//! `afterAllResolved` is shelled out to a one-shot node child per
//! install. `readPackage` runs inside the resolver's hot loop and is
//! served by a single long-lived node child (see [`ReadPackageHost`])
//! that exchanges newline-delimited JSON messages — one request per
//! version-picked package. Keeping the child resident avoids
//! spawning a fresh `node` per hook (which, on macOS especially,
//! costs tens of milliseconds each and would dominate the resolver
//! budget) and lets the resolver `await` each call in sequence, so
//! its own loop still looks synchronous from its point of view.
//!
//! TODO: once the ecosystem settles, consider replacing the node
//! shellout with an embedded JS runtime or Wasm sandbox so we can
//! drop the hard dependency on `node` at resolve time and cut the
//! per-process overhead.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};

use aube_lockfile::LockfileGraph;
use aube_registry::VersionMetadata;
use aube_resolver::ReadPackageHook;
use miette::{IntoDiagnostic, Result, WrapErr, miette};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout};

pub const PNPMFILE_MJS_NAME: &str = ".pnpmfile.mjs";
pub const PNPMFILE_CJS_NAME: &str = ".pnpmfile.cjs";

/// Whether [`detect`] probes the cwd-default `.pnpmfile.mjs` /
/// `.pnpmfile.cjs` when no explicit `--pnpmfile` / `pnpm-workspace.yaml`
/// `pnpmfilePath` override is given. Sourced from the engine context;
/// defaults to `true` (upstream behavior). An embedder whose active
/// package manager isn't pnpm — and for whom a stray cwd `.pnpmfile` is
/// another tool's resolution-shaping config, not theirs to honor — sets
/// `pnpmfile_default_enabled = false` on the context to gate the default
/// arm off. The explicit `--pnpmfile` / `--global-pnpmfile` /
/// workspace-yaml `pnpmfilePath` overrides are untouched: a path a user
/// named on purpose still loads. See the brand-boundary note in nub's
/// `engine_brand_preflight`.
fn pnpmfile_default_enabled() -> bool {
    aube_util::engine_context().pnpmfile_default_enabled
}

/// The cwd-default pnpmfile path if one exists, ignoring the
/// [`pnpmfile_default_enabled`] gate. Lets an embedder discover that a
/// default `.pnpmfile` is present *before* it suppresses detection, so it
/// can emit a one-line "ignored" warning naming the file. Mirrors the
/// `.mjs`-over-`.cjs` precedence [`detect`] uses.
pub fn default_path(cwd: &Path) -> Option<PathBuf> {
    for name in [PNPMFILE_MJS_NAME, PNPMFILE_CJS_NAME] {
        let p = cwd.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Flipped on by `main` when `--reporter=ndjson` is in effect. Read by
/// the per-hook stderr forwarder to decide whether `ctx.log` records get
/// re-emitted as `pnpm:hook` ndjson on stdout (machine-readable mode) or
/// fall back to a friendly `[pnpmfile] message` on stderr.
static NDJSON_REPORTER: AtomicBool = AtomicBool::new(false);

pub fn set_ndjson_reporter(on: bool) {
    NDJSON_REPORTER.store(on, Ordering::Relaxed);
}

fn ndjson_reporter() -> bool {
    NDJSON_REPORTER.load(Ordering::Relaxed)
}

/// Sentinel our shims prepend to every `ctx.log` line they write to
/// stderr. Internal contract between the shim and this crate; chosen to
/// be unique enough that user-authored `console.error` output (require()
/// stack traces, hook-body diagnostics) never collides.
const HOOK_LOG_SENTINEL: &str = "__AUBE_HOOK_LOG__ ";

/// Return the path to the project's pnpmfile if one exists.
///
/// Override precedence is `cli > workspace_yaml > default`:
/// * `cli_pnpmfile` mirrors pnpm's `--pnpmfile <path>` flag — relative
///   paths resolve against `cwd`. A typo here is a hard miss (returns
///   `None`) with a warning so the user notices.
/// * `workspace_pnpmfile_path` is the `pnpmfilePath` override from
///   `pnpm-workspace.yaml` (pnpm v10 lets users keep the hook file
///   outside the project root). Same hard-miss semantics on a typo.
/// * Otherwise: `cwd/.pnpmfile.mjs` (preferred) or `cwd/.pnpmfile.cjs`,
///   *unless* the cwd-default is gated off via the engine context's
///   `pnpmfile_default_enabled` (a non-pnpm embedder treating a
///   stray `.pnpmfile` as another tool's config). The missing-default
///   case stays silent because "no pnpmfile" is the common case, not a
///   misconfiguration.
pub fn detect(
    cwd: &Path,
    cli_pnpmfile: Option<&Path>,
    workspace_pnpmfile_path: Option<&str>,
) -> Option<PathBuf> {
    if let Some(rel) = cli_pnpmfile {
        let p = if rel.is_absolute() {
            rel.to_path_buf()
        } else {
            cwd.join(rel)
        };
        if !p.is_file() {
            tracing::warn!(
                code = aube_codes::warnings::WARN_AUBE_PNPMFILE_NOT_FOUND,
                "--pnpmfile override {:?} points at a missing file — hooks will not run",
                p.display().to_string(),
            );
            return None;
        }
        return Some(p);
    }
    if let Some(rel) = workspace_pnpmfile_path {
        let p = cwd.join(rel);
        if !p.is_file() {
            tracing::warn!(
                code = aube_codes::warnings::WARN_AUBE_PNPMFILE_NOT_FOUND,
                "pnpmfilePath override {:?} (from pnpm-workspace.yaml) points at a missing file — hooks will not run",
                p.display().to_string(),
            );
            return None;
        }
        return Some(p);
    }
    // The cwd-default arm is gated by the embedder seam: under a
    // non-pnpm incumbent a stray `.pnpmfile` is another tool's
    // resolution-shaping config and is not honored. Explicit overrides
    // above are unaffected.
    if !pnpmfile_default_enabled() {
        return None;
    }
    default_path(cwd)
}

/// Resolve `--global-pnpmfile <path>`. Unlike [`detect`], there is no
/// default location for the global pnpmfile — pnpm requires the path
/// to be passed explicitly. Relative paths resolve against `cwd` (so
/// `--global-pnpmfile=../hooks.cjs` works the same as in pnpm). A typo
/// is a hard miss with a warning, matching the local-pnpmfile shape.
pub fn detect_global(cwd: &Path, cli_global: Option<&Path>) -> Option<PathBuf> {
    let rel = cli_global?;
    let p = if rel.is_absolute() {
        rel.to_path_buf()
    } else {
        cwd.join(rel)
    };
    if !p.is_file() {
        tracing::warn!(
            code = aube_codes::warnings::WARN_AUBE_PNPMFILE_NOT_FOUND,
            "--global-pnpmfile override {:?} points at a missing file — global hooks will not run",
            p.display().to_string(),
        );
        return None;
    }
    Some(p)
}

/// Order pnpm runs hook files in: global first, then local. Local hook
/// mutations land on top of global ones so a project can override its
/// org-wide rules. Use this to flatten the (`global`, `local`) pair into
/// a single iteration order at every call site.
pub fn ordered_paths(global: Option<&Path>, local: Option<&Path>) -> Vec<PathBuf> {
    let mut paths = Vec::with_capacity(2);
    if let Some(p) = global {
        paths.push(p.to_path_buf());
    }
    if let Some(p) = local {
        paths.push(p.to_path_buf());
    }
    paths
}

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct LockfileWire {
    importers: BTreeMap<String, Vec<DirectDepWire>>,
    packages: BTreeMap<String, PackageWire>,
}

#[derive(Serialize, Deserialize, Clone)]
struct DirectDepWire {
    name: String,
    version: String,
}

#[derive(Serialize, Deserialize, Clone, Default)]
struct PackageWire {
    name: String,
    version: String,
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "peerDependencies")]
    peer_dependencies: BTreeMap<String, String>,
}

fn to_wire(graph: &LockfileGraph) -> LockfileWire {
    let importers = graph
        .importers
        .iter()
        .map(|(path, deps)| {
            let wire = deps
                .iter()
                .map(|d| DirectDepWire {
                    name: d.name.clone(),
                    version: d.dep_path.clone(),
                })
                .collect();
            (path.clone(), wire)
        })
        .collect();
    let packages = graph
        .packages
        .iter()
        .map(|(key, pkg)| {
            (
                key.clone(),
                PackageWire {
                    name: pkg.name.clone(),
                    version: pkg.version.clone(),
                    dependencies: pkg.dependencies.clone(),
                    peer_dependencies: pkg.peer_dependencies.clone(),
                },
            )
        })
        .collect();
    LockfileWire {
        importers,
        packages,
    }
}

fn apply(wire: LockfileWire, graph: &mut LockfileGraph) {
    // Only packages[].dependencies and packages[].peerDependencies
    // are honored. Mutations to importers or to a package's
    // name/version are ignored because they would require re-running
    // the resolver to stay consistent; warn about them so the
    // pnpmfile author knows the edit was a no-op.
    for (path, wire_deps) in &wire.importers {
        if let Some(graph_deps) = graph.importers.get(path) {
            let same = graph_deps.len() == wire_deps.len()
                && graph_deps
                    .iter()
                    .zip(wire_deps.iter())
                    .all(|(g, w)| g.name == w.name && g.dep_path == w.version);
            if !same {
                tracing::warn!(
                    code = aube_codes::warnings::WARN_AUBE_HOOK_IMPORTER_MUTATED,
                    "[pnpmfile] afterAllResolved mutated importers[{path}]; \
                     aube ignores importer edits because they would require \
                     re-running the resolver",
                );
            }
        } else {
            tracing::warn!(
                code = aube_codes::warnings::WARN_AUBE_HOOK_IMPORTER_ADDED,
                "[pnpmfile] afterAllResolved added importers[{path}]; \
                 aube ignores new importer entries",
            );
        }
    }
    for (key, pkg) in wire.packages {
        if let Some(locked) = graph.packages.get_mut(&key) {
            if pkg.name != locked.name || pkg.version != locked.version {
                tracing::warn!(
                    code = aube_codes::warnings::WARN_AUBE_HOOK_IDENTITY_REWRITTEN,
                    "[pnpmfile] afterAllResolved rewrote name/version for {key} \
                     (to {}@{}); aube ignores identity edits on existing packages",
                    pkg.name,
                    pkg.version,
                );
            }
            if locked.dependencies != pkg.dependencies {
                locked.dependencies = pkg.dependencies;
            }
            if locked.peer_dependencies != pkg.peer_dependencies {
                locked.peer_dependencies = pkg.peer_dependencies;
            }
        } else {
            tracing::warn!(
                code = aube_codes::warnings::WARN_AUBE_HOOK_PACKAGE_ADDED,
                "[pnpmfile] afterAllResolved added a new package entry {key}; \
                 aube ignores newly-introduced packages from the hook",
            );
        }
    }
}

const LOAD_PNPMFILE_JS: &str = r#"
const path = require('path');
const { pathToFileURL } = require('url');
async function loadPnpmfile(file) {
  const resolved = path.resolve(file);
  const mod = resolved.endsWith('.mjs')
    ? await import(pathToFileURL(resolved).href)
    : require(resolved);
  if (mod && mod.default && !mod.default.hooks && mod.hooks) {
    console.error('[pnpmfile] default export has no hooks; using named hooks export');
    return mod;
  }
  return (mod && (mod.default || mod)) || {};
}
"#;

const SHIM: &str = r#"
const pnpmfile = process.env.AUBE_PNPMFILE;
const hookName = process.env.AUBE_HOOK;
const SENTINEL = '__AUBE_HOOK_LOG__ ';
let chunks = [];
process.stdin.on('data', (c) => chunks.push(c));
process.stdin.on('end', async () => {
  try {
    const input = JSON.parse(Buffer.concat(chunks).toString('utf8'));
    const mod = await loadPnpmfile(pnpmfile);
    const hooks = (mod && mod.hooks) || {};
    const fn = hooks[hookName];
    let result = input;
    if (typeof fn === 'function') {
      const ctx = {
        log: (...args) => {
          const message = args.map((a) => typeof a === 'string' ? a : require('util').inspect(a)).join(' ');
          process.stderr.write(SENTINEL + JSON.stringify({hook: hookName, message}) + '\n');
        },
      };
      const out = await fn(input, ctx);
      if (out && typeof out === 'object') result = out;
    }
    process.stdout.write(JSON.stringify(result));
  } catch (err) {
    console.error('[pnpmfile] hook failed:', (err && err.stack) || err);
    process.exit(1);
  }
});
"#;

/// Generic shim used for any one-shot hook (`afterAllResolved`,
/// `preResolution`, …). Dispatches on `process.env.AUBE_HOOK` so a new
/// one-shot hook only needs a `run_one_shot_hook(.., name, ..)` call —
/// don't add a parallel shim.
fn one_shot_hook_shim() -> String {
    format!("{LOAD_PNPMFILE_JS}{SHIM}")
}

/// Drain the child's stderr line-by-line. Lines tagged with
/// `HOOK_LOG_SENTINEL` are `ctx.log` records: in ndjson reporter mode
/// they're enriched with `prefix` (project root) and `from` (pnpmfile
/// path) and re-emitted as `{"name":"pnpm:hook",…}` on stdout, matching
/// pnpm's `--reporter=ndjson` surface. In default mode they fall back to
/// the legacy `[pnpmfile] message` format on stderr. Untagged lines are
/// passed through verbatim so require()-time errors and user
/// `console.error` calls keep working unchanged.
fn spawn_stderr_forwarder(
    stderr: ChildStderr,
    prefix: PathBuf,
    from: PathBuf,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        let mut stdout = tokio::io::stdout();
        let mut stderr_w = tokio::io::stderr();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(rest) = line.strip_prefix(HOOK_LOG_SENTINEL) {
                forward_hook_log(rest, &prefix, &from, &mut stdout, &mut stderr_w).await;
            } else {
                let _ = stderr_w.write_all(line.as_bytes()).await;
                let _ = stderr_w.write_all(b"\n").await;
            }
        }
    })
}

#[derive(Deserialize)]
struct HookLogRecord {
    hook: String,
    message: String,
}

#[derive(Serialize)]
struct PnpmHookLog<'a> {
    name: &'a str,
    prefix: &'a str,
    from: &'a str,
    hook: &'a str,
    message: &'a str,
}

async fn forward_hook_log(
    payload: &str,
    prefix: &Path,
    from: &Path,
    stdout: &mut tokio::io::Stdout,
    stderr_w: &mut tokio::io::Stderr,
) {
    let Ok(rec) = serde_json::from_str::<HookLogRecord>(payload) else {
        // Malformed sentinel record. Pass the original line through so
        // the user sees something rather than swallowing it silently.
        let _ = stderr_w.write_all(HOOK_LOG_SENTINEL.as_bytes()).await;
        let _ = stderr_w.write_all(payload.as_bytes()).await;
        let _ = stderr_w.write_all(b"\n").await;
        return;
    };
    if ndjson_reporter() {
        let out = PnpmHookLog {
            name: "pnpm:hook",
            prefix: prefix.to_str().unwrap_or(""),
            from: from.to_str().unwrap_or(""),
            hook: &rec.hook,
            message: &rec.message,
        };
        if let Ok(s) = serde_json::to_string(&out) {
            let _ = stdout.write_all(s.as_bytes()).await;
            let _ = stdout.write_all(b"\n").await;
            let _ = stdout.flush().await;
        }
    } else {
        let line = format!("[pnpmfile] {}\n", rec.message);
        let _ = stderr_w.write_all(line.as_bytes()).await;
    }
}

/// Spawn a one-shot `node` child running the shared shim for `hook_name`,
/// pipe `input_json` in on stdin, and return the captured stdout. Shared
/// scaffolding for `afterAllResolved` (which round-trips a lockfile) and
/// `preResolution` (which fires-and-forgets a context object). `prefix`
/// is the project root used to enrich `ctx.log` records when the ndjson
/// reporter is active.
async fn run_one_shot_hook(
    pnpmfile: &Path,
    prefix: &Path,
    hook_name: &str,
    input_json: &[u8],
) -> Result<Vec<u8>> {
    tracing::debug!("running pnpmfile hook {hook_name} ({})", pnpmfile.display());

    let mut cmd = tokio::process::Command::new(crate::runtime::node_program());
    cmd.arg("-e")
        .arg(one_shot_hook_shim())
        .env("AUBE_PNPMFILE", pnpmfile)
        .env("AUBE_HOOK", hook_name)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // Match ReadPackageHost::spawn below. Without kill_on_drop the
        // Node child keeps running when the parent future is cancelled
        // (install panics, user Ctrl-C's, etc) and the hook body races
        // on past stdin close. Unlikely to bite in practice but
        // zero-cost to guard.
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .into_diagnostic()
        .wrap_err("failed to spawn `node` for pnpmfile hook — is node installed and on PATH?")?;

    let forwarder = child
        .stderr
        .take()
        .map(|stderr| spawn_stderr_forwarder(stderr, prefix.to_path_buf(), pnpmfile.to_path_buf()));

    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| miette!("failed to open stdin for pnpmfile node child"))?;
        stdin
            .write_all(input_json)
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to write JSON to pnpmfile {hook_name} hook"))?;
        stdin
            .shutdown()
            .await
            .into_diagnostic()
            .wrap_err("failed to close stdin for pnpmfile hook")?;
    }

    let output = child
        .wait_with_output()
        .await
        .into_diagnostic()
        .wrap_err("pnpmfile hook child process failed")?;
    // Drain any remaining buffered stderr lines before returning, so
    // their `pnpm:hook` records aren't reordered after the install
    // step that follows this hook.
    if let Some(handle) = forwarder {
        let _ = handle.await;
    }
    if !output.status.success() {
        return Err(miette!(
            "pnpmfile hook `{hook_name}` exited with status {}",
            output.status
        ));
    }
    Ok(output.stdout)
}

/// Run the `afterAllResolved` hook against a resolved lockfile graph.
/// Mutations to `packages[].dependencies` and `packages[].peerDependencies`
/// are applied in place. All other fields are round-tripped but
/// ignored on the way back. `prefix` is the project root used to enrich
/// `ctx.log` records under `--reporter=ndjson`.
pub async fn run_after_all_resolved(
    pnpmfile: &Path,
    prefix: &Path,
    graph: &mut LockfileGraph,
) -> Result<()> {
    let input = to_wire(graph);
    let input_json = serde_json::to_vec(&input)
        .into_diagnostic()
        .wrap_err("failed to serialize lockfile for pnpmfile hook")?;
    let stdout = run_one_shot_hook(pnpmfile, prefix, "afterAllResolved", &input_json).await?;
    let wire: LockfileWire = serde_json::from_slice(&stdout)
        .into_diagnostic()
        .wrap_err("pnpmfile hook returned invalid JSON from afterAllResolved")?;
    apply(wire, graph);
    Ok(())
}

/// Run `afterAllResolved` for each pnpmfile in `paths` in order. pnpm
/// runs the global hook first and the local hook second, so local
/// mutations land on top. Empty list is a no-op. `prefix` is the
/// project root used to enrich `ctx.log` records under
/// `--reporter=ndjson`; the same value flows through every entry.
pub async fn run_after_all_resolved_chain(
    paths: &[PathBuf],
    prefix: &Path,
    graph: &mut LockfileGraph,
) -> Result<()> {
    for p in paths {
        run_after_all_resolved(p, prefix, graph)
            .await
            .wrap_err_with(|| format!("pnpmfile afterAllResolved hook failed ({})", p.display()))?;
    }
    Ok(())
}

/// Snapshot passed to the `preResolution` hook before resolve starts.
/// Mirrors pnpm's context shape (camelCase on the wire) so existing
/// pnpmfiles can read the fields they expect.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreResolutionContext<'a> {
    pub lockfile_dir: &'a Path,
    pub store_dir: Option<&'a Path>,
    pub current_lockfile: Option<LockfileWire>,
    pub wanted_lockfile: Option<LockfileWire>,
    pub exists_current_lockfile: bool,
    pub exists_non_empty_wanted_lockfile: bool,
    pub registries: BTreeMap<String, String>,
}

impl<'a> PreResolutionContext<'a> {
    /// Build the snapshot for `lockfile_dir`. `existing` is the on-disk
    /// lockfile graph (or `None` when there isn't one); both
    /// `currentLockfile` and `wantedLockfile` are derived from it
    /// because at preResolution time they're identical — pnpm only
    /// diverges them after resolve has produced the wanted graph.
    pub fn from_existing(
        lockfile_dir: &'a Path,
        store_dir: Option<&'a Path>,
        existing: Option<&LockfileGraph>,
        registries: BTreeMap<String, String>,
    ) -> Self {
        let wire = existing.map(to_wire);
        let exists_current_lockfile = existing.is_some();
        let exists_non_empty_wanted_lockfile = wire
            .as_ref()
            .is_some_and(|w| !w.importers.is_empty() || !w.packages.is_empty());
        Self {
            lockfile_dir,
            store_dir,
            current_lockfile: wire.clone(),
            wanted_lockfile: wire,
            exists_current_lockfile,
            exists_non_empty_wanted_lockfile,
            registries,
        }
    }
}

/// Run the `preResolution` hook before the resolver walks the graph.
/// Fire-and-forget — the hook's return value is discarded by pnpm and
/// by aube. Skips spawning `node` when the pnpmfile doesn't reference
/// `preResolution` so a hook-less pnpmfile doesn't pay the per-install
/// node-startup cost on every command. `prefix` is the project root
/// used to enrich `ctx.log` records under `--reporter=ndjson`.
pub async fn run_pre_resolution(
    pnpmfile: &Path,
    prefix: &Path,
    ctx: &PreResolutionContext<'_>,
) -> Result<()> {
    if !has_hook(pnpmfile, "preResolution").await? {
        return Ok(());
    }
    let input_json = serde_json::to_vec(ctx)
        .into_diagnostic()
        .wrap_err("failed to serialize preResolution context")?;
    run_one_shot_hook(pnpmfile, prefix, "preResolution", &input_json).await?;
    Ok(())
}

/// Run `preResolution` for each pnpmfile in `paths` (global first,
/// then local). pnpm fires both hooks against the same context, so we
/// don't thread state between them — each hook gets the original
/// pre-resolve snapshot. `prefix` is the project root used to enrich
/// `ctx.log` records under `--reporter=ndjson`.
pub async fn run_pre_resolution_chain(
    paths: &[PathBuf],
    prefix: &Path,
    ctx: &PreResolutionContext<'_>,
) -> Result<()> {
    for p in paths {
        run_pre_resolution(p, prefix, ctx)
            .await
            .wrap_err_with(|| format!("pnpmfile preResolution hook failed ({})", p.display()))?;
    }
    Ok(())
}

/// Node shim for the long-lived `readPackage` host. Reads NDJSON
/// requests of the form `{"id":N,"pkg":{...}}` on stdin and writes
/// one response per line on stdout: either `{"id":N,"pkg":{...}}` or
/// `{"id":N,"error":"..."}`. The hook module is `require`d exactly
/// once at startup, so filesystem I/O and V8 compilation aren't
/// repeated per call. Calls are processed sequentially — the
/// resolver already serializes them, and a sequential loop sidesteps
/// the interleaving hazards you'd otherwise get from async readline
/// callbacks.
const READ_PACKAGE_SHIM: &str = r#"
const readline = require('readline');
const pnpmfile = process.env.AUBE_PNPMFILE;
const SENTINEL = '__AUBE_HOOK_LOG__ ';
const ctx = {
  log: (...args) => {
    const message = args.map((a) => typeof a === 'string' ? a : require('util').inspect(a)).join(' ');
    process.stderr.write(SENTINEL + JSON.stringify({hook: 'readPackage', message}) + '\n');
  },
};
process.stdout.on('error', (e) => {
  if (e && e.code === 'EPIPE') process.exit(0);
});
async function main() {
  const mod = await loadPnpmfile(pnpmfile);
  const hooks = (mod && mod.hooks) || {};
  const readPackage = hooks.readPackage;
  const rl = readline.createInterface({
    input: process.stdin,
    crlfDelay: Infinity,
  });
  for await (const line of rl) {
    if (!line) continue;
    let id = null;
    try {
      const req = JSON.parse(line);
      id = req.id;
      let result = req.pkg;
      if (typeof readPackage === 'function') {
        const out = await readPackage(req.pkg, ctx);
        // Match pnpm: a defined hook must return the (possibly mutated)
        // manifest. Silently passing through req.pkg on a missing return
        // hides the common bug of forgetting `return pkg` in an else branch.
        if (!out || typeof out !== 'object') {
          const name = (req.pkg && req.pkg.name) || '<unknown>';
          const version = (req.pkg && req.pkg.version) || '<unknown>';
          throw new Error('hook did not return a package manifest object (called with ' + name + '@' + version + ')');
        }
        result = out;
      }
      process.stdout.write(JSON.stringify({ id, pkg: result }) + '\n');
    } catch (err) {
      const msg = (err && err.stack) || String(err);
      process.stdout.write(JSON.stringify({ id, error: String(msg) }) + '\n');
    }
  }
}
main().catch((err) => {
  console.error('[pnpmfile] readPackage host crashed:', (err && err.stack) || err);
  process.exit(1);
});
"#;

fn read_package_shim() -> String {
    format!("{LOAD_PNPMFILE_JS}{READ_PACKAGE_SHIM}")
}

/// Long-lived node child that answers `readPackage` calls one at a
/// time. Owned by the install command for the span of a single
/// resolve, then dropped (which kills the child). Implements
/// [`ReadPackageHook`] so the resolver can call it directly from its
/// hot loop.
///
/// The stderr forwarder JoinHandle is intentionally returned alongside
/// the host (not stored inside) so callers can `await` it after
/// dropping the host — that drain step is what makes sure every
/// `ctx.log` record reaches stdout before the next install phase
/// starts emitting its own output. Aborting in Drop would race the
/// forwarder against any sentinel lines still sitting in the OS pipe
/// buffer at host-teardown time.
pub struct ReadPackageHost {
    // Held only so Drop kills the child when the host is torn down;
    // `kill_on_drop(true)` above wires the actual kill, which closes
    // the child's stderr write end and lets the forwarder EOF cleanly.
    #[allow(dead_code)]
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    line_buf: String,
}

/// Forwarder JoinHandle returned from [`ReadPackageHost::spawn`].
/// `await` it (via [`ReadPackageHost::drain_forwarder`]) after
/// dropping the host so all buffered `ctx.log` records flush to
/// stdout before the next install phase runs.
pub type ReadPackageForwarder = tokio::task::JoinHandle<()>;

#[derive(Serialize)]
struct ReadPackageRequest<'a> {
    id: u64,
    pkg: &'a VersionMetadata,
}

#[derive(Deserialize)]
struct ReadPackageResponse {
    #[serde(default)]
    id: Option<u64>,
    #[serde(default)]
    pkg: Option<VersionMetadata>,
    #[serde(default)]
    error: Option<String>,
}

impl ReadPackageHost {
    /// Spawn the node child for `pnpmfile`. Returns `Ok(None)` if the
    /// pnpmfile does not declare a `readPackage` hook (callers can
    /// skip attaching a hook entirely in that case and save the
    /// per-call JSON round-trip), otherwise the live host paired with
    /// the stderr forwarder JoinHandle (see [`ReadPackageForwarder`]).
    /// `prefix` is the project root used to enrich `ctx.log` records
    /// under `--reporter=ndjson`.
    pub async fn spawn(
        pnpmfile: &Path,
        prefix: &Path,
    ) -> Result<Option<(Self, ReadPackageForwarder)>> {
        if !has_hook(pnpmfile, "readPackage").await? {
            return Ok(None);
        }
        tracing::debug!(
            "spawning pnpmfile readPackage host ({})",
            pnpmfile.display()
        );
        let mut cmd = tokio::process::Command::new(crate::runtime::node_program());
        cmd.arg("-e")
            .arg(read_package_shim())
            .env("AUBE_PNPMFILE", pnpmfile)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().into_diagnostic().wrap_err(
            "failed to spawn `node` for pnpmfile readPackage hook — is node installed and on PATH?",
        )?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| miette!("failed to open stdin for pnpmfile readPackage host"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| miette!("failed to open stdout for pnpmfile readPackage host"))?;
        let stderr = child.stderr.take().ok_or_else(|| {
            miette!("failed to open stderr for pnpmfile readPackage host (Stdio::piped lost?)")
        })?;
        let forwarder =
            spawn_stderr_forwarder(stderr, prefix.to_path_buf(), pnpmfile.to_path_buf());
        Ok(Some((
            Self {
                child,
                stdin,
                stdout: BufReader::new(stdout),
                next_id: 0,
                line_buf: String::new(),
            },
            forwarder,
        )))
    }

    /// Drain the forwarder, surfacing any task-panic via `tracing::warn!`.
    /// Call after the host has been dropped (or otherwise torn down) so
    /// every `ctx.log` record flushes to stdout before subsequent
    /// install phases emit their own output.
    pub async fn drain_forwarder(forwarder: ReadPackageForwarder) {
        if let Err(e) = forwarder.await {
            tracing::warn!(
                code = aube_codes::warnings::WARN_AUBE_PNPMFILE_STDERR_FORWARDER,
                "pnpmfile readPackage stderr forwarder task failed: {e}"
            );
        }
    }

    async fn call(&mut self, pkg: VersionMetadata) -> Result<VersionMetadata, String> {
        self.next_id = self.next_id.wrapping_add(1);
        let id = self.next_id;
        let req = ReadPackageRequest { id, pkg: &pkg };
        let mut line = serde_json::to_string(&req)
            .map_err(|e| format!("serialize readPackage request: {e}"))?;
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("write to readPackage host: {e}"))?;
        self.line_buf.clear();
        let n = self
            .stdout
            .read_line(&mut self.line_buf)
            .await
            .map_err(|e| format!("read from readPackage host: {e}"))?;
        if n == 0 {
            return Err(
                "readPackage host closed stdout unexpectedly (check stderr for the hook stack trace)"
                    .to_string(),
            );
        }
        let resp: ReadPackageResponse = serde_json::from_str(self.line_buf.trim_end())
            .map_err(|e| format!("parse readPackage response: {e}"))?;
        // Protocol sanity check. The resolver calls us strictly in
        // lockstep, so a mismatch here means the node shim printed an
        // extra line to stdout (usually a `require`-time warning) and
        // we've fallen out of sync. Fail loudly instead of silently
        // consuming a stale response — a future debug session will
        // thank us.
        if let Some(resp_id) = resp.id
            && resp_id != id
        {
            return Err(format!(
                "readPackage response id mismatch: sent {id}, got {resp_id} \
                 (did the pnpmfile print to stdout at require time?)"
            ));
        }
        // The hook's return value is surfaced untouched — the resolver
        // owns identity/platform restoration *after* its own warning
        // check, so pre-sanitizing here would silently swallow hook
        // edits to name/version.
        if let Some(err) = resp.error {
            return Err(err);
        }
        resp.pkg
            .ok_or_else(|| "readPackage response missing `pkg`".to_string())
    }
}

impl ReadPackageHook for ReadPackageHost {
    fn read_package<'a>(
        &'a mut self,
        pkg: VersionMetadata,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<VersionMetadata, String>> + Send + 'a>>
    {
        Box::pin(self.call(pkg))
    }
}

/// Chains multiple [`ReadPackageHost`]s so pnpm's `--global-pnpmfile`
/// composes with the local `.pnpmfile.cjs`: the global hook runs first,
/// the local hook receives the global's output, and the resolver sees
/// the final result.
///
/// With a single host this is a thin wrapper over
/// [`ReadPackageHost::call`]; the multi-host path is what makes the
/// global-plus-local pnpm test cases (hooks.ts:135 and :176) pass.
///
/// Each host is paired with the pnpmfile path it was spawned from so a
/// rejection from one node child surfaces with its source file in the
/// error string — without it, the resolver-side error reads the same
/// whether the global or local hook is to blame.
pub struct ReadPackageHostChain {
    hosts: Vec<(PathBuf, ReadPackageHost)>,
}

impl ReadPackageHostChain {
    /// Spawn one node child per pnpmfile in `paths` that declares a
    /// `readPackage` hook. Returns `Ok(None)` when nothing in the chain
    /// uses the hook (saves the resolver from per-call JSON
    /// round-trips), otherwise the live chain paired with one
    /// [`ReadPackageForwarder`] per spawned host. `prefix` is the
    /// project root used to enrich `ctx.log` records under
    /// `--reporter=ndjson`; the same value flows to every host.
    pub async fn spawn(
        paths: &[PathBuf],
        prefix: &Path,
    ) -> Result<Option<(Self, Vec<ReadPackageForwarder>)>> {
        let mut hosts = Vec::new();
        let mut forwarders = Vec::new();
        for p in paths {
            if let Some((host, forwarder)) = ReadPackageHost::spawn(p, prefix).await? {
                hosts.push((p.clone(), host));
                forwarders.push(forwarder);
            }
        }
        if hosts.is_empty() {
            return Ok(None);
        }
        Ok(Some((Self { hosts }, forwarders)))
    }

    /// Drain every forwarder JoinHandle returned by [`spawn`]. Logs any
    /// task panic via `tracing::warn!` rather than swallowing it.
    pub async fn drain_forwarders(forwarders: Vec<ReadPackageForwarder>) {
        for f in forwarders {
            ReadPackageHost::drain_forwarder(f).await;
        }
    }

    async fn call(&mut self, pkg: VersionMetadata) -> Result<VersionMetadata, String> {
        let mut current = pkg;
        for (path, host) in &mut self.hosts {
            current = host
                .call(current)
                .await
                .map_err(|e| format!("readPackage hook ({}): {e}", path.display()))?;
        }
        Ok(current)
    }
}

impl ReadPackageHook for ReadPackageHostChain {
    fn read_package<'a>(
        &'a mut self,
        pkg: VersionMetadata,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<VersionMetadata, String>> + Send + 'a>>
    {
        Box::pin(self.call(pkg))
    }
}

/// Quick scan of the pnpmfile source for a hook identifier. Avoids
/// the cost of spawning a node child when the hook doesn't exist —
/// the vast majority of pnpmfiles use only `afterAllResolved`. False
/// positives are fine: if a pnpmfile references the hook name in a
/// comment but doesn't export it, the child spawns, the hook is
/// absent, and the call short-circuits through the
/// `typeof ... === 'function'` check in the shim.
async fn has_hook(pnpmfile: &Path, name: &str) -> Result<bool> {
    let contents = tokio::fs::read_to_string(pnpmfile)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to read pnpmfile at {}", pnpmfile.display()))?;
    Ok(contents.contains(name))
}

/// Node shim that loads the pnpmfile the way pnpm does and prints `1`
/// when it exports a `hooks` object, `0` otherwise. Reuses the shared
/// [`LOAD_PNPMFILE_JS`] loader so default-vs-named export resolution
/// matches the hook-execution path exactly.
const HOOKS_GATE_SHIM: &str = r#"
loadPnpmfile(process.env.AUBE_PNPMFILE)
  .then((mod) => { process.stdout.write(mod != null && mod.hooks != null ? '1' : '0'); })
  .catch((err) => {
    console.error('[pnpmfile] failed to load for checksum gate:', (err && err.stack) || err);
    process.exit(1);
  });
"#;

/// Whether the pnpmfile, loaded the way pnpm loads it, exports a
/// `hooks` object.
///
/// pnpm records `pnpmfileChecksum` in the lockfile **only** when a
/// loaded pnpmfile actually exports hooks — `requireHooks` gates
/// `calculatePnpmfileChecksum` on `entries.some((e) => e.hooks != null)`.
/// A pnpmfile that exists but exports nothing or no hooks (e.g. an empty
/// `.pnpmfile.cjs`) yields no checksum. Stamping one regardless makes
/// pnpm abort a frozen install with `ERR_PNPM_LOCKFILE_CONFIG_MISMATCH`,
/// so this gate must look at the *export*, not at file existence.
///
/// Runs the module through [`LOAD_PNPMFILE_JS`] so
/// `module.exports = { hooks }`, `export default { hooks }`, and
/// `export const hooks = …` all count, mirroring pnpm's resolution.
pub async fn exports_hooks(pnpmfile: &Path) -> Result<bool> {
    let shim = format!("{LOAD_PNPMFILE_JS}{HOOKS_GATE_SHIM}");
    let output = tokio::process::Command::new(crate::runtime::node_program())
        .arg("-e")
        .arg(shim)
        .env("AUBE_PNPMFILE", pnpmfile)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .into_diagnostic()
        .wrap_err(
            "failed to spawn `node` to inspect pnpmfile hooks — is node installed and on PATH?",
        )?;
    if !output.status.success() {
        return Err(miette!(
            "pnpmfile {} failed to load: {}",
            pnpmfile.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout.first() == Some(&b'1'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_returns_default_when_present_and_no_override() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(PNPMFILE_CJS_NAME), "").unwrap();
        let found = detect(dir.path(), None, None);
        assert_eq!(
            found.as_deref(),
            Some(dir.path().join(PNPMFILE_CJS_NAME).as_path())
        );
    }

    #[test]
    fn detect_returns_mjs_when_only_mjs_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(PNPMFILE_MJS_NAME), "").unwrap();
        let found = detect(dir.path(), None, None);
        assert_eq!(
            found.as_deref(),
            Some(dir.path().join(PNPMFILE_MJS_NAME).as_path())
        );
    }

    #[test]
    fn detect_prefers_mjs_over_cjs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(PNPMFILE_MJS_NAME), "").unwrap();
        std::fs::write(dir.path().join(PNPMFILE_CJS_NAME), "").unwrap();
        let found = detect(dir.path(), None, None);
        assert_eq!(
            found.as_deref(),
            Some(dir.path().join(PNPMFILE_MJS_NAME).as_path())
        );
    }

    #[test]
    fn detect_returns_none_when_default_missing_and_no_override() {
        let dir = tempfile::tempdir().unwrap();
        assert!(detect(dir.path(), None, None).is_none());
    }

    #[test]
    fn detect_honors_workspace_pnpmfile_path_override() {
        // pnpm v10 allows `pnpmfilePath: config/hooks.cjs` in
        // pnpm-workspace.yaml to keep hooks outside the project root.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("config")).unwrap();
        let custom = dir.path().join("config/hooks.cjs");
        std::fs::write(&custom, "").unwrap();
        // Even though .pnpmfile.cjs doesn't exist at the default
        // location, the workspace override points at the real file.
        let found = detect(dir.path(), None, Some("config/hooks.cjs"));
        assert_eq!(found.as_deref(), Some(custom.as_path()));
    }

    #[test]
    fn detect_workspace_override_returns_none_when_target_missing() {
        // A typo in `pnpmfilePath` must surface as "not loaded" rather
        // than silently falling back to `.pnpmfile.cjs` — otherwise the
        // user thinks their hooks are running when they aren't.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(PNPMFILE_CJS_NAME), "").unwrap();
        assert!(detect(dir.path(), None, Some("typo/missing.cjs")).is_none());
    }

    #[test]
    fn detect_cli_override_beats_workspace_yaml() {
        // pnpm's `--pnpmfile <path>` flag takes precedence over the
        // workspace yaml `pnpmfilePath` entry.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("config")).unwrap();
        std::fs::create_dir(dir.path().join("ws")).unwrap();
        let cli_target = dir.path().join("config/cli.cjs");
        let yaml_target = dir.path().join("ws/yaml.cjs");
        std::fs::write(&cli_target, "").unwrap();
        std::fs::write(&yaml_target, "").unwrap();
        let found = detect(
            dir.path(),
            Some(Path::new("config/cli.cjs")),
            Some("ws/yaml.cjs"),
        );
        assert_eq!(found.as_deref(), Some(cli_target.as_path()));
    }

    #[test]
    fn detect_cli_override_returns_none_when_target_missing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(PNPMFILE_CJS_NAME), "").unwrap();
        assert!(detect(dir.path(), Some(Path::new("typo/missing.cjs")), None).is_none());
    }

    #[test]
    fn detect_cli_override_supports_absolute_path() {
        // pnpm tests pass absolute paths via `path.resolve('..',
        // '.pnpmfile.cjs')`. An absolute CLI path must NOT get joined
        // onto cwd.
        let dir = tempfile::tempdir().unwrap();
        let custom = dir.path().join("hooks.cjs");
        std::fs::write(&custom, "").unwrap();
        let found = detect(dir.path(), Some(custom.as_path()), None);
        assert_eq!(found.as_deref(), Some(custom.as_path()));
    }

    #[test]
    fn detect_global_returns_none_when_unset() {
        let dir = tempfile::tempdir().unwrap();
        assert!(detect_global(dir.path(), None).is_none());
    }

    #[test]
    fn detect_global_resolves_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let custom = dir.path().join("global.cjs");
        std::fs::write(&custom, "").unwrap();
        let found = detect_global(dir.path(), Some(custom.as_path()));
        assert_eq!(found.as_deref(), Some(custom.as_path()));
    }

    #[test]
    fn detect_global_returns_none_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(detect_global(dir.path(), Some(Path::new("nope.cjs"))).is_none());
    }

    #[test]
    fn ordered_paths_runs_global_before_local() {
        let global = PathBuf::from("/g.cjs");
        let local = PathBuf::from("/l.cjs");
        let paths = ordered_paths(Some(&global), Some(&local));
        assert_eq!(paths, vec![global.clone(), local.clone()]);
    }

    #[test]
    fn ordered_paths_skips_absent_entries() {
        let local = PathBuf::from("/l.cjs");
        assert_eq!(ordered_paths(None, Some(&local)), vec![local.clone()]);
        assert!(ordered_paths(None, None).is_empty());
    }

    #[test]
    fn default_gate_off_suppresses_cwd_default_but_default_path_still_sees_it() {
        // The embedder seam is a process-global; flip it for the body of
        // this test and restore it so sibling tests keep upstream-default
        // behavior regardless of run order.
        let prev = pnpmfile_default_enabled();
        aube_util::update_engine_context(|ctx| ctx.pnpmfile_default_enabled = false);
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join(PNPMFILE_CJS_NAME);
        std::fs::write(&f, "").unwrap();
        // detect() honors the gate: the cwd default is not loaded.
        assert!(
            detect(dir.path(), None, None).is_none(),
            "gated-off default must not load"
        );
        // default_path() ignores the gate so an embedder can warn about
        // the file it just suppressed.
        assert_eq!(default_path(dir.path()).as_deref(), Some(f.as_path()));
        aube_util::update_engine_context(|ctx| ctx.pnpmfile_default_enabled = prev);
    }

    #[test]
    fn explicit_override_loads_even_when_default_gate_off() {
        // Gating the cwd default off must NOT suppress an explicitly named
        // `--pnpmfile` path — a path the user pointed at on purpose still
        // loads regardless of the incumbent.
        let prev = pnpmfile_default_enabled();
        aube_util::update_engine_context(|ctx| ctx.pnpmfile_default_enabled = false);
        let dir = tempfile::tempdir().unwrap();
        let custom = dir.path().join("hooks.cjs");
        std::fs::write(&custom, "").unwrap();
        let found = detect(dir.path(), Some(custom.as_path()), None);
        assert_eq!(found.as_deref(), Some(custom.as_path()));
        aube_util::update_engine_context(|ctx| ctx.pnpmfile_default_enabled = prev);
    }

    /// True iff `node --version` exits 0. The `exports_hooks` tests gate
    /// on this so CI runners without node skip rather than fail.
    fn node_available() -> bool {
        std::process::Command::new(crate::runtime::node_program())
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn exports_hooks_false_for_empty_pnpmfile() {
        if !node_available() {
            eprintln!("skipping: `node` not on PATH");
            return;
        }
        // pnpm's repro: an empty `.pnpmfile.cjs` exports no hooks, so no
        // pnpmfileChecksum is recorded.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(PNPMFILE_CJS_NAME);
        std::fs::write(&p, "").unwrap();
        assert!(!exports_hooks(&p).await.unwrap());
    }

    #[tokio::test]
    async fn exports_hooks_false_when_no_hooks_export() {
        if !node_available() {
            eprintln!("skipping: `node` not on PATH");
            return;
        }
        // A pnpmfile that exports an object but no `hooks` key must not
        // get a checksum either — file existence alone is not enough.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(PNPMFILE_CJS_NAME);
        std::fs::write(&p, "module.exports = { notHooks: 1 }\n").unwrap();
        assert!(!exports_hooks(&p).await.unwrap());
    }

    #[tokio::test]
    async fn exports_hooks_true_for_cjs_hooks_object() {
        if !node_available() {
            eprintln!("skipping: `node` not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(PNPMFILE_CJS_NAME);
        std::fs::write(
            &p,
            "module.exports = { hooks: { readPackage: (pkg) => pkg } }\n",
        )
        .unwrap();
        assert!(exports_hooks(&p).await.unwrap());
    }

    #[tokio::test]
    async fn exports_hooks_true_for_esm_default_hooks() {
        if !node_available() {
            eprintln!("skipping: `node` not on PATH");
            return;
        }
        // `.mjs` with a default export carrying hooks — the shared loader
        // unwraps `default`, matching the hook-execution path.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(PNPMFILE_MJS_NAME);
        std::fs::write(
            &p,
            "export default { hooks: { readPackage(pkg) { return pkg } } }\n",
        )
        .unwrap();
        assert!(exports_hooks(&p).await.unwrap());
    }
}
