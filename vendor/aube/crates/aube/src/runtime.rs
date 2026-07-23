//! Node runtime context: which node aube should put on PATH for scripts,
//! exec, dlx, and lifecycle hooks.
//!
//! Explicit installs resolve once per invocation inside [`scope`], so parallel
//! projects cannot replace one another's selected runtime. Other CLI commands
//! retain the process-wide fallback resolved through [`ensure`]. Every spawn
//! site reads the active snapshot through [`current`] / [`path_entries`] /
//! [`node_program`] / [`apply_child_env`]. A project with no runtime
//! configuration resolves to a pass-through context — PATH untouched.

use aube_manifest::PackageJson;
use aube_settings::ResolveCtx;
use miette::miette;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Where the version requirement came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeSource {
    DevEngines,
    NodeVersionFile,
    Nvmrc,
    /// No requirement configured (or policy said keep the ambient
    /// node) — PATH is left alone.
    PathFallback,
    /// A host embedding aube (e.g. mise) supplied the node runtime to
    /// use for lifecycle scripts, rather than aube resolving one itself.
    Embedder,
}

impl RuntimeSource {
    pub fn label(self) -> &'static str {
        match self {
            RuntimeSource::DevEngines => "devEngines.runtime",
            RuntimeSource::NodeVersionFile => ".node-version",
            RuntimeSource::Nvmrc => ".nvmrc",
            RuntimeSource::PathFallback => "PATH",
            RuntimeSource::Embedder => "embedder",
        }
    }
}

/// Who provided the resolved node binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeProvenance {
    Mise,
    AubeManaged,
    System,
}

impl RuntimeProvenance {
    pub fn label(self) -> &'static str {
        match self {
            RuntimeProvenance::Mise => "mise",
            RuntimeProvenance::AubeManaged => aube_util::embedder().name,
            RuntimeProvenance::System => "system",
        }
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeContext {
    /// Directory to prepend to PATH for child processes. `None` means
    /// no switching (ambient node already satisfies, or no config).
    pub bin_dir: Option<PathBuf>,
    /// Absolute path of the selected node binary, when one resolved.
    pub node_bin: Option<PathBuf>,
    /// Exact resolved version (`"24.4.1"`), when known.
    pub version: Option<String>,
    /// The requested range/spec as written (`"^24.4.0"`, `"lts/jod"`).
    pub requested: Option<String>,
    pub source: RuntimeSource,
    pub provenance: RuntimeProvenance,
    /// Full per-platform pin computed during a network resolve —
    /// the install pipeline records it into the lockfile.
    pub fresh_pin: Option<aube_runtime::PinnedNode>,
}

impl RuntimeContext {
    /// Pass-through context: PATH untouched, no probing. Deliberately
    /// lazy — `aubr <script>` on a project with no runtime config must
    /// not pay a `node --version` spawn; consumers that need the
    /// ambient version (engines checks, doctor) probe on their own
    /// memoized path.
    fn path_fallback() -> RuntimeContext {
        RuntimeContext {
            bin_dir: None,
            node_bin: None,
            version: None,
            requested: None,
            source: RuntimeSource::PathFallback,
            provenance: RuntimeProvenance::System,
            fresh_pin: None,
        }
    }
}

static RUNTIME: tokio::sync::OnceCell<Arc<RuntimeContext>> = tokio::sync::OnceCell::const_new();

type RuntimeSlot = Arc<tokio::sync::OnceCell<Arc<RuntimeContext>>>;

tokio::task_local! {
    static INSTALL_RUNTIME: RuntimeSlot;
}

/// Run an install with an isolated runtime slot. Standalone commands outside
/// this scope retain the process-wide runtime selected by the CLI.
pub async fn scope<F: Future>(future: F) -> F::Output {
    INSTALL_RUNTIME
        .scope(Arc::new(tokio::sync::OnceCell::new()), future)
        .await
}

/// Propagate the current install's runtime slot into a spawned task.
pub fn scope_current<F: Future>(future: F) -> impl Future<Output = F::Output> {
    let runtime = INSTALL_RUNTIME.try_with(Arc::clone).ok();
    async move {
        match runtime {
            Some(runtime) => INSTALL_RUNTIME.scope(runtime, future).await,
            None => future.await,
        }
    }
}

/// The resolved context, if [`ensure`] has run.
pub fn current() -> Option<Arc<RuntimeContext>> {
    match INSTALL_RUNTIME.try_with(|runtime| runtime.get().map(Arc::clone)) {
        Ok(runtime) => runtime,
        Err(_) => RUNTIME.get().map(Arc::clone),
    }
}

/// Seed the current install's runtime slot with a node binary supplied by an
/// embedding host (e.g. mise), so lifecycle scripts spawn on that node and
/// find it on PATH instead of relying on an ambient `node`.
///
/// `bin_dir` is the directory containing the `node` executable; it is
/// prepended to the script PATH ([`path_entries`]) and its `node` is used as
/// [`node_program`]. Must be called inside a [`scope`] (the install task) and
/// before [`ensure`] runs — `ensure` returns early when the slot is already
/// set, so this override wins without aube probing for its own runtime. A
/// no-op outside a scope or if the slot is already populated.
pub async fn seed_embedder_node(bin_dir: PathBuf) {
    // No-op outside an install scope, or when the slot is already seeded.
    // Check first so the path/version probing below is skipped entirely in
    // those cases (a later `set` would be a no-op anyway).
    let should_seed = INSTALL_RUNTIME
        .try_with(|slot| slot.get().is_none())
        .unwrap_or(false);
    if !should_seed {
        return;
    }
    // Absolutize: lifecycle scripts may run with a different working
    // directory, so both `node_program()` and the prepended PATH entry must
    // resolve independently of cwd. `absolute` doesn't require the dir to
    // exist or touch symlinks; fall back to the input if it errors.
    let bin_dir = std::path::absolute(&bin_dir).unwrap_or(bin_dir);
    let node_exe = if cfg!(windows) { "node.exe" } else { "node" };
    let node_bin = bin_dir.join(node_exe);
    // Probe the supplied node for its version so engine checks and
    // virtual-store hashing key off the *same* node as lifecycle scripts,
    // instead of `effective_node_version` falling back to an ambient `node`.
    // Async spawn so the install task doesn't block a Tokio worker on the
    // child process.
    let version = tokio::process::Command::new(&node_bin)
        .arg("--version")
        .output()
        .await
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|v| v.trim().trim_start_matches('v').to_string())
        .filter(|v| !v.is_empty());
    let ctx = RuntimeContext {
        node_bin: Some(node_bin),
        bin_dir: Some(bin_dir),
        version,
        requested: None,
        source: RuntimeSource::Embedder,
        provenance: RuntimeProvenance::Mise,
        fresh_pin: None,
    };
    let _ = INSTALL_RUNTIME.try_with(|slot| {
        let _ = slot.set(Arc::new(ctx));
    });
}

