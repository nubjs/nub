//! nub-launcher — the runtime half of `nub compile` (the denort-style model).
//!
//! At compile time `nub compile` injects a payload section into a copy of this
//! binary (JS bundle, and for the default shape a zstd-compressed Node) and ad-hoc
//! re-signs it. At runtime the launcher:
//!   1. reads its own payload section,
//!   2. acquires Node — `embed` shape decompresses the bundled Node into nub's
//!      shared cache once (skipping when a compatible Node is already cached);
//!      `smol` shape discovers an existing Node, else downloads one via
//!      curl/wget and extracts it through nub-core's capped archive reader,
//!   3. extracts the bundled app into the cache,
//!   4. injects version-appropriate Node flags via nub-core's `compute_inject_flags`,
//!   5. spawns Node on the app entry, forwarding signals + the exit code
//!      (nub-core's `status_forwarding_signals`, the same machinery as `nub run`).
//!
//! Node itself is bit-exact official Node — never patched. The compile preamble
//! restores `process.execPath` to this outer compiled launcher, matching the
//! process identity exposed by other compiled executable runtimes.

// Match the workspace convention (nub-core/nub-cli): collapsing nested `if let {
// if }` into let-chains is cosmetic churn, so allow it.
#![allow(clippy::collapsible_if)]

mod cache;
mod ui;

use std::borrow::Cow;
use std::fs;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use nub_core::compile::{self, AppFile, Manifest, PayloadView, Shape, is_safe_relative_name};
use nub_core::node::{
    discovery, flags, spawn,
    version::{NodeVersion, VersionPin},
};
use nub_core::version_management::{extract_verified_node_archive, parse_target_spec};
use ui::FirstRun;

/// Internal launcher control channel. The values are set only by Nub's compile
/// verifier and parent-death reaper; application arguments are never reserved.
const INTERNAL_MODE_ENV: &str = "__NUB_COMPILED_LAUNCHER_MODE";
/// Private process-identity channel. The launcher always overwrites this on the
/// child Command, so an inherited value can never spoof the compiled executable.
const COMPILED_EXEC_PATH_ENV: &str = "__NUB_COMPILED_EXEC_PATH";
/// Interrupted staging trees are never current after this age. The bounded
/// cleanup is deliberately limited to the launcher's own temp prefixes.
const ORPHAN_STAGE_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// How long a PUBLISHED extraction survives without being the tree this artifact
/// resolves to. Nothing else reclaims these: an app tree is ~the bundle and a Node
/// tree is ~107 MB, and a machine that compiles repeatedly accumulates one per
/// distinct payload forever. Eviction is never incorrect — the trees are content
/// keyed and re-extract on demand — so the cost of being wrong is one slow start,
/// which is why this is safe to do automatically. 30 days is long enough that an
/// artifact used even monthly keeps its tree.
const UNUSED_CACHE_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

#[derive(Debug, PartialEq, Eq)]
enum InternalMode {
    Probe,
    PdeathWatch,
    Licenses,
}

fn internal_mode(args: &[String], mode: Option<&str>) -> Option<InternalMode> {
    match mode {
        // The verifier supplies no application arguments. A user running an app
        // with an empty argv never has this internal environment value.
        Some("probe") if args.len() == 1 => Some(InternalMode::Probe),
        // Release CI's embedded-notice gate. It rides this private channel
        // rather than a reserved argument spelling so that a compiled app keeps
        // its whole argv surface: no flag a publisher might already use is
        // intercepted, at any argument count.
        Some("licenses") if args.len() == 1 => Some(InternalMode::Licenses),
        // PID and read-fd remain ordinary positional values; only this private
        // environment channel selects their interpretation.
        Some("pdeath-watch") if args.len() == 3 => Some(InternalMode::PdeathWatch),
        _ => None,
    }
}

fn main() {
    std::process::exit(run());
}

/// Startup phase timing, emitted to stderr only when `__NUB_LAUNCHER_TIMING` is set.
///
/// A compiled artifact's warm start is the product's headline number and it is
/// assembled from a dozen small steps across two processes, so "which step" is not
/// answerable by reading the code — every attempt to attribute it by reasoning has
/// been wrong. This makes the split observable in the shipped binary at the cost of
/// one env lookup per phase on a path that is already doing filesystem work.
static LAUNCH_T0: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
static TIMING_ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

fn timing_enabled() -> bool {
    *TIMING_ON.get_or_init(|| std::env::var_os("__NUB_LAUNCHER_TIMING").is_some())
}

fn phase(name: &str) {
    if !timing_enabled() {
        return;
    }
    let t0 = LAUNCH_T0.get_or_init(std::time::Instant::now);
    eprintln!(
        "[nub-timing] {:>8.2} ms  {name}",
        t0.elapsed().as_secs_f64() * 1000.0
    );
}

/// `phase` for a message that costs something to build. The closure is not called
/// when timing is off, so the `format!` never allocates on a normal launch — which
/// matters because these sit inside the per-file cache comparison.
fn phase_with(f: impl FnOnce() -> String) {
    if timing_enabled() {
        phase(&f());
    }
}

fn run() -> i32 {
    // nub-core's signal-forwarding spawn plants a macOS SIGKILL-backstop watcher by
    // re-invoking `current_exe <pgid> <fd>` under the private pdeath-watch mode —
    // which, for a compiled binary, IS this launcher. Dispatch that mode to the
    // watcher entry instead of decoding the payload and re-launching the app.
    LAUNCH_T0.get_or_init(std::time::Instant::now);
    phase("process entry");
    let args: Vec<String> = std::env::args().collect();
    match internal_mode(&args, std::env::var(INTERNAL_MODE_ENV).ok().as_deref()) {
        #[cfg(unix)]
        Some(InternalMode::PdeathWatch) => return spawn::run_pdeath_watch(&args[1..]),
        Some(InternalMode::Probe) => {
            // Compile-time self-check: `nub compile` runs this on the produced
            // binary to catch an under-padded / corrupt section injection (which
            // traps with SIGILL) BEFORE shipping it to the user. Exercises the
            // section-read + decode + String allocation path without extracting
            // or spawning Node.
            return probe();
        }
        Some(InternalMode::Licenses) => return print_licenses(),
        _ => {}
    }

    let section = match libsui::find_section(compile::SECTION_NAME) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            eprintln!(
                "nub: this executable carries no compiled payload (it is a bare launcher template)"
            );
            return 70;
        }
        Err(e) => {
            eprintln!("nub: could not read the compiled payload: {e}");
            return 70;
        }
    };

    phase("section found");
    let view = match compile::decode(section) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("nub: {e:#}");
            return 70;
        }
    };
    phase("payload decoded");

    let launcher_path =
        match std::env::current_exe().context("resolving the compiled executable path") {
            Ok(path) => path,
            Err(e) => {
                eprintln!("nub: {e:#}");
                return 1;
            }
        };

    phase("current_exe resolved");
    match launch(&view, &launcher_path) {
        Ok(status) => exit_code(status),
        Err(e) => {
            eprintln!("nub: {e:#}");
            1
        }
    }
}

fn launch(view: &PayloadView<'_>, launcher_path: &Path) -> Result<ExitStatus> {
    // Prove a usable external `--smol` Node before resolving the cache. That
    // Node executes outside the cache, so its app payload may live on a writable
    // noexec volume. Cache-owned Nodes and every provisioning path still require
    // an executable cache. The discovery result is threaded into acquisition so
    // PATH's `node --version` is never spawned twice.
    let external_smol = match view.manifest.shape {
        Shape::Smol => discover_external_smol_node(&view.manifest)?,
        Shape::Embed => None,
    };
    let cache_use = cache_use_for(
        &view.manifest.shape,
        external_smol.is_some(),
        view.has_executable_file(),
    );

    // Resolved ONCE and threaded through: every write this process makes lands
    // under the one directory that was proven writable, and the probe (a mkdir
    // plus a zero-byte file) is not repeated per payload. A candidate already
    // holding this payload's artifacts skips the write half of that probe.
    let resolved = cache::resolve(cache_use, &|dir: &Path| {
        if external_smol.is_some() {
            app_cache_is_ready(view, &app_cache_dir(dir, &view.manifest))
                .then_some(CacheWarm::AppOnly)
        } else {
            verify_warm_cache(view, dir).map(CacheWarm::Managed)
        }
    })?;
    phase("cache::resolve (incl. warm verify)");
    let base = resolved.path;
    cleanup_launcher_orphans(&base, &view.manifest);
    phase("orphan cleanup");
    let notice = FirstRun::new(view.manifest.install_message.as_deref());

    let ((node_path, version, origin), app_dir) = match (external_smol, resolved.warm) {
        (Some((node_path, version)), Some(CacheWarm::AppOnly)) => (
            (node_path, version, NodeOrigin::Discovered),
            app_cache_dir(&base, &view.manifest),
        ),
        (Some((node_path, version)), None) => (
            (node_path, version, NodeOrigin::Discovered),
            ensure_app(view, &base)?,
        ),
        (None, Some(CacheWarm::Managed(warm))) => {
            phase("ARM: warm managed (fast path)");
            let version = view
                .manifest
                .node_version
                .parse()
                .unwrap_or_else(|_| NodeVersion::new(22, 15, 0));
            ((warm.node_path, version, NodeOrigin::Managed), warm.app_dir)
        }
        (None, None) => {
            phase("ARM: COLD — acquire_node + ensure_app");
            let n = acquire_node(view, &base, &notice, None)?;
            phase("  acquire_node done");
            let a = ensure_app(view, &base)?;
            phase("  ensure_app done");
            (n, a)
        }
        // The warm callback ties its variant to `external_smol`; retaining this
        // arm makes a future callback refactor fail closed rather than executing
        // a cache Node from a data-only cache.
        _ => bail!("cache warm state did not match the selected Node source"),
    };
    // Resolution proves the whole cache namespace cannot be renamed by another
    // principal. Repeat the read-only gate after extraction and immediately
    // before handing its paths to Command so deployment-time ACL/mode changes
    // cannot turn a validated cache into a cross-principal execution handoff.
    phase("node + app resolved");
    cache::revalidate(&base).context("revalidating the executable cache namespace")?;
    phase("cache revalidated");
    // Hand the terminal back BEFORE anything the app might print — the box lives
    // on the alternate screen, so this restores the user's scrollback intact.
    notice.finish();
    // `cache::resolve` canonicalizes `app_dir`'s root, so these are absolute paths
    // to files already covered by the exact payload-cache verification. They are
    // the only two cache paths that leave Rust and become Node arguments, so the
    // verbatim spelling stops here (see `node_argument`).
    let entry = node_argument(&app_dir.join(&view.manifest.entry), cfg!(windows));
    let bootstrap = node_argument(
        &app_dir.join(compile::COMPILE_BOOTSTRAP_NAME),
        cfg!(windows),
    );

    let user_args: Vec<String> = std::env::args().skip(1).collect();
    let node_options = std::env::var("NODE_OPTIONS").ok();
    // A DISCOVERED Node is intersected with what the binary actually accepts; a
    // MANAGED one takes the version band alone. The probe is what stops an
    // open-ended `Unflag [lo, ∞)` band from injecting a flag Node has since
    // hard-removed (`--experimental-permission` died at 24.0) and aborting startup
    // — and it is also the backstop for a version inferred from a directory name
    // that lies. Skipping it for a Node nub embedded or just provisioned keeps the
    // common path at zero extra spawns; `accepted_env_flags` caches per
    // (path, mtime), so even the discovered path pays the probe once.
    let accepted = match origin {
        NodeOrigin::Managed => None,
        NodeOrigin::Discovered => discovery::accepted_env_flags(&node_path),
    };
    let mut inject = flags::compute_inject_flags(
        version.clone(),
        // The compiled entry is already the first positional argument to Node;
        // everything in `user_args` follows it and is application argv, not Node
        // argv. Only inherited NODE_OPTIONS is a user-controlled Node flag channel.
        &[],
        node_options.as_deref(),
        false,
        accepted.as_ref(),
    );
    // Compiled launchers need the closed 22.4–<25 experimental flag band for
    // sessionStorage. Their compile preamble can neutralize the flag's throwing
    // localStorage getter, but this launcher never synthesizes a storage file or
    // claims persistent localStorage is available.
    let (inject_webstorage, neutralize_localstorage) =
        compile_webstorage_policy(&version, node_options.as_deref(), accepted.as_ref());
    if inject_webstorage {
        inject.push("--experimental-webstorage");
    }
    // Matrix-derived ARGV-only V8 unflags (`Mitigation::UnflagArgv`). These bypass
    // `compute_inject_flags` for the same reason they do in nub-core's spawn path:
    // Node refuses them in NODE_OPTIONS, so they are absent from the accepted-flag
    // set that gates Stage 4. `user_args` is empty here — everything after the
    // compiled entry is application argv — so no user polarity can be present.
    // Skip the probe for a Node nub provisioned or embedded itself, matching why
    // `accepted_env_flags` is skipped above: its accepted flags follow from its version,
    // and the launcher is the hot path.
    let argv_probe_path = match origin {
        NodeOrigin::Managed => None,
        NodeOrigin::Discovered => Some(node_path.as_path()),
    };
    let argv_only = flags::argv_inject_flags(argv_probe_path, &version, &[]);
    inject.extend(argv_only.iter().copied());

    let mut cmd = Command::new(node_path.as_os_str());
    // Node runs CommonJS preloads before ESM `--import` hooks, including ones
    // inherited through NODE_OPTIONS. Keep this absolute payload preload ahead
    // of Nub's injected flags and the compiled entry on every supported Node.
    cmd.arg(compiled_bootstrap_require_arg(&bootstrap));
    // argv0 fidelity: process.argv0 / process.title report "node", matching
    // nub-core's spawn path. The compile preamble separately restores execPath
    // to this outer artifact.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.arg0("node");
    }
    for flag in &inject {
        cmd.arg(flag);
    }
    // `--node-flag`, baked in at compile time. AFTER nub's own injected flags,
    // because Node takes the last occurrence of a repeated flag — so a publisher
    // who asked for one gets it even where nub injects the opposite. Still ahead
    // of the entry, which is where application argv begins.
    for flag in &view.manifest.node_flags {
        // Exact duplicates only. A repeated boolean flag is noise in
        // `process.execArgv`; a flag that merely shares a NAME with an injected
        // one (`--max-old-space-size=…`) must still be emitted, because coming
        // last is how it wins.
        if inject.iter().any(|injected| injected == flag) {
            continue;
        }
        cmd.arg(flag);
    }
    cmd.arg(&entry);
    cmd.args(&user_args);
    configure_compiled_process_identity(&mut cmd, launcher_path);
    configure_compiled_compile_cache(&mut cmd, &base, &view.manifest);
    if neutralize_localstorage {
        // The compile preamble consumes this internal signal before application
        // code runs, removing Node 22.4–24's throwing localStorage getter. A
        // real --localstorage-file in NODE_OPTIONS leaves the getter untouched;
        // this launcher never creates a storage file itself.
        cmd.env(flags::NEUTRALIZE_LOCALSTORAGE_ENV, "1");
    }
    // Same execArgv-hiding signal the ordinary spawn path sets: a compiled app that
    // forwards `process.execArgv` into a Worker would otherwise hand Node a flag it
    // refuses in NODE_OPTIONS. See `flags::ARGV_ONLY_FLAGS_ENV`.
    if !argv_only.is_empty() {
        cmd.env(flags::ARGV_ONLY_FLAGS_ENV, argv_only.join(" "));
    }
    cmd.stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());

    // Own process group + terminating/diagnostic-signal forwarding + TTY
    // foreground handoff + macOS SIGKILL backstop — the same faithful spawn
    // `nub run`'s file path uses. NODE_OPTIONS is inherited untouched (honored).
    phase_with(|| {
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let envs: Vec<String> = cmd
            .get_envs()
            .map(|(k, v)| {
                format!(
                    "{}={}",
                    k.to_string_lossy(),
                    v.map(|v| v.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "<removed>".into())
                )
            })
            .collect();
        format!(
            "about to spawn node\n  argv: {}\n  env:  {}",
            args.join(" "),
            envs.join(" ")
        )
    });
    let status = spawn::status_forwarding_signals(&mut cmd)
        .map_err(|error| node_spawn_error(&node_path, &base, error));
    phase("node exited");
    status
}

fn node_spawn_error(node_path: &Path, base: &Path, error: std::io::Error) -> anyhow::Error {
    #[cfg(not(unix))]
    let _ = base;
    // Checked first: this failure also presents as a plain OS error about a path
    // that is right there on disk, and only the length explains it.
    #[cfg(windows)]
    if let Some(remedy) = long_path_remedy(node_path, &error) {
        return anyhow!(remedy);
    }
    // Windows has no mount-level noexec state: ERROR_ACCESS_DENIED there is an
    // ACL, application-control, or antivirus failure and must retain its real
    // error. Other Unix hosts use this exec-time fallback when no mount query is
    // available; Linux/macOS normally reject the mount during cache resolution.
    #[cfg(unix)]
    if cache::exec_denied(&error) && node_path.starts_with(base) {
        return anyhow!(cache::noexec_remedy(base));
    }
    anyhow::Error::new(error).context("spawning Node")
}

/// Re-spell a cache path as something Node can load.
///
/// Rust's `fs::canonicalize` — which `cache::resolve` uses to prove the cache
/// namespace cannot be renamed underneath us — returns Windows' verbatim `\\?\`
/// form, and Node cannot resolve a module through one: CJS resolution ends in
/// `fs.realpathSync`, which lstats the path's root and dies
/// `EISDIR: … lstat 'C:'` (mechanism in `spawn::strip_verbatim`). It failed on
/// the launcher's own `--require` preload, so no compiled artifact ran at all on
/// Windows.
///
/// The strip happens HERE, at the argument boundary, rather than inside
/// `cache::resolve`: the canonical spelling is what the cache's ownership/DACL
/// checks and `starts_with` containment tests are written against, and nothing
/// on the Rust side is helped by changing it. Nothing is lost past MAX_PATH
/// either — libuv re-applies the namespace prefix itself for every path it opens.
///
/// Pure over `windows` so both branches test on any host.
fn node_argument(path: &Path, windows: bool) -> PathBuf {
    spawn::strip_verbatim_path(path, windows)
}

/// Build the absolute CommonJS preload argument for the cache-verified compile
/// bootstrap. This must remain a CLI argument rather than NODE_OPTIONS so Node
/// runs it before inherited user ESM imports and loaders.
fn compiled_bootstrap_require_arg(bootstrap: &Path) -> std::ffi::OsString {
    let mut arg = std::ffi::OsString::from("--require=");
    arg.push(bootstrap);
    arg
}

/// Point Node's compile cache at a directory BESIDE the app extraction, never
/// inside it, unless the user chose their own.
///
/// `nub run` already gets one (`spawn.rs`'s `<cache>/nub/v8-compile-cache`); a
/// compiled artifact got none, so it re-compiled the same bundle on every launch.
/// It must live OUTSIDE the extracted tree. `app_cache_is_ready` requires that
/// directory to hold exactly the payload's files and nothing else, so a compile
/// cache written inside it makes the tree mismatch on every launch — and the app
/// is then re-extracted every time, forever, because the fix-up re-creates the
/// very directory that breaks the check. Measured at ~25 ms per launch on a
/// one-file fixture before this was moved out, and the cache itself never
/// survived, so the feature cost time instead of saving it.
///
/// Keyed by `app_sha256` for the same reason the extraction is: a rebuilt artifact
/// lands on a different key and cannot read a stale cache, so there is nothing to
/// invalidate. Respect an inherited value — a user pointing
/// this somewhere deliberately, or disabling it with `0`, outranks our default.
fn configure_compiled_compile_cache(cmd: &mut Command, base: &Path, manifest: &Manifest) {
    if std::env::var_os("NODE_COMPILE_CACHE").is_some() {
        return;
    }
    cmd.env("NODE_COMPILE_CACHE", compile_cache_dir(base, manifest));
}

/// Where Node's compile cache lives: beside the app extraction, never inside it.
/// Keyed by `app_sha256` exactly as the extraction is, so a rebuilt artifact lands
/// on a fresh key and can never read a stale cache.
fn compile_cache_dir(base: &Path, manifest: &Manifest) -> PathBuf {
    base.join("compile-v8")
        .join(short_key(&manifest.app_sha256))
}

fn configure_compiled_process_identity(cmd: &mut Command, launcher_path: &Path) {
    // `Command::env` replaces an inherited value of the same name. Keep the
    // value in child environments: Node workers, fork(), and re-exec inherit it
    // until the preamble establishes their public process identity.
    cmd.env(COMPILED_EXEC_PATH_ENV, launcher_path);
    // The mode channel is consumed HERE and must not survive into the app. `nub`
    // itself honors it — nub-cli's pdeath-watch dispatch reads this same variable
    // — so an inherited value would let an ambient env var reinterpret an ordinary
    // `nub run <script>` inside the compiled app as an internal mode. Same
    // reasoning as the identity var above: a private channel is set by its owner,
    // never inherited from whatever launched us.
    cmd.env_remove(INTERNAL_MODE_ENV);
}

