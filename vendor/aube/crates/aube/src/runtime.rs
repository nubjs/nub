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
    /// An embedding host described the node invocation directly (see
    /// [`EmbedderRuntime`]) — aube neither resolved nor probed it.
    Embedder,
}

impl RuntimeProvenance {
    pub fn label(self) -> &'static str {
        match self {
            RuntimeProvenance::Mise => "mise",
            RuntimeProvenance::AubeManaged => aube_util::embedder().name,
            RuntimeProvenance::System => "system",
            RuntimeProvenance::Embedder => aube_util::embedder().name,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeContext {
    /// Directory to prepend to PATH for child processes. `None` means
    /// no switching (ambient node already satisfies, or no config).
    pub bin_dir: Option<PathBuf>,
    /// Whether `bin_dir` must precede project-local `.bin` directories.
    /// Wrappers require this so a dependency-provided `node` cannot bypass
    /// the shim; selectors deliberately keep project-local binaries first.
    pub bin_dir_precedes_project_bins: bool,
    /// The node aube spawns and exports as `NODE`: the program a
    /// script's bare `node` / `$NODE` resolves to. For a selector this
    /// is the real binary; for a wrapper it is the shim. `None` falls
    /// back to a bare `node` PATH lookup at spawn time.
    pub node_program: Option<PathBuf>,
    /// The node exported as `npm_node_execpath` — the *real* binary
    /// node-gyp and native-addon tooling read to find Node's install
    /// prefix. Equals [`Self::node_program`] for a selector; points at
    /// the unwrapped binary for a wrapper. `None` falls back to
    /// `node_program`, then to the ambient `node`.
    pub node_execpath: Option<PathBuf>,
    /// The node aube's internal machinery spawns (pnpmfile hooks,
    /// security scanner, version probes) when it should differ from
    /// `node_program` — a wrapping embedder points this at the real
    /// binary so aube's own hot paths skip the wrapper hop. `None`
    /// falls back to `node_program`.
    pub internal_node: Option<PathBuf>,
    /// Exact resolved version (`"24.4.1"`), when known without probing.
    /// An embedder may leave this unset; [`crate::engines`] probes
    /// [`Self::node_execpath`] lazily (memoized) when a version is
    /// actually needed.
    pub version: Option<String>,
    /// The requested range/spec as written (`"^24.4.0"`, `"lts/jod"`).
    pub requested: Option<String>,
    pub source: RuntimeSource,
    pub provenance: RuntimeProvenance,
    /// Embedder-supplied environment applied last to every spawn (see
    /// [`EmbedderRuntime::env_append`] / [`EmbedderRuntime::env_set`]).
    /// Empty unless an embedder contributed env.
    pub env: Vec<EmbedderEnv>,
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
            bin_dir_precedes_project_bins: false,
            node_program: None,
            node_execpath: None,
            internal_node: None,
            version: None,
            requested: None,
            source: RuntimeSource::PathFallback,
            provenance: RuntimeProvenance::System,
            env: Vec::new(),
            fresh_pin: None,
        }
    }
}

static RUNTIME: tokio::sync::OnceCell<Arc<RuntimeContext>> = tokio::sync::OnceCell::const_new();

type RuntimeSlot = Arc<tokio::sync::OnceCell<Arc<RuntimeContext>>>;

tokio::task_local! {
    static INSTALL_RUNTIME: RuntimeSlot;
    /// A per-call embedder runtime for an in-process embed invocation.
    /// Unlike [`INSTALL_RUNTIME`], it is *not* re-scoped by [`scope`], so
    /// it survives the fresh scope `install::run` opens for a nested
    /// install (e.g. `dlx`'s transient install, or `run`/`exec`
    /// auto-install) — that's what lets a per-call runtime reach lifecycle
    /// scripts in those nested installs.
    static PER_CALL_RUNTIME: Arc<RuntimeContext>;
}

/// Run an install with an isolated runtime slot. Standalone commands outside
/// this scope retain the process-wide runtime selected by the CLI.
pub async fn scope<F: Future>(future: F) -> F::Output {
    INSTALL_RUNTIME
        .scope(Arc::new(tokio::sync::OnceCell::new()), future)
        .await
}

/// Propagate the current install's runtime slot — and any active
/// per-call embed runtime — into a spawned task. Task-locals don't cross
/// `tokio::spawn`, so a fan-out that skips this would fall back to the
/// process-wide state inside the child task.
pub fn scope_current<F: Future>(future: F) -> impl Future<Output = F::Output> {
    let runtime = INSTALL_RUNTIME.try_with(Arc::clone).ok();
    let per_call = per_call_runtime();
    async move {
        let future = async move {
            match runtime {
                Some(runtime) => INSTALL_RUNTIME.scope(runtime, future).await,
                None => future.await,
            }
        };
        match per_call {
            Some(per_call) => PER_CALL_RUNTIME.scope(per_call, future).await,
            None => future.await,
        }
    }
}