/// The node executable spawn sites should use: the switched runtime's
/// binary when one resolved, otherwise the embedder-provisioned node
/// ([`EngineContext::runtime_node_bin`]) when an embedder owns provisioning,
/// otherwise bare `"node"` (PATH lookup at spawn time, today's behavior).
///
/// [`EngineContext::runtime_node_bin`]: aube_util::EngineContext::runtime_node_bin
pub fn node_program() -> PathBuf {
    resolve_node_bin(
        current().and_then(|c| c.node_bin.clone()),
        aube_util::engine_context().runtime_node_bin,
        None,
    )
    .unwrap_or_else(|| PathBuf::from("node"))
}

/// PATH entries to prepend (after `node_modules/.bin`) when spawning
/// scripts/binaries. The switched runtime's bin dir when one resolved;
/// otherwise the embedder-provisioned node dir
/// ([`EngineContext::runtime_node_dir`]) when an embedder owns provisioning
/// (`runtime_switching = false`) — the boundary that lets a fetched-bin
/// `exec node` shebang resolve on a machine with no system `node`. Empty when
/// neither applies (standalone aube with no active switch — unchanged).
///
/// [`EngineContext::runtime_node_dir`]: aube_util::EngineContext::runtime_node_dir
pub fn path_entries() -> Vec<PathBuf> {
    resolve_path_entry(
        current().and_then(|c| c.bin_dir.clone()),
        aube_util::engine_context().runtime_node_dir,
    )
    .into_iter()
    .collect()
}

/// Set the npm-compat env vars on a child command: `npm_node_execpath` (and
/// `NODE`, which npm also exports) naming the node binary, and
/// `npm_config_user_agent` naming the running PM.
///
/// The UA is pnpm parity for the dlx/exec bin paths (the four callers of this
/// fn): pnpm exports `npm_config_user_agent` to `pnpm dlx` / `pnpm create` /
/// `pnpm exec` children exactly as it does to lifecycle scripts, and create-*
/// scaffolders sniff it to emit the invoking PM's commands — without it they
/// fall back to npm-mode. Same product/format as the lifecycle export
/// ([`aube_scripts::aube_user_agent`]), so a tool sees one UA whether it ran
/// as a postinstall or under dlx.
///
/// Prefers the switched runtime's node; then the embedder-provisioned node
/// ([`EngineContext::runtime_node_bin`]) when an embedder owns provisioning —
/// so on a node-less machine these stay the provisioned node instead of
/// resolving to nothing; then the ambient `node` on `PATH`. pnpm/npm always
/// set them, and tools (`node-gyp`, `node-pre-gyp`, re-spawners) read
/// `npm_node_execpath` to locate the exact node that drove the package manager.
///
/// [`EngineContext::runtime_node_bin`]: aube_util::EngineContext::runtime_node_bin
pub fn apply_child_env(cmd: &mut tokio::process::Command) {
    cmd.env("npm_config_user_agent", aube_scripts::aube_user_agent());
    let node_bin = resolve_node_bin(
        current().and_then(|ctx| ctx.node_bin.clone()),
        aube_util::engine_context().runtime_node_bin,
        aube_runtime::node_on_path(),
    );
    if let Some(node_bin) = node_bin {
        cmd.env("npm_node_execpath", &node_bin);
        cmd.env("NODE", &node_bin);
    }
}