/// The compiled launcher gives Node its entry before every user argument, so
/// those later values are application argv rather than Node argv. Inherited
/// `NODE_OPTIONS` is the sole user Node-flag channel for this shape.
fn compile_webstorage_policy(
    node_version: &NodeVersion,
    node_options: Option<&str>,
    accepted_env_flags: Option<&std::collections::BTreeSet<String>>,
) -> (bool, bool) {
    // Match `compute_inject_flags`' Stage 4: a discovered Node's own
    // `allowedNodeEnvironmentFlags` is authoritative over our version band.
    // The webstorage flag is intentionally computed outside that helper so it
    // can honor an explicit user polarity, so apply the same intersection here.
    let accepted =
        accepted_env_flags.is_none_or(|flags| flags.contains("--experimental-webstorage"));
    let inject =
        accepted && flags::should_inject_experimental_webstorage(node_version, &[], node_options);
    let neutralize = inject
        && flags::should_neutralize_experimental_webstorage_localstorage(
            node_version,
            &[],
            node_options,
        );
    (inject, neutralize)
}

/// Classify the Windows path-length failure, which otherwise reports an error
/// that flatly contradicts the filesystem.
///
/// Rust's `std::fs` and Node both prefix `\\?\`, so extraction writes — and asset
/// reads open — a 267-character `node.exe` without complaint. `CreateProcessW`
/// does NOT accept that prefix, which leaves this spawn as the single place
/// MAX_PATH still binds, and it fails with `ERROR_PATH_NOT_FOUND` ("the system
/// cannot find the path specified") for a file this process just created.
/// Measured on Windows Server 2022 with `LongPathsEnabled=0`: a 210-character
/// cache root spawns, 220 does not.
#[cfg(windows)]
fn long_path_remedy(node_path: &Path, e: &std::io::Error) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;

    const ERROR_PATH_NOT_FOUND: i32 = 3;
    const MAX_PATH: usize = 260;
    // `\compile-node\<version>-<hash>\node.exe`, appended to the cache root.
    const APPENDED: usize = 47;
    // The limit is applied AFTER normalization — an 8.3-shortened or junction
    // component expands — so the length measurable here is a floor and the
    // classification claims a margin below the limit. Further down than this,
    // ERROR_PATH_NOT_FOUND means what it says and keeps the generic message.
    const MARGIN: usize = 16;

    if e.raw_os_error() != Some(ERROR_PATH_NOT_FOUND) {
        return None;
    }
    // MAX_PATH counts UTF-16 code units, not bytes.
    let len = node_path.as_os_str().encode_wide().count();
    if len + MARGIN < MAX_PATH {
        return None;
    }
    Some(format!(
        "the extracted Node's path is {len} characters, at or past Windows' \
         MAX_PATH limit of {MAX_PATH}, so it cannot be started even though it \
         exists:\n\
         \x20   {path}\n\
         \x20 Any one of these fixes it:\n\
         \x20   - use a shorter standard cache or user-profile path: the launcher \
         appends {APPENDED} characters below it, so the cache root has a \
         {budget}-character budget\n\
         \x20   - build with --smol against a Node already installed on the box, \
         which runs that Node where it sits instead of extracting one",
        path = node_path.display(),
        budget = MAX_PATH - APPENDED,
    ))
}

/// The accepted paths in a complete embed-shape cache. Carrying these out of
/// cache selection avoids revalidating the Node and app tree twice on every warm
/// launch.
struct VerifiedWarmCache {
    node_path: PathBuf,
    app_dir: PathBuf,
}

/// Verify that `dir` already holds every artifact this run needs, so the run
/// writes nothing and the cache resolver may admit the directory without proving
/// it is writable.
///
/// The two conditions mirror the early returns in `acquire_embedded_node` and
/// `ensure_app` — via the same path helpers, so a warm verdict and extraction
/// cannot disagree. A marker is only the publication barrier; the manifest's
/// Node metadata and the exact payload-file checks remain the acceptance policy.
///
/// An already provisioned official Node needs no compile-cache marker because
/// its own store is the complete artifact and `acquire_embedded_node` returns it
/// directly. `smol` never qualifies: its Node is discovered or PROVISIONED, and
/// provisioning writes.
fn verify_warm_cache(view: &PayloadView<'_>, dir: &Path) -> Option<VerifiedWarmCache> {
    let m = &view.manifest;
    let app_dir = app_cache_dir(dir, m);
    if m.shape != Shape::Embed || !app_cache_is_ready(view, &app_dir) {
        return None;
    }

    let node_dir = node_cache_dir(dir, m);
    let node_path = if embedded_node_cache_is_ready(m, &node_dir) {
        phase("  warm: node cache READY");
        node_dir.join(node_exe_name())
    } else if path_entry_exists(&node_dir) {
        phase("  warm: FAIL — node dir exists but is not ready");
        return None;
    } else {
        phase("  warm: node dir absent, trying official store");
        match official_node_for_manifest(dir, m) {
            Some(p) => p,
            None => {
                phase("  warm: FAIL — no official node either");
                return None;
            }
        }
    };
    phase("  warm: VERIFIED");
    Some(VerifiedWarmCache { node_path, app_dir })
}

/// Publication writes this root-reserved marker only after every staged file has
/// been flushed and the tree synced. It proves publication completed, not that
/// each cached entry still meets its acceptance policy; the ready predicates
/// below revalidate that separately.
const CACHE_COMPLETE_MARKER: &str = ".nub-complete";

fn completion_marker_is_ready(cache_dir: &Path) -> bool {
    let marker = cache_dir.join(CACHE_COMPLETE_MARKER);
    fs::symlink_metadata(&marker).is_ok_and(|metadata| {
        metadata_is_trusted_regular_file(&marker, &metadata) && metadata.len() == 0
    })
}

/// A completed embedded-Node cache is exactly one trusted regular executable plus
/// the empty completion marker. Current payloads check its expected length; a
/// payload predating `node_size` falls back to its content digest.
fn embedded_node_cache_is_ready(manifest: &Manifest, cache_dir: &Path) -> bool {
    if !cache_artifact_directory_is_trusted(cache_dir) || !completion_marker_is_ready(cache_dir) {
        return false;
    }

    let expected_node = std::ffi::OsStr::new(node_exe_name());
    let marker = std::ffi::OsStr::new(CACHE_COMPLETE_MARKER);
    let mut saw_node = false;
    let mut saw_marker = false;
    let Ok(entries) = fs::read_dir(cache_dir) else {
        return false;
    };
    for entry in entries {
        let Ok(entry) = entry else { return false };
        let name = entry.file_name();
        if name.as_os_str() == marker {
            if saw_marker || !completion_marker_is_ready(cache_dir) {
                return false;
            }
            saw_marker = true;
        } else if name.as_os_str() == expected_node {
            if saw_node
                || !is_executable_file(&entry.path())
                || !embedded_node_file_is_ready(&entry.path(), manifest)
            {
                return false;
            }
            saw_node = true;
        } else {
            return false;
        }
    }
    saw_node && saw_marker
}

/// A completed app cache is an exact materialization of `PayloadView::app_files`
/// plus the empty marker: no changed bytes, symlinks, special files, empty extra
/// directories, or package/module-resolution inputs absent from the payload.
fn app_cache_is_ready(view: &PayloadView<'_>, cache_dir: &Path) -> bool {
    if !cache_artifact_directory_is_trusted(cache_dir) {
        phase("  app_cache: NOT READY (directory not trusted)");
        return false;
    }
    if !completion_marker_is_ready(cache_dir) {
        phase("  app_cache: NOT READY (completion marker)");
        return false;
    }

    let mut expected = std::collections::BTreeMap::new();
    for file in &view.app_files {
        if !is_safe_relative_name(&file.name) || file.name == CACHE_COMPLETE_MARKER {
            return false;
        }
        if expected
            .insert(
                PathBuf::from(&file.name),
                (
                    match app_bytes(&view.manifest, file) {
                        Ok(bytes) => bytes,
                        Err(_) => return false,
                    },
                    file.executable,
                ),
            )
            .is_some()
        {
            return false;
        }
    }
    if !expected.contains_key(Path::new(&view.manifest.entry)) {
        phase("  app_cache: NOT READY (entry missing from payload)");
        return false;
    }
    phase("  app_cache: expected map built");
    let matched = app_cache_tree_matches(cache_dir, cache_dir, &mut expected);
    if !matched {
        phase("  app_cache: NOT READY (tree mismatch)");
    } else if !expected.is_empty() {
        phase("  app_cache: NOT READY (files missing on disk)");
    } else {
        phase("  app_cache: READY");
    }
    matched && expected.is_empty()
}

fn app_cache_tree_matches(
    root: &Path,
    dir: &Path,
    expected: &mut std::collections::BTreeMap<PathBuf, (Cow<'_, [u8]>, bool)>,
) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    for entry in entries {
        let Ok(entry) = entry else { return false };
        let path = entry.path();
        let Ok(relative) = path.strip_prefix(root) else {
            return false;
        };
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            return false;
        };

        if metadata_is_trusted_real_directory(&path, &metadata) {
            if !expected.keys().any(|name| name.starts_with(relative)) {
                phase_with(|| format!("    MISMATCH: unexpected directory {relative:?}"));
                return false;
            }
            if !app_cache_tree_matches(root, &path, expected) {
                return false;
            }
        } else if metadata_is_trusted_regular_file(&path, &metadata) {
            if relative == Path::new(CACHE_COMPLETE_MARKER) {
                if !completion_marker_is_ready(root) {
                    return false;
                }
                continue;
            }
            let Some((bytes, executable)) = expected.remove(relative) else {
                phase_with(|| format!("    MISMATCH: unexpected file on disk {relative:?}"));
                return false;
            };
            if !regular_file_matches_bytes(&path, &bytes) {
                phase_with(|| {
                    format!(
                        "    MISMATCH: bytes differ for {relative:?} (disk {} vs expected {})",
                        metadata.len(),
                        bytes.len()
                    )
                });
                return false;
            }
            if executable && !file_metadata_is_executable(&metadata) {
                phase_with(|| format!("    MISMATCH: exec bit for {relative:?}"));
                return false;
            }
        } else {
            // Includes symlinks, junction-like reparse entries, sockets, and FIFOs.
            phase_with(|| format!("    MISMATCH: not a trusted regular file/dir {relative:?}"));
            return false;
        }
    }
    true
}

fn is_real_directory(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata_is_trusted_real_directory(path, &metadata))
}

fn cache_artifact_directory_is_trusted(path: &Path) -> bool {
    is_real_directory(path) && path.parent().is_some_and(is_real_directory)
}

fn metadata_is_trusted_real_directory(path: &Path, metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_dir()
        && !metadata_is_windows_reparse_point(metadata)
        && cache_metadata_is_trusted(path, metadata)
}

fn metadata_is_trusted_regular_file(path: &Path, metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_file()
        && !metadata_is_windows_reparse_point(metadata)
        && cache_metadata_is_trusted(path, metadata)
}

#[cfg(unix)]
fn cache_metadata_is_trusted(_path: &Path, metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    metadata.uid() == unsafe { libc::geteuid() } && metadata.permissions().mode() & 0o022 == 0
}

/// Whether an already-extracted file carries an executable bit.
///
/// Deliberately consulted in ONE direction only — a payload file marked
/// executable must have the bit, and a file the payload does not mark is not
/// checked. A cache extracted by a launcher that predates per-file modes is
/// repaired by the first half; the second half would permanently disarm the warm
/// path on a filesystem that synthesizes modes (msdos/exFAT mounts report every
/// file 0o755), turning every run into a re-extraction. What it would buy is
/// nothing the threat model needs: only the owning uid can write into the tree,
/// which `cache_metadata_is_trusted` already requires.
#[cfg(unix)]
fn file_metadata_is_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;

    metadata.permissions().mode() & 0o111 != 0
}

/// Windows has no executable mode bit, so there is nothing to verify.
#[cfg(not(unix))]
fn file_metadata_is_executable(_metadata: &fs::Metadata) -> bool {
    true
}

#[cfg(windows)]
fn cache_metadata_is_trusted(path: &Path, _metadata: &fs::Metadata) -> bool {
    cache::object_is_stable(path).unwrap_or(false)
}

#[cfg(not(any(unix, windows)))]
fn cache_metadata_is_trusted(_path: &Path, _metadata: &fs::Metadata) -> bool {
    true
}

#[cfg(windows)]
fn metadata_is_windows_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn metadata_is_windows_reparse_point(_metadata: &fs::Metadata) -> bool {
    false
}

fn path_entry_exists(path: &Path) -> bool {
    match fs::symlink_metadata(path) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        // An unreadable entry is not safely equivalent to an absent one.
        Err(_) => true,
    }
}

fn regular_file_matches_bytes(path: &Path, expected: &[u8]) -> bool {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata_is_trusted_regular_file(path, &metadata) => metadata,
        _ => return false,
    };
    if metadata.len() != expected.len() as u64 {
        return false;
    }

    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    let mut buffer = [0u8; 64 * 1024];
    for chunk in expected.chunks(buffer.len()) {
        if file.read_exact(&mut buffer[..chunk.len()]).is_err() || &buffer[..chunk.len()] != chunk {
            return false;
        }
    }
    let mut extra = [0u8; 1];
    matches!(file.read(&mut extra), Ok(0))
}

/// Does this extracted Node satisfy the payload format's acceptance policy?
///
/// Size, not a digest. The digest is already in the PATH — the tree lives at
/// `compile-node/<version>-<short_key(node_sha256)>` — so re-hashing established no
/// identity the directory name did not already assert. It only detected a change
/// since extraction, and against a same-uid attacker it detected nothing worth
/// having: `metadata_is_trusted_regular_file` has already established the file is
/// owner-only and non-group-writable, and the same uid can rewrite the artifact
/// binary itself far more cheaply than it can doctor this file.
///
/// The failure this DOES need to catch is real and observed: an OS or antivirus
/// sweep truncating or clearing cached files. That is what drove `vercel/pkg` to
/// ship an always-copy warm path and then revert it, and macOS `~/Library/Caches`
/// is purgeable, so nub sits in the same blast radius. Truncation changes the size.
///
/// `compile` has not shipped yet, so there is no released payload format predating
/// `node_size`. Zero is rejected rather than carrying an unreachable digest fallback.
fn embedded_node_file_is_ready(path: &Path, manifest: &Manifest) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    if !metadata_is_trusted_regular_file(path, &metadata) {
        return false;
    }
    manifest.node_size != 0 && metadata.len() == manifest.node_size
}

/// Where this payload's embedded Node extracts to, keyed by content so two
/// binaries carrying the same Node share one extraction.
fn node_cache_dir(base: &Path, m: &Manifest) -> PathBuf {
    base.join("compile-node")
        .join(format!("{}-{}", m.node_version, short_key(&m.node_sha256)))
}

/// Where this payload's app files extract to, keyed the same way.
fn app_cache_dir(base: &Path, m: &Manifest) -> PathBuf {
    base.join("compile-app").join(short_key(&m.app_sha256))
}

/// The private probe-mode self-check `nub compile` runs on the produced binary
/// at compile time. Reads + decodes the injected section and touches a `String`
/// allocation (the exact code an under-padded libsui injection corrupts into a
/// SIGILL trap), then prints a stable marker — WITHOUT extracting or spawning
/// Node.
fn probe() -> i32 {
    let Ok(Some(section)) = libsui::find_section(compile::SECTION_NAME) else {
        eprintln!("nub-probe: no payload section");
        return 70;
    };
    match compile::decode(section) {
        Ok(view) => {
            let key = short_key(&view.manifest.app_sha256);
            println!("nub-probe ok {} {}", view.manifest.entry, key);
            0
        }
        Err(e) => {
            eprintln!("nub-probe: {e:#}");
            70
        }
    }
}

/// Stream the embedded Node notice for release CI's license gate. Like `probe`,
/// this reads and decodes the payload section and touches nothing else: no cache
/// resolution, no extraction, no Node. Reached only through the private control
/// channel, so it costs the compiled application no argument spelling.
fn print_licenses() -> i32 {
    let section = match libsui::find_section(compile::SECTION_NAME) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            eprintln!(
                "nub: this executable carries no compiled payload (it is a bare launcher template)"
            );
            return 70;
        }
        Err(e) => {
            eprintln!("nub: could not read the compiled payload: {e}");
            return 70;
        }
    };
    let view = match compile::decode(section) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("nub: {e:#}");
            return 70;
        }
    };
    match print_embedded_node_license(&view) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("nub: {e:#}");
            1
        }
    }
}

// ---- Node acquisition ---------------------------------------------------------

/// Cache capability follows the actual execution source, not the payload shape
/// alone: a proved external smol Node executes outside the app cache.
///
/// The data-only shortcut rests on the app payload being inert. A payload that
/// carries an executable file — a bundled platform binary the app spawns — makes
/// the cache an execution surface again, and must be rejected at SELECTION with
/// the actionable noexec message rather than surfacing later as a bare `EACCES`
/// from the application's own child process.
fn cache_use_for(
    shape: &Shape,
    external_smol_found: bool,
    payload_has_executable: bool,
) -> cache::CacheUse {
    if payload_has_executable {
        return cache::CacheUse::Executable;
    }
    match shape {
        Shape::Smol if external_smol_found => cache::CacheUse::DataOnly,
        Shape::Smol | Shape::Embed => cache::CacheUse::Executable,
    }
}

/// How much nub knows about the Node it is about to spawn, which decides whether
/// its accepted-flag set has to be probed before injecting a version band.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum NodeOrigin {
    /// nub shipped this binary or just installed it at a version it chose, so the
    /// version is exact and the band alone is sound.
    Managed,
    /// An arbitrary Node found on the host. Its version was inferred from a
    /// directory name or a `--version` call, and which experimental flags it still
    /// accepts is not predictable from that version.
    Discovered,
}

/// The exact payload state cache resolution verified. An external smol Node only
/// needs the app tree; every managed source requires both artifacts.
enum CacheWarm {
    Managed(VerifiedWarmCache),
    AppOnly,
}

/// The Node to run AND its own concrete version — the version the flag injection
/// must be keyed to. For `smol` the manifest carries only the acceptance FLOOR, so
/// keying off it would hand a discovered Node 26 the 22.x flag band: every flag
/// added since 22 is withheld (`--experimental-ffi`, `--experimental-vfs`, …) and
/// any flag Node has since REMOVED is injected, which is a startup abort. Embed is
/// the one case where the manifest is the truth, because it bakes the exact binary.
fn acquire_node(
    view: &PayloadView<'_>,
    base: &Path,
    notice: &FirstRun,
    external_smol: Option<(PathBuf, NodeVersion)>,
) -> Result<(PathBuf, NodeVersion, NodeOrigin)> {
    match view.manifest.shape {
        Shape::Embed => {
            let path = acquire_embedded_node(view, base, notice)?;
            let version = view
                .manifest
                .node_version
                .parse()
                .unwrap_or_else(|_| NodeVersion::new(22, 15, 0));
            Ok((path, version, NodeOrigin::Managed))
        }
        Shape::Smol => acquire_smol_node(&view.manifest, base, notice, external_smol),
    }
}

/// `embed` shape: use a cached compatible Node if present, else decompress the
/// bundled one into nub's shared cache once. Dedup rule (design gap #6): the
/// embedded Node is stripped and the official provisioned Node is not, but both
/// run identically, so an already-present official Node of the same version is
/// accepted and decompression is skipped.
fn acquire_embedded_node(
    view: &PayloadView<'_>,
    base: &Path,
    notice: &FirstRun,
) -> Result<PathBuf> {
    let m = &view.manifest;
    let node_cache = node_cache_dir(base, m);
    let node_bin = node_cache.join(node_exe_name());

    // Warm: this exact embedded Node was fully extracted, published, and still
    // hashes to the immutable manifest value.
    if embedded_node_cache_is_ready(m, &node_cache) {
        return Ok(node_bin);
    }

    // Dedup an official Node only when the embedded cache is absent. A present
    // but invalid tree must be staged-repaired rather than silently left behind:
    // a later environment without the official Node must never rediscover stale
    // native code. The store is keyed by version alone, so reuse is also gated on
    // the candidate actually matching this target's libc.
    if !path_entry_exists(&node_cache) {
        if let Some(official) = official_node_for_manifest(base, m) {
            return Ok(official);
        }
    }

    // Cold: decompress the embedded blob into the cache, then publish a fully
    // synced staging tree behind a completion marker.
    if view.node_blob.is_empty() {
        bail!("embed-shape payload is missing its Node blob");
    }
    notice.announce();
    let tmp = base.join(format!(
        ".compile-node.{}.{}.tmp",
        std::process::id(),
        rand_suffix()
    ));
    cleanup_orphan_staging(base, &tmp, ".compile-node.");
    let _ = fs::remove_dir_all(&tmp);
    create_staging_dir(&tmp)?;
    let tmp_bin = tmp.join(node_exe_name());
    // Sub-phases, because acquiring the Node is ~90% of a cold start and this was
    // one opaque span — which is what stopped an investigation into where that
    // second goes. Free unless __NUB_LAUNCHER_TIMING is set.
    phase("      node: staging ready");
    decompress_to_file(view.node_blob, &tmp_bin).context("decompressing the embedded Node")?;
    phase("      node: decompressed + written");
    set_executable(&tmp_bin)?;
    sync_file(&tmp_bin)?;
    phase("      node: chmod + fsync");

    publish_cache_dir(&tmp, &node_cache, |dir| {
        embedded_node_cache_is_ready(m, dir)
    })?;
    phase("      node: published");
    explain_if_node_cannot_start(&node_bin)?;
    Ok(node_bin)
}