/// The resolved context, if [`ensure`] has run.
pub fn current() -> Option<Arc<RuntimeContext>> {
    match INSTALL_RUNTIME.try_with(|runtime| runtime.get().map(Arc::clone)) {
        // Inside an install scope the slot is authoritative (it may be
        // `None` mid-resolution, before `ensure`/seed fills it).
        Ok(runtime) => runtime,
        // Non-install commands (dlx/exec/run/node). Precedence: a per-call
        // embed runtime, then a process-wide `set_embedder_runtime` (both
        // authoritative over a `RUNTIME` an earlier `ensure` may have
        // populated, so registration order can't strand these paths on a
        // pre-registration runtime), then the ambient resolved runtime.
        Err(_) => per_call_runtime()
            .or_else(|| EMBEDDER_CONTEXT.get().map(Arc::clone))
            .or_else(|| RUNTIME.get().map(Arc::clone)),
    }
}

fn per_call_runtime() -> Option<Arc<RuntimeContext>> {
    PER_CALL_RUNTIME.try_with(Arc::clone).ok()
}

/// How an embedder-supplied env var combines with any value aube would
/// otherwise set for the same key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvMerge {
    /// Overwrite the key outright.
    Replace,
    /// Space-join after the value aube resolved (e.g. the `nodeOptions`
    /// setting), so an embedder's `--import` preload adds to the user's
    /// `NODE_OPTIONS` rather than dropping it. When aube has no value
    /// for the key this is just the embedder's value.
    Append,
}

/// One embedder-contributed environment entry.
#[derive(Debug, Clone)]
pub struct EmbedderEnv {
    pub key: std::ffi::OsString,
    pub val: std::ffi::OsString,
    pub merge: EnvMerge,
}

/// A host's description of *how Node should be invoked*, rather than
/// merely which `node` to select.
///
/// A version manager selects a toolchain — one binary that is `NODE`,
/// `npm_node_execpath`, and the PATH entry all at once. A wrapper
/// (instrumenting runtime, transpiling loader, sandbox) interposes on
/// Node, and needs those three distinct: a shim on PATH and at `NODE`
/// so `$NODE child.js` stays wrapped, but the *real* binary at
/// `npm_node_execpath` so node-gyp finds Node's install prefix.
///
/// Construct via [`selector`](Self::selector) (today's single-bin-dir
/// behavior) or [`wrapper`](Self::wrapper); unspecified fields derive
/// from what you supply, so no invalid combination is representable.
#[derive(Debug, Clone, Default)]
pub struct EmbedderRuntime {
    bin_dir: Option<PathBuf>,
    path_unchanged: bool,
    bin_dir_precedes_project_bins: bool,
    node_program: Option<PathBuf>,
    node_execpath: Option<PathBuf>,
    internal_node: Option<PathBuf>,
    version: Option<String>,
    env: Vec<EmbedderEnv>,
}

impl EmbedderRuntime {
    /// A version-manager-style runtime: `bin_dir` holds `node` (plus
    /// `npm`/`npx`) and is prepended to PATH; that `node` is both `NODE`
    /// and `npm_node_execpath`. This is the degenerate case that
    /// reproduces the pre-wrapper `node_bin_dir` behavior.
    pub fn selector(bin_dir: impl Into<PathBuf>) -> Self {
        Self {
            bin_dir: Some(bin_dir.into()),
            ..Default::default()
        }
    }

    /// A wrapping runtime: `node_program` is the shim aube spawns and
    /// exports as `NODE`. PATH defaults to its parent directory and
    /// `npm_node_execpath` defaults to it; override with
    /// [`real_node`](Self::real_node) / [`path_dir`](Self::path_dir).
    pub fn wrapper(node_program: impl Into<PathBuf>) -> Self {
        Self {
            node_program: Some(node_program.into()),
            bin_dir_precedes_project_bins: true,
            ..Default::default()
        }
    }

    /// The real, unwrapped node exported as `npm_node_execpath`.
    pub fn real_node(mut self, path: impl Into<PathBuf>) -> Self {
        self.node_execpath = Some(path.into());
        self
    }