/// The node-binary fallback ladder shared by [`node_program`] and
/// [`apply_child_env`]: the switched runtime's node wins; then the
/// embedder-provisioned node (set when an embedder owns provisioning and the
/// resolver stayed inert); then the ambient fallback the caller supplies (the
/// PATH `node` for env vars, `None` for `node_program`'s bare-`"node"` case).
/// Pure so the precedence is unit-tested without touching the process globals.
fn resolve_node_bin(
    switched: Option<PathBuf>,
    embedder: Option<PathBuf>,
    ambient: Option<PathBuf>,
) -> Option<PathBuf> {
    switched.or(embedder).or(ambient)
}

/// The PATH-entry fallback for [`path_entries`]: the switched runtime's bin dir
/// wins; else the embedder-provisioned node dir. Pure for the same reason.
fn resolve_path_entry(switched: Option<PathBuf>, embedder: Option<PathBuf>) -> Option<PathBuf> {
    switched.or(embedder)
}

/// The runtime-relevant settings, extracted from a `ResolveCtx` so
/// async resolution doesn't need to hold the (non-`'static`) context
/// across awaits.
#[derive(Debug, Clone, Default)]
pub struct RuntimeSettings {
    pub installer: aube_runtime::InstallerMode,
    pub on_fail_override: Option<aube_manifest::OnFail>,
    pub mirror: Option<String>,
    /// `--offline` blocks runtime downloads the same way it blocks
    /// registry fetches (caches still serve). `--prefer-offline` maps
    /// to Online — the runtime caches are already consulted first.
    pub network: aube_runtime::NetworkMode,
    /// `Embedder::runtime_switching` (aube default true). When false the
    /// resolver is inert: no version-file probe, no provisioning, `PATH`
    /// untouched. An embedder that owns Node provisioning itself sets this
    /// off. Embedder-fixed, not a per-project setting.
    pub switching: bool,
}

impl RuntimeSettings {
    pub fn from_ctx(ctx: &ResolveCtx<'_>) -> Self {
        let installer = match aube_settings::resolved::runtime_installer(ctx) {
            aube_settings::resolved::RuntimeInstaller::Auto => aube_runtime::InstallerMode::Auto,
            aube_settings::resolved::RuntimeInstaller::Mise => aube_runtime::InstallerMode::Mise,
            aube_settings::resolved::RuntimeInstaller::Aube => aube_runtime::InstallerMode::Aube,
        };
        let on_fail_override =
            aube_settings::resolved::runtime_on_fail(ctx).map(|forced| match forced {
                aube_settings::resolved::RuntimeOnFail::Download => aube_manifest::OnFail::Download,
                aube_settings::resolved::RuntimeOnFail::Error => aube_manifest::OnFail::Error,
                aube_settings::resolved::RuntimeOnFail::Warn => aube_manifest::OnFail::Warn,
                aube_settings::resolved::RuntimeOnFail::Ignore => aube_manifest::OnFail::Ignore,
            });
        RuntimeSettings {
            installer,
            on_fail_override,
            mirror: release_mirror(ctx),
            network: aube_runtime::NetworkMode::Online,
            switching: aube_util::embedder().runtime_switching,
        }
    }
}

/// The lockfile's recorded `node` runtime pin, read cheaply enough
/// for hot `aubr` paths: only the pnpm-shaped lockfiles can carry a
/// pin, and a substring probe gates the full YAML parse — unpinned
/// projects (the overwhelming majority) pay a page-cached file read,
/// pinned projects pay one parse. The process-global OnceCell means
/// this runs at most once per process, so an `aubr` warm path and the
/// install pipeline resolve from the *same* pin — without this, the
/// first `ensure_for_cwd` caller would lock in a pin-less resolution
/// and `aubr` could drift from what `aube install` pinned.
///
/// Branch lockfiles (`gitBranchLockfile`) and custom `lockfileDir`
/// layouts aren't probed — those projects resolve the range fresh,
/// which is the pre-pin behavior, never an error.
pub(crate) fn lockfile_node_pin(
    project_dir: &Path,
    manifest: &PackageJson,
    parse_options: aube_lockfile::ParseOptions,
) -> Option<aube_lockfile::RuntimePin> {
    let pinned = std::iter::once(aube_util::embedder().lockfile_basename)
        .chain(
            aube_util::embedder()
                .lockfile_legacy_basenames
                .iter()
                .copied(),
        )
        .chain(std::iter::once("pnpm-lock.yaml"))
        .any(|name| {
            std::fs::read_to_string(project_dir.join(name))
                .map(|s| s.contains("specifier: runtime:"))
                .unwrap_or(false)
        });
    if !pinned {
        return None;
    }
    let (graph, _) =
        aube_lockfile::parse_lockfile_with_kind_and_options(project_dir, manifest, parse_options)
            .ok()?;
    graph.runtimes.get("node").cloned()
}