/// Turn a dynamic-linker failure into something the reader can act on.
///
/// The artifact carries its own Node, so nothing about it needs installing —
/// except that Node itself links a handful of system libraries, and a minimal
/// container image can lack one. On Linux the usual absentee is `libatomic`, and
/// what the user sees without this is the linker's own line, which names a
/// filename and says nothing about nub, the binary they ran, or the fix.
///
/// Only on the COLD path, where the ~107 MB decompression above already dominates,
/// so the extra process is not measurable. It cannot be done on the warm path at
/// all: the launcher execs Node with inherited stdio, so the child's failure text
/// goes straight to the terminal and is never seen here.
fn explain_if_node_cannot_start(node_bin: &Path) -> Result<()> {
    let Ok(out) = Command::new(node_bin).arg("--version").output() else {
        // Could not run it at all — the caller's own spawn will produce a better
        // error than a guess from here.
        return Ok(());
    };
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let Some(missing) = missing_shared_library(&stderr) else {
        return Ok(());
    };
    bail!(
        "the embedded Node cannot start: it needs {missing}, which is not installed.\n\
         \x20 This binary carries its own Node, but Node links a few system libraries,\n\
         \x20 and this machine is missing one.\n\
         \x20 Debian or Ubuntu:  apt-get install {}\n\
         \x20 Alpine:            apk add {}",
        debian_package_for(&missing),
        alpine_package_for(&missing)
    )
}

/// The library named in a dynamic-linker failure, if that is what this is.
///
/// The two loaders word it differently, and matching only glibc's phrasing meant
/// every musl failure fell through to the raw error — found on Alpine, where the
/// message is not merely reworded but singular and differently punctuated:
///
///   glibc: node: error while loading shared libraries: libatomic.so.1: cannot open …
///   musl:  Error loading shared library libstdc++.so.6: No such file or directory
fn missing_shared_library(stderr: &str) -> Option<String> {
    for line in stderr.lines() {
        // glibc: the name follows a colon.
        if let Some(after) = line.split_once("error while loading shared libraries:") {
            let name = after.1.split(':').next().unwrap_or("").trim();
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
        // musl: the name follows a space, and the line may be prefixed by the
        // dlopen path rather than starting with the message.
        if let Some(after) = line.split_once("Error loading shared library ") {
            let name = after.1.split(':').next().unwrap_or("").trim();
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// Debian's package for a shared object. Falls back to the library's own name,
/// which is wrong as a package name but still the most useful thing to search.
fn debian_package_for(lib: &str) -> String {
    match lib.split('.').next() {
        Some("libatomic") => "libatomic1".to_string(),
        Some("libstdc++") => "libstdc++6".to_string(),
        Some("libgcc_s") => "libgcc-s1".to_string(),
        _ => lib.to_string(),
    }
}

fn alpine_package_for(lib: &str) -> String {
    match lib.split('.').next() {
        Some("libatomic") => "libatomic".to_string(),
        Some("libstdc++") => "libstdc++".to_string(),
        Some("libgcc_s") => "libgcc".to_string(),
        _ => lib.to_string(),
    }
}

/// `smol` shape discovery + provisioning. Order: nub's own Node store → the known
/// version-manager layouts → PATH → shell-out provision. Legacy payloads accept
/// any Node at or above their target; new payloads enforce exact targets and
/// explicit ranges. Provisioning prefers `provision_version` — the newest release
/// the COMPILER resolved for the pin — and falls back to the manifest version when
/// the payload names none. Acceptance is unaffected either way.
fn acquire_smol_node(
    m: &Manifest,
    base: &Path,
    notice: &FirstRun,
    external_smol: Option<(PathBuf, NodeVersion)>,
) -> Result<(PathBuf, NodeVersion, NodeOrigin)> {
    let target = smol_target(m)?;

    // 1. nub's Node store, then every version manager's install root. nub's store
    //    is checked first (it is the one nub itself provisioned into), but WITHIN
    //    the version managers the newest satisfying install wins rather than
    //    whichever manager happens to sort first.
    for store in node_stores(base) {
        // These are nub's OWN stores, so the trusted half is what answers; the
        // unprobed list is empty for a cache-owned directory. Probing the tail
        // anyway keeps this correct if a store ever stops being cache-owned,
        // rather than silently skipping it.
        let (owned, candidates) = best_node_in(&store, &target, &m.triple);
        if let Some((path, ver)) = owned {
            return Ok((path, ver, NodeOrigin::Discovered));
        }
        for (path, _) in candidates {
            if let Some(actual) = probe_node_version(&path) {
                if smol_candidate_matches(&actual, &target) {
                    return Ok((path, actual, NodeOrigin::Discovered));
                }
            }
        }
    }
    // 2. A version-manager or PATH Node proved before cache resolution. This is
    // deliberately not rediscovered: PATH probing can spawn Node, and the early
    // proof is also what authorized a noexec app cache.
    if let Some((path, ver)) = external_smol {
        return Ok((path, ver, NodeOrigin::Discovered));
    }

    // 3. Provision via shell-out. Prefer the version the COMPILER resolved as the
    // newest satisfying the pin: the target's floor may be the oldest release this
    // binary tolerates — `--target 26` landing on 26.0.0. Legacy manifests name no
    // preference and still get the floor.
    let fetch = smol_provision_version(m).unwrap_or_else(|| target.floor.clone());
    if !target.matches(&fetch) {
        bail!("compiled provision version {fetch} does not satisfy its runtime target");
    }
    let path = provision_smol_node(&fetch, base, notice)?;
    Ok((path, fetch, NodeOrigin::Managed))
}

/// The version to DOWNLOAD when discovery finds nothing, when the manifest names
/// one. Never consulted as the ACCEPTANCE policy — that is the exact/range/floor
/// target parsed separately. An absent or unparseable value falls back to the
/// floor, which is always a valid thing to fetch.
fn smol_provision_version(m: &Manifest) -> Option<NodeVersion> {
    let raw = m.provision_version.trim();
    if raw.is_empty() {
        return None;
    }
    raw.parse().ok()
}

/// The complete Node acceptance policy carried by a `smol` artifact.
#[derive(Debug)]
struct SmolTarget {
    floor: NodeVersion,
    exact: bool,
    range: Option<VersionPin>,
}

impl SmolTarget {
    #[cfg(test)]
    fn legacy(floor: NodeVersion, exact: bool) -> Self {
        Self {
            floor,
            exact,
            range: None,
        }
    }

    fn matches(&self, candidate: &NodeVersion) -> bool {
        if let Some(range) = &self.range {
            candidate.satisfies(range)
        } else if self.exact {
            candidate == &self.floor
        } else {
            candidate >= &self.floor
        }
    }
}

/// Parse the smol acceptance target once for both early external discovery and
/// cache-backed acquisition. A malformed payload must fail before its cache is
/// relaxed for noexec app data.
fn smol_target(m: &Manifest) -> Result<SmolTarget> {
    let floor: NodeVersion = m.node_version.parse().map_err(|_| {
        anyhow!(
            "compiled target version '{}' is unparseable",
            m.node_version
        )
    })?;
    let raw_range = m.smol_version_range.trim();
    let range = if raw_range.is_empty() {
        None
    } else {
        if m.smol_exact_target {
            bail!("compiled target cannot be both exact and a semver range");
        }
        let parsed = parse_target_spec(raw_range)
            .map_err(|_| anyhow!("compiled target range {raw_range:?} is unparseable"))?;
        if !matches!(parsed, VersionPin::Range(_)) {
            bail!("compiled target range {raw_range:?} is not a semver range");
        }
        if !floor.satisfies(&parsed) {
            bail!("compiled target floor {floor} does not satisfy its range {raw_range:?}");
        }
        Some(parsed)
    };
    Ok(SmolTarget {
        floor,
        exact: m.smol_exact_target,
        range,
    })
}

/// Find a compatible Node outside the selected cache. Version-manager installs
/// and PATH are external trust domains; a Node store below the cache is assessed
/// only after cache resolution because it would execute from that cache.
/// Find a Node this `--smol` artifact will accept.
///
/// Rank every external candidate by its CLAIMED version across all managers before
/// executing any of them, then probe in that order and stop at the first that
/// verifies. Verification is unchanged — an external Node is still executed before
/// it is trusted — but the ordering is now decided from directory names, which cost
/// nothing to read.
///
/// The old shape asked each manager for its best and probed inside every one, so a
/// machine with nvm plus mise spent a `node` spawn per manager purely to learn what
/// the names already said. Measured on a four-manager host: 4 execs and 75 ms
/// before the app's own Node was spawned, where the FIRST probe had already found a
/// satisfying 26.5.0.
///
/// A dishonestly-named directory therefore costs one extra probe and is skipped, as
/// before — it can never be accepted unverified, which is the property that matters.
fn discover_external_smol_node(m: &Manifest) -> Result<Option<(PathBuf, NodeVersion)>> {
    let target = smol_target(m)?;
    let mut trusted: Option<(PathBuf, NodeVersion)> = None;
    let mut ranked: Vec<(PathBuf, NodeVersion)> = Vec::new();
    for dir in version_manager_dirs() {
        let (owned, candidates) = best_node_in(&dir, &target, &m.triple);
        if let Some(found) = owned {
            if trusted.as_ref().is_none_or(|(_, cur)| found.1 > *cur) {
                trusted = Some(found);
            }
        }
        ranked.extend(candidates);
    }
    // Our own store outranks any external claim: it needs no exec at all.
    if trusted.is_some() {
        return Ok(trusted);
    }
    ranked.sort_by(|(_, left), (_, right)| right.cmp(left));
    let verified = ranked.into_iter().find_map(|(path, _)| {
        let actual = probe_node_version(&path)?;
        smol_candidate_matches(&actual, &target).then_some((path, actual))
    });
    Ok(verified.or_else(|| probe_path_node(&target)))
}

/// Node stores to READ, nearest first: the probed cache base, then the location
/// nub-core would name. They differ whenever the probe fell past `~/.cache/nub`
/// — an internal compile-cache override, or a read-only home on Lambda — and a
/// Node the CLI installed earlier still counts on a box where only one of the two
/// is writable now.
fn node_stores(base: &Path) -> Vec<NodeDir> {
    let mut out = vec![NodeDir::plain(base.join("node"))];
    if let Some(store) = discovery::node_store_dir() {
        if store != out[0].root {
            out.push(NodeDir::plain(store));
        }
    }
    out
}

fn official_node_for_manifest(base: &Path, manifest: &Manifest) -> Option<PathBuf> {
    node_stores(base).into_iter().find_map(|store| {
        cache::revalidate(&store.root).ok()?;
        let version_dir = store.root.join(&manifest.node_version);
        let node = node_in_version_dir(&version_dir);
        (trusted_node_in_version_dir(&version_dir)
            && store_node_matches_target(&node, &manifest.triple))
        .then_some(node)
    })
}

/// A directory holding one sub-directory per installed Node. Every layout below
/// is `<root>/<version>/<inner>/bin/node` — only the root and the interior
/// segment differ, so one scanner covers nub's own store and every version
/// manager.
struct NodeDir {
    root: PathBuf,
    /// Between the version dir and the executable dir. Empty everywhere but fnm,
    /// which nests the unpacked dist under `installation/`.
    inner: &'static str,
    /// Nub's executable cache needs owner/DACL/namespace validation. External
    /// version-manager installs follow that manager's own trust boundary.
    cache_owned: bool,
}

/// One candidate install root: `(base, subpath-to-the-version-dirs, interior)`.
/// Kept as data so the platform tables below read as layouts rather than control
/// flow, and a `None` base (env unset, no home) drops out with the missing dirs.
type Candidate = (Option<PathBuf>, &'static str, &'static str);

impl NodeDir {
    fn plain(root: PathBuf) -> Self {
        Self {
            root,
            inner: "",
            cache_owned: true,
        }
    }
}

/// The install roots of the version managers a developer machine actually has —
/// nvm, fnm, Volta, asdf, mise. Without these a box with a perfectly good Node
/// under `~/.nvm/versions/node/` downloads another one on first run, because the
/// managers put nothing on a non-interactive `PATH` (they are shell-hook /
/// shim-based, and a compiled binary is routinely launched from a service manager
/// or a cron shell that sourced no profile).
///
/// EVERY candidate root is probed rather than resolved to one winner: the managers
/// themselves pick a base dir by first-existing (fnm walks XDG data → legacy
/// `~/.fnm` → the macOS Application Support dir), so a box that has migrated may
/// still hold installs under the old one.
///
/// Layouts were read from each tool's own source, never from memory — fnm
/// `src/directories.rs` + `src/config.rs`, Volta `crates/volta-layout/src/v4.rs` +
/// `layout/{unix,windows}.rs`, mise `src/env.rs`, nvm `install.sh`, asdf
/// `internal/config`. Two traps they encode: nvm's non-env default is
/// `$XDG_CONFIG_HOME/nvm` (CONFIG, not DATA) when that var is set, and the Windows
/// bases are the AppData dirs, not `~/.<tool>`.
///
/// A version dir whose name is not a full `x.y.z` is skipped, which is what drops
/// mise's alias dirs (`22`, `lts`, `latest`) without a special case — each is a
/// duplicate of a concrete install that IS listed.
fn version_manager_dirs() -> Vec<NodeDir> {
    // Absolute-only: an empty or relative override would otherwise make every root
    // below CWD-relative, and scan — then execute a `node` from — whatever tree the
    // process happens to be sitting in.
    let env_dir = |key: &str| {
        std::env::var_os(key)
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
    };
    #[cfg(not(windows))]
    let home = dirs_next::home_dir();
    #[cfg(not(windows))]
    let under_home = |rel: &'static str| home.as_ref().map(|h| h.join(rel));

    #[cfg(not(windows))]
    let candidates: Vec<Candidate> = {
        let xdg_data = env_dir("XDG_DATA_HOME").or_else(|| under_home(".local/share"));
        let under_xdg = |rel: &'static str| xdg_data.as_ref().map(|d| d.join(rel));
        vec![
            // nvm — `<base>/versions/node/v<ver>/bin/node`.
            (env_dir("NVM_DIR"), "versions/node", ""),
            (
                env_dir("XDG_CONFIG_HOME").map(|d| d.join("nvm")),
                "versions/node",
                "",
            ),
            (under_home(".nvm"), "versions/node", ""),
            // fnm — `<base>/node-versions/v<ver>/installation/bin/node`.
            (env_dir("FNM_DIR"), "node-versions", "installation"),
            (under_xdg("fnm"), "node-versions", "installation"),
            (under_home(".fnm"), "node-versions", "installation"),
            (
                under_home("Library/Application Support/fnm"),
                "node-versions",
                "installation",
            ),
            // Volta — `<base>/tools/image/node/<ver>/bin/node`.
            (env_dir("VOLTA_HOME"), "tools/image/node", ""),
            (under_home(".volta"), "tools/image/node", ""),
            // asdf — `<base>/installs/nodejs/<ver>/bin/node`. The plugin is named
            // `nodejs`, unlike mise's `node`.
            (env_dir("ASDF_DATA_DIR"), "installs/nodejs", ""),
            (under_home(".asdf"), "installs/nodejs", ""),
            // mise — `<base>/installs/node/<ver>/bin/node`.
            (env_dir("MISE_DATA_DIR"), "installs/node", ""),
            (under_xdg("mise"), "installs/node", ""),
        ]
    };

    // Windows managers key off the AppData dirs, and nvm-windows drops the
    // `versions/node` segment entirely — `%NVM_HOME%\v<ver>\node.exe`. asdf and
    // nvm-sh are Unix-only, so neither appears here.
    #[cfg(windows)]
    let candidates: Vec<Candidate> = vec![
        (env_dir("NVM_HOME"), "", ""),
        (env_dir("FNM_DIR"), "node-versions", "installation"),
        (
            env_dir("APPDATA").map(|d| d.join("fnm")),
            "node-versions",
            "installation",
        ),
        (env_dir("VOLTA_HOME"), "tools/image/node", ""),
        (
            env_dir("LOCALAPPDATA").map(|d| d.join("Volta")),
            "tools/image/node",
            "",
        ),
        (env_dir("MISE_DATA_DIR"), "installs/node", ""),
        (
            env_dir("LOCALAPPDATA").map(|d| d.join("mise")),
            "installs/node",
            "",
        ),
    ];

    candidates
        .into_iter()
        .filter_map(|(base, rel, inner)| {
            let root = base?.join(rel);
            root.is_dir().then_some(NodeDir {
                root,
                inner,
                cache_owned: false,
            })
        })
        .collect()
}

/// Does a discovered `candidate` satisfy this smol payload's version policy?
/// Kept as one predicate so managed-layout scans and PATH discovery cannot drift.
fn smol_candidate_matches(candidate: &NodeVersion, target: &SmolTarget) -> bool {
    target.matches(candidate)
}

/// Scan one install root for the newest Node satisfying the payload's version
/// policy that can also actually RUN here — the same libc gate the embed-shape
/// dedup applies, which matters more for a version manager's tree than for nub's
/// own store, since nothing nub controls put it there.
/// Candidates from one directory: `(already trusted, needs probing)`.
///
/// Split because verifying an external candidate costs a full `node` process spawn
/// (~28 ms measured), and the old shape paid one PER MANAGER just to discover what
/// the directory names already said. See [`discover_external_smol_node`].
type NodeDirScan = (Option<(PathBuf, NodeVersion)>, Vec<(PathBuf, NodeVersion)>);

fn best_node_in(dir: &NodeDir, target: &SmolTarget, triple: &str) -> NodeDirScan {
    if dir.cache_owned && cache::revalidate(&dir.root).is_err() {
        return (None, Vec::new());
    }
    let mut candidates = Vec::new();
    let Ok(entries) = fs::read_dir(&dir.root) else {
        return (None, Vec::new());
    };
    for entry in entries.flatten() {
        let Ok(claimed) = entry.file_name().to_string_lossy().parse::<NodeVersion>() else {
            continue;
        };
        let mut version_dir = entry.path();
        if !dir.inner.is_empty() {
            version_dir = version_dir.join(dir.inner);
        }
        let bin = node_in_version_dir(&version_dir);
        if (dir.cache_owned && !trusted_node_in_version_dir(&version_dir))
            || (!dir.cache_owned && !is_executable_file(&bin))
            || !store_node_matches_target(&bin, triple)
        {
            continue;
        }

        if dir.cache_owned {
            if smol_candidate_matches(&claimed, target) {
                candidates.push((bin, claimed));
            }
        } else {
            // The directory name is only an ordering hint in an external trust
            // domain. Execute the candidate before it can relax the app cache to
            // DataOnly; this proves both that it runs and which version it is.
            candidates.push((bin, claimed));
        }
    }
    candidates.sort_by(|(_, left), (_, right)| right.cmp(left));
    if dir.cache_owned {
        // Our own store: the directory name is ours and already trusted, so the
        // best candidate needs no exec to confirm it.
        return (candidates.into_iter().next(), Vec::new());
    }
    // An external manager's directory names are only an ORDERING HINT, so nothing
    // here is accepted yet. Hand the ranked list back unprobed and let the caller
    // rank across every manager before spending a process on one.
    (None, candidates)
}

/// A half-extracted or permission-stripped `bin/node` in a tree nub does not own
/// must lose to the next candidate rather than be selected and fail at spawn — the
/// `noexec` remedy only covers paths under the launcher's own cache base, so a
/// version manager's would surface as a bare "Permission denied".
#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}
#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

/// Return a PATH candidate only when it meets the same policy as store scans.
fn select_path_node(
    candidate: (PathBuf, NodeVersion),
    target: &SmolTarget,
) -> Option<(PathBuf, NodeVersion)> {
    smol_candidate_matches(&candidate.1, target).then_some(candidate)
}

/// Resolve `node` on PATH to its path + version, or `None` if absent, unparseable,
/// or unsuitable for this payload.
fn probe_path_node(target: &SmolTarget) -> Option<(PathBuf, NodeVersion)> {
    let path = which_on_path(node_exe_name())?;
    let ver = probe_node_version(&path)?;
    select_path_node((path, ver), target)
}

/// Counts probe execs for `__NUB_LAUNCHER_TIMING`. Each one is a full `node`
/// process spawn (~28 ms), so the count is the whole story for smol start-up.
/// The version of the Node at `path`, from a remembered verdict when one applies
/// and an exec otherwise.
///
/// The exec is why `--smol` starts slower than the embed shape: `--smol` discovers
/// a Node it did not install, so it runs it once per launch to learn what it really
/// is. That is a full process spawn and it accounts for the whole warm gap between
/// the two shapes. Embed needs none of this — it knows its Node by content hash.
///
/// A stale or untrusted entry simply means another exec, so every failure path here
/// is the slow answer rather than a wrong one.
fn probe_node_version(path: &Path) -> Option<NodeVersion> {
    let stamp = node_binary_stamp(path);
    if let Some(stamp) = &stamp {
        if let Some(version) = cache::read_node_version(path, stamp) {
            if let Ok(parsed) = version.parse() {
                phase_with(|| format!("  probe cache HIT: {}", path.display()));
                return Some(parsed);
            }
        }
    }
    phase_with(|| format!("  PROBE EXEC: {}", path.display()));
    let version = probe_node_version_inner(path)?;
    if let Some(stamp) = &stamp {
        cache::write_node_version(path, stamp, &version.to_string());
    }
    Some(version)
}

/// Identity of the binary this verdict describes, so a replaced Node is re-probed.
///
/// `mtime + size` has a documented failure — two different files sharing both — and
/// this repo has already been bitten by exactly that shape in `ROOT_MANIFEST_CACHE`
/// on Windows. It is acceptable here because distinct Node builds differ in size,
/// so a collision needs a same-size rebuild landing inside one timestamp tick, and
/// the cost of losing that bet is one launch on a mislabelled Node rather than a
/// safety property: the posture checks that gate cache execution are unaffected.
fn node_binary_stamp(path: &Path) -> Option<String> {
    let metadata = fs::metadata(path).ok()?;
    let mtime = metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some(format!("{mtime}:{}", metadata.len()))
}

fn probe_node_version_inner(path: &Path) -> Option<NodeVersion> {
    let out = Command::new(path).arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// Minimal `which`: first PATH entry containing an executable `name`.
fn which_on_path(name: &str) -> Option<PathBuf> {
    which_in_path(std::env::var_os("PATH").as_deref(), name)
}

fn which_in_path(path: Option<&std::ffi::OsStr>, name: &str) -> Option<PathBuf> {
    nub_core::find_on_path_in(path, &[name])
}

/// Provision `version` for the host from nodejs.org. The launcher keeps TLS out
/// of process by downloading through curl/wget, then uses nub-core's capped
/// `.tar.xz`/`.zip` extractor. Installs into Nub's store so `nub run` and future
/// launches reuse it.
fn provision_smol_node(version: &NodeVersion, base: &Path, notice: &FirstRun) -> Result<PathBuf> {
    let token = host_platform_token();
    let archive_ext = host_archive_ext();
    let mirror = smol_mirror_base();
    let filename = format!("node-v{version}-{token}.{archive_ext}");
    let url = format!("{mirror}/v{version}/{filename}");

    // The probed base, not `discovery::node_store_dir()`: provisioning WRITES, so
    // it must land where the write probe succeeded.
    let store = base.join("node");
    ensure_trusted_cache_parent(&store)?;
    let work = store.join(format!(
        ".smol.{version}.{}.{}.tmp",
        std::process::id(),
        rand_suffix()
    ));
    cleanup_orphan_staging(&store, &work, ".smol.");
    let _ = fs::remove_dir_all(&work);
    create_staging_dir(&work)?;

    let archive = work.join(&filename);
    notice.announce();
    download_via_shell(&url, &archive, notice).inspect_err(|_| {
        let _ = fs::remove_dir_all(&work);
    })?;

    // Verify the archive's SHA-256 against SHASUMS256.txt BEFORE anything reaches
    // the shared store — `nub run` and the embed-shape dedup path later TRUST that
    // store without re-verifying, so an unverified write would poison them. Matches
    // version_management::provision_node_from's fail-closed commit gate.
    let shasums_path = work.join("SHASUMS256.txt");
    let shasums_url = format!("{mirror}/v{version}/SHASUMS256.txt");
    download_via_shell(&shasums_url, &shasums_path, notice).inspect_err(|_| {
        let _ = fs::remove_dir_all(&work);
    })?;
    let shasums = fs::read_to_string(&shasums_path)
        .with_context(|| format!("reading {}", shasums_path.display()))?;
    verify_archive_checksum(&archive, &shasums, &filename).inspect_err(|_| {
        let _ = fs::remove_dir_all(&work);
    })?;

    let unpack = work.join("unpack");
    create_staging_dir(&unpack).inspect_err(|_| {
        let _ = fs::remove_dir_all(&work);
    })?;
    let extracted = extract_verified_node_archive(&archive, &unpack).inspect_err(|_| {
        let _ = fs::remove_dir_all(&work);
    })?;

    let expected_top = format!("node-v{version}-{token}");
    if extracted.file_name() != Some(std::ffi::OsStr::new(&expected_top)) {
        let _ = fs::remove_dir_all(&work);
        bail!(
            "Node archive extracted an unexpected top-level directory: {}",
            extracted.display()
        );
    }
    let final_dir = store.join(version.to_string());
    publish_cache_dir(&extracted, &final_dir, smol_node_cache_is_ready).inspect_err(|_| {
        let _ = fs::remove_dir_all(&work);
    })?;
    let _ = fs::remove_dir_all(&work);

    let node = node_in_version_dir(&final_dir);
    if !trusted_node_in_version_dir(&final_dir) {
        bail!(
            "provisioned Node {version} but its binary is missing at {}",
            node.display()
        );
    }
    Ok(node)
}

/// A newly provisioned smol tree carries the completion marker. A complete
/// unmarked tree is also accepted because the ordinary Nub Node installer
/// publishes the same version key atomically without this launcher's marker.
/// In both cases every path component used to execute Node must be a trusted
/// real directory/file rather than a symlink or Windows reparse point.
fn smol_node_cache_is_ready(version_dir: &Path) -> bool {
    if !trusted_node_in_version_dir(version_dir) {
        return false;
    }
    let marker = version_dir.join(CACHE_COMPLETE_MARKER);
    !path_entry_exists(&marker) || completion_marker_is_ready(version_dir)
}

fn trusted_node_in_version_dir(version_dir: &Path) -> bool {
    if !cache_artifact_directory_is_trusted(version_dir) {
        return false;
    }
    let node = node_in_version_dir(version_dir);
    #[cfg(unix)]
    if !is_real_directory(&version_dir.join("bin")) {
        return false;
    }
    fs::symlink_metadata(&node)
        .is_ok_and(|metadata| metadata_is_trusted_regular_file(&node, &metadata))
        && is_executable_file(&node)
}

/// Download `url` to `dest` via `curl`, then `wget`. When neither exists, the
/// no-downloader error names the fix (a binary built with the default shape).
///
/// While the first-run box owns the terminal the downloader's own progress meter
/// is silenced — two writers on one screen is torn output; the box's spinner is
/// the only feedback. It is silenced off a TTY too: nobody is watching a pipe or
/// a log file, and a progress meter there is the one thing that breaks the
/// otherwise byte-exact silence a non-interactive first run promises. An
/// interactive run without the box still gets the meter.
fn download_via_shell(url: &str, dest: &Path, notice: &FirstRun) -> Result<()> {
    download_via_commands(
        url,
        dest,
        notice,
        which_on_path("curl").as_deref(),
        which_on_path("wget").as_deref(),
    )
}

fn download_via_commands(
    url: &str,
    dest: &Path,
    notice: &FirstRun,
    curl: Option<&Path>,
    wget: Option<&Path>,
) -> Result<()> {
    let muted = notice.owns_terminal() || !std::io::stderr().is_terminal();
    let mut failures = Vec::new();

    if let Some(curl) = curl {
        let mut cmd = Command::new(curl);
        cmd.args(["-fSL", "--retry", "2"]);
        if muted {
            cmd.arg("-s");
        }
        match cmd.arg("-o").arg(dest).arg(url).status() {
            Ok(status) if status.success() => return Ok(()),
            Ok(status) => failures.push(format!("curl failed to download {url} ({status})")),
            Err(error) => failures.push(format!("running curl: {error}")),
        }
    }
    if let Some(wget) = wget {
        let mut cmd = Command::new(wget);
        if muted {
            cmd.arg("-q");
        }
        match cmd.arg("-O").arg(dest).arg(url).status() {
            Ok(status) if status.success() => return Ok(()),
            Ok(status) => failures.push(format!("wget failed to download {url} ({status})")),
            Err(error) => failures.push(format!("running wget: {error}")),
        }
    }
    if !failures.is_empty() {
        bail!("failed to download {url}: {}", failures.join("; "));
    }
    bail!(
        "no HTTPS downloader found (need `curl` or `wget`) to provision Node.\n\
         \x20\x20Install curl or wget, or use a binary built with the default \
         (embed-Node) shape, which needs no download."
    )
}

/// Fail-closed SHA-256 gate: the archive's digest must match its line in
/// SHASUMS256.txt, or the install aborts before anything is committed. Mirrors
/// `version_management::download::verify_checksum` exactly.
fn verify_archive_checksum(archive: &Path, shasums: &str, filename: &str) -> Result<()> {
    let expected = checksum_for(shasums, filename)
        .with_context(|| format!("{filename} is not listed in SHASUMS256.txt — refusing"))?;
    let actual = sha256_hex_of_file(archive)?;
    if actual.eq_ignore_ascii_case(&expected) {
        Ok(())
    } else {
        bail!("checksum mismatch for {filename}: expected {expected}, got {actual}");
    }
}

/// The SHASUMS256.txt hash for `filename` (`<64-hex>  <name>`), lowercased, or
/// `None` if absent/malformed. Byte-identical to nub-core's `checksum_for`.
fn checksum_for(shasums: &str, filename: &str) -> Option<String> {
    shasums.lines().find_map(|line| {
        let (hash, rest) = line.split_once(char::is_whitespace)?;
        let name = rest.trim_start();
        let valid = hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit());
        (valid && name == filename).then(|| hash.to_ascii_lowercase())
    })
}

fn sha256_hex_of_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut file = fs::File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("reading {}", path.display()))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

/// The `NODEJS_ORG_MIRROR` override (nvm/n convention), else the default dist
/// base (unofficial-builds for musl).
fn smol_mirror_base() -> String {
    if let Ok(m) = std::env::var("NODEJS_ORG_MIRROR") {
        let m = m.trim_end_matches('/');
        if !m.is_empty() {
            return m.to_string();
        }
    }
    if is_musl() {
        "https://unofficial-builds.nodejs.org/download/release".to_string()
    } else {
        "https://nodejs.org/dist".to_string()
    }
}

// ---- app extraction -----------------------------------------------------------

/// Extract the bundled app files into `<cache>/compile-app/<app-key>/`. A warm
/// run accepts only a completed extraction; an older or interrupted directory is
/// repaired through the same staged publication as the embedded Node.
fn ensure_app(view: &PayloadView<'_>, base: &Path) -> Result<PathBuf> {
    let app_dir = app_cache_dir(base, &view.manifest);
    if app_cache_is_ready(view, &app_dir) {
        return Ok(app_dir);
    }

    let tmp = base.join(format!(
        ".compile-app.{}.{}.tmp",
        std::process::id(),
        rand_suffix()
    ));
    cleanup_orphan_staging(base, &tmp, ".compile-app.");
    let _ = fs::remove_dir_all(&tmp);
    create_staging_dir(&tmp)?;
    for file in &view.app_files {
        let name = &file.name;
        // Refuse a payload file name that could escape the extraction dir — a
        // corrupted/hostile section must not write outside `tmp` via `..`, an
        // absolute path, or a leading separator. Names are nested and partly
        // user-derived since `--include` landed, so `nub compile` checks the
        // same predicate at build time; this stays as the last line of defense.
        if !is_safe_relative_name(name) {
            bail!("compiled payload has an unsafe file name: {name:?}");
        }
        if name == CACHE_COMPLETE_MARKER {
            bail!("compiled payload uses reserved cache file name: {name:?}");
        }
        let dest = tmp.join(name);
        if let Some(parent) = dest.parent() {
            create_staging_subdirs(&tmp, parent)?;
        }
        write_file(&dest, &app_bytes(&view.manifest, file)?)?;
        if file.executable {
            // Mode only. `seal_cache_dir`'s sweep covers this file's data AND its
            // metadata before the publish rename, so syncing here would put the
            // write-time fsync back on exactly the payload this change exists for:
            // a native island or an `--include`d tree, where the executables are.
            set_app_file_executable(&dest)?;
        }
    }
    publish_cache_dir(&tmp, &app_dir, |dir| app_cache_is_ready(view, dir))?;
    Ok(app_dir)
}

// ---- helpers ------------------------------------------------------------------

/// A short, filesystem-safe key from a hex content hash.
fn short_key(hex: &str) -> String {
    let s: String = hex.chars().take(16).collect();
    if s.is_empty() { "0".into() } else { s }
}

/// Print the Node runtime notice embedded in a V2 payload. This is deliberately
/// called before cache resolution: the notice must be readable without
/// extracting, provisioning, or executing anything.
fn print_embedded_node_license(view: &PayloadView<'_>) -> Result<()> {
    if view.node_license_blob.is_empty() {
        bail!(
            "this compiled binary has no embedded runtime notice \
             (it was built with --smol or uses a legacy payload)"
        );
    }
    let mut decoder = zstd::stream::Decoder::new(view.node_license_blob)
        .map_err(|e| anyhow!("decompressing the embedded runtime notice: {e}"))?;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    std::io::copy(&mut decoder, &mut out).context("writing the embedded runtime notice")?;
    out.flush()
        .context("flushing the embedded runtime notice")?;
    Ok(())
}

/// Write one extracted file. Durability is `seal_cache_dir`'s job, not this one's:
/// fsyncing here forces a flush while the write is still hot, which serializes
/// every file against the disk and dominates a many-file payload — a `--include`d
/// directory or a native island. The end-of-tree sweep gives the same guarantee
/// for practically nothing, because the OS has already flushed by then.
/// A payload file's real bytes, decompressing only if the payload stores them
/// compressed. Borrowed on the uncompressed path, so an older payload stays
/// zero-copy out of the mapped image.
fn app_bytes<'a>(manifest: &Manifest, file: &AppFile<&'a [u8]>) -> Result<Cow<'a, [u8]>> {
    if !manifest.app_compressed {
        return Ok(Cow::Borrowed(file.bytes));
    }
    zstd::decode_all(file.bytes)
        .map(Cow::Owned)
        .with_context(|| format!("decompressing {} from the payload", file.name))
}

fn write_file(dest: &Path, data: &[u8]) -> Result<()> {
    let mut file =
        create_private_file(dest).with_context(|| format!("creating {}", dest.display()))?;
    file.write_all(data)
        .with_context(|| format!("writing {}", dest.display()))
}

fn sync_file(path: &Path) -> Result<()> {
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|file| file.sync_all())
        .with_context(|| format!("flushing {} to disk", path.display()))
}