    /// The directory prepended to PATH, when it differs from the
    /// program's parent (e.g. a shim dir separate from the real bin).
    pub fn path_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.bin_dir = Some(dir.into());
        self.path_unchanged = false;
        self
    }

    /// Leave PATH unchanged while still using `node_program` for direct
    /// spawns and exporting it as `NODE`. Bare `node` commands then resolve
    /// from the inherited PATH by explicit host choice.
    pub fn without_path(mut self) -> Self {
        self.path_unchanged = true;
        self
    }

    /// The node aube's *internal* machinery spawns — pnpmfile hooks, the
    /// security scanner, version probes — when it should differ from
    /// [`wrapper`](Self::wrapper)'s shim. A wrapping host typically points
    /// this at the real binary so aube's own hot paths skip the wrapper
    /// hop; user-facing spawns (scripts, `NODE`, `node`) stay wrapped.
    /// Defaults to `node_program`.
    pub fn internal_node(mut self, path: impl Into<PathBuf>) -> Self {
        self.internal_node = Some(path.into());
        self
    }

    /// Supply the node version instead of having aube probe for it.
    pub fn version(mut self, version: impl Into<String>) -> Self {
        self.version = Some(version.into());
        self
    }

    /// Contribute an env var to every spawn, appended after any value
    /// aube resolves for the key (see [`EnvMerge::Append`]). This is how
    /// a wrapper adds a `NODE_OPTIONS` preload without clobbering the
    /// user's `nodeOptions`.
    pub fn env_append(
        mut self,
        key: impl Into<std::ffi::OsString>,
        val: impl Into<std::ffi::OsString>,
    ) -> Self {
        self.env.push(EmbedderEnv {
            key: key.into(),
            val: val.into(),
            merge: EnvMerge::Append,
        });
        self
    }

    /// Contribute an env var to every spawn, overwriting any value aube
    /// would set for the key (see [`EnvMerge::Replace`]).
    pub fn env_set(
        mut self,
        key: impl Into<std::ffi::OsString>,
        val: impl Into<std::ffi::OsString>,
    ) -> Self {
        self.env.push(EmbedderEnv {
            key: key.into(),
            val: val.into(),
            merge: EnvMerge::Replace,
        });
        self
    }

    /// Materialize into a [`RuntimeContext`], filling derived paths.
    /// Paths are absolutized so they resolve independently of a script's
    /// working directory; `absolute` neither requires existence nor
    /// touches symlinks, so a not-yet-created shim is fine.
    fn resolve(&self) -> RuntimeContext {
        let abs = |p: &PathBuf| std::path::absolute(p).unwrap_or_else(|_| p.clone());
        let node_exe = if cfg!(windows) { "node.exe" } else { "node" };
        let node_program = self
            .node_program
            .clone()
            .or_else(|| self.bin_dir.as_ref().map(|dir| dir.join(node_exe)))
            .map(|p| abs(&p));
        let bin_dir = if self.path_unchanged {
            None
        } else {
            self.bin_dir.clone().or_else(|| {
                node_program
                    .as_ref()
                    .and_then(|p| p.parent())
                    .map(Path::to_path_buf)
            })
        }
        .map(|p| abs(&p));
        let node_execpath = self
            .node_execpath
            .clone()
            .map(|p| abs(&p))
            .or_else(|| node_program.clone());
        let internal_node = self.internal_node.clone().map(|p| abs(&p));
        RuntimeContext {
            bin_dir,
            bin_dir_precedes_project_bins: self.bin_dir_precedes_project_bins,
            node_program,
            node_execpath,
            internal_node,
            version: self.version.clone().filter(|v| !v.is_empty()),
            requested: None,
            source: RuntimeSource::Embedder,
            provenance: RuntimeProvenance::Embedder,
            env: self.env.clone(),
            fresh_pin: None,
        }
    }
}

/// Process-wide embedder runtime resolved from [`set_embedder_runtime`].
/// Covers every spawn path outside an install scope (dlx/exec/run/node);
/// the install scope seeds its own slot from it (or a per-call override).
static EMBEDDER_CONTEXT: std::sync::OnceLock<Arc<RuntimeContext>> = std::sync::OnceLock::new();

/// Register a process-wide embedder runtime so every aube spawn path —
/// install lifecycle scripts, `dlx`, `exec`, `run`, `node` — invokes
/// Node the way the host describes. First-write-wins, mirroring
/// `set_embedder` / `set_embedder_defaults`. A per-call runtime on
/// `InstallOptions` (or on the embed run-class entry points) takes
/// precedence over this for that call.
///
/// Honored regardless of `runtime_switching`: an explicit invocation is
/// an override, not a resolution aube performs. Registration order
/// doesn't matter — [`current`] and [`ensure`] consult this before any
/// previously-resolved process-wide runtime.
pub fn set_embedder_runtime(runtime: EmbedderRuntime) {
    let _ = EMBEDDER_CONTEXT.set(Arc::new(runtime.resolve()));
}

/// Seed the current install's runtime slot from an embedder-supplied
/// runtime, so lifecycle scripts invoke Node the way the host describes
/// instead of relying on an ambient `node`. `per_call` (from
/// `InstallOptions`) wins outright; otherwise the process-wide runtime
/// from [`set_embedder_runtime`] applies; otherwise this is a no-op and
/// aube resolves its own runtime.
///
/// Must be called inside a [`scope`] (the install task) and before
/// [`ensure`] runs — `ensure` returns early when the slot is set, so
/// this override wins without aube resolving its own runtime. No probing:
/// the version, if any, is supplied on the [`EmbedderRuntime`].
pub fn seed_install_embedder_runtime(per_call: Option<&EmbedderRuntime>) {
    let ctx = match per_call {
        Some(rt) => Arc::new(rt.resolve()),
        // No explicit per-install runtime: inherit the enclosing embed
        // call's per-call runtime (survives this fresh install scope),
        // else the process-wide registration. Neither → aube resolves its
        // own runtime for the install.
        None => match per_call_runtime().or_else(|| EMBEDDER_CONTEXT.get().map(Arc::clone)) {
            Some(ctx) => ctx,
            None => return,
        },
    };
    let _ = INSTALL_RUNTIME.try_with(|slot| {
        let _ = slot.set(ctx);
    });
}