/// [`ensure`] for commands that haven't loaded settings/manifests yet
/// (dlx, run/exec warm paths): loads the settings for `cwd`'s project
/// root, reads the lockfile pin, and resolves from there.
pub async fn ensure_for_cwd(cwd: &Path) -> miette::Result<Arc<RuntimeContext>> {
    if let Some(ctx) = current() {
        return Ok(ctx);
    }
    let project_dir = crate::dirs::find_project_root(cwd).unwrap_or_else(|| cwd.to_path_buf());
    let manifest =
        aube_manifest::PackageJson::from_path_cached(&project_dir.join("package.json")).ok();
    let settings = crate::commands::with_settings_ctx(&project_dir, RuntimeSettings::from_ctx);
    let parse_options =
        crate::commands::with_settings_ctx(&project_dir, |ctx| aube_lockfile::ParseOptions {
            strict_store_integrity: aube_settings::resolved::strict_store_integrity(ctx)
                || aube_settings::resolved::paranoid(ctx),
        });
    let pin = manifest
        .as_deref()
        .and_then(|m| lockfile_node_pin(&project_dir, m, parse_options));
    ensure(&project_dir, manifest.as_deref(), settings, pin.as_ref()).await
}

/// Resolve the project's runtime once for this process.
///
/// `manifest` is the root manifest when the caller already has it
/// parsed (install path); commands without one (dlx outside a
/// project) pass `None` and only version files apply. `lock_pin` is
/// the lockfile's recorded pin for `node`, if any.
pub async fn ensure(
    project_dir: &Path,
    manifest: Option<&PackageJson>,
    settings: RuntimeSettings,
    lock_pin: Option<&aube_lockfile::RuntimePin>,
) -> miette::Result<Arc<RuntimeContext>> {
    let lock_pin = lock_pin.cloned();
    let project_dir = project_dir.to_path_buf();
    let manifest = manifest.cloned();
    if let Ok(runtime) = INSTALL_RUNTIME.try_with(Arc::clone) {
        return runtime
            .get_or_try_init(|| async {
                resolve_context(project_dir, manifest, settings, lock_pin)
                    .await
                    .map(Arc::new)
            })
            .await
            .map(Arc::clone);
    }
    RUNTIME
        .get_or_try_init(|| async {
            resolve_context(project_dir, manifest, settings, lock_pin)
                .await
                .map(Arc::new)
        })
        .await
        .map(Arc::clone)
}

