//! Process-global Node runtime context: which node this aube process
//! should put on PATH for scripts, exec, dlx, and lifecycle hooks.
//!
//! Resolution happens once per process via [`ensure`]; every spawn
//! site reads the snapshot through [`current`] / [`path_entries`] /
//! [`node_program`] / [`apply_child_env`]. A project with no runtime
//! configuration resolves to a pass-through context — PATH untouched,
//! behavior identical to aube before runtime switching existed.

use aube_manifest::PackageJson;
use aube_settings::ResolveCtx;
use miette::miette;
use std::path::{Path, PathBuf};

/// Where the version requirement came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeSource {
    DevEngines,
    NodeVersionFile,
    Nvmrc,
    /// No requirement configured (or policy said keep the ambient
    /// node) — PATH is left alone.
    PathFallback,
}

impl RuntimeSource {
    pub fn label(self) -> &'static str {
        match self {
            RuntimeSource::DevEngines => "devEngines.runtime",
            RuntimeSource::NodeVersionFile => ".node-version",
            RuntimeSource::Nvmrc => ".nvmrc",
            RuntimeSource::PathFallback => "PATH",
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

static RUNTIME: tokio::sync::OnceCell<RuntimeContext> = tokio::sync::OnceCell::const_new();

/// The resolved context, if [`ensure`] has run.
pub fn current() -> Option<&'static RuntimeContext> {
    RUNTIME.get()
}

/// The node executable spawn sites should use: the switched runtime's
/// binary when one resolved, otherwise bare `"node"` (PATH lookup at
/// spawn time, today's behavior).
pub fn node_program() -> PathBuf {
    current()
        .and_then(|c| c.node_bin.clone())
        .unwrap_or_else(|| PathBuf::from("node"))
}

/// PATH entries to prepend (after `node_modules/.bin`) when spawning
/// scripts/binaries. Empty when no switching is active.
pub fn path_entries() -> Vec<PathBuf> {
    current()
        .and_then(|c| c.bin_dir.clone())
        .into_iter()
        .collect()
}

/// Set the npm-compat env vars naming the node binary on a child
/// command (`npm_node_execpath`, and `NODE` which npm also exports).
///
/// Prefers the switched runtime's node; when no switch is active,
/// falls back to the ambient `node` resolved on `PATH` so these vars
/// are populated on every spawn — pnpm/npm always set them, and tools
/// (`node-gyp`, `node-pre-gyp`, re-spawners) read `npm_node_execpath`
/// to locate the exact node that drove the package manager.
pub fn apply_child_env(cmd: &mut tokio::process::Command) {
    let node_bin = current()
        .and_then(|ctx| ctx.node_bin.clone())
        .or_else(aube_runtime::node_on_path);
    if let Some(node_bin) = node_bin {
        cmd.env("npm_node_execpath", &node_bin);
        cmd.env("NODE", &node_bin);
    }
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
) -> Option<aube_lockfile::RuntimePin> {
    let pinned = [aube_util::embedder().lockfile_basename, "pnpm-lock.yaml"]
        .iter()
        .any(|name| {
            std::fs::read_to_string(project_dir.join(name))
                .map(|s| s.contains("specifier: runtime:"))
                .unwrap_or(false)
        });
    if !pinned {
        return None;
    }
    let graph = aube_lockfile::parse_lockfile(project_dir, manifest).ok()?;
    graph.runtimes.get("node").cloned()
}

/// [`ensure`] for commands that haven't loaded settings/manifests yet
/// (dlx, run/exec warm paths): loads the settings for `cwd`'s project
/// root, reads the lockfile pin, and resolves from there.
pub async fn ensure_for_cwd(cwd: &Path) -> miette::Result<&'static RuntimeContext> {
    if let Some(ctx) = current() {
        return Ok(ctx);
    }
    let project_dir = crate::dirs::find_project_root(cwd).unwrap_or_else(|| cwd.to_path_buf());
    let manifest =
        aube_manifest::PackageJson::from_path_cached(&project_dir.join("package.json")).ok();
    let settings = crate::commands::with_settings_ctx(&project_dir, RuntimeSettings::from_ctx);
    let pin = manifest
        .as_deref()
        .and_then(|m| lockfile_node_pin(&project_dir, m));
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
) -> miette::Result<&'static RuntimeContext> {
    let lock_pin = lock_pin.cloned();
    let project_dir = project_dir.to_path_buf();
    let manifest = manifest.cloned();
    RUNTIME
        .get_or_try_init(|| resolve_context(project_dir, manifest, settings, lock_pin))
        .await
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