fn create_staging_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    let result = {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        builder.create(path)
    };
    #[cfg(not(unix))]
    let result = fs::create_dir(path);
    result.with_context(|| format!("creating {}", path.display()))?;
    normalize_staging_dir(path)
}

fn create_staging_subdirs(root: &Path, path: &Path) -> Result<()> {
    let relative = path
        .strip_prefix(root)
        .with_context(|| format!("staging path escapes {}", root.display()))?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            bail!("invalid staging directory path: {}", path.display());
        };
        current.push(name);

        #[cfg(unix)]
        let result = {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            builder.create(&current)
        };
        #[cfg(not(unix))]
        let result = fs::create_dir(&current);
        if let Err(error) = result {
            if error.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(
                    anyhow::Error::new(error).context(format!("creating {}", current.display()))
                );
            }
        }
        normalize_staging_dir(&current)?;
    }
    Ok(())
}

fn normalize_staging_dir(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("reading staging directory {}", path.display()))?;
    if !metadata.file_type().is_dir() || metadata_is_windows_reparse_point(&metadata) {
        bail!(
            "staging directory is not a real directory: {}",
            path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("setting private mode on {}", path.display()))?;
    }
    Ok(())
}

fn create_private_file(path: &Path) -> std::io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

/// Write the completion marker LAST, then sync every staged file and directory
/// before the tree becomes visible at its final cache key. This is the durability
/// barrier, and it is where the syncing belongs: by the time it runs the OS has
/// already flushed the data, so each fsync finds nothing dirty and the whole sweep
/// costs about as much as no fsync at all. Syncing at WRITE time instead forces a
/// flush while the write is hot and serializes every file against the disk.
fn seal_cache_dir(tmp: &Path) -> Result<()> {
    let marker = tmp.join(CACHE_COMPLETE_MARKER);
    create_private_file(&marker)
        .and_then(|file| file.sync_all())
        .with_context(|| format!("recording completion at {}", marker.display()))?;
    sync_cache_tree(tmp)
}

fn sync_cache_tree(dir: &Path) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
        let path = entry.path();
        let ty = entry
            .file_type()
            .with_context(|| format!("reading {}", path.display()))?;
        if ty.is_dir() {
            sync_cache_tree(&path)?;
        } else if ty.is_file() {
            sync_file(&path)?;
        }
    }
    sync_directory(dir)
}

/// `File::open` can sync directories on Unix. Windows needs a directory handle
/// opened with `FILE_FLAG_BACKUP_SEMANTICS`, which std does not expose; all files
/// still flush there before the rename.
#[cfg(unix)]
fn sync_directory(dir: &Path) -> Result<()> {
    fs::File::open(dir)
        .and_then(|file| file.sync_all())
        .with_context(|| format!("flushing directory {}", dir.display()))
}

#[cfg(not(unix))]
fn sync_directory(_dir: &Path) -> Result<()> {
    Ok(())
}

/// Seal, content-validate, and atomically publish a staged cache tree. A valid
/// destination is a safe concurrent winner. Anything else at the final key is
/// stale or interrupted state, so move it aside atomically before publishing the
/// replacement rather than treating one marker's presence as a valid cache hit.
///
/// The lock is held around both the completeness check and stale-path swap. A
/// prior check followed by `remove_dir_all` let two launchers interleave: the
/// loser could observe an incomplete path, a winner could publish, then the
/// loser could recursively delete that complete winner. Advisory file locks are
/// released by the OS if a launcher crashes, so they serialize publication
/// without leaving a stale lock behind.
fn publish_cache_dir<F>(tmp: &Path, dest: &Path, is_complete: F) -> Result<()>
where
    F: Fn(&Path) -> bool,
{
    seal_cache_dir(tmp)?;
    if !is_complete(tmp) {
        let _ = fs::remove_dir_all(tmp);
        bail!(
            "staged cache failed integrity validation at {}",
            tmp.display()
        );
    }
    let parent = dest
        .parent()
        .ok_or_else(|| anyhow!("cache path has no parent: {}", dest.display()))?;
    ensure_trusted_cache_parent(parent).inspect_err(|_| {
        let _ = fs::remove_dir_all(tmp);
    })?;
    cache::revalidate(parent)
        .with_context(|| format!("revalidating publication parent {}", parent.display()))
        .inspect_err(|_| {
            let _ = fs::remove_dir_all(tmp);
        })?;
    let _publication_lock = lock_cache_publication(dest)?;

    // Retain a concurrent winner only after the same exact content validation
    // every reader performs; a marker alone is never a completeness verdict.
    if is_complete(dest) {
        let _ = fs::remove_dir_all(tmp);
        return Ok(());
    }

    let install = |e: std::io::Error| {
        anyhow::Error::new(e).context(format!("publishing extracted cache at {}", dest.display()))
    };
    match fs::rename(tmp, dest) {
        Ok(()) => {}
        Err(first) => match move_incomplete_cache_path_aside_locked(dest, &is_complete)? {
            ExistingCachePath::Complete => {
                let _ = fs::remove_dir_all(tmp);
                return Ok(());
            }
            ExistingCachePath::Missing => match fs::rename(tmp, dest) {
                Ok(()) => {}
                Err(_) if is_complete(dest) => {
                    let _ = fs::remove_dir_all(tmp);
                    return Ok(());
                }
                Err(second) => return Err(install(second).context(first)),
            },
            ExistingCachePath::Moved(stale) => match fs::rename(tmp, dest) {
                Ok(()) => {
                    remove_moved_cache_path(&stale);
                }
                Err(_) if is_complete(dest) => {
                    let _ = fs::remove_dir_all(tmp);
                    remove_moved_cache_path(&stale);
                    return Ok(());
                }
                Err(second) => {
                    let _ = fs::remove_dir_all(tmp);
                    // Do not strand a usable old tree if publication failed after
                    // moving it aside. Another winner takes precedence if it
                    // appeared despite the advisory lock.
                    if !path_entry_exists(dest) {
                        let _ = fs::rename(&stale, dest);
                    }
                    return Err(install(second).context(first));
                }
            },
        },
    }

    // A lost directory-entry update merely causes a future re-extraction; it
    // cannot make an unmarked tree look ready. Best effort keeps the launcher
    // portable while making the normal Unix crash-consistency path durable.
    #[cfg(unix)]
    let _ = sync_directory(parent);
    Ok(())
}

fn ensure_trusted_cache_parent(path: &Path) -> Result<()> {
    let create = match fs::symlink_metadata(path) {
        Ok(_) => false,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(error) => return Err(anyhow::Error::new(error)),
    };
    if create {
        #[cfg(unix)]
        let result = {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            builder.create(path)
        };
        #[cfg(not(unix))]
        let result = fs::create_dir(path);
        if let Err(error) = result {
            if error.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(
                    anyhow::Error::new(error).context(format!("creating {}", path.display()))
                );
            }
        }
    }
    if !is_real_directory(path) {
        bail!(
            "cache artifact parent is not a trusted directory: {}",
            path.display()
        );
    }
    cache::revalidate(path)
        .with_context(|| format!("revalidating cache artifact parent {}", path.display()))?;
    Ok(())
}