/// Run an in-process embed command (`run`/`exec`/`dlx`/`node`) with its
/// runtime isolated to this call. Always opens a fresh [`scope`] so the
/// call's runtime resolution lands in its own slot rather than reusing a
/// process-wide `RUNTIME` an earlier call resolved — sequential embed
/// calls to different projects don't cross-contaminate.
///
/// Both arms seed the fresh slot through
/// [`seed_install_embedder_runtime`]`(None)`, whose lookup chain is
/// per-call → process-wide [`set_embedder_runtime`] → none (aube
/// resolves for this call). Seeding in the `None` arm is what keeps a
/// *globally* registered runtime effective inside the isolation scope —
/// without it, `ensure` would resolve aube's own runtime in the empty
/// slot and silently bypass the registered wrapper.
///
/// A per-call `runtime` is additionally published to `PER_CALL_RUNTIME`
/// so it reaches lifecycle scripts in any nested install (`dlx`'s
/// transient install, `run`/`exec` auto-install), which open their own
/// scope and otherwise wouldn't see this one's slot.
pub async fn with_embedder_runtime<F: Future>(
    runtime: Option<EmbedderRuntime>,
    future: F,
) -> F::Output {
    let seeded_scope = |future: F| {
        scope(async move {
            seed_install_embedder_runtime(None);
            future.await
        })
    };
    match runtime {
        Some(rt) => {
            PER_CALL_RUNTIME
                .scope(Arc::new(rt.resolve()), seeded_scope(future))
                .await
        }
        None => seeded_scope(future).await,
    }
}

/// The node executable spawn sites should use: the runtime's
/// `node_program` (the wrapper/shim for an embedder, the selected binary
/// otherwise); otherwise the embedder-provisioned node
/// ([`EngineContext::runtime_node_bin`]) when an embedder owns provisioning;
/// else bare `"node"` (PATH lookup at spawn time).
///
/// [`EngineContext::runtime_node_bin`]: aube_util::EngineContext::runtime_node_bin
pub fn node_program() -> PathBuf {
    resolve_node_bin(
        current().and_then(|c| c.node_program.clone()),
        aube_util::engine_context().runtime_node_bin,
        None,
    )
    .unwrap_or_else(|| PathBuf::from("node"))
}