async fn resolve_context(
    project_dir: PathBuf,
    manifest: Option<PackageJson>,
    settings: RuntimeSettings,
    lock_pin: Option<aube_lockfile::RuntimePin>,
) -> miette::Result<RuntimeContext> {
    // `Embedder::runtime_switching == false` makes the resolver inert: no
    // version-file probe, no provisioning, PATH untouched. Returns the same fallback the
    // no-pin path produces, so every downstream `current()` consumer sees an
    // unswitched runtime.
    if !settings.switching {
        return Ok(RuntimeContext::path_fallback());
    }
    let project_dir = project_dir.as_path();
    let manifest = manifest.as_ref();
    let lock_pin = lock_pin.as_ref();
    let dev_engines = manifest
        .and_then(|m| m.dev_engines.as_ref())
        .and_then(|d| d.node_runtime())
        .and_then(|r| {
            r.version
                .as_deref()
                .map(|v| (v, r.on_fail, project_dir.join("package.json")))
        });
    if let Some(unsupported) = manifest
        .and_then(|m| m.dev_engines.as_ref())
        .map(|d| d.unsupported_runtimes())
        .filter(|u| !u.is_empty())
    {
        tracing::debug!(
            runtimes = ?unsupported,
            "ignoring non-node devEngines.runtime entries"
        );
    }

    let request = aube_runtime::effective_request(
        dev_engines.as_ref().map(|(v, f, p)| (*v, *f, p.as_path())),
        project_dir,
    )
    .map_err(|e| miette!(code = e.code(), "{e}"))?;

    let Some(mut request) = request else {
        return Ok(RuntimeContext::path_fallback());
    };

    // `runtimeOnFail` overrides whatever the manifest / version-file
    // defaults said (pnpm 11 parity; `error` is the air-gapped-CI
    // "never download" switch).
    if let Some(forced) = settings.on_fail_override {
        request.on_fail = forced;
    }

    let cfg = aube_runtime::RuntimeConfig {
        installer: settings.installer,
        mirror: settings.mirror.clone(),
        network: settings.network,
        retries: 2,
    };

    let source = match request.source {
        aube_runtime::RequestSource::DevEngines => RuntimeSource::DevEngines,
        aube_runtime::RequestSource::NodeVersionFile => RuntimeSource::NodeVersionFile,
        aube_runtime::RequestSource::Nvmrc => RuntimeSource::Nvmrc,
    };
    let requested = request.raw.clone();

    // Only honor the lockfile pin when it still satisfies the request
    // — a drifted pin must not win over the manifest (the install
    // pipeline re-pins separately).
    let pinned = lock_pin
        .filter(|pin| pin.specifier == requested)
        .map(pinned_from_lockfile)
        .transpose()
        .map_err(|e| miette!("{e}"))?;

    let runtime = aube_runtime::NodeRuntime::new(cfg);
    let resolved = runtime
        .resolve(&request, pinned.as_ref(), &CliProgress::node())
        .await
        .map_err(|e| miette!(code = e.code(), "{e}"))?;

    Ok(match resolved {
        None => {
            // onFail ignore/warn kept the ambient node; the warn (if
            // any) already went through tracing.
            let mut ctx = RuntimeContext::path_fallback();
            ctx.requested = Some(requested);
            ctx.source = source;
            ctx
        }
        Some(res) => RuntimeContext {
            bin_dir: res.bin_dir.clone(),
            node_bin: Some(res.node_bin.clone()),
            version: Some(res.version.to_string()),
            requested: Some(requested),
            source,
            provenance: match res.from {
                aube_runtime::ResolvedFrom::PathEnv => RuntimeProvenance::System,
                aube_runtime::ResolvedFrom::Installed(origin)
                | aube_runtime::ResolvedFrom::FreshInstall(origin) => match origin {
                    aube_runtime::InstallOrigin::Mise => RuntimeProvenance::Mise,
                    aube_runtime::InstallOrigin::Aube => RuntimeProvenance::AubeManaged,
                },
            },
            fresh_pin: res.fresh_pin,
        },
    })
}

/// `nodeDownloadMirrors.release` from the raw workspace yaml (pnpm 11
/// keeps this map in pnpm-workspace.yaml; there is no flat npmrc
/// spelling for a nested map).
fn release_mirror(ctx: &ResolveCtx<'_>) -> Option<String> {
    let yaml_serde::Value::Mapping(map) = ctx.workspace_yaml.get("nodeDownloadMirrors")? else {
        return None;
    };
    map.iter().find_map(|(k, v)| match (k, v) {
        (yaml_serde::Value::String(key), yaml_serde::Value::String(url))
            if key == "release" && !url.trim().is_empty() =>
        {
            Some(url.trim().to_string())
        }
        _ => None,
    })
}

/// Convert the lockfile's recorded pin into the resolver's interchange
/// shape (one [`aube_runtime::PinnedVariant`] per target triple).
fn pinned_from_lockfile(
    pin: &aube_lockfile::RuntimePin,
) -> Result<aube_runtime::PinnedNode, aube_runtime::Error> {
    let version = node_semver::Version::parse(&pin.version).map_err(|e| {
        aube_runtime::Error::NoMatchingVersion {
            requested: format!("lockfile pin {}: {e}", pin.version),
            platform_note: String::new(),
        }
    })?;
    let mut variants = Vec::new();
    for v in &pin.variants {
        for t in &v.targets {
            variants.push(aube_runtime::PinnedVariant {
                os: t.os.clone(),
                cpu: t.cpu.clone(),
                libc: t.libc.clone(),
                archive: v.archive.clone(),
                url: v.url.clone(),
                integrity_sri: v.integrity.clone(),
                bin: v.bin.clone(),
                prefix: v.prefix.clone(),
            });
        }
    }
    Ok(aube_runtime::PinnedNode { version, variants })
}