/// Open the per-artifact publication lock. It lives next to the artifact rather
/// than inside it, so a stale artifact can be deleted while its lock remains
/// stable. `File::lock` is cross-platform and releases on process exit.
fn lock_cache_publication(dest: &Path) -> Result<fs::File> {
    let parent = dest
        .parent()
        .ok_or_else(|| anyhow!("cache path has no parent: {}", dest.display()))?;
    let name = dest
        .file_name()
        .ok_or_else(|| anyhow!("cache path has no file name: {}", dest.display()))?;
    let mut lock_name = name.to_os_string();
    lock_name.push(".nub-publish.lock");
    let lock_path = parent.join(lock_name);
    cache::revalidate(parent)
        .with_context(|| format!("revalidating cache lock parent {}", parent.display()))?;
    if let Ok(metadata) = fs::symlink_metadata(&lock_path)
        && !metadata_is_trusted_regular_file(&lock_path, &metadata)
    {
        bail!(
            "cache publication lock is not a trusted regular file: {}",
            lock_path.display()
        );
    }
    let mut options = fs::OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock = options
        .open(&lock_path)
        .with_context(|| format!("opening cache publication lock {}", lock_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        lock.set_permissions(fs::Permissions::from_mode(0o600))
            .with_context(|| {
                format!("protecting cache publication lock {}", lock_path.display())
            })?;
    }
    let metadata = fs::symlink_metadata(&lock_path)
        .with_context(|| format!("reading cache publication lock {}", lock_path.display()))?;
    if !metadata_is_trusted_regular_file(&lock_path, &metadata) {
        bail!(
            "cache publication lock is not a trusted regular file: {}",
            lock_path.display()
        );
    }
    cache::revalidate(parent)
        .with_context(|| format!("revalidating cache lock parent {}", parent.display()))?;
    lock.lock()
        .with_context(|| format!("locking cache publication at {}", dest.display()))?;
    Ok(lock)
}

/// State observed at a final cache key while publication holds its lock.
enum ExistingCachePath {
    Missing,
    Complete,
    /// The incomplete object was atomically moved out of the canonical name.
    Moved(PathBuf),
}

/// Move stale state aside before replacing it. Recursive deletion in place is
/// unsafe on Windows: it can delete unlocked files in a live tree, then stop at
/// a loaded DLL and leave the canonical path half-deleted. A successful rename
/// proves this process has exclusive enough access to clean the old tree later.
fn move_incomplete_cache_path_aside_locked<F>(
    path: &Path,
    is_complete: F,
) -> Result<ExistingCachePath>
where
    F: Fn(&Path) -> bool,
{
    if is_complete(path) {
        return Ok(ExistingCachePath::Complete);
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ExistingCachePath::Missing);
        }
        Err(error) => return Err(anyhow::Error::new(error)),
    };
    if !metadata_is_trusted_real_directory(path, &metadata)
        && !metadata_is_trusted_regular_file(path, &metadata)
    {
        bail!(
            "refusing to replace an untrusted incomplete cache path: {}",
            path.display()
        );
    }
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("cache path has no parent: {}", path.display()))?;
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("cache path has no file name: {}", path.display()))?;
    let stale = parent.join(format!(
        ".{}.stale.{}.{}",
        name.to_string_lossy(),
        std::process::id(),
        rand_suffix()
    ));
    match fs::rename(path, &stale) {
        Ok(()) => Ok(ExistingCachePath::Moved(stale)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(ExistingCachePath::Missing)
        }
        Err(error) if cache_path_is_in_use(&error, path) => bail!(
            "cannot repair cache at {} because it is in use by another process; close the running compiled application and retry (the existing tree was left untouched)",
            path.display()
        ),
        Err(error) => Err(anyhow::Error::new(error))
            .with_context(|| format!("moving incomplete cache path aside from {}", path.display())),
    }
}

/// On Windows a loaded DLL prevents renaming its tree. The destination must
/// still exist: otherwise a concurrent actor already moved it, so it is not an
/// in-use diagnostic and normal publication handling may continue.
#[cfg(windows)]
fn cache_path_is_in_use(error: &std::io::Error, path: &Path) -> bool {
    path.exists() && matches!(error.raw_os_error(), Some(5 | 32))
}

#[cfg(not(windows))]
fn cache_path_is_in_use(_error: &std::io::Error, _path: &Path) -> bool {
    false
}

/// A moved old target can theoretically be a regular file from a corrupted
/// cache. Clean it by type without following redirects; publication itself only
/// moved objects that passed the trusted-object gate.
fn remove_moved_cache_path(path: &Path) {
    let remove = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => fs::remove_dir_all(path),
        Ok(metadata) if metadata.file_type().is_file() => fs::remove_file(path),
        _ => return,
    };
    let _ = remove;
}

/// Reclaim this launcher's abandoned work even when the requested artifacts are
/// already warm. Cold-only cleanup misses the common crash race where another
/// process publishes the winner and every later process takes the warm return.
fn cleanup_launcher_orphans(base: &Path, manifest: &Manifest) {
    cleanup_orphan_entries(base, None, ORPHAN_STAGE_MAX_AGE, |name| {
        launcher_stage_name(name, ".compile-app.") || launcher_stage_name(name, ".compile-node.")
    });
    for parent in [
        base.join("compile-app"),
        base.join("compile-node"),
        base.join("node"),
    ] {
        cleanup_orphan_entries(&parent, None, ORPHAN_STAGE_MAX_AGE, launcher_stale_name);
    }
    cleanup_orphan_entries(&base.join("node"), None, ORPHAN_STAGE_MAX_AGE, |name| {
        launcher_stage_name(name, ".smol.")
    });

    // Published trees this run does not use. `current` is what keeps a
    // continuously-used artifact alive: its own tree is skipped by path, so only
    // trees belonging to payloads that have not run here in 30 days are removed.
    let app_dir = app_cache_dir(base, manifest);
    cleanup_orphan_entries(
        &base.join("compile-app"),
        Some(&app_dir),
        UNUSED_CACHE_MAX_AGE,
        published_app_key,
    );
    let node_dir = node_cache_dir(base, manifest);
    cleanup_orphan_entries(
        &base.join("compile-node"),
        Some(&node_dir),
        UNUSED_CACHE_MAX_AGE,
        published_node_key,
    );
}

/// A published app extraction: the short hex key of `app_sha256`, nothing else.
/// Matching the SHAPE rather than "anything not staging" keeps the sweep from
/// ever removing a directory it does not recognise.
fn published_app_key(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A published Node extraction: `<version>-<short hex key>`.
fn published_node_key(name: &str) -> bool {
    name.rsplit_once('-').is_some_and(|(version, key)| {
        !version.is_empty()
            && version
                .bytes()
                .all(|b| b.is_ascii_digit() || b == b'.' || b == b'-')
            && !key.is_empty()
            && key.bytes().all(|b| b.is_ascii_hexdigit())
    })
}

fn launcher_stage_name(name: &str, prefix: &str) -> bool {
    let Some(body) = name
        .strip_prefix(prefix)
        .and_then(|name| name.strip_suffix(".tmp"))
    else {
        return false;
    };
    match prefix {
        ".compile-app." | ".compile-node." => {
            let mut parts = body.split('.');
            matches!(
                (parts.next(), parts.next(), parts.next()),
                (Some(pid), Some(nonce), None)
                    if pid.parse::<u32>().is_ok() && nonce.parse::<u64>().is_ok()
            )
        }
        ".smol." => {
            let mut parts = body.rsplitn(3, '.');
            let Some(nonce) = parts.next() else {
                return false;
            };
            let Some(pid) = parts.next() else {
                return false;
            };
            let Some(version) = parts.next() else {
                return false;
            };
            nonce.parse::<u64>().is_ok()
                && pid.parse::<u32>().is_ok()
                && version.parse::<NodeVersion>().is_ok()
        }
        _ => false,
    }
}

fn launcher_stale_name(name: &str) -> bool {
    let Some(body) = name.strip_prefix('.') else {
        return false;
    };
    let Some((destination, suffix)) = body.rsplit_once(".stale.") else {
        return false;
    };
    if destination.is_empty() {
        return false;
    }
    let mut parts = suffix.split('.');
    matches!(
        (parts.next(), parts.next(), parts.next()),
        (Some(pid), Some(nonce), None)
            if pid.parse::<u32>().is_ok() && nonce.parse::<u64>().is_ok()
    )
}

/// Best-effort age-based cleanup for only this launcher's abandoned staging
/// and moved-aside directories. `parent` is revalidated before enumeration,
/// names must match the caller-provided launcher grammar, and
/// recent/current/arbitrary entries are never removed. This is intentionally not
/// a general cache janitor.
fn cleanup_orphan_staging(parent: &Path, current: &Path, prefix: &str) {
    cleanup_orphan_entries(parent, Some(current), ORPHAN_STAGE_MAX_AGE, |name| {
        launcher_stage_name(name, prefix)
    });
}

fn cleanup_orphan_entries(
    parent: &Path,
    current: Option<&Path>,
    max_age: Duration,
    matches_name: impl Fn(&str) -> bool,
) {
    if cache::revalidate(parent).is_err() {
        return;
    }
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        if current.is_some_and(|current| path == current) {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !matches_name(&name) {
            continue;
        }
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata_is_trusted_real_directory(&path, &metadata) {
            continue;
        }
        let stale = metadata
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > max_age);
        if stale {
            let _ = fs::remove_dir_all(&path);
        }
    }
}

/// Stream the decoder into the file rather than materializing the whole ~94 MB
/// Node in a `Vec` first. Same wall time (the decode dominates; the write is
/// ~60 ms of it) at roughly half the peak RSS — which is what decides the run on
/// a memory-capped host whose writable filesystem is tmpfs charged against that
/// same limit.
fn decompress_to_file(compressed: &[u8], dest: &Path) -> Result<()> {
    let mut decoder =
        zstd::stream::Decoder::new(compressed).map_err(|e| anyhow!("zstd init: {e}"))?;
    let file = fs::File::create(dest).with_context(|| format!("creating {}", dest.display()))?;
    let mut out = std::io::BufWriter::with_capacity(1 << 20, file);
    std::io::copy(&mut decoder, &mut out)
        .with_context(|| format!("decompressing Node into {}", dest.display()))?;
    out.flush()
        .with_context(|| format!("writing {}", dest.display()))?;
    out.get_ref()
        .sync_all()
        .with_context(|| format!("flushing {} to disk", dest.display()))?;
    Ok(())
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .with_context(|| format!("chmod +x {}", path.display()))
}
#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

/// Make an extracted app file executable at 0o700 — the executable analogue of
/// the 0o600 every other app file gets, inside the 0o700 tree they share.
///
/// Deliberately NOT the 0o755 the embedded Node takes: an embedded helper is the
/// publisher's private application content, nothing outside the owning uid ever
/// needs to read or run it, and the extra `g+rx`/`o+rx` would be the one place
/// the app cache stops being owner-only.
#[cfg(unix)]
fn set_app_file_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("chmod +x {}", path.display()))
}
/// Windows executability is an ACL and an extension, not a mode bit — the file
/// is already runnable by the user who extracted it.
#[cfg(not(unix))]
fn set_app_file_executable(_path: &Path) -> Result<()> {
    Ok(())
}

/// The Node executable's filename on this platform.
fn node_exe_name() -> &'static str {
    if cfg!(windows) { "node.exe" } else { "node" }
}

/// The Node executable inside a provisioned version dir. The two dist layouts
/// differ: the Windows zip puts `node.exe` at the root, the tarballs put
/// `bin/node`.
fn node_in_version_dir(version_dir: &Path) -> PathBuf {
    if cfg!(windows) {
        version_dir.join("node.exe")
    } else {
        version_dir.join("bin").join("node")
    }
}

fn host_platform_token() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "linux" => "linux",
        "windows" => "win",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "arm" => "armv7l",
        "powerpc64" => "ppc64le",
        "s390x" => "s390x",
        "x86" => "x86",
        other => other,
    };
    if os == "linux" && is_musl() {
        format!("{os}-{arch}-musl")
    } else {
        format!("{os}-{arch}")
    }
}

fn archive_ext_for_os(os: &str) -> &'static str {
    if os == "windows" { "zip" } else { "tar.xz" }
}

fn host_archive_ext() -> &'static str {
    archive_ext_for_os(std::env::consts::OS)
}

/// Whether this launcher runs on musl — its OWN build-target libc, since the
/// per-triple launcher is built for and shipped to exactly one platform. NOT a
/// `/lib` scan: a `ld-musl-*` loader file merely existing (a glibc host with musl
/// cross-libs) is not this host running musl, and treating it as such made
/// `--smol` fetch a musl Node onto a glibc box. Mirrors nub-core's `detect_musl`.
fn is_musl() -> bool {
    cfg!(target_env = "musl")
}

// ---- store-node libc compatibility --------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ElfLibc {
    Glibc,
    Musl,
}

/// Whether a Node already in nub's version store may be reused for a payload
/// targeting `triple`, instead of decompressing the embedded one. The store is
/// keyed by version alone, so this is the guard that a musl and a glibc Node of
/// the SAME version — not interchangeable — are told apart.
///
/// Only Linux splits libc; a macOS/Windows store node of the right version is
/// format-compatible by construction (the store lives on the host the payload
/// targets), so those short-circuit to reuse. On Linux the candidate's libc is
/// read from its OWN ELF interpreter (`PT_INTERP`) — `ld-musl-*` ⇒ musl,
/// `ld-linux*` ⇒ glibc — which is definitional and needs no exec. A static Node
/// (no interpreter) runs against any libc, so it is accepted; anything unreadable
/// as an ELF is refused, degrading to the always-correct embedded-Node path.
fn store_node_matches_target(node: &Path, triple: &str) -> bool {
    if !triple.starts_with("linux") {
        return true;
    }
    let want_musl = triple.ends_with("-musl");
    match read_elf_interp_libc(node) {
        Some(Some(ElfLibc::Musl)) => want_musl,
        Some(Some(ElfLibc::Glibc)) => !want_musl,
        Some(None) => true, // valid ELF, no interpreter (static) → libc-agnostic
        None => false,      // unreadable / not a 64-bit LE ELF → don't risk reuse
    }
}

/// Read a candidate's `PT_INTERP` and classify its libc, over a BOUNDED prefix —
/// the interpreter string lives in the first LOAD segment, so a Node binary's
/// tens of MB are never slurped to answer this.
fn read_elf_interp_libc(path: &Path) -> Option<Option<ElfLibc>> {
    use std::io::Read as _;
    let mut buf = Vec::new();
    fs::File::open(path)
        .ok()?
        .take(64 * 1024)
        .read_to_end(&mut buf)
        .ok()?;
    classify_elf_interp(&buf)
}

/// The pure parser behind [`read_elf_interp_libc`], over an in-memory prefix — the
/// seam the tests drive with hand-built ELF headers. `None` = not a 64-bit LE ELF
/// (or an unrecognized interpreter); `Some(None)` = valid ELF, no interpreter
/// (statically linked); `Some(Some(libc))` = the dynamic linker's libc.
fn classify_elf_interp(buf: &[u8]) -> Option<Option<ElfLibc>> {
    // ELF ident: magic, EI_CLASS==2 (64-bit), EI_DATA==1 (LE). `nub compile` only
    // targets linux-{x64,arm64}, both 64-bit LE, so any other ELF in the store is
    // foreign — refuse rather than classify it.
    if buf.len() < 64 || &buf[0..4] != b"\x7fELF" || buf[4] != 2 || buf[5] != 1 {
        return None;
    }
    let rd_u16 =
        |o: usize| -> Option<u16> { Some(u16::from_le_bytes(buf.get(o..o + 2)?.try_into().ok()?)) };
    let rd_u32 =
        |o: usize| -> Option<u32> { Some(u32::from_le_bytes(buf.get(o..o + 4)?.try_into().ok()?)) };
    let rd_u64 =
        |o: usize| -> Option<u64> { Some(u64::from_le_bytes(buf.get(o..o + 8)?.try_into().ok()?)) };

    let e_phoff = rd_u64(0x20)? as usize;
    let e_phentsize = rd_u16(0x36)? as usize;
    let e_phnum = rd_u16(0x38)? as usize;
    const PT_INTERP: u32 = 3;
    for i in 0..e_phnum {
        let ph = e_phoff.checked_add(i.checked_mul(e_phentsize)?)?;
        if rd_u32(ph)? != PT_INTERP {
            continue;
        }
        let p_offset = rd_u64(ph + 8)? as usize;
        let p_filesz = rd_u64(ph + 32)? as usize;
        let end = p_offset.checked_add(p_filesz)?;
        let interp = buf.get(p_offset..end)?;
        let interp = interp.split(|&b| b == 0).next().unwrap_or(interp);
        let s = std::str::from_utf8(interp).ok()?;
        if s.contains("ld-musl") {
            return Some(Some(ElfLibc::Musl));
        }
        if s.contains("ld-linux") {
            return Some(Some(ElfLibc::Glibc));
        }
        return None; // unrecognized interpreter — refuse rather than guess
    }
    Some(None) // no PT_INTERP → statically linked
}