/// The node for aube's *internal* spawns — pnpmfile hooks, the security
/// scanner, version probes. A wrapping embedder may split this off via
/// [`EmbedderRuntime::internal_node`] so aube's own machinery runs on
/// the real binary while user-facing spawns stay wrapped; otherwise it
/// is [`node_program`] — including its embedder-provisioned fallback, so a
/// host that owns provisioning keeps aube's own hooks off an absent ambient
/// `node`.
pub fn internal_node_program() -> PathBuf {
    resolve_node_bin(
        current().and_then(|c| c.internal_node.clone().or_else(|| c.node_program.clone())),
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

/// PATH entries for a Node process dispatched through the activated tool
/// shim. The real Node executable is resolved before these entries are
/// applied, so restoring the tool shim cannot recurse into `aube node`.
/// Wrapping embedder runtimes still lead so their descendant `node` spawns
/// remain wrapped; selectors leave the activated package-manager shims first.
pub(crate) fn node_child_path_entries() -> Vec<PathBuf> {
    let context = current();
    let mut entries = context
        .as_ref()
        .and_then(|c| c.bin_dir.clone())
        .into_iter()
        .collect::<Vec<_>>();
    if let Some(dir) = crate::tool_shims::activated_shim_dir() {
        if context.is_some_and(|c| c.bin_dir_precedes_project_bins) {
            entries.push(dir);
        } else {
            entries.insert(0, dir);
        }
    }
    entries
}

/// Place the active runtime PATH entry around project-local `.bin`
/// directories. Wrappers lead so a local `node` cannot bypass the shim;
/// selectors follow so package-provided commands retain normal precedence.
///
/// The runtime entry resolves exactly as [`path_entries`] does: the switched
/// runtime's bin dir, else the embedder-provisioned node dir. Without that
/// fallback a fetched bin's `#!/usr/bin/env node` under an embedder that owns
/// provisioning found no `node` at all — or the system one, unaugmented.
/// The embedder dir follows the project bins, as it always has.
pub fn path_entries_with_project_bins(project_bins: Vec<PathBuf>) -> Vec<PathBuf> {
    let context = current();
    let switched = context.as_ref().and_then(|c| c.bin_dir.clone());
    let runtime_precedes = switched.is_some()
        && context
            .as_ref()
            .is_some_and(|c| c.bin_dir_precedes_project_bins);
    let runtime_bin = resolve_path_entry(switched, aube_util::engine_context().runtime_node_dir);
    aube_scripts::order_path_entries(project_bins, runtime_bin.as_deref(), runtime_precedes)
}

/// The binary to probe for a node version when one wasn't supplied: the
/// real `npm_node_execpath` (so a wrapper's `--version` reflects the
/// underlying Node), falling back to `node_program`, then to the
/// embedder-provisioned node when a host owns provisioning.
pub fn node_execpath() -> Option<PathBuf> {
    resolve_node_bin(
        current().and_then(|c| c.node_execpath.clone().or_else(|| c.node_program.clone())),
        aube_util::engine_context().runtime_node_bin,
        None,
    )
}

/// Set the npm-compat env vars naming the node binary on a child
/// command: `NODE` (the program scripts re-spawn) and `npm_node_execpath`
/// (the *real* node node-gyp reads to find Node's install prefix), then
/// apply the embedder's env contributions. See [`child_env_vars`].
///
/// Also stamps `npm_config_user_agent`, which the plain [`child_env_vars`]
/// pairs deliberately omit: pnpm exports it to `pnpm dlx` / `pnpm create` /
/// `pnpm exec` children exactly as it does to lifecycle scripts, and create-*
/// scaffolders sniff it to emit the invoking PM's commands — without it they
/// fall back to npm-mode. Same product/format as the lifecycle export
/// ([`aube_scripts::aube_user_agent`]), so a tool sees one UA whether it ran
/// as a postinstall or under dlx.
pub fn apply_child_env(cmd: &mut tokio::process::Command) {
    cmd.env("npm_config_user_agent", aube_scripts::aube_user_agent());
    for (key, value) in child_env_vars() {
        cmd.env(key, value);
    }
}

/// The child env — `NODE`, `npm_node_execpath`, and the embedder's env
/// contributions — as resolved `(key, value)` pairs, for spawn paths
/// that inherit the parent environment (dlx/exec/node — no jail
/// `env_clear`). Exposed as pairs (rather than only mutating a
/// `tokio::process::Command`) so the `aube node` exec path, which uses a
/// `std::process::Command`, can apply the same env.
///
/// `NODE`/`npm_node_execpath` prefer the switched runtime's node, then the
/// embedder-provisioned node ([`EngineContext::runtime_node_bin`]) when a host
/// owns provisioning and the resolver stayed inert, then the ambient `node` on
/// `PATH` so they're populated on every spawn (pnpm/npm parity).
/// [`EnvMerge::Append`] joins after the inherited value of the key (so a
/// `NODE_OPTIONS` preload adds to the user's ambient value rather than dropping
/// it); [`EnvMerge::Replace`] overwrites. Jailed lifecycle scripts don't use
/// this — they resolve `NODE_OPTIONS` through [`merge_node_options`] (the base
/// is the `nodeOptions` setting, not the cleared env) and stamp the rest via
/// [`embedder_extra_env`].
///
/// [`EngineContext::runtime_node_bin`]: aube_util::EngineContext::runtime_node_bin
pub fn child_env_vars() -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    let ctx = current();
    let embedder_node = aube_util::engine_context().runtime_node_bin;
    let execpath = resolve_node_bin(
        ctx.as_ref()
            .and_then(|c| c.node_execpath.clone().or_else(|| c.node_program.clone())),
        embedder_node.clone(),
        aube_runtime::node_on_path(),
    );
    let node = resolve_node_bin(
        ctx.as_ref().and_then(|c| c.node_program.clone()),
        embedder_node,
        execpath.clone(),
    );
    let mut vars: Vec<(std::ffi::OsString, std::ffi::OsString)> = Vec::new();
    if let Some(execpath) = execpath {
        vars.push(("npm_node_execpath".into(), execpath.into_os_string()));
    }
    if let Some(node) = node {
        vars.push(("NODE".into(), node.into_os_string()));
    }
    if let Some(ctx) = ctx {
        // Non-jailed spawns inherit the parent env, so `Append` composes
        // onto the inherited value of the key.
        vars.extend(fold_env(&ctx.env, |key| {
            std::env::var_os(key).filter(|v| !v.is_empty())
        }));
    }
    vars
}

/// The node-binary fallback ladder shared by [`node_program`],
/// [`internal_node_program`], [`node_execpath`] and [`child_env_vars`]: the
/// switched runtime's node wins; then the embedder-provisioned node (set when
/// an embedder owns provisioning and the resolver stayed inert); then the
/// ambient fallback the caller supplies (the PATH `node` for env vars, `None`
/// for the bare-`"node"` cases). Pure so the precedence is unit-tested without
/// touching the process globals.
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

/// Fold embedder env entries into resolved `(key, value)` pairs, in the
/// order keys first appear. [`EnvMerge::Append`] composes onto the
/// running value for that key — seeded from `base(key)` (the inherited
/// env for non-jailed spawns, `None` in the cleared jail) — so several
/// appends to one key chain in order; [`EnvMerge::Replace`] overwrites,
/// and a later append composes onto the replaced value. Kept pure (the
/// base is supplied by the caller) so it is unit-testable without
/// touching the process environment.
fn fold_env(
    entries: &[EmbedderEnv],
    base: impl Fn(&std::ffi::OsStr) -> Option<std::ffi::OsString>,
) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    let mut order: Vec<std::ffi::OsString> = Vec::new();
    let mut acc: std::collections::HashMap<std::ffi::OsString, std::ffi::OsString> =
        std::collections::HashMap::new();
    for entry in entries {
        if !acc.contains_key(&entry.key) {
            order.push(entry.key.clone());
        }
        let value = match entry.merge {
            EnvMerge::Replace => entry.val.clone(),
            EnvMerge::Append => {
                let running = acc
                    .get(&entry.key)
                    .cloned()
                    .or_else(|| base(&entry.key))
                    .filter(|v| !v.is_empty());
                match running {
                    Some(mut existing) => {
                        existing.push(" ");
                        existing.push(&entry.val);
                        existing
                    }
                    None => entry.val.clone(),
                }
            }
        };
        acc.insert(entry.key.clone(), value);
    }
    order
        .into_iter()
        .map(|key| {
            let value = acc.remove(&key).unwrap_or_default();
            (key, value)
        })
        .collect()
}