/// Bring `graph.runtimes["node"]` in line with the manifest's
/// `devEngines.runtime` and the resolved runtime context. Called by
/// the install pipeline right before the lockfile is written.
///
/// - devEngines absent → any stale pin is dropped (version-file pins
///   are never recorded; pnpm parity).
/// - Foreign lockfile formats (npm/yarn/bun) have no runtime shape:
///   warn once and leave the graph alone.
/// - Pin current (same range, same resolved version) → no-op.
/// - Otherwise record the pin, reusing the resolution's fresh
///   SHASUMS-derived variant set when available and fetching it
///   (cached) when the runtime was satisfied locally.
pub async fn refresh_lockfile_pin(
    graph: &mut aube_lockfile::LockfileGraph,
    manifest: &PackageJson,
    settings: RuntimeSettings,
    write_kind: aube_lockfile::LockfileKind,
) -> miette::Result<()> {
    let declared = manifest
        .dev_engines
        .as_ref()
        .and_then(|d| d.node_runtime())
        .and_then(|r| r.version.clone());
    let Some(range) = declared else {
        graph.runtimes.remove("node");
        return Ok(());
    };
    if !matches!(
        write_kind,
        aube_lockfile::LockfileKind::Aube | aube_lockfile::LockfileKind::Pnpm
    ) {
        if !graph.runtimes.contains_key("node") {
            tracing::warn!(
                code = aube_codes::warnings::WARN_AUBE_RUNTIME_PIN_NOT_RECORDED,
                format = ?write_kind,
                "devEngines.runtime resolved but this lockfile format cannot record a runtime pin; subsequent runs re-resolve the range"
            );
        }
        return Ok(());
    }
    let Some(version) = current().and_then(|c| c.version.clone()) else {
        // Resolution kept the ambient node (onFail warn/ignore) or
        // never ran — nothing concrete to pin.
        return Ok(());
    };
    if graph
        .runtimes
        .get("node")
        .is_some_and(|p| p.specifier == range && p.version == version)
    {
        return Ok(());
    }
    let fresh = current().and_then(|c| c.fresh_pin.clone());
    let pin = match fresh.filter(|p| p.version.to_string() == version) {
        Some(p) => p,
        None => {
            let cfg = aube_runtime::RuntimeConfig {
                installer: settings.installer,
                mirror: settings.mirror.clone(),
                network: aube_runtime::NetworkMode::Online,
                retries: 2,
            };
            let spec = aube_runtime::NodeSpec::parse(&version)
                .map_err(|e| miette!(code = e.code(), "{e}"))?;
            match aube_runtime::NodeRuntime::new(cfg)
                .resolve_for_lockfile(&spec)
                .await
            {
                Ok(p) => p,
                Err(e) => {
                    // Recording the pin is best-effort: an offline
                    // install that satisfied the range locally must
                    // not fail because checksums couldn't be fetched.
                    tracing::warn!(
                        code = aube_codes::warnings::WARN_AUBE_RUNTIME_PIN_NOT_RECORDED,
                        error = %e,
                        "could not fetch runtime checksums to record the lockfile pin"
                    );
                    return Ok(());
                }
            }
        }
    };
    graph
        .runtimes
        .insert("node".to_string(), lockfile_pin_from(&pin, &range));
    Ok(())
}

/// Convert a freshly-resolved pin into the lockfile shape, tagged with
/// the request range. `dev: true` matches pnpm (devEngines pins land
/// under devDependencies).
pub fn lockfile_pin_from(
    pin: &aube_runtime::PinnedNode,
    specifier: &str,
) -> aube_lockfile::RuntimePin {
    aube_lockfile::RuntimePin {
        specifier: specifier.to_string(),
        version: pin.version.to_string(),
        dev: true,
        has_bin: true,
        variants: pin
            .variants
            .iter()
            .map(|v| aube_lockfile::RuntimeVariant {
                targets: vec![aube_lockfile::RuntimeTarget {
                    os: v.os.clone(),
                    cpu: v.cpu.clone(),
                    libc: v.libc.clone(),
                }],
                archive: v.archive.clone(),
                url: v.url.clone(),
                integrity: v.integrity_sri.clone(),
                bin: v.bin.clone(),
                bin_is_bare_string: false,
                prefix: v.prefix.clone(),
            })
            .collect(),
    }
}

/// Progress reporter for runtime installs.
///
/// Two cooperating modes, mirroring how `aube install` treats the
/// terminal:
///
/// - **Self-downloads** get a live clx progress bar (spinner, byte
///   counts, phase label) — the same renderer `aube install` uses —
///   degrading to plain `safe_eprintln` lines when clx is in text
///   mode (`--silent`, `-v`, line reporters) or stderr is not a
///   terminal.
/// - **mise delegation** pauses any live clx renderer for the
///   duration of the child (`on_external_tool_*`) so mise's own
///   progress output owns the terminal instead of fighting ours.
pub(crate) struct CliProgress {
    /// Display name: `Node.js` for runtime installs, `aube` for
    /// self-version installs.
    tool: &'static str,
    state: std::sync::Mutex<CliProgressState>,
}

#[derive(Default)]
struct CliProgressState {
    version: Option<String>,
    job: Option<std::sync::Arc<clx::progress::ProgressJob>>,
    /// True when the text-mode fallback announced the download.
    announced: bool,
    downloaded: u64,
    total: Option<u64>,
    /// Whether `on_external_tool_start` paused a previously-running
    /// renderer (and so `on_external_tool_end` must resume it).
    paused_for_tool: bool,
}