fn rand_suffix() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
        ^ (std::process::id() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

#[cfg(unix)]
fn exit_code(status: ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    if let Some(code) = status.code() {
        code
    } else if let Some(sig) = status.signal() {
        128 + sig
    } else {
        1
    }
}
#[cfg(not(unix))]
fn exit_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use nub_core::compile::AppFile;

    use super::*;

    #[test]
    fn compile_bootstrap_require_precedes_injected_flags_and_entry() {
        let app_dir = Path::new("/absolute/cache/compile-app/key");
        let entry = app_dir.join("main.js");
        let mut cmd = Command::new("node");
        let bootstrap = app_dir.join(compile::COMPILE_BOOTSTRAP_NAME);
        cmd.arg(compiled_bootstrap_require_arg(&bootstrap));
        cmd.arg("--import=runtime/preload.mjs");
        cmd.arg(&entry);

        let args: Vec<_> = cmd.get_args().map(|arg| arg.to_os_string()).collect();
        let mut expected_preload = std::ffi::OsString::from("--require=");
        expected_preload.push(&bootstrap);
        assert_eq!(
            args,
            vec![
                expected_preload,
                std::ffi::OsString::from("--import=runtime/preload.mjs"),
                entry.into_os_string(),
            ]
        );
    }

    /// `cache::resolve` canonicalizes, so on Windows every cache path arrives in
    /// the verbatim `\\?\` spelling — and Node cannot resolve a module through
    /// one: its CJS loader's `fs.realpathSync` dies `EISDIR: … lstat 'C:'`. That
    /// killed the launcher's own `--require` preload, so no compiled artifact ran
    /// on Windows at all. Neither Node argument may carry the prefix.
    #[test]
    fn a_verbatim_cache_path_never_reaches_node() {
        let app_dir = r"\\?\C:\Users\r\AppData\Local\nub\compile-app\key";
        let plain = &app_dir[r"\\?\".len()..];

        let entry = node_argument(Path::new(&format!(r"{app_dir}\app.mjs")), true);
        assert_eq!(entry, PathBuf::from(format!(r"{plain}\app.mjs")));

        let bootstrap = node_argument(Path::new(&format!(r"{app_dir}\boot.cjs")), true);
        let mut expected = std::ffi::OsString::from("--require=");
        expected.push(format!(r"{plain}\boot.cjs"));
        assert_eq!(compiled_bootstrap_require_arg(&bootstrap), expected);

        // A network cache root keeps its UNC spelling rather than losing two
        // leading separators, and off Windows nothing is a prefix at all.
        assert_eq!(
            node_argument(Path::new(r"\\?\UNC\srv\share\app.mjs"), true),
            PathBuf::from(r"\\srv\share\app.mjs")
        );
        assert_eq!(
            node_argument(Path::new(r"\\?\C:\a\app.mjs"), false),
            PathBuf::from(r"\\?\C:\a\app.mjs")
        );
    }

    #[test]
    fn compiled_exec_path_overwrites_an_inherited_spoof() {
        let launcher_path = Path::new("/outer/compiled-app");
        let mut cmd = Command::new("node");
        cmd.env(COMPILED_EXEC_PATH_ENV, "/inherited/spoof");

        configure_compiled_process_identity(&mut cmd, launcher_path);

        let configured = cmd
            .get_envs()
            .find_map(|(key, value)| {
                (key == std::ffi::OsStr::new(COMPILED_EXEC_PATH_ENV))
                    .then(|| value.map(PathBuf::from))
                    .flatten()
            })
            .expect("compiled executable path must be set on the Node command");
        assert_eq!(configured, launcher_path);
    }

    /// The old single-call semantics, for tests: take the trusted answer, else
    /// probe the ranked candidates in order. `best_node_in` was split so the real
    /// caller can rank across every version manager BEFORE spending a process on
    /// any one of them; these tests are about which node a directory yields, not
    /// about that ordering, so they go through this.
    fn best_node_in_probed(
        dir: &NodeDir,
        target: &NodeVersion,
        exact_target: bool,
        triple: &str,
    ) -> Option<(PathBuf, NodeVersion)> {
        let target = SmolTarget::legacy(target.clone(), exact_target);
        best_node_in_policy_probed(dir, &target, triple)
    }

    fn best_node_in_policy_probed(
        dir: &NodeDir,
        target: &SmolTarget,
        triple: &str,
    ) -> Option<(PathBuf, NodeVersion)> {
        let (owned, candidates) = best_node_in(dir, target, triple);
        owned.or_else(|| {
            candidates.into_iter().find_map(|(path, _)| {
                let actual = probe_node_version(&path)?;
                smol_candidate_matches(&actual, target).then_some((path, actual))
            })
        })
    }

    fn fresh_cache_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nub-launcher-cache-{name}-{}-{}",
            std::process::id(),
            rand_suffix()
        ));
        create_staging_dir(&dir).unwrap();
        // `cache::resolve` canonicalizes production cache roots before they are
        // revalidated. Mirror that here so macOS's stable `/var` redirect is not
        // mistaken for an attacker-introduced post-resolution symlink.
        fs::canonicalize(dir).unwrap()
    }

    fn test_manifest() -> Manifest {
        use sha2::{Digest, Sha256};
        Manifest {
            shape: Shape::Embed,
            entry: "main.js".to_string(),
            node_version: "22.15.0".to_string(),
            provision_version: String::new(),
            smol_exact_target: false,
            smol_version_range: String::new(),
            triple: "darwin-arm64".to_string(),
            node_sha256: format!("{:x}", Sha256::digest(b"node")),
            node_blake3: String::new(),
            node_size: b"node".len() as u64,
            app_compressed: false,
            app_sha256: "app-cache-key".to_string(),
            minify: false,
            install_message: None,
            node_flags: Vec::new(),
        }
    }

    fn test_view() -> PayloadView<'static> {
        PayloadView {
            manifest: test_manifest(),
            app_files: vec![
                AppFile::plain("main.js", &b"app"[..]),
                AppFile::plain("nested/data.json", &br#"{"ok":true}"#[..]),
            ],
            node_blob: &[],
            node_license_blob: &[],
        }
    }

    fn materialize_test_node(view: &PayloadView<'_>, base: &Path) -> PathBuf {
        let dir = node_cache_dir(base, &view.manifest);
        create_staging_subdirs(base, &dir).unwrap();
        let node = dir.join(node_exe_name());
        write_file(&node, b"node").unwrap();
        set_executable(&node).unwrap();
        seal_cache_dir(&dir).unwrap();
        dir
    }

    fn materialize_test_app(view: &PayloadView<'_>, base: &Path) -> PathBuf {
        let dir = app_cache_dir(base, &view.manifest);
        create_staging_subdirs(base, &dir).unwrap();
        for file in &view.app_files {
            let path = dir.join(&file.name);
            create_staging_subdirs(&dir, path.parent().unwrap()).unwrap();
            write_file(&path, file.bytes).unwrap();
            if file.executable {
                set_app_file_executable(&path).unwrap();
            }
        }
        seal_cache_dir(&dir).unwrap();
        dir
    }

    fn marked_test_artifact_is_ready(cache_dir: &Path, artifact: &Path) -> bool {
        completion_marker_is_ready(cache_dir)
            && fs::symlink_metadata(artifact).is_ok_and(|metadata| metadata.file_type().is_file())
    }

    #[cfg(unix)]
    fn downloader_script(dir: &Path, name: &str, exit_code: u8) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path = dir.join(name);
        fs::write(&path, format!("#!/bin/sh\nexit {exit_code}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn download_falls_back_to_wget_after_curl_fails() {
        let dir = fresh_cache_dir("download-curl-fallback");
        let curl = downloader_script(&dir, "curl", 19);
        let wget = downloader_script(&dir, "wget", 0);
        let result = download_via_commands(
            "https://example.invalid/node.tar.xz",
            &dir.join("node.tar.xz"),
            &FirstRun::new(None),
            Some(&curl),
            Some(&wget),
        );

        assert!(result.is_ok(), "wget must run after curl fails: {result:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn download_reports_curl_and_wget_failures() {
        let dir = fresh_cache_dir("download-both-fail");
        let curl = downloader_script(&dir, "curl", 19);
        let wget = downloader_script(&dir, "wget", 23);
        let error = download_via_commands(
            "https://example.invalid/node.tar.xz",
            &dir.join("node.tar.xz"),
            &FirstRun::new(None),
            Some(&curl),
            Some(&wget),
        )
        .unwrap_err();
        let message = format!("{error:#}");

        assert!(
            message.contains("curl failed to download"),
            "got: {message}"
        );
        assert!(
            message.contains("wget failed to download"),
            "got: {message}"
        );
        assert!(
            message.find("curl failed").unwrap() < message.find("wget failed").unwrap(),
            "failure order must be deterministic: {message}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn downloader_discovery_skips_non_executable_curl() {
        use std::os::unix::fs::PermissionsExt;

        let dir = fresh_cache_dir("non-executable-curl");
        let curl = downloader_script(&dir, "curl", 0);
        fs::set_permissions(&curl, fs::Permissions::from_mode(0o600)).unwrap();
        let path = std::env::join_paths([dir.as_path()]).unwrap();

        assert!(which_in_path(Some(path.as_os_str()), "curl").is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn staging_helpers_pin_private_unix_modes() {
        use std::os::unix::fs::PermissionsExt;

        let base = fresh_cache_dir("private-staging-modes");
        let stage = base.join("stage");
        let nested = stage.join("nested").join("deeper");
        let existing = stage.join("existing");
        let payload = nested.join("app.js");
        create_staging_dir(&stage).unwrap();
        fs::create_dir(&existing).unwrap();
        fs::set_permissions(&existing, fs::Permissions::from_mode(0o777)).unwrap();
        create_staging_subdirs(&stage, &existing).unwrap();
        create_staging_subdirs(&stage, &nested).unwrap();
        write_file(&payload, b"app").unwrap();
        seal_cache_dir(&stage).unwrap();

        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&stage), 0o700);
        assert_eq!(mode(&existing), 0o700);
        assert_eq!(mode(&stage.join("nested")), 0o700);
        assert_eq!(mode(&nested), 0o700);
        assert_eq!(mode(&payload), 0o600);
        assert_eq!(mode(&stage.join(CACHE_COMPLETE_MARKER)), 0o600);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn smol_with_a_proven_external_node_uses_a_data_only_cache() {
        assert_eq!(
            cache_use_for(&Shape::Embed, false, false),
            cache::CacheUse::Executable,
            "embedded Node always executes from the cache"
        );
        assert_eq!(
            cache_use_for(&Shape::Smol, false, false),
            cache::CacheUse::Executable,
            "smol provisioning still executes its cached Node"
        );
        assert_eq!(
            cache_use_for(&Shape::Smol, true, false),
            cache::CacheUse::DataOnly,
            "a proved version-manager/PATH Node leaves the cache as app data only"
        );
        assert_eq!(
            cache_use_for(&Shape::Smol, true, true),
            cache::CacheUse::Executable,
            "a payload carrying a spawnable binary makes the cache an execution \
             surface, so a noexec volume must be refused at selection"
        );
    }

    #[cfg(unix)]
    fn age_stage_for_cleanup(path: &Path) {
        age_past(path, ORPHAN_STAGE_MAX_AGE);
    }

    #[cfg(unix)]
    fn age_past(path: &Path, ttl: Duration) {
        use std::os::unix::ffi::OsStrExt;

        let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        let old = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .saturating_sub(ttl.as_secs() + 1) as libc::time_t;
        let times = [
            libc::timeval {
                tv_sec: old,
                tv_usec: 0,
            },
            libc::timeval {
                tv_sec: old,
                tv_usec: 0,
            },
        ];
        assert_eq!(unsafe { libc::utimes(path.as_ptr(), times.as_ptr()) }, 0);
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_removes_only_old_owned_orphan_staging_trees() {
        let base = fresh_cache_dir("orphan-staging");
        let store = base.join("node");
        create_staging_subdirs(&base, &store).unwrap();
        let app_old = base.join(".compile-app.100.1.tmp");
        let node_old = base.join(".compile-node.100.2.tmp");
        let app_current = base.join(".compile-app.101.3.tmp");
        let node_current = base.join(".compile-node.101.4.tmp");
        let app_recent = base.join(".compile-app.102.5.tmp");
        let arbitrary = base.join("keep-me.tmp");
        let deceptive = base.join(".compile-app.not-owned.tmp");
        let smol_old = store.join(".smol.22.15.0.100.6.tmp");
        let smol_current = store.join(".smol.22.15.0.101.7.tmp");
        for path in [
            &app_old,
            &node_old,
            &app_current,
            &node_current,
            &app_recent,
            &arbitrary,
            &deceptive,
            &smol_old,
            &smol_current,
        ] {
            create_staging_dir(path).unwrap();
        }
        for path in [
            &app_old,
            &node_old,
            &app_current,
            &node_current,
            &smol_old,
            &smol_current,
        ] {
            age_stage_for_cleanup(path);
        }

        cleanup_orphan_staging(&base, &app_current, ".compile-app.");
        cleanup_orphan_staging(&base, &node_current, ".compile-node.");
        cleanup_orphan_staging(&store, &smol_current, ".smol.");

        assert!(!app_old.exists(), "old interrupted app stage was retained");
        assert!(
            !node_old.exists(),
            "old interrupted embedded-Node stage was retained"
        );
        assert!(
            !smol_old.exists(),
            "old interrupted smol stage was retained"
        );
        assert!(app_current.exists(), "current app stage was removed");
        assert!(
            node_current.exists(),
            "current embedded-Node stage was removed"
        );
        assert!(smol_current.exists(), "current smol stage was removed");
        assert!(app_recent.exists(), "recent stage was removed");
        assert!(arbitrary.exists(), "arbitrary sibling was removed");
        assert!(deceptive.exists(), "a non-launcher lookalike was removed");
        let _ = fs::remove_dir_all(&base);
    }

    #[cfg(windows)]
    #[test]
    fn windows_in_use_rename_is_classified_only_while_destination_remains() {
        let base = fresh_cache_dir("windows-in-use-cache");
        let dest = base.join("dest");
        create_staging_dir(&dest).unwrap();
        assert!(cache_path_is_in_use(
            &std::io::Error::from_raw_os_error(32),
            &dest
        ));
        assert!(cache_path_is_in_use(
            &std::io::Error::from_raw_os_error(5),
            &dest
        ));
        fs::remove_dir(&dest).unwrap();
        assert!(!cache_path_is_in_use(
            &std::io::Error::from_raw_os_error(32),
            &dest
        ));
        let _ = fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn unix_cache_permission_denial_keeps_the_noexec_remedy() {
        let base = Path::new("/tmp/nub-cache");
        let error = node_spawn_error(
            &base.join("compile-node/node"),
            base,
            std::io::Error::from_raw_os_error(libc::EACCES),
        );
        assert!(format!("{error:#}").contains("mounted noexec"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_cache_permission_denial_preserves_the_spawn_error() {
        let base = Path::new(r"C:\nub-cache");
        let error = node_spawn_error(
            &base.join("compile-node").join("node.exe"),
            base,
            std::io::Error::from_raw_os_error(5),
        );
        let message = format!("{error:#}");
        assert!(message.contains("spawning Node"), "got: {message}");
        assert!(!message.contains("noexec"), "got: {message}");
    }

    #[test]
    fn compiled_webstorage_policy_reads_only_node_options() {
        let version = NodeVersion::new(22, 15, 0);
        assert_eq!(
            compile_webstorage_policy(&version, None, None),
            (true, true)
        );
        assert_eq!(
            compile_webstorage_policy(&version, Some("--localstorage-file=/tmp/store"), None),
            (true, false),
            "the launcher injects sessionStorage support but preserves a real user store"
        );
        assert_eq!(
            compile_webstorage_policy(&version, Some("--no-experimental-webstorage"), None),
            (false, false)
        );
        assert_eq!(
            compile_webstorage_policy(&NodeVersion::new(25, 0, 0), None, None),
            (false, false)
        );
    }

    #[test]
    fn compiled_webstorage_policy_respects_discovered_flag_probe() {
        let version = NodeVersion::new(22, 15, 0);
        let absent = std::collections::BTreeSet::from(["--enable-source-maps".to_string()]);
        assert_eq!(
            compile_webstorage_policy(&version, None, Some(&absent)),
            (false, false),
            "a discovered Node that rejects the flag must not receive it"
        );

        let present = std::collections::BTreeSet::from([
            "--enable-source-maps".to_string(),
            "--experimental-webstorage".to_string(),
        ]);
        assert_eq!(
            compile_webstorage_policy(&version, None, Some(&present)),
            (true, true)
        );
    }

    #[test]
    fn internal_modes_require_the_launcher_control_channel() {
        let probe = vec!["compiled-app".to_string()];
        let pdeath = vec![
            "compiled-app".to_string(),
            "123".to_string(),
            "3".to_string(),
        ];
        let app_args = vec!["compiled-app".to_string(), "__probe".to_string()];
        let no_args = vec!["compiled-app".to_string()];

        assert_eq!(internal_mode(&probe, None), None);
        assert_eq!(internal_mode(&pdeath, None), None);
        assert_eq!(internal_mode(&app_args, Some("probe")), None);
        assert_eq!(
            internal_mode(&probe, Some("probe")),
            Some(InternalMode::Probe)
        );
        assert_eq!(
            internal_mode(&pdeath, Some("pdeath-watch")),
            Some(InternalMode::PdeathWatch)
        );
        assert_eq!(internal_mode(&probe, Some("pdeath-watch")), None);
        assert_eq!(internal_mode(&pdeath, Some("probe")), None);

        assert_eq!(
            internal_mode(&no_args, Some("licenses")),
            Some(InternalMode::Licenses)
        );
        assert_eq!(internal_mode(&no_args, None), None);
        assert_eq!(internal_mode(&app_args, Some("licenses")), None);
    }

    /// `--licenses` is an ordinary application argument. The compiled launcher
    /// reserves no argument spelling at any count, so a publisher whose CLI
    /// already defines `--licenses` keeps it; the release gate reaches the
    /// embedded notice through the private control channel instead.
    #[test]
    fn licenses_is_never_reserved_as_an_application_argument() {
        let sole = vec!["compiled-app".to_string(), "--licenses".to_string()];
        let with_more = vec![
            "compiled-app".to_string(),
            "--licenses".to_string(),
            "extra".to_string(),
        ];

        for argv in [&sole, &with_more] {
            for channel in [None, Some("licenses"), Some("probe")] {
                assert_eq!(
                    internal_mode(argv, channel),
                    None,
                    "argv {argv:?} on channel {channel:?} must reach the application"
                );
            }
        }
    }

    #[test]
    fn embedded_node_requires_its_marker_and_manifest_metadata() {
        let base = fresh_cache_dir("node-integrity");
        let view = test_view();
        let dir = node_cache_dir(&base, &view.manifest);
        create_staging_subdirs(&base, &dir).unwrap();
        let node = dir.join(node_exe_name());
        write_file(&node, b"node").unwrap();
        set_executable(&node).unwrap();
        assert!(
            !embedded_node_cache_is_ready(&view.manifest, &dir),
            "a matching binary without the completion marker is not published"
        );
        seal_cache_dir(&dir).unwrap();
        assert!(embedded_node_cache_is_ready(&view.manifest, &dir));

        fs::write(&node, b"attacker-controlled native code").unwrap();
        set_executable(&node).unwrap();
        assert!(
            !embedded_node_cache_is_ready(&view.manifest, &dir),
            "a marked binary whose expected metadata changed must never execute"
        );

        fs::write(&node, b"node").unwrap();
        set_executable(&node).unwrap();
        write_file(&dir.join("unexpected"), b"resolution input").unwrap();
        assert!(
            !embedded_node_cache_is_ready(&view.manifest, &dir),
            "the embedded-Node tree is exact, not merely marker plus executable"
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn completed_exact_node_and_app_make_a_warm_cache() {
        let base = fresh_cache_dir("warm-exact");
        let view = test_view();
        assert!(
            verify_warm_cache(&view, &base).is_none(),
            "absent artifacts are cold"
        );
        materialize_test_node(&view, &base);
        materialize_test_app(&view, &base);
        assert!(
            verify_warm_cache(&view, &base).is_some(),
            "byte-exact completed artifacts are accepted without a write probe"
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn app_cache_rejects_changed_marker_files_and_resolution_inputs() {
        let base = fresh_cache_dir("app-integrity");
        let view = test_view();
        let app_dir = materialize_test_app(&view, &base);
        let entry = app_dir.join(&view.manifest.entry);
        let marker = app_dir.join(CACHE_COMPLETE_MARKER);
        assert!(app_cache_is_ready(&view, &app_dir));

        fs::write(&entry, b"attacker-controlled JavaScript").unwrap();
        assert!(
            !app_cache_is_ready(&view, &app_dir),
            "a modified entry must never become executable cache state"
        );
        fs::write(&entry, b"app").unwrap();

        let nested = app_dir.join("nested/data.json");
        fs::write(&nested, br#"{"ok":null}"#).unwrap();
        assert!(
            !app_cache_is_ready(&view, &app_dir),
            "every payload file is byte-checked, not only the executable entry"
        );
        fs::write(&nested, br#"{"ok":true}"#).unwrap();

        fs::write(&marker, b"attacker marker").unwrap();
        assert!(
            !app_cache_is_ready(&view, &app_dir),
            "the completion marker has one valid representation: an empty regular file"
        );
        fs::write(&marker, b"").unwrap();

        fs::remove_file(&nested).unwrap();
        assert!(
            !app_cache_is_ready(&view, &app_dir),
            "every payload file must still exist under its exact name"
        );
        write_file(&nested, br#"{"ok":true}"#).unwrap();

        write_file(&app_dir.join("package.json"), br#"{"type":"commonjs"}"#).unwrap();
        assert!(
            !app_cache_is_ready(&view, &app_dir),
            "an unexpected package.json can change Node module interpretation"
        );
        fs::remove_file(app_dir.join("package.json")).unwrap();

        create_staging_subdirs(&app_dir, &app_dir.join("node_modules")).unwrap();
        assert!(
            !app_cache_is_ready(&view, &app_dir),
            "even an empty unexpected directory makes the extracted tree stale"
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn executable_caches_reject_symlinks_at_expected_files() {
        let base = fresh_cache_dir("cache-symlinks");
        let view = test_view();
        let app_dir = materialize_test_app(&view, &base);
        let entry = app_dir.join(&view.manifest.entry);
        let attacker = base.join("attacker.mjs");
        fs::write(&attacker, b"attacker-controlled JavaScript").unwrap();
        fs::remove_file(&entry).unwrap();
        std::os::unix::fs::symlink(&attacker, &entry).unwrap();

        assert!(
            !app_cache_is_ready(&view, &app_dir),
            "matching names reached through symlinks are not payload files"
        );

        let node_dir = materialize_test_node(&view, &base);
        let node = node_dir.join(node_exe_name());
        fs::remove_file(&node).unwrap();
        std::os::unix::fs::symlink(&attacker, &node).unwrap();
        assert!(
            !embedded_node_cache_is_ready(&view.manifest, &node_dir),
            "a symlink cannot satisfy the embedded Node cache gate"
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn executable_caches_reject_other_principal_write_access() {
        use std::os::unix::fs::PermissionsExt;

        let base = fresh_cache_dir("cache-write-access");
        let view = test_view();
        let app_dir = materialize_test_app(&view, &base);
        let nested = app_dir.join("nested");
        fs::set_permissions(&nested, fs::Permissions::from_mode(0o775)).unwrap();
        assert!(
            !app_cache_is_ready(&view, &app_dir),
            "a writable payload directory permits another principal to replace files"
        );
        fs::set_permissions(&nested, fs::Permissions::from_mode(0o755)).unwrap();

        let node_dir = materialize_test_node(&view, &base);
        let node = node_dir.join(node_exe_name());
        fs::set_permissions(&node, fs::Permissions::from_mode(0o775)).unwrap();
        assert!(
            !embedded_node_cache_is_ready(&view.manifest, &node_dir),
            "a writable native executable remains mutable after its hash check"
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn ensure_app_repairs_a_tampered_completed_tree() {
        let base = fresh_cache_dir("app-repair");
        let view = test_view();
        let app_dir = materialize_test_app(&view, &base);
        fs::write(
            app_dir.join(&view.manifest.entry),
            b"attacker-controlled JavaScript",
        )
        .unwrap();
        write_file(&app_dir.join("package.json"), br#"{"type":"commonjs"}"#).unwrap();

        assert_eq!(ensure_app(&view, &base).unwrap(), app_dir);
        assert_eq!(
            fs::read(app_dir.join(&view.manifest.entry)).unwrap(),
            b"app"
        );
        assert!(!app_dir.join("package.json").exists());
        assert!(app_cache_is_ready(&view, &app_dir));
        let _ = fs::remove_dir_all(&base);
    }

    /// A bundled platform binary (esbuild's Go executable, a per-platform
    /// `bin/<tool>`) has to come out of the cache spawnable, and everything else
    /// has to stay as private as it was before per-file modes existed.
    #[cfg(unix)]
    #[test]
    fn extraction_restores_the_executable_bit_only_where_the_payload_marks_it() {
        use std::os::unix::fs::PermissionsExt;

        let base = fresh_cache_dir("app-exec-bit");
        let mut view = test_view();
        view.manifest.app_sha256 = "app-exec-key".to_string();
        view.app_files.push(AppFile {
            name: "bin/helper".to_string(),
            bytes: &b"#!/bin/sh\necho hi\n"[..],
            executable: true,
        });

        let app_dir = ensure_app(&view, &base).unwrap();
        let mode = |name: &str| {
            fs::metadata(app_dir.join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode("bin/helper"), 0o700, "a marked file must be spawnable");
        assert_eq!(mode("main.js"), 0o600, "an unmarked file stays owner-rw");
        assert_eq!(mode("nested/data.json"), 0o600);
        assert!(app_cache_is_ready(&view, &app_dir));

        // A tree extracted before per-file modes existed carries the same bytes at
        // 0o600, so only the mode can tell it apart — and it must be repaired
        // rather than reused with a helper the app cannot spawn.
        fs::set_permissions(
            app_dir.join("bin/helper"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        assert!(!app_cache_is_ready(&view, &app_dir));
        assert_eq!(ensure_app(&view, &base).unwrap(), app_dir);
        assert_eq!(mode("bin/helper"), 0o700);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn acquire_embedded_node_repairs_a_completed_binary_with_the_wrong_length() {
        let base = fresh_cache_dir("node-repair");
        let test = test_view();
        let mut manifest = test.manifest;
        manifest.node_version = "99.99.99".to_string();
        let node_blob = zstd::encode_all(&b"node"[..], 1).unwrap();
        let view = PayloadView {
            manifest,
            app_files: test.app_files,
            node_blob: &node_blob,
            node_license_blob: &[],
        };
        let node_dir = materialize_test_node(&view, &base);
        let node = node_dir.join(node_exe_name());
        fs::write(&node, b"attacker-controlled native code").unwrap();
        set_executable(&node).unwrap();
        let official = node_in_version_dir(&base.join("node").join(&view.manifest.node_version));
        create_staging_subdirs(&base, official.parent().unwrap()).unwrap();
        write_file(&official, b"official node").unwrap();
        set_executable(&official).unwrap();

        let notice = FirstRun::new(None);
        assert_eq!(acquire_embedded_node(&view, &base, &notice).unwrap(), node);
        assert_eq!(fs::read(&node).unwrap(), b"node");
        assert_ne!(
            fs::read(&node).unwrap(),
            fs::read(&official).unwrap(),
            "a stale embedded cache is repaired rather than hidden by store dedup"
        );
        assert!(embedded_node_cache_is_ready(&view.manifest, &node_dir));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn acquire_embedded_node_refuses_a_blob_that_misses_the_manifest_hash() {
        let base = fresh_cache_dir("node-staged-integrity");
        let test = test_view();
        let mut manifest = test.manifest;
        manifest.node_version = "98.98.98".to_string();
        let node_blob = zstd::encode_all(&b"different native bytes"[..], 1).unwrap();
        let view = PayloadView {
            manifest,
            app_files: test.app_files,
            node_blob: &node_blob,
            node_license_blob: &[],
        };

        let error = acquire_embedded_node(&view, &base, &FirstRun::new(None)).unwrap_err();
        assert!(
            format!("{error:#}").contains("staged cache failed integrity validation"),
            "got: {error:#}"
        );
        assert!(
            !node_cache_dir(&base, &view.manifest).exists(),
            "mismatched native bytes must not be published"
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn an_official_store_node_and_completed_app_make_a_warm_cache() {
        let base = fresh_cache_dir("official-store-warm");
        let view = test_view();
        let official = node_in_version_dir(&base.join("node").join(&view.manifest.node_version));
        create_staging_subdirs(&base, official.parent().unwrap()).unwrap();
        write_file(&official, b"official node").unwrap();
        set_executable(&official).unwrap();
        materialize_test_app(&view, &base);

        let warm =
            verify_warm_cache(&view, &base).expect("a valid official Node plus exact app is warm");
        assert_eq!(warm.node_path, official);
        assert!(
            !node_cache_dir(&base, &view.manifest).exists(),
            "the official store is the Node artifact; no extracted duplicate is needed"
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn completed_cache_stays_warm_when_its_root_is_read_only() {
        use std::os::unix::fs::PermissionsExt;

        let base = fresh_cache_dir("read-only-warm");
        let view = test_view();
        materialize_test_node(&view, &base);
        materialize_test_app(&view, &base);

        fs::set_permissions(&base, fs::Permissions::from_mode(0o555)).unwrap();
        assert!(
            verify_warm_cache(&view, &base).is_some(),
            "content verification must not require write access"
        );
        fs::set_permissions(&base, fs::Permissions::from_mode(0o755)).unwrap();
        let _ = fs::remove_dir_all(&base);
    }

    /// A published extraction from a payload that no longer runs here is reclaimed,
    /// and the tree THIS artifact resolves to is not — even when it is just as old.
    ///
    /// That second half is the whole safety property: published trees are keyed by
    /// content, so their mtime is creation time and never advances, and an artifact
    /// used daily for a year looks exactly as old as one abandoned on day one. Only
    /// the `current` exclusion tells them apart.
    #[cfg(unix)]
    #[test]
    fn an_unused_published_extraction_is_reclaimed_but_the_live_one_is_not() {
        let base = fresh_cache_dir("unused-cache-eviction");
        // A REALISTIC key: `test_view`'s default is the literal `app-cache-key`,
        // which is not hex, so the sweep would never consider it a candidate and
        // the "live tree survives" assertion below would pass without the
        // exclusion doing any work. Verified by breaking the exclusion on purpose.
        let mut view = test_view();
        view.manifest.app_sha256 = "0f1e2d3c4b5a69788796a5b4c3d2e1f0".to_string();
        let view = view;
        materialize_test_node(&view, &base);
        materialize_test_app(&view, &base);
        assert!(verify_warm_cache(&view, &base).is_some());

        let app = app_cache_dir(&base, &view.manifest);
        let node = node_cache_dir(&base, &view.manifest);

        // Another payload's trees, in the shapes the sweep recognises.
        let other_app = app.parent().unwrap().join("00112233445566778899aabb");
        create_staging_dir(&other_app).unwrap();
        let other_node = node.parent().unwrap().join("22.1.0-ffeeddccbbaa9988");
        create_staging_dir(&other_node).unwrap();
        // A name the sweep must not claim: neither key shape.
        let unrelated = app.parent().unwrap().join("not-a-content-key");
        create_staging_dir(&unrelated).unwrap();

        for path in [&app, &node, &other_app, &other_node, &unrelated] {
            age_past(path, UNUSED_CACHE_MAX_AGE);
        }

        cleanup_launcher_orphans(&base, &view.manifest);

        assert!(
            !other_app.exists(),
            "an unused published app extraction should be reclaimed"
        );
        assert!(
            !other_node.exists(),
            "an unused published Node extraction should be reclaimed"
        );
        assert!(
            app.exists() && node.exists(),
            "the trees this artifact resolves to must survive at any age"
        );
        assert!(
            unrelated.exists(),
            "a directory that is not a content key is not ours to remove"
        );
        assert!(
            verify_warm_cache(&view, &base).is_some(),
            "the cache must still verify warm after a sweep"
        );
    }

    #[cfg(unix)]
    #[test]
    fn warm_launch_reclaims_owned_stage_and_moved_stale_orphans() {
        let base = fresh_cache_dir("warm-orphan-cleanup");
        let view = test_view();
        materialize_test_node(&view, &base);
        materialize_test_app(&view, &base);
        assert!(verify_warm_cache(&view, &base).is_some());

        let stage = base.join(".compile-app.400.1.tmp");
        create_staging_dir(&stage).unwrap();
        let app = app_cache_dir(&base, &view.manifest);
        let app_stale = app.parent().unwrap().join(format!(
            ".{}.stale.400.2",
            app.file_name().unwrap().to_string_lossy()
        ));
        create_staging_dir(&app_stale).unwrap();
        let node = node_cache_dir(&base, &view.manifest);
        let node_stale = node.parent().unwrap().join(format!(
            ".{}.stale.400.3",
            node.file_name().unwrap().to_string_lossy()
        ));
        create_staging_dir(&node_stale).unwrap();
        let retired_stale = app.parent().unwrap().join(".retired-payload.stale.400.4");
        create_staging_dir(&retired_stale).unwrap();
        let unrelated = app.parent().unwrap().join(format!(
            ".{}.stale.not-a-launcher-suffix",
            app.file_name().unwrap().to_string_lossy()
        ));
        create_staging_dir(&unrelated).unwrap();
        for path in [&stage, &app_stale, &node_stale, &retired_stale, &unrelated] {
            age_stage_for_cleanup(path);
        }

        // Process A left the old paths, process B already published the complete
        // artifacts above, and process C reaches this unconditionally after warm
        // cache resolution.
        cleanup_launcher_orphans(&base, &view.manifest);

        assert!(!stage.exists(), "old app stage survived a warm launch");
        assert!(!app_stale.exists(), "old app swap survived a warm launch");
        assert!(!node_stale.exists(), "old Node swap survived a warm launch");
        assert!(
            !retired_stale.exists(),
            "a swap from an older compiled payload survived a warm launch"
        );
        assert!(
            unrelated.exists(),
            "unrecognized stale lookalike was removed"
        );
        assert!(verify_warm_cache(&view, &base).is_some());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn publication_moves_an_incomplete_tree_aside_before_replacement() {
        let base = fresh_cache_dir("transactional-repair");
        let dest = base.join("dest");
        create_staging_dir(&dest).unwrap();
        write_file(&dest.join("partially-extracted"), b"old").unwrap();

        let stale = match move_incomplete_cache_path_aside_locked(&dest, |_| false).unwrap() {
            ExistingCachePath::Moved(stale) => stale,
            _ => panic!("incomplete destination was not moved aside"),
        };
        assert!(
            !dest.exists(),
            "the old tree must leave the canonical name before anything deletes it"
        );
        assert_eq!(fs::read(stale.join("partially-extracted")).unwrap(), b"old");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn publication_replaces_unmarked_destination() {
        let base = fresh_cache_dir("repair");
        let tmp = base.join("tmp");
        let dest = base.join("dest");
        create_staging_dir(&tmp).unwrap();
        write_file(&tmp.join("node"), b"complete").unwrap();
        create_staging_dir(&dest).unwrap();
        write_file(&dest.join("node"), b"interrupted").unwrap();

        publish_cache_dir(&tmp, &dest, |dir| {
            marked_test_artifact_is_ready(dir, &dir.join("node"))
        })
        .unwrap();
        assert_eq!(fs::read(dest.join("node")).unwrap(), b"complete");
        assert!(dest.join(CACHE_COMPLETE_MARKER).is_file());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn publication_adopts_a_complete_concurrent_winner() {
        let base = fresh_cache_dir("winner");
        let tmp = base.join("tmp");
        let dest = base.join("dest");
        create_staging_dir(&tmp).unwrap();
        write_file(&tmp.join("node"), b"candidate").unwrap();
        create_staging_dir(&dest).unwrap();
        write_file(&dest.join("node"), b"winner").unwrap();
        seal_cache_dir(&dest).unwrap();

        publish_cache_dir(&tmp, &dest, |dir| {
            marked_test_artifact_is_ready(dir, &dir.join("node"))
        })
        .unwrap();
        assert_eq!(fs::read(dest.join("node")).unwrap(), b"winner");
        assert!(
            !tmp.exists(),
            "the losing staged tree must be discarded after adopting the winner"
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn concurrent_publications_serialize_stale_cleanup() {
        use std::sync::{Arc, Barrier};

        let base = fresh_cache_dir("concurrent-publication");
        let dest = base.join("dest");
        create_staging_dir(&dest).unwrap();
        write_file(&dest.join("node"), b"interrupted").unwrap();

        let make_tmp = |name: &str, contents: &[u8]| {
            let tmp = base.join(name);
            create_staging_dir(&tmp).unwrap();
            write_file(&tmp.join("node"), contents).unwrap();
            tmp
        };
        let first = make_tmp("first", b"first complete");
        let second = make_tmp("second", b"second complete");

        // Start two real publishers at the same stale destination. Each seals a
        // distinct tree, then contends on the production per-artifact lock;
        // exactly one may remove the unmarked tree and the other must adopt the
        // completed winner.
        let start = Arc::new(Barrier::new(3));
        let publish = |tmp: PathBuf| {
            let dest = dest.clone();
            let start = Arc::clone(&start);
            std::thread::spawn(move || {
                start.wait();
                publish_cache_dir(&tmp, &dest, |dir| {
                    marked_test_artifact_is_ready(dir, &dir.join("node"))
                })
            })
        };
        let first_publisher = publish(first);
        let second_publisher = publish(second);
        start.wait();

        first_publisher.join().unwrap().unwrap();
        second_publisher.join().unwrap().unwrap();
        let node = fs::read(dest.join("node")).unwrap();
        assert!(node == b"first complete" || node == b"second complete");
        assert!(marked_test_artifact_is_ready(&dest, &dest.join("node")));
        let _ = fs::remove_dir_all(&base);
    }

    /// The `--smol` checksum gate: an archive whose SHA-256 does not match
    /// SHASUMS256.txt must be REFUSED (so provisioning bails before the
    /// rename-into-store), and a matching one accepted. This is what keeps a
    /// shell-out-provisioned Node from poisoning the shared store.
    #[test]
    fn checksum_gate_refuses_mismatch_and_accepts_match() {
        let dir = std::env::temp_dir().join(format!("nub-smol-sum-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let filename = "node-v99.99.99-test.tar.xz";
        let tarball = dir.join(filename);
        fs::write(&tarball, b"pretend tarball bytes").unwrap();
        let real = sha256_hex_of_file(&tarball).unwrap();

        // Wrong hash listed → refused (mismatch), nothing extracted.
        let bad = format!("{}  {filename}\n", "0".repeat(64));
        let err = verify_archive_checksum(&tarball, &bad, filename).unwrap_err();
        assert!(
            format!("{err:#}").contains("checksum mismatch"),
            "got: {err:#}"
        );

        // Not listed at all → refused (fail-closed, never accept an absent entry).
        let absent = format!("{real}  some-other-file.tar.xz\n");
        assert!(verify_archive_checksum(&tarball, &absent, filename).is_err());

        // Correct hash → accepted.
        let good = format!("{real}  {filename}\n");
        assert!(verify_archive_checksum(&tarball, &good, filename).is_ok());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn smol_selects_the_archive_format_published_for_each_host() {
        assert_eq!(archive_ext_for_os("windows"), "zip");
        assert_eq!(archive_ext_for_os("macos"), "tar.xz");
        assert_eq!(archive_ext_for_os("linux"), "tar.xz");
    }

    #[test]
    fn smol_publication_marks_and_validates_the_staged_node_tree() {
        let base = fresh_cache_dir("smol-publication");
        let store = base.join("node");
        create_staging_subdirs(&base, &store).unwrap();
        let staged = base.join("staged-node");
        create_staging_dir(&staged).unwrap();
        let staged_node = node_in_version_dir(&staged);
        create_staging_subdirs(&staged, staged_node.parent().unwrap()).unwrap();
        write_file(&staged_node, b"node").unwrap();
        set_executable(&staged_node).unwrap();
        let dest = store.join("22.15.0");

        publish_cache_dir(&staged, &dest, smol_node_cache_is_ready).unwrap();
        assert!(completion_marker_is_ready(&dest));
        assert!(smol_node_cache_is_ready(&dest));
        let _ = fs::remove_dir_all(&base);
    }

    #[cfg(windows)]
    fn write_everyone_writable_test_file(path: &Path, bytes: &[u8]) {
        let shared = path
            .parent()
            .unwrap()
            .join(format!(".shared-{}", rand_suffix()));
        cache::create_shared_test_directory(&shared).unwrap();
        let inherited = shared.join("payload");
        fs::write(&inherited, bytes).unwrap();
        fs::rename(&inherited, path).unwrap();
        fs::remove_dir(&shared).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_readiness_rejects_shared_nested_app_and_node_files() {
        let base = fresh_cache_dir("windows-shared-nested-artifacts");
        let view = test_view();
        let app = materialize_test_app(&view, &base);
        let nested = app.join("nested").join("data.json");
        fs::remove_file(&nested).unwrap();
        write_everyone_writable_test_file(&nested, br#"{"ok":true}"#);
        assert!(
            !app_cache_is_ready(&view, &app),
            "an Everyone-writable nested app file was accepted"
        );

        let version_dir = base.join("node").join(&view.manifest.node_version);
        create_staging_subdirs(&base, &version_dir).unwrap();
        let node = node_in_version_dir(&version_dir);
        write_everyone_writable_test_file(&node, b"node");
        seal_cache_dir(&version_dir).unwrap();
        assert!(
            !smol_node_cache_is_ready(&version_dir),
            "an Everyone-writable Node executable was accepted"
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[cfg(windows)]
    #[test]
    fn windows_publication_rejects_a_shared_parent() {
        let base = fresh_cache_dir("windows-shared-publication-parent");
        let tmp = base.join("tmp");
        create_staging_dir(&tmp).unwrap();
        write_file(&tmp.join("node"), b"candidate").unwrap();
        let parent = base.join("shared-parent");
        cache::create_shared_test_directory(&parent).unwrap();
        let dest = parent.join("dest");

        let error = publish_cache_dir(&tmp, &dest, |dir| {
            marked_test_artifact_is_ready(dir, &dir.join("node"))
        })
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("trusted directory")
                || format!("{error:#}").contains("revalidating"),
            "got: {error:#}"
        );
        assert!(
            !dest.exists(),
            "artifact was published below a shared parent"
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[cfg(windows)]
    #[test]
    fn windows_publication_rejects_a_shared_lock_file() {
        let base = fresh_cache_dir("windows-shared-publication-lock");
        let tmp = base.join("tmp");
        create_staging_dir(&tmp).unwrap();
        write_file(&tmp.join("node"), b"candidate").unwrap();
        let dest = base.join("dest");
        write_everyone_writable_test_file(&base.join("dest.nub-publish.lock"), b"");

        let error = publish_cache_dir(&tmp, &dest, |dir| {
            marked_test_artifact_is_ready(dir, &dir.join("node"))
        })
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("not a trusted regular file"),
            "got: {error:#}"
        );
        assert!(
            !dest.exists(),
            "artifact was published through a shared lock"
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn smol_candidate_policy_matches_path_and_store_discovery() {
        let floor = NodeVersion::new(22, 15, 0);
        let older = NodeVersion::new(20, 11, 0);
        let newer = NodeVersion::new(24, 14, 0);
        let exact = SmolTarget::legacy(floor.clone(), true);
        let legacy_floor = SmolTarget::legacy(floor.clone(), false);

        assert!(smol_candidate_matches(&floor, &exact));
        assert!(!smol_candidate_matches(&older, &exact));
        assert!(!smol_candidate_matches(&newer, &exact));
        assert!(smol_candidate_matches(&newer, &legacy_floor));

        // PATH probing shells out only to identify `node --version`; selection is
        // pure so this exercises the shared policy without launching a process.
        assert!(select_path_node((PathBuf::from("node"), floor), &exact).is_some());
        assert!(select_path_node((PathBuf::from("node"), newer), &exact).is_none());

        let range = SmolTarget {
            floor: NodeVersion::new(22, 0, 0),
            exact: false,
            range: Some(parse_target_spec(">=22 <23").unwrap()),
        };
        assert!(smol_candidate_matches(&NodeVersion::new(22, 23, 1), &range));
        assert!(!smol_candidate_matches(&NodeVersion::new(21, 7, 3), &range));
        assert!(!smol_candidate_matches(&NodeVersion::new(23, 0, 0), &range));
        assert!(!smol_candidate_matches(&NodeVersion::new(26, 5, 0), &range));
        assert!(
            select_path_node((PathBuf::from("node"), NodeVersion::new(26, 5, 0)), &range).is_none(),
            "PATH must apply the range ceiling too"
        );
    }

    #[test]
    fn smol_manifest_range_is_parsed_once_and_malformed_data_fails_closed() {
        let mut manifest = test_manifest();
        manifest.shape = Shape::Smol;
        manifest.node_version = "22.0.0".to_string();
        manifest.smol_version_range = ">=22 <23".to_string();
        let target = smol_target(&manifest).unwrap();
        assert!(target.matches(&NodeVersion::new(22, 23, 1)));
        assert!(!target.matches(&NodeVersion::new(23, 0, 0)));

        manifest.smol_version_range = ">=".to_string();
        assert!(
            smol_target(&manifest)
                .unwrap_err()
                .to_string()
                .contains("compiled target range"),
            "corrupt range metadata must fail before any external Node relaxes the cache"
        );

        manifest.smol_version_range = "22".to_string();
        assert!(
            smol_target(&manifest)
                .unwrap_err()
                .to_string()
                .contains("is not a semver range"),
            "the dedicated field must not silently reinterpret another pin form"
        );

        manifest.smol_exact_target = true;
        manifest.smol_version_range = ">=22 <23".to_string();
        assert!(
            smol_target(&manifest)
                .unwrap_err()
                .to_string()
                .contains("both exact and a semver range"),
            "conflicting manifest policy bits must fail closed"
        );

        manifest.smol_exact_target = false;
        manifest.node_version = "24.0.0".to_string();
        assert!(
            smol_target(&manifest)
                .unwrap_err()
                .to_string()
                .contains("does not satisfy its range"),
            "the encoded floor must remain consistent with the encoded range"
        );
    }

    #[cfg(unix)]
    fn fake_version_manager_node(root: &Path, claimed: &str, script: &str) -> PathBuf {
        let node = node_in_version_dir(&root.join(claimed));
        fs::create_dir_all(node.parent().unwrap()).unwrap();
        fs::write(&node, script).unwrap();
        set_executable(&node).unwrap();
        node
    }

    #[cfg(unix)]
    #[test]
    fn version_manager_scan_rejects_a_deceptive_directory_version_then_falls_through() {
        let base = fresh_cache_dir("smol-probed-version-manager");
        let root = base.join("versions");
        fake_version_manager_node(&root, "26.0.0", "#!/bin/sh\nprintf 'v20.11.0\\n'\n");
        let valid =
            fake_version_manager_node(&root, "24.14.0", "#!/bin/sh\nprintf 'v24.14.0\\n'\n");
        let dir = NodeDir {
            root,
            inner: "",
            cache_owned: false,
        };

        let found = best_node_in_probed(&dir, &NodeVersion::new(22, 15, 0), false, "darwin-arm64");
        assert_eq!(found, Some((valid, NodeVersion::new(24, 14, 0))));
        let _ = fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn version_manager_scan_skips_an_unrunnable_candidate_then_falls_through() {
        let base = fresh_cache_dir("smol-unrunnable-version-manager");
        let root = base.join("versions");
        fake_version_manager_node(&root, "26.0.0", "#!/definitely/not/a/real/interpreter\n");
        let valid =
            fake_version_manager_node(&root, "24.14.0", "#!/bin/sh\nprintf 'v24.14.0\\n'\n");
        let dir = NodeDir {
            root,
            inner: "",
            cache_owned: false,
        };

        let found = best_node_in_probed(&dir, &NodeVersion::new(22, 15, 0), false, "darwin-arm64");
        assert_eq!(found, Some((valid, NodeVersion::new(24, 14, 0))));
        let _ = fs::remove_dir_all(&base);
    }

    /// A truncated extraction is rejected, and an intact one accepted.
    ///
    /// This is the failure the size check exists for, and it is not hypothetical: an
    /// OS or antivirus sweep clearing cached files is what made `vercel/pkg` ship an
    /// always-copy warm path and then revert it, and macOS `~/Library/Caches` is
    /// purgeable, so a compiled artifact sits in the same blast radius. The digest
    /// that used to run here caught this too — for the cost of re-hashing ~107 MB on
    /// every launch, which measured as 62% of the artifact's entire warm start.
    ///
    /// The intact half is the control: without it a check that rejected everything
    /// would satisfy the truncation assertion and look correct.
    #[cfg(unix)]
    #[test]
    fn a_truncated_extraction_is_rejected_and_an_intact_one_is_not() {
        let base = fresh_cache_dir("node-size-check");
        let node = base.join("node");
        let bytes = b"#!/bin/sh\nprintf 'v26.5.0\\n'\n";
        fs::write(&node, bytes).unwrap();

        let mut manifest = test_view().manifest;
        // Digests that can never match, so any fallback to hashing fails BOTH halves
        // — the test cannot pass by silently taking the old path.
        manifest.node_blake3 = "0".repeat(64);
        manifest.node_sha256 = "0".repeat(64);
        manifest.node_size = bytes.len() as u64;

        assert!(
            embedded_node_file_is_ready(&node, &manifest),
            "an intact extraction whose length matches the manifest must be accepted"
        );

        fs::write(&node, &bytes[..bytes.len() - 1]).unwrap();
        assert!(
            !embedded_node_file_is_ready(&node, &manifest),
            "a truncated extraction must be rejected — this is the field failure the \
             size check replaced the per-launch digest to catch"
        );
        fs::write(&node, bytes).unwrap();
        manifest.node_size = 0;
        manifest.node_blake3 = blake3::hash(bytes).to_hex().to_string();
        assert!(
            !embedded_node_file_is_ready(&node, &manifest),
            "a zero node_size must be rejected rather than reaching an unshipped \
             digest-compatibility path"
        );
        let _ = fs::remove_dir_all(&base);
    }

    /// A compile cache written by Node must not invalidate the app extraction.
    ///
    /// `app_cache_is_ready` requires the extraction directory to hold EXACTLY the
    /// payload's files, so anything Node writes inside it makes the tree mismatch —
    /// and the app is then re-extracted on every launch, forever, because the
    /// re-extraction re-creates the directory that breaks the check. That is not
    /// hypothetical: pointing `NODE_COMPILE_CACHE` at `app_dir/.v8-compile-cache`
    /// did exactly this and cost 32.6 ms per launch, measured, while the cache
    /// itself never survived. The feature spent time instead of saving it.
    #[cfg(unix)]
    #[test]
    fn a_node_written_compile_cache_does_not_invalidate_the_app_extraction() {
        let base = fresh_cache_dir("compile-cache-outside-app");
        let view = test_view();
        let app_dir = app_cache_dir(&base, &view.manifest);
        fs::create_dir_all(&app_dir).unwrap();
        for file in &view.app_files {
            let dest = app_dir.join(&file.name);
            fs::create_dir_all(dest.parent().unwrap()).unwrap();
            fs::write(&dest, file.bytes).unwrap();
        }
        // The marker is an EMPTY regular file — `completion_marker_is_ready`
        // requires len == 0, so any content here would fail the control below.
        fs::write(app_dir.join(CACHE_COMPLETE_MARKER), b"").unwrap();
        assert!(
            app_cache_is_ready(&view, &app_dir),
            "control: a freshly written extraction must be ready, or the assertion \
             below would pass for the wrong reason"
        );

        // Exactly what Node does with NODE_COMPILE_CACHE pointed at a directory.
        let cache = compile_cache_dir(&base, &view.manifest);
        fs::create_dir_all(&cache).unwrap();
        fs::write(cache.join("12345.blob"), b"v8 code cache").unwrap();

        assert!(
            app_cache_is_ready(&view, &app_dir),
            "the compile cache must live OUTSIDE the extraction: a warm launch that \
             re-extracts the app can never converge, because re-extracting re-creates \
             the directory whose presence broke the check"
        );
        assert!(
            !cache.starts_with(&app_dir),
            "the compile cache directory must not be inside the app extraction"
        );
        let _ = fs::remove_dir_all(&base);
    }

    /// Collecting external candidates must not execute any of them.
    ///
    /// This is the property the split exists for: ranking is decided from directory
    /// NAMES, which cost nothing, so a host with several version managers spends one
    /// `node` spawn instead of one per manager. Measured on a four-manager box:
    /// 4 execs and 75 ms before the app's own Node started, where the first probe had
    /// already found a satisfying release; after, 1 exec and 26 ms.
    ///
    /// The fixtures are deliberately NOT runnable as Node — they exit non-zero and
    /// print no version. If collection probed them they would be rejected and the
    /// list would come back empty, so an empty list is exactly the failure this
    /// catches. Verification still happens, just later and only for the winner.
    #[cfg(unix)]
    #[test]
    fn external_candidates_are_ranked_by_name_without_being_executed() {
        let base = fresh_cache_dir("external-rank-no-exec");
        let root = base.join("versions");
        // `exit 1`: a real probe of either of these yields None.
        fake_version_manager_node(&root, "22.15.0", "#!/bin/sh\nexit 1\n");
        fake_version_manager_node(&root, "26.5.0", "#!/bin/sh\nexit 1\n");

        // `cache_owned: false` is the whole point — `NodeDir::plain` means nub's OWN
        // store, whose directory names it wrote itself and therefore trusts. An
        // external manager's names are only a hint, which is the path under test.
        let dir = NodeDir {
            root,
            inner: "",
            cache_owned: false,
        };
        let target = SmolTarget::legacy(NodeVersion::new(22, 15, 0), false);
        let (owned, ranked) = best_node_in(&dir, &target, "darwin-arm64");

        assert!(
            owned.is_none(),
            "an external directory is never trusted unprobed"
        );
        assert_eq!(
            ranked.len(),
            2,
            "both candidates must be collected WITHOUT being executed — an empty or \
             short list means collection probed them"
        );
        assert_eq!(
            ranked[0].1,
            NodeVersion::new(26, 5, 0),
            "candidates must be ranked newest-first, so the caller probes the best one"
        );

        // And the control for the claim above: these fixtures really are unprobeable,
        // so the assertions cannot be passing because probing happens to succeed.
        assert!(
            probe_node_version(&ranked[0].0).is_none(),
            "the fixture must be unprobeable, otherwise this test proves nothing"
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn version_manager_scan_applies_exact_policy_to_the_reported_version() {
        let base = fresh_cache_dir("smol-exact-probed-version-manager");
        let root = base.join("versions");
        fake_version_manager_node(&root, "26.0.0", "#!/bin/sh\nprintf 'v24.14.0\\n'\n");
        let valid =
            fake_version_manager_node(&root, "22.15.0", "#!/bin/sh\nprintf 'v22.15.0\\n'\n");
        let dir = NodeDir {
            root,
            inner: "",
            cache_owned: false,
        };

        let found = best_node_in_probed(&dir, &NodeVersion::new(22, 15, 0), true, "darwin-arm64");
        assert_eq!(found, Some((valid, NodeVersion::new(22, 15, 0))));
        let _ = fs::remove_dir_all(&base);
    }

    /// The install-root scanner behind both nub's own store and every version
    /// manager: newest satisfying install wins, a below-floor or non-`x.y.z` dir
    /// name is skipped (mise's `22` / `lts` alias dirs), and the fnm layout's
    /// `installation/` segment is honored.
    #[test]
    fn provisioning_prefers_the_resolved_version_over_the_floor() {
        // The floor is the ACCEPTANCE rule, not a download instruction. Fetching it
        // installs the oldest release the binary tolerates, which is how `--target 26`
        // landed on 26.0.0 with 26.6.0 already out.
        let mut m = test_manifest();
        m.node_version = "26.0.0".to_string();
        m.provision_version = "26.6.0".to_string();
        assert_eq!(
            smol_provision_version(&m).map(|v| v.to_string()).as_deref(),
            Some("26.6.0"),
            "a resolved provision version must win over the floor"
        );

        // A legacy payload names none, and a malformed one must not fail the launch:
        // the floor is always a valid thing to fetch.
        for raw in ["", "   ", "not-a-version"] {
            m.provision_version = raw.to_string();
            assert!(
                smol_provision_version(&m).is_none(),
                "{raw:?} must fall back to the floor rather than failing the launch"
            );
        }
    }

    #[test]
    fn scans_an_install_root_for_the_newest_satisfying_node() {
        let dir = fresh_cache_dir("smol-scan");
        let plain = dir.join("plain");
        let install = |root: &Path, ver: &str, inner: &str| {
            let bin = node_in_version_dir(&root.join(ver).join(inner));
            create_staging_subdirs(&dir, bin.parent().unwrap()).unwrap();
            fs::write(&bin, b"#!/bin/sh\n").unwrap();
            set_executable(&bin).unwrap();
        };
        for ver in ["v20.11.0", "v22.15.0", "v24.14.0", "lts", "22"] {
            install(&plain, ver, "");
        }

        let target = NodeVersion::new(22, 15, 0);
        let found = best_node_in_probed(
            &NodeDir::plain(plain.clone()),
            &target,
            false,
            "darwin-arm64",
        );
        assert_eq!(
            found.map(|(_, v)| v.to_string()).as_deref(),
            Some("24.14.0"),
            "the newest install at or above the floor must win"
        );

        // Nothing satisfies a floor above every install → provisioning territory.
        let above = NodeVersion::new(26, 0, 0);
        assert!(
            best_node_in_probed(
                &NodeDir::plain(plain.clone()),
                &above,
                false,
                "darwin-arm64"
            )
            .is_none(),
            "a floor above every install must find nothing rather than a lower Node"
        );

        assert_eq!(
            best_node_in_probed(
                &NodeDir::plain(plain.clone()),
                &target,
                true,
                "darwin-arm64"
            )
            .map(|(_, v)| v),
            Some(target.clone()),
            "exact-target payloads must reject the newer install and select only their target"
        );

        // Once the shared scanner rejects every exact-policy candidate,
        // `acquire_smol_node` reaches its existing exact-version provisioning step.
        let exact_miss = dir.join("exact-miss");
        install(&exact_miss, "v24.14.0", "");
        assert!(
            best_node_in_probed(&NodeDir::plain(exact_miss), &target, true, "darwin-arm64")
                .is_none(),
            "a rejected exact candidate must leave discovery empty for provisioning"
        );

        let bounded = SmolTarget {
            floor: NodeVersion::new(22, 0, 0),
            exact: false,
            range: Some(parse_target_spec(">=22 <23").unwrap()),
        };
        assert_eq!(
            best_node_in_policy_probed(&NodeDir::plain(plain.clone()), &bounded, "darwin-arm64")
                .map(|(_, version)| version),
            Some(NodeVersion::new(22, 15, 0)),
            "the install-root scan must skip newer Nodes above a compiled range ceiling"
        );

        // fnm nests the dist under `installation/`; without the segment the same
        // tree looks empty.
        let fnm = dir.join("fnm");
        install(&fnm, "v24.14.0", "installation");
        let as_fnm = NodeDir {
            root: fnm.clone(),
            inner: "installation",
            // This assertion isolates the nested layout; external execution and
            // version probing are covered by the dedicated Unix fixtures above.
            cache_owned: true,
        };
        assert!(best_node_in_probed(&as_fnm, &target, false, "darwin-arm64").is_some());
        assert!(
            best_node_in_probed(&NodeDir::plain(fnm), &target, false, "darwin-arm64").is_none(),
            "the fnm layout must not resolve without its installation/ segment"
        );

        // The libc gate runs INSIDE the scan, not just as a standalone predicate: a
        // shell script where a Linux payload expects an ELF must not be selected and
        // then fail at spawn. (A darwin triple short-circuits the gate, which is why
        // the assertions above could not have caught this.)
        let elf = dir.join("elf");
        let glibc = node_in_version_dir(&elf.join("24.14.0"));
        create_staging_subdirs(&dir, glibc.parent().unwrap()).unwrap();
        fs::write(&glibc, elf_with_interp("/lib64/ld-linux-x86-64.so.2")).unwrap();
        set_executable(&glibc).unwrap();
        assert!(
            best_node_in_probed(&NodeDir::plain(elf.clone()), &target, false, "linux-x64")
                .is_some(),
            "a glibc Node must be accepted for a glibc target"
        );
        assert!(
            best_node_in_probed(&NodeDir::plain(elf), &target, false, "linux-x64-musl").is_none(),
            "a glibc Node must not be selected for a musl target"
        );

        #[cfg(unix)]
        {
            // A present but non-executable node loses to the next candidate rather
            // than being chosen and failing at spawn. Windows has no executable
            // mode bit; a regular `node.exe` is its executable-file predicate.
            let stripped = dir.join("stripped");
            let bin = node_in_version_dir(&stripped.join("24.14.0"));
            create_staging_subdirs(&dir, bin.parent().unwrap()).unwrap();
            fs::write(&bin, b"#!/bin/sh\n").unwrap();
            assert!(
                best_node_in_probed(&NodeDir::plain(stripped), &target, false, "darwin-arm64")
                    .is_none(),
                "a non-executable bin/node must be skipped"
            );
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_escaping_payload_file_names() {
        assert!(is_safe_relative_name("main.js"));
        assert!(is_safe_relative_name("chunk-abc.js"));
        assert!(is_safe_relative_name("nested/chunk.js"));
        assert!(!is_safe_relative_name(""));
        assert!(!is_safe_relative_name("../evil"));
        assert!(!is_safe_relative_name("a/../../evil"));
        assert!(!is_safe_relative_name("/etc/passwd"));
        assert!(!is_safe_relative_name("./main.js"));
    }

    /// A minimal 64-bit LE ELF carrying one `PT_INTERP` program header pointing at
    /// `interp` — enough to exercise the libc classifier without a real binary.
    fn elf_with_interp(interp: &str) -> Vec<u8> {
        let mut b = vec![0u8; 64];
        b[0..4].copy_from_slice(b"\x7fELF");
        b[4] = 2; // 64-bit
        b[5] = 1; // little-endian
        b[0x20..0x28].copy_from_slice(&64u64.to_le_bytes()); // e_phoff
        b[0x36..0x38].copy_from_slice(&56u16.to_le_bytes()); // e_phentsize (64-bit)
        b[0x38..0x3a].copy_from_slice(&1u16.to_le_bytes()); // e_phnum
        let interp_off = 64 + 56;
        let mut interp_bytes = interp.as_bytes().to_vec();
        interp_bytes.push(0);
        let mut ph = vec![0u8; 56];
        ph[0..4].copy_from_slice(&3u32.to_le_bytes()); // p_type = PT_INTERP
        ph[8..16].copy_from_slice(&(interp_off as u64).to_le_bytes()); // p_offset
        ph[32..40].copy_from_slice(&(interp_bytes.len() as u64).to_le_bytes()); // p_filesz
        b.extend_from_slice(&ph);
        b.extend_from_slice(&interp_bytes);
        b
    }

    /// A 64-bit LE ELF with no program headers → statically linked.
    fn elf_static() -> Vec<u8> {
        let mut b = vec![0u8; 64];
        b[0..4].copy_from_slice(b"\x7fELF");
        b[4] = 2;
        b[5] = 1;
        b[0x20..0x28].copy_from_slice(&64u64.to_le_bytes());
        b[0x36..0x38].copy_from_slice(&56u16.to_le_bytes());
        b[0x38..0x3a].copy_from_slice(&0u16.to_le_bytes());
        b
    }

    #[test]
    fn elf_interp_classifier_reads_the_libc() {
        assert_eq!(
            classify_elf_interp(&elf_with_interp("/lib64/ld-linux-x86-64.so.2")),
            Some(Some(ElfLibc::Glibc))
        );
        assert_eq!(
            classify_elf_interp(&elf_with_interp("/lib/ld-linux-aarch64.so.1")),
            Some(Some(ElfLibc::Glibc))
        );
        assert_eq!(
            classify_elf_interp(&elf_with_interp("/lib/ld-musl-x86_64.so.1")),
            Some(Some(ElfLibc::Musl))
        );
        // Valid ELF, no interpreter → static, libc-agnostic.
        assert_eq!(classify_elf_interp(&elf_static()), Some(None));
        // Not an ELF at all → unclassifiable.
        assert_eq!(classify_elf_interp(b"#!/usr/bin/env node\n"), None);
    }

    /// The dedup gate: a musl store Node is refused for a glibc payload (the VM
    /// bug), a glibc one for a musl payload, and either is reused for its matching
    /// libc; a non-Linux target has no libc split and always reuses.
    /// A dynamic-linker failure is recognised and named; anything else is not.
    ///
    /// The negative half is what keeps this from hijacking unrelated failures. A
    /// Node that exits non-zero for its own reasons — a bad flag, a syntax error —
    /// must fall through untouched, or the launcher replaces a real diagnostic
    /// with a confident guess about a library that is fine.
    #[test]
    fn a_missing_shared_library_is_named_and_other_failures_are_left_alone() {
        let glibc = "node: error while loading shared libraries: libatomic.so.1: \
                     cannot open shared object file: No such file or directory";
        assert_eq!(
            missing_shared_library(glibc).as_deref(),
            Some("libatomic.so.1"),
            "the library's name is what the reader needs to act on"
        );
        // musl words it differently, and matching only glibc's phrasing left every
        // Alpine failure showing the raw loader error.
        let musl = "Error loading shared library libstdc++.so.6: No such file or \
                    directory (needed by /root/.cache/nub/.../addon.node)";
        assert_eq!(
            missing_shared_library(musl).as_deref(),
            Some("libstdc++.so.6"),
            "musl's wording must be recognised too"
        );
        assert_eq!(alpine_package_for("libgcc_s.so.1"), "libgcc");
        assert_eq!(debian_package_for("libatomic.so.1"), "libatomic1");
        assert_eq!(alpine_package_for("libatomic.so.1"), "libatomic");

        // An unmapped library still yields something searchable rather than
        // nothing, so the message degrades instead of disappearing.
        assert_eq!(debian_package_for("libfoo.so.9"), "libfoo.so.9");

        for unrelated in [
            "node: bad option: --nope",
            "SyntaxError: Unexpected token",
            "",
            // The case that actually exercises the prefix check. The three above
            // are also rejected by the parse that follows it, so on their own they
            // pass whether or not the check exists — a control that cannot fail.
            "note: for background on shared libraries: see the linker manual",
        ] {
            assert_eq!(
                missing_shared_library(unrelated),
                None,
                "an ordinary failure must not be reported as a missing library: {unrelated:?}"
            );
        }
    }

    #[test]
    fn store_node_gate_matches_only_compatible_libc() {
        let dir = std::env::temp_dir().join(format!("nub-libc-gate-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let glibc = dir.join("glibc-node");
        let musl = dir.join("musl-node");
        fs::write(&glibc, elf_with_interp("/lib64/ld-linux-x86-64.so.2")).unwrap();
        fs::write(&musl, elf_with_interp("/lib/ld-musl-x86_64.so.1")).unwrap();

        assert!(store_node_matches_target(&glibc, "linux-x64"));
        assert!(!store_node_matches_target(&musl, "linux-x64"));
        assert!(store_node_matches_target(&musl, "linux-x64-musl"));
        assert!(!store_node_matches_target(&glibc, "linux-x64-musl"));
        // No libc concept off Linux → always compatible.
        assert!(store_node_matches_target(&musl, "darwin-arm64"));

        let _ = fs::remove_dir_all(&dir);
    }
}