/// The `NODE_OPTIONS` an embedder contributed, folded over `base` (the
/// resolved `nodeOptions` setting). Used by the jailed lifecycle-script
/// path, which builds env through a settings snapshot rather than
/// [`apply_child_env`]. Returns `base` unchanged when no embedder
/// `NODE_OPTIONS` entry is active; multiple appends chain in order.
pub fn merge_node_options(base: Option<String>) -> Option<String> {
    let ctx = current();
    let entries = ctx.as_ref().map(|c| c.env.as_slice()).unwrap_or(&[]);
    let node_options: Vec<EmbedderEnv> = entries
        .iter()
        .filter(|e| e.key == std::ffi::OsStr::new("NODE_OPTIONS"))
        .cloned()
        .collect();
    if node_options.is_empty() {
        return base;
    }
    let base_os = base.map(std::ffi::OsString::from);
    fold_env(&node_options, |_| base_os.clone())
        .into_iter()
        .next()
        .map(|(_, v)| v.to_string_lossy().into_owned())
        .or_else(|| base_os.map(|b| b.to_string_lossy().into_owned()))
}

/// Embedder env entries other than `NODE_OPTIONS` (which
/// [`merge_node_options`] handles), as resolved `(key, value)` pairs for
/// the jailed lifecycle-script path. `env_clear` means there is no
/// inherited base, so `Append` composes only across the embedder's own
/// entries for a key; `Replace` overwrites.
pub fn embedder_extra_env() -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    let Some(ctx) = current() else {
        return Vec::new();
    };
    let entries: Vec<EmbedderEnv> = ctx
        .env
        .iter()
        .filter(|e| e.key != std::ffi::OsStr::new("NODE_OPTIONS"))
        .cloned()
        .collect();
    fold_env(&entries, |_| None)
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
    let (settings, parse_options) = crate::commands::with_settings_ctx(&project_dir, |ctx| {
        (
            RuntimeSettings::from_ctx(ctx),
            aube_lockfile::ParseOptions {
                strict_store_integrity: aube_settings::resolved::strict_store_integrity(ctx)
                    || aube_settings::resolved::paranoid(ctx),
            },
        )
    });
    let pin = manifest
        .as_deref()
        .and_then(|m| lockfile_node_pin(&project_dir, m, parse_options));
    ensure(&project_dir, manifest.as_deref(), settings, pin.as_ref()).await
}