impl CliProgress {
    pub(crate) fn node() -> Self {
        Self::for_tool("Node.js")
    }

    pub(crate) fn aube() -> Self {
        Self::for_tool(aube_util::embedder().name)
    }

    fn for_tool(tool: &'static str) -> Self {
        CliProgress {
            tool,
            state: std::sync::Mutex::new(CliProgressState::default()),
        }
    }

    fn fancy_output() -> bool {
        use std::io::IsTerminal;
        clx::progress::output() != clx::progress::ProgressOutput::Text
            && std::io::stderr().is_terminal()
    }

    fn label(&self, version: &str, phase: &str) -> String {
        if phase.is_empty() {
            format!("{} v{version}", self.tool)
        } else {
            format!("{} v{version} ({phase})", self.tool)
        }
    }

    fn bytes_prop(state: &CliProgressState) -> String {
        match state.total {
            Some(total) if total > 0 => format!(
                "{} / {}",
                crate::progress::format_bytes(state.downloaded),
                crate::progress::format_bytes(total)
            ),
            _ => crate::progress::format_bytes(state.downloaded),
        }
    }
}

impl aube_runtime::DownloadProgress for CliProgress {
    fn on_phase(&self, version: Option<&node_semver::Version>, phase: aube_runtime::InstallPhase) {
        use aube_runtime::InstallPhase;
        let mut state = self.state.lock().unwrap();
        if let Some(v) = version {
            state.version = Some(v.to_string());
        }
        let version = state.version.clone().unwrap_or_default();
        match phase {
            InstallPhase::Resolving => {}
            InstallPhase::Downloading => {
                if !Self::fancy_output() && !state.announced {
                    state.announced = true;
                    crate::progress::safe_eprintln(&format!(
                        "Downloading {} v{version}…",
                        self.tool
                    ));
                }
            }
            InstallPhase::Verifying => {
                if let Some(job) = &state.job {
                    job.prop("label", &self.label(&version, "verifying…"));
                }
            }
            InstallPhase::Extracting => {
                if let Some(job) = &state.job {
                    job.prop("label", &self.label(&version, "extracting…"));
                }
            }
        }
    }

    fn on_download_start(&self, total_bytes: Option<u64>) {
        if !Self::fancy_output() {
            return;
        }
        let mut state = self.state.lock().unwrap();
        state.total = total_bytes;
        let version = state.version.clone().unwrap_or_default();
        let builder = clx::progress::ProgressJobBuilder::new()
            .body("{{spinner()}} {{label}}  {{progress_bar(flex=true)}} {{bytes}}")
            .body_text(Some("{{label}} {{bytes}}"))
            .prop("label", &self.label(&version, ""))
            .prop("bytes", "")
            .status(clx::progress::ProgressStatus::Running)
            .progress_current(0)
            // No Content-Length → hold the bar empty and let the byte
            // counter carry the signal (both GitHub and nodejs.org
            // send a length in practice).
            .progress_total(total_bytes.unwrap_or(1).max(1) as usize);
        state.job = Some(builder.start());
    }

    fn on_download_chunk(&self, bytes: u64) {
        let mut state = self.state.lock().unwrap();
        state.downloaded += bytes;
        let bytes_text = Self::bytes_prop(&state);
        if let Some(job) = &state.job {
            if state.total.is_some() {
                job.progress_current(state.downloaded as usize);
            }
            job.prop("bytes", &bytes_text);
        }
    }

    fn on_done(&self) {
        let state = self.state.lock().unwrap();
        if let Some(job) = &state.job {
            job.set_status(clx::progress::ProgressStatus::Done);
        } else if state.announced {
            crate::progress::safe_eprintln(&format!(
                "{} v{} installed",
                self.tool,
                state.version.clone().unwrap_or_default()
            ));
        }
    }

    fn on_external_tool_start(&self) {
        let mut state = self.state.lock().unwrap();
        if !clx::progress::is_paused() {
            clx::progress::pause();
            state.paused_for_tool = true;
        }
    }