/// Resolve from a manifest the caller already loaded for this project.
/// `aubr` uses this to share one live parse between runtime selection and
/// script dispatch without relying on process-lifetime manifest state.
pub(crate) async fn ensure_for_cwd_with_manifest(
    cwd: &Path,
    manifest: &PackageJson,
) -> miette::Result<Arc<RuntimeContext>> {
    if let Some(ctx) = current() {
        return Ok(ctx);
    }
    let project_dir = crate::dirs::find_project_root(cwd).unwrap_or_else(|| cwd.to_path_buf());
    let (settings, parse_options) = crate::commands::with_settings_ctx(&project_dir, |ctx| {
        (
            RuntimeSettings::from_ctx(ctx),
            aube_lockfile::ParseOptions {
                strict_store_integrity: aube_settings::resolved::strict_store_integrity(ctx)
                    || aube_settings::resolved::paranoid(ctx),
            },
        )
    });
    let pin = lockfile_node_pin(&project_dir, manifest, parse_options);
    ensure(&project_dir, Some(manifest), settings, pin.as_ref()).await
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
    // No scope: a registered embedder runtime is authoritative and
    // registration-order independent — prefer it over (and never let it
    // race with) a `RUNTIME` an earlier resolution may have filled.
    if let Some(ctx) = EMBEDDER_CONTEXT.get() {
        return Ok(Arc::clone(ctx));
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
            bin_dir_precedes_project_bins: false,
            // A resolved runtime is a selector: `NODE` and
            // `npm_node_execpath` are the same binary.
            node_program: Some(res.node_bin.clone()),
            node_execpath: Some(res.node_bin.clone()),
            internal_node: None,
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
            env: Vec::new(),
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

    fn abs(p: &str) -> PathBuf {
        let p = PathBuf::from(p);
        std::path::absolute(&p).unwrap_or(p)
    }

    #[test]
    fn selector_derives_one_binary_for_all_three_paths() {
        let node_exe = if cfg!(windows) { "node.exe" } else { "node" };
        let ctx = EmbedderRuntime::selector("/opt/mise/node/bin").resolve();
        let bin = abs("/opt/mise/node/bin");
        assert_eq!(ctx.bin_dir.as_deref(), Some(bin.as_path()));
        // `NODE` and `npm_node_execpath` are the same binary for a selector.
        assert_eq!(
            ctx.node_program.as_deref(),
            Some(bin.join(node_exe).as_path())
        );
        assert_eq!(ctx.node_execpath, ctx.node_program);
        assert!(!ctx.bin_dir_precedes_project_bins);
        assert_eq!(ctx.source, RuntimeSource::Embedder);
    }

    #[test]
    fn wrapper_splits_shim_from_real_node() {
        let ctx = EmbedderRuntime::wrapper("/shim/node")
            .real_node("/node-24.4.1/bin/node")
            .internal_node("/node-24.4.1/bin/node")
            .version("24.4.1")
            .env_append("NODE_OPTIONS", "--import /preload.mjs")
            .resolve();
        // PATH entry defaults to the shim's parent; `NODE` is the shim; the
        // execpath is the real binary node-gyp reads; internal spawns
        // (pnpmfile, scanner) skip the wrapper hop.
        assert_eq!(ctx.bin_dir.as_deref(), Some(abs("/shim").as_path()));
        assert!(ctx.bin_dir_precedes_project_bins);
        assert_eq!(
            ctx.node_program.as_deref(),
            Some(abs("/shim/node").as_path())
        );
        assert_eq!(
            ctx.node_execpath.as_deref(),
            Some(abs("/node-24.4.1/bin/node").as_path())
        );
        assert_eq!(
            ctx.internal_node.as_deref(),
            Some(abs("/node-24.4.1/bin/node").as_path())
        );
        assert_eq!(ctx.version.as_deref(), Some("24.4.1"));
        assert_eq!(ctx.env.len(), 1);
        assert_eq!(ctx.env[0].merge, EnvMerge::Append);
    }

    #[test]
    fn wrapper_can_leave_path_unchanged() {
        let ctx = EmbedderRuntime::wrapper("/shim/node")
            .real_node("/real/node")
            .without_path()
            .resolve();
        assert_eq!(ctx.bin_dir, None);
        assert_eq!(
            ctx.node_program.as_deref(),
            Some(abs("/shim/node").as_path())
        );
        assert_eq!(
            ctx.node_execpath.as_deref(),
            Some(abs("/real/node").as_path())
        );
        assert!(ctx.bin_dir_precedes_project_bins);
    }

    #[test]
    fn last_path_builder_call_wins() {
        let explicit = EmbedderRuntime::wrapper("/shim/node")
            .without_path()
            .path_dir("/custom/bin")
            .resolve();
        assert_eq!(
            explicit.bin_dir.as_deref(),
            Some(abs("/custom/bin").as_path())
        );

        let unchanged = EmbedderRuntime::wrapper("/shim/node")
            .path_dir("/custom/bin")
            .without_path()
            .resolve();
        assert_eq!(unchanged.bin_dir, None);
    }

    #[test]
    fn selector_without_path_keeps_selected_node() {
        let node_exe = if cfg!(windows) { "node.exe" } else { "node" };
        let ctx = EmbedderRuntime::selector("/opt/node/bin")
            .without_path()
            .resolve();
        assert_eq!(ctx.bin_dir, None);
        assert_eq!(
            ctx.node_program.as_deref(),
            Some(abs("/opt/node/bin").join(node_exe).as_path())
        );
        assert_eq!(ctx.node_execpath, ctx.node_program);
    }

    #[tokio::test]
    async fn internal_node_defaults_to_node_program() {
        scope(async {
            let rt = EmbedderRuntime::wrapper("/shim/node");
            seed_install_embedder_runtime(Some(&rt));
            // Without an explicit internal_node, internal spawns use the
            // wrapper too (today's behavior for a plain wrapper).
            assert_eq!(internal_node_program(), abs("/shim/node"));
        })
        .await;
        scope(async {
            let rt = EmbedderRuntime::wrapper("/shim/node").internal_node("/real/node");
            seed_install_embedder_runtime(Some(&rt));
            assert_eq!(internal_node_program(), abs("/real/node"));
            // User-facing program stays the shim.
            assert_eq!(node_program(), abs("/shim/node"));
        })
        .await;
    }

    #[tokio::test]
    async fn with_embedder_runtime_scopes_a_per_call_runtime() {
        // A per-call runtime is visible inside the scoped future and gone
        // after it — the process-wide state is untouched, so a host can
        // vary the runtime per invocation (fresh shim dir per command).
        let seen = with_embedder_runtime(Some(EmbedderRuntime::wrapper("/shim-a/node")), async {
            node_program()
        })
        .await;
        assert_eq!(seen, abs("/shim-a/node"));
        // A fresh scope afterwards starts empty — nothing leaked.
        scope(async {
            assert!(current().is_none());
        })
        .await;
    }

    #[tokio::test]
    async fn none_arm_still_seeds_from_the_ambient_embedder_runtime() {
        // Regression: the `None` arm of `with_embedder_runtime` opens an
        // isolation scope, and an empty slot there means `ensure` would
        // resolve aube's own runtime — silently bypassing a registered
        // embedder runtime. Both arms must seed through
        // `seed_install_embedder_runtime(None)`, whose lookup chain is
        // per-call → process-wide. Exercised via the per-call tier (the
        // process-wide `set_embedder_runtime` is once-per-process and
        // would leak into unrelated tests in this binary; both tiers flow
        // through the same seed path).
        with_embedder_runtime(Some(EmbedderRuntime::wrapper("/shim-c/node")), async {
            with_embedder_runtime(None, async {
                // Inside the inner isolation scope the ambient embedder
                // runtime must already be seeded — not an empty slot that
                // `ensure` would fill with aube's own resolution.
                assert_eq!(
                    current().map(|c| c.node_program.clone()),
                    Some(Some(abs("/shim-c/node")))
                );
            })
            .await;
        })
        .await;
    }

    #[tokio::test]
    async fn per_call_runtime_reaches_a_nested_install_scope() {
        // The per-call runtime must survive the *fresh* scope a nested
        // install opens (dlx's transient install, run/exec auto-install):
        // `seed_install_embedder_runtime(None)` inside that inner scope
        // should still pick it up, so lifecycle scripts run wrapped.
        with_embedder_runtime(Some(EmbedderRuntime::wrapper("/shim-b/node")), async {
            // Simulate `install::run`: a fresh scope with no per-install
            // runtime of its own.
            scope(async {
                assert!(current().is_none(), "nested scope starts empty");
                seed_install_embedder_runtime(None);
                assert_eq!(node_program(), abs("/shim-b/node"));
            })
            .await;
        })
        .await;
    }

    #[tokio::test]
    async fn seed_install_runtime_drives_node_program_and_path() {
        scope(async {
            assert!(current().is_none(), "slot starts empty");
            let node_exe = if cfg!(windows) { "node.exe" } else { "node" };
            let bin = abs("/opt/mise/node/bin");
            let rt = EmbedderRuntime::selector("/opt/mise/node/bin");
            seed_install_embedder_runtime(Some(&rt));

            let ctx = current().expect("slot seeded");
            assert_eq!(ctx.source, RuntimeSource::Embedder);
            assert_eq!(ctx.bin_dir.as_deref(), Some(bin.as_path()));
            assert_eq!(node_program(), bin.join(node_exe));
            assert_eq!(path_entries(), vec![bin.clone()]);

            // Slot is set-once: a second seed does not clobber the first.
            let other = EmbedderRuntime::selector("/other");
            seed_install_embedder_runtime(Some(&other));
            assert_eq!(node_program(), bin.join(node_exe));
        })
        .await;
    }

    #[tokio::test]
    async fn wrapper_path_leads_while_selector_follows_project_bins() {
        let project_bin = abs("/project/node_modules/.bin");

        let wrapper_entries =
            with_embedder_runtime(Some(EmbedderRuntime::wrapper("/shim/node")), async {
                path_entries_with_project_bins(vec![project_bin.clone()])
            })
            .await;
        assert_eq!(wrapper_entries, vec![abs("/shim"), project_bin.clone()]);

        let selector_entries =
            with_embedder_runtime(Some(EmbedderRuntime::selector("/opt/node/bin")), async {
                path_entries_with_project_bins(vec![project_bin.clone()])
            })
            .await;
        assert_eq!(selector_entries, vec![project_bin, abs("/opt/node/bin")]);
    }

    #[test]
    fn fold_env_composes_appends_and_honors_replace() {
        fn os(s: &str) -> std::ffi::OsString {
            std::ffi::OsString::from(s)
        }
        fn entry(k: &str, v: &str, merge: EnvMerge) -> EmbedderEnv {
            EmbedderEnv {
                key: os(k),
                val: os(v),
                merge,
            }
        }
        // Append seeds from the caller's base and chains multiple entries
        // for the same key in order; a key with no base starts empty.
        let entries = vec![
            entry("NODE_OPTIONS", "--a", EnvMerge::Append),
            entry("NODE_OPTIONS", "--b", EnvMerge::Append),
            entry("FRESH", "x", EnvMerge::Append),
        ];
        let out = fold_env(&entries, |k| {
            (k == std::ffi::OsStr::new("NODE_OPTIONS")).then(|| os("--base"))
        });
        assert_eq!(out[0], (os("NODE_OPTIONS"), os("--base --a --b")));
        assert_eq!(out[1], (os("FRESH"), os("x")));

        // Replace overwrites even a present base; a later append composes
        // onto the replaced value, not the base.
        let entries = vec![
            entry("K", "one", EnvMerge::Replace),
            entry("K", "two", EnvMerge::Append),
        ];
        let out = fold_env(&entries, |_| Some(os("IGNORED")));
        assert_eq!(out, vec![(os("K"), os("one two"))]);
    }

    #[tokio::test]
    async fn merge_node_options_appends_embedder_preload() {
        scope(async {
            let rt = EmbedderRuntime::wrapper("/shim/node")
                .env_append("NODE_OPTIONS", "--import /preload.mjs");
            seed_install_embedder_runtime(Some(&rt));
            // Appends after the user's resolved value; falls back to just the
            // embedder value when there's no base.
            assert_eq!(
                merge_node_options(Some("--enable-source-maps".into())).as_deref(),
                Some("--enable-source-maps --import /preload.mjs")
            );
            assert_eq!(
                merge_node_options(None).as_deref(),
                Some("--import /preload.mjs")
            );
        })
        .await;
    }

    #[tokio::test]
    async fn seed_install_runtime_is_a_noop_outside_scope() {
        // No install scope active: the seed can't set a task-local slot, so it
        // must not panic and must not leak into a later scope.
        let rt = EmbedderRuntime::selector("/opt/mise/node/bin");
        seed_install_embedder_runtime(Some(&rt));
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