    fn on_external_tool_end(&self) {
        let mut state = self.state.lock().unwrap();
        if state.paused_for_tool {
            clx::progress::resume();
            state.paused_for_tool = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_context(requested: &str) -> RuntimeContext {
        let mut context = RuntimeContext::path_fallback();
        context.requested = Some(requested.to_string());
        context
    }

    #[tokio::test]
    async fn seed_embedder_node_drives_node_program_and_path() {
        scope(async {
            assert!(current().is_none(), "slot starts empty");
            let bin_dir = PathBuf::from("/opt/mise/node/bin");
            seed_embedder_node(bin_dir.clone()).await;

            // The seed absolutizes the dir; compute the expected the same way
            // so this holds on Windows (where `/opt/...` isn't absolute).
            let expected = std::path::absolute(&bin_dir).unwrap_or(bin_dir);
            let node_exe = if cfg!(windows) { "node.exe" } else { "node" };

            let ctx = current().expect("slot seeded");
            assert_eq!(ctx.source, RuntimeSource::Embedder);
            assert_eq!(ctx.bin_dir.as_deref(), Some(expected.as_path()));
            assert_eq!(node_program(), expected.join(node_exe));
            assert_eq!(path_entries(), vec![expected.clone()]);

            // `ensure`-style early return: a second seed does not clobber the
            // first (OnceCell is set once).
            seed_embedder_node(PathBuf::from("/other")).await;
            assert_eq!(node_program(), expected.join(node_exe));
        })
        .await;
    }

    #[tokio::test]
    async fn seed_embedder_node_is_a_noop_outside_scope() {
        // No install scope active: the seed can't set a task-local slot, so it
        // must not panic and must not leak into a later scope. Asserting via a
        // fresh `scope` reads the (empty) task-local, not the process-wide
        // `RUNTIME`, so this stays deterministic regardless of test order.
        seed_embedder_node(PathBuf::from("/opt/mise/node/bin")).await;
        scope(async {
            assert!(current().is_none(), "seed outside a scope must not leak in");
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn install_runtime_is_isolated_and_propagated() {
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let first_barrier = Arc::clone(&barrier);
        let second_barrier = Arc::clone(&barrier);

        let first = scope(async move {
            INSTALL_RUNTIME.with(|runtime| runtime.set(Arc::new(test_context("first"))).unwrap());
            first_barrier.wait().await;
            tokio::spawn(scope_current(async {
                current().and_then(|runtime| runtime.requested.clone())
            }))
            .await
            .unwrap()
        });
        let second = scope(async move {
            INSTALL_RUNTIME.with(|runtime| runtime.set(Arc::new(test_context("second"))).unwrap());
            second_barrier.wait().await;
            tokio::spawn(scope_current(async {
                current().and_then(|runtime| runtime.requested.clone())
            }))
            .await
            .unwrap()
        });

        let (first, second) = tokio::join!(first, second);
        assert_eq!(first.as_deref(), Some("first"));
        assert_eq!(second.as_deref(), Some("second"));
    }

    #[test]
    fn node_bin_prefers_switched_then_embedder_then_ambient() {
        let sw = PathBuf::from("/switched/node");
        let emb = PathBuf::from("/embedder/node");
        let amb = PathBuf::from("/usr/bin/node");
        // Switched runtime wins over everything.
        assert_eq!(
            resolve_node_bin(Some(sw.clone()), Some(emb.clone()), Some(amb.clone())),
            Some(sw)
        );
        // No switch (embedder owns provisioning) → the embedder's node, NOT the
        // ambient PATH node. This is the #303 fix: on a node-less machine the
        // embedder-provisioned node pins NODE/npm_node_execpath.
        assert_eq!(
            resolve_node_bin(None, Some(emb.clone()), Some(amb.clone())),
            Some(emb)
        );
        // Standalone aube (no embedder node) → ambient fallback, unchanged.
        assert_eq!(resolve_node_bin(None, None, Some(amb.clone())), Some(amb));
        // Nothing anywhere → None (node_program's bare-"node" case).
        assert_eq!(resolve_node_bin(None, None, None), None);
    }

    #[test]
    fn path_entry_switch_wins_else_embedder_dir() {
        let sw = PathBuf::from("/switched/bin");
        let emb = PathBuf::from("/embedder/shim");
        assert_eq!(
            resolve_path_entry(Some(sw.clone()), Some(emb.clone())),
            Some(sw)
        );
        // No switch → the embedder shim dir, so a fetched-bin `exec node`
        // resolves on a node-less machine (#303). Empty for standalone aube.
        assert_eq!(resolve_path_entry(None, Some(emb.clone())), Some(emb));
        assert_eq!(resolve_path_entry(None, None), None);
    }

    #[test]
    fn lockfile_pin_round_trip_shapes() {
        let pin = aube_runtime::PinnedNode {
            version: "24.4.1".parse().unwrap(),
            variants: vec![aube_runtime::PinnedVariant {
                os: "darwin".into(),
                cpu: "arm64".into(),
                libc: None,
                archive: "tarball".into(),
                url: "https://nodejs.org/download/release/v24.4.1/node-v24.4.1-darwin-arm64.tar.gz"
                    .into(),
                integrity_sri: "sha256-AAAA".into(),
                bin: [("node".to_string(), "bin/node".to_string())].into(),
                prefix: None,
            }],
        };
        let lf = lockfile_pin_from(&pin, "^24.4.0");
        assert_eq!(lf.specifier, "^24.4.0");
        assert_eq!(lf.version, "24.4.1");
        assert!(lf.dev);
        let back = pinned_from_lockfile(&lf).unwrap();
        assert_eq!(back, pin);
    }
}
