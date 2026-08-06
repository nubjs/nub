//! The config compiler (Boundary A): the ONLY code that understands surface
//! syntax. Input is already-parsed data (a `serde_json::Value` for the `sandbox`
//! block + a [`CompileCtx`] of host-provided paths/env); output is a fully
//! resolved [`SandboxPolicy`]. The compiler NEVER reads config files — it may
//! canonicalize fs paths (a filesystem read, not a config read).
//!
//! Pipeline (design.md §2.2): wrapper trichotomy → preset expansion → per-axis
//! fold (with `...:#/pointer` list reuse + last-match-wins order) → env grammar +
//! `$(…)` → emit. A policy is a COMPLETE, self-contained statement — there is no
//! implicit inheritance; reuse of another list is explicit, through a `...:#/pointer`
//! reference into the same document (see [`reuse`]). The `--sandbox` entry is single-block.

mod builtin_sets;
mod clobber;
mod curated;
// Crate-visible rather than module-private so the Linux mount planner's tests can run the
// whole-disk read allow-set this module emits straight into `compile_mount_plan`. While it was
// private neither side could test the seam between them, which is exactly where the
// reserved-kernel-tree refusal hid.
pub(crate) mod defaults;
mod env_grammar;
mod fold;
mod package_network;
mod preset;
mod resolve;
mod reuse;
// Crate-visible rather than module-private: `catalog_v2::Entry::grant_for` resolves its version
// bands through the SAME range matcher the compiler's own scoped tables use, so the v2 bands
// never become a second semver dialect.
pub(crate) mod version_scope;

/// Exposed on EVERY platform, unlike the stamp itself, so `stdio_shim_semantics` drives the
/// payload that actually ships rather than the source file it is derived from — comment-stripping
/// is part of the delivery, so testing the unstripped file would leave it unmeasured.
pub use defaults::build_jail_stdio_preload_js;
pub use defaults::{build_jail_node_options, net_gate_node_options, realpath_shim_node_options};
#[cfg(windows)]
pub use defaults::{
    windows_build_jail_node_options, windows_native_realpath_shim_node_options,
    windows_realpath_node_options,
};
pub use package_network::{
    PACKAGE_NETWORK_ALLOWED, build_jail_net_allowed, package_network_allowed,
};
pub use preset::{PROJECT_VIRTUAL_STORE_LEAF, compile_build_jail, jail_private_home};
pub use resolve::{CommandRunner, ShellRunner};

/// The `$downloads` host set, re-exported for the EMBEDDER's out-of-jail prefetch alone,
/// which derives its fetch allowlist FROM this array rather than restating it.
/// [`download_hosts`] is the accessor every consumer should read — it honours the dev-only
/// catalog override; the `const` is the compiled floor behind it.
pub use builtin_sets::{DOWNLOAD_HOSTS, download_hosts};

/// The secret-file deny floor, re-exported for the Linux backend ALONE — it recognizes
/// the floor positionally and by membership, and must read the same arrays
/// `finalize_env_deny` emits rather than restate them. Exposing these three rather than
/// the whole `defaults` module keeps the policy CONSTRUCTORS out of backend reach, so
/// "backends replicate the IR, they do not author policy" stays enforced by the compiler
/// rather than by convention.
#[cfg(any(target_os = "linux", test))]
pub(crate) use defaults::ENV_DENY_LEAF_GLOBS;
#[cfg(target_os = "linux")]
pub(crate) use defaults::{ENV_DENY_SUBTREE_GLOBS, env_deny_floor_start};

use crate::matcher::path::Homes;
use crate::policy::{Effect, EnvPolicy, FsPolicy, Inspection, NetPolicy, ProxyMode, SandboxPolicy};
use serde_json::Value;
use std::collections::BTreeMap;

/// A non-fatal compile diagnostic. Distinct from [`CompileError`]: the policy
/// still compiles, but something in the surface is a smell worth surfacing — today
/// only the clobber warning (a later array entry that fully shadows an earlier
/// one, making it dead). Carried on the side of the result, NEVER in the resolved
/// [`SandboxPolicy`] IR (backends consume policy, not diagnostics).
#[derive(Debug, Clone, PartialEq)]
pub struct CompileWarning {
    /// The surface path the warning occurred at (e.g. `fs`, `env`).
    pub path: String,
    pub message: String,
}

impl std::fmt::Display for CompileWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "sandbox.{}: {}", self.path, self.message)
    }
}

/// The dynamic compile capabilities a single config SCOPE is permitted. Capability
/// is decided by scope IDENTITY, never inferred from a repository/checkout heuristic:
/// approved user config (`nub.jsonc` / `scriptsMeta`) gets the full set; dependency-
/// controlled config (`dependenciesMeta`) gets none. The two axes are modeled
/// independently because the invariant is phrased per capability ("MAY use `$(…)`",
/// "MAY use credential brokering") — a future scope could hold one without the other — even
/// though today's two callers set them together.
///
/// Filesystem `$(…)` substitution is deliberately ABSENT: an fs path is inert data
/// (never a credential or an injected header), so it is UNCONDITIONAL in every scope
/// and never gated. Only these two dynamic capabilities discriminate by trust.
#[derive(Debug, Clone, Copy)]
pub struct ScopeCapabilities {
    /// `$(…)` command substitution in env values. Denied to dependency-controlled
    /// config: it must never spawn a command the user did not author.
    pub env_substitution: bool,
    /// Credential brokering (`secrets.<name>.brokerTo`) — releasing a named parent
    /// secret only on requests to the listed hosts, where its opaque marker occurs,
    /// via a terminating proxy. Denied to dependency-controlled config.
    pub credential_broker: bool,
}

impl ScopeCapabilities {
    /// Approved user configuration (`nub.jsonc` / `scriptsMeta`): the full capability set.
    pub fn approved() -> Self {
        Self {
            env_substitution: true,
            credential_broker: true,
        }
    }

    /// Dependency-controlled configuration (`dependenciesMeta`): no dynamic capability.
    pub fn dependency() -> Self {
        Self {
            env_substitution: false,
            credential_broker: false,
        }
    }
}

/// Host-provided context for a compile. All fields are ALREADY-PARSED data — the
/// engine stays PM-pure (Boundary B): nub-cli does file discovery/parse and the
/// ambient-env snapshot, then hands them here.
pub struct CompileCtx {
    /// Per-OS home anchors symbolic roots expand against.
    pub homes: Homes,
    /// The current working directory (for diagnostics / relative anchoring).
    pub cwd: std::path::PathBuf,
    /// EVERY file the policy was sourced from (absolute, canonicalized). Each is injected
    /// as a high-precedence fs DENY (read AND write) so a sandboxed process can neither
    /// read nor tamper with the policy that confines it, even under a broad
    /// `fs: ["."]`/`["/"]`. See `fold::finalize_policy_file_deny`.
    ///
    /// A SET, not one path, because the source chain can be longer than one hop: a
    /// nub.jsonc `sandbox: "./policy.jsonc"` is confined by BOTH files, and denying only
    /// the referenced one leaves the referencing config writable — rewrite it to name a
    /// permissive policy and the next run is unconfined. Empty for an inline policy with
    /// no distinct source file (`--sandbox true|<preset>`, nub's own build-jail).
    pub policy_files: Vec<std::path::PathBuf>,
    /// The capabilities of this single-block compile — the `compile`/`compile_with_warnings`
    /// entry, whose one scope IS this whole ctx (the `--sandbox <file>` / `run_sandboxed`
    /// path, always approved user config). The fold gates (`$(…)` substitution, credential
    /// brokering) read this.
    pub caps: ScopeCapabilities,
    /// The ambient env snapshot the child env is constructed from.
    pub ambient_env: BTreeMap<String, String>,
    /// The whole parsed policy DOCUMENT (root), for resolving `...:#/pointer` list reuse.
    /// `#` in a reuse pointer is this value's root, NOT the compiled `sandbox` block —
    /// a reused list is typically a sibling of `sandbox` (see sandbox.mdx "Reuse …").
    /// `Value::Null` (the default) when the compile has no reuse; `run_sandboxed` sets it
    /// to the whole parsed file via [`CompileCtx::with_document`]. All fields on this ctx
    /// are already-parsed host data (the Boundary-B contract), so an owned `Value` fits.
    pub document: Value,
    /// The build-jail interpreter-closure paths — the Node binaries the lifecycle
    /// scripts run under. The build-jail preset grants each (and its bin dir) READ,
    /// because nub provisions its OWN Node under its store rather than `/usr`, so the
    /// Linux `RootView::Minimal` auto-mount of `ESSENTIAL_READ_PATHS` (which covers a
    /// system-installed interpreter) does NOT reach it — the empirically-load-bearing
    /// grant the build-jail read-set spike identified. It is a SET, not one path,
    /// because under nub a bare `node` resolves via a PATH-prepended shim (`$NODE`)
    /// while `npm_node_execpath` names the real provisioned binary — both must be
    /// readable/executable. Empty (the default) for a policy with no interpreter to
    /// grant (`--sandbox <file>`, the static `build-jail` preset). Set per-spawn by the
    /// lifecycle interposition via [`CompileCtx::with_interpreter`].
    pub interpreter: Vec<std::path::PathBuf>,
    /// The `$(…)` command runner (production shells out; tests inject a stub).
    pub runner: Box<dyn CommandRunner>,
}

impl CompileCtx {
    /// A ctx with the real shell runner and the given homes/env and single-block caps.
    pub fn new(
        homes: Homes,
        cwd: std::path::PathBuf,
        caps: ScopeCapabilities,
        mut ambient_env: BTreeMap<String, String>,
    ) -> Self {
        // Boundary B ingestion. Filtering cmd.exe's shell-positional entries HERE, once,
        // covers every posture that reads the ctx's ambient map: `sandbox: false` clones it
        // wholesale and a `vars: ["*"]` glob matches any name, so either would otherwise
        // carry a `=`-named key into `constructed` and fail the spawn under any cmd.exe
        // ancestor. Filtering at ingestion also keeps them out of `withheld`, where they
        // would read as policy decisions about variables that were never variables.
        //
        // It does NOT cover the build jail: `compile_build_jail` hands its own unfiltered
        // `ambient_env` to `lifecycle_scrubbed_env`, never routing it through here. That
        // posture is safe by its default-deny allowlist — no admitted name starts with
        // `=` — not by this filter.
        ambient_env.retain(|key, _| !crate::policy::is_shell_positional_env_key(key));
        Self {
            homes,
            cwd,
            policy_files: Vec::new(),
            caps,
            ambient_env,
            document: Value::Null,
            interpreter: Vec::new(),
            runner: Box::new(ShellRunner),
        }
    }

    /// Attach the policy SOURCE-FILE paths (absolute, canonicalized) so the compiler
    /// self-excludes each from every fs grant. The caller passes the WHOLE chain — the
    /// referencing config as well as the file it references — not just the document the
    /// rules were literally read from. Empty (the default) leaves the policy with no
    /// self-exclusion: the inline-policy case, where there is no source file to hide.
    /// See [`CompileCtx::policy_files`].
    pub fn with_policy_files(mut self, policy_files: Vec<std::path::PathBuf>) -> Self {
        self.policy_files = policy_files;
        self
    }

    /// Attach the whole parsed policy DOCUMENT so `...:#/pointer` reuse can resolve a
    /// `#`-rooted pointer against it. `Value::Null` (the default) leaves reuse with no
    /// document — any `...:#/pointer` then fails loud as a dangling pointer. The
    /// `run_sandboxed` path passes the WHOLE parsed file (not the extracted `sandbox`
    /// block) so a `#/shared/*` sibling pointer reaches its target. See [`CompileCtx::document`].
    pub fn with_document(mut self, document: Value) -> Self {
        self.document = document;
        self
    }

    /// Attach the build-jail interpreter-closure paths (the provisioned Node + shim the
    /// lifecycle script runs under). The `build-jail` preset then grants each path + its
    /// bin dir READ. Empty (the default) leaves the preset with no interpreter grant.
    /// See [`CompileCtx::interpreter`].
    pub fn with_interpreter(mut self, interpreter: Vec<std::path::PathBuf>) -> Self {
        self.interpreter = interpreter;
        self
    }
}

/// A compile failure. Every variant carries the surface path it occurred at so
/// diagnostics point at the offending field.
#[derive(Debug, Clone, PartialEq)]
pub enum CompileError {
    /// A structural/shape violation (wrong type, unknown key, bad ladder value).
    Shape { path: String, message: String },
    /// A `"<preset>"` name not in the closed table.
    UnknownPreset {
        name: String,
        supported: Vec<String>,
    },
    /// A nested file-ref (`"./x.json"`) inside a compiled block — the engine does
    /// not resolve file-refs (the caller loads the file; a nested ref is deferred).
    FileRefUnresolved { path: String, reference: String },
    /// A `$(…)` in an untrusted home (`dependenciesMeta`).
    UntrustedSubstitution { path: String },
    /// A `$(…)` that failed to run.
    Substitution { path: String, message: String },
    /// A value failed its env type validation.
    Validation { path: String, message: String },
    /// A required (non-optional, no-default) env key had no value.
    MissingRequired { key: String },
}

impl CompileError {
    pub(crate) fn shape(path: &str, message: &str) -> Self {
        Self::Shape {
            path: path.to_string(),
            message: message.to_string(),
        }
    }
    pub(crate) fn unknown_preset(name: &str, supported: &[&str]) -> Self {
        Self::UnknownPreset {
            name: name.to_string(),
            supported: supported.iter().map(|s| s.to_string()).collect(),
        }
    }
    pub(crate) fn untrusted_substitution(path: &str) -> Self {
        Self::UntrustedSubstitution {
            path: path.to_string(),
        }
    }
    pub(crate) fn substitution(path: &str, message: &str) -> Self {
        Self::Substitution {
            path: path.to_string(),
            message: message.to_string(),
        }
    }
    pub(crate) fn validation(path: &str, message: &str) -> Self {
        Self::Validation {
            path: path.to_string(),
            message: message.to_string(),
        }
    }
    pub(crate) fn missing_required(key: &str) -> Self {
        Self::MissingRequired {
            key: key.to_string(),
        }
    }
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Shape { path, message } => write!(f, "sandbox.{path}: {message}"),
            Self::UnknownPreset { name, supported } => write!(
                f,
                "unknown sandbox preset `{name}` — supported: {}",
                supported.join(", ")
            ),
            Self::FileRefUnresolved { path, reference } => write!(
                f,
                "sandbox.{path}: nested file-ref `{reference}` is not resolved by the engine"
            ),
            Self::UntrustedSubstitution { path } => write!(
                f,
                "sandbox.{path}: `$(…)` command substitution is not permitted in an untrusted (dependenciesMeta) grant"
            ),
            Self::Substitution { path, message } => write!(f, "sandbox.{path}: {message}"),
            Self::Validation { path, message } => write!(f, "sandbox.{path}: {message}"),
            Self::MissingRequired { key } => write!(f, "required env var `{key}` is not set"),
        }
    }
}

impl std::error::Error for CompileError {}

/// Compile a `sandbox` surface block into a resolved [`SandboxPolicy`]. Discards
/// any [`CompileWarning`]s; use [`compile_with_warnings`] to surface them.
pub fn compile(surface: &Value, ctx: &CompileCtx) -> Result<SandboxPolicy, CompileError> {
    compile_with_warnings(surface, ctx).map(|(policy, _)| policy)
}

/// Compile a `sandbox` surface block, returning the resolved policy AND any
/// non-fatal warnings (the clobber smell). Single-block entry: a policy is a
/// complete statement; there is no parent scope. List reuse is explicit via
/// `...:#/pointer` (resolved against [`CompileCtx::document`]).
pub fn compile_with_warnings(
    surface: &Value,
    ctx: &CompileCtx,
) -> Result<(SandboxPolicy, Vec<CompileWarning>), CompileError> {
    let mut warnings = Vec::new();
    let policy = compile_scope(surface, ctx, ctx.caps, &mut warnings)?;
    Ok((policy, warnings))
}

/// Resolve one block's surface into a policy — the wrapper trichotomy; a granular
/// object goes to [`compile_object`]. An unlisted axis FLOORS (complete statement);
/// list reuse is explicit via `...:#/pointer`.
pub(crate) fn compile_scope(
    surface: &Value,
    ctx: &CompileCtx,
    caps: ScopeCapabilities,
    warnings: &mut Vec<CompileWarning>,
) -> Result<SandboxPolicy, CompileError> {
    match surface {
        // `false` — fully unjail: every axis relaxed. No gated op (caps inert).
        Value::Bool(false) => Ok(unjailed(ctx)),
        // `true` — secure defaults per axis (see `secure_default`). No gated op.
        Value::Bool(true) => secure_default(ctx),
        // `"<preset>"` (bare) or `"./file"` (path-like).
        Value::String(s) => match classify_string(s) {
            StringKind::Preset => {
                let expanded = preset::resolve(s)?;
                let mut policy = compile_object(&expanded, ctx, caps, warnings)?;
                // build-jail read-set closure. Both grants are post-fold because they need
                // SPECULATIVE origin (absent on a host that has neither installed the
                // project's dependencies nor bootstrapped node-gyp) and the surface fold
                // marks every entry AUTHORED.
                preset::grant_build_jail_dependency_reads(s, &mut policy, ctx, None);
                // The provisioned interpreter lives under nub's store (not `/usr`), so the
                // tight-read base does not reach it.
                preset::grant_build_jail_interpreter(s, &mut policy, ctx);
                // The build jail is a pure allowlist: strip every deny the fold added, so
                // the policy is grants-only and the allowlist backends can enforce it.
                preset::enforce_pure_allowlist(s, &mut policy);
                Ok(policy)
            }
            StringKind::FileRef => Err(CompileError::FileRefUnresolved {
                path: String::new(),
                reference: s.clone(),
            }),
        },
        Value::Object(_) => compile_object(surface, ctx, caps, warnings),
        _ => Err(CompileError::shape(
            "",
            "sandbox must be a boolean, a preset name, a file-ref, or a { fs, net, vars, secrets } object",
        )),
    }
}

/// Fold a granular `{ fs, net, vars, secrets }` object. A present block is a COMPLETE
/// statement: an axis it does NOT list FLOORS (deny fs, deny-all-enforcing net,
/// strip env) — least-exposure, fails closed. There is NO implicit inheritance: `{}`
/// = deny-all; `{ "fs": [...] }` floors net + env (both `vars` and `secrets`). To
/// reuse another list, reference it explicitly with a `...:#/pointer` array entry.
fn compile_object(
    surface: &Value,
    ctx: &CompileCtx,
    caps: ScopeCapabilities,
    warnings: &mut Vec<CompileWarning>,
) -> Result<SandboxPolicy, CompileError> {
    let obj = surface
        .as_object()
        .ok_or_else(|| CompileError::shape("", "expected a { fs, net, vars, secrets } object"))?;
    // `proxy` is no longer authored — the compiler DERIVES the proxy tier from the net
    // allowlist and any brokered secrets. Give a stray `proxy` a targeted migration
    // error rather than a generic unknown-key one.
    if obj.contains_key("proxy") {
        return Err(CompileError::shape(
            "proxy",
            "`proxy` is no longer authored — the sandbox derives the proxy tier from the `net` allowlist and any brokered secrets; remove it",
        ));
    }
    // Object-level `"..."` inheritance was removed (v2: a policy is a complete statement).
    // Give it a targeted migration error rather than a generic unknown-key one.
    if obj.contains_key("...") {
        return Err(CompileError::shape(
            "...",
            "object-level `...` inheritance was removed — a policy is a complete statement; to reuse a list, reference it with a `...:#/pointer` array entry (e.g. `\"fs\": [\"...:#/shared/fs\", …]`)",
        ));
    }
    fold::reject_unknown_keys(obj, &["fs", "net", "vars", "secrets"], "")?;

    // Clobber detection runs per ARRAY axis (a total shadow between two entries of
    // the SAME array — D2b/D6); object forms have unique keys, so their granular
    // overrides are the intended idiom. `vars` and `secrets` are both env-family
    // arrays; each is clobber-checked independently (cross-axis collision is not a
    // clobber — a secrets entry deliberately wins over a same-named vars entry).
    if let Some(Value::Array(items)) = obj.get("fs") {
        clobber::detect_fs(items, &ctx.homes, "fs", warnings);
    }
    if let Some(Value::Array(items)) = obj.get("net") {
        clobber::detect_net(items, "net", warnings);
    }
    if let Some(Value::Array(items)) = obj.get("vars") {
        clobber::detect_env(items, "vars", warnings);
    }
    if let Some(Value::Array(items)) = obj.get("secrets") {
        clobber::detect_env(items, "secrets", warnings);
    }

    let fs = match obj.get("fs") {
        Some(v) => fold::fold_fs(v, ctx, "fs")?,
        None => floor_fs(),
    };
    let mut net = match obj.get("net") {
        Some(v) => fold::fold_net(v, ctx, "net")?,
        None => floor_net(),
    };
    // The env-family axes (`vars` + `secrets`) fold together into one EnvPolicy plus
    // the broker intents harvested from `secrets.<name>.brokerTo`. Both absent: floor
    // (strip). Either present: an explicit, complete env statement — the other axis
    // defaults to no entries.
    let vars = obj.get("vars");
    let secrets = obj.get("secrets");
    let (mut env, broker_intents) = match (vars, secrets) {
        (None, None) => (floor_env(ctx), Vec::new()),
        _ => fold::fold_env_axes(vars, secrets, ctx, caps)?,
    };
    // Cross-axis transpose: brokered secrets become per-host credential brokers on the
    // net axis (coalesced by host). This runs AFTER fold_net so inherited brokers and
    // the authored ones merge into one list; the broker validation below and the tier
    // derive then see the complete, final net policy.
    fold::transpose_brokers(&mut net, &broker_intents);
    validate_brokers(&net)?;
    // Derive the proxy posture + inspection tier (pure function of enforce/allow/brokers).
    finalize_net_inspection(&mut net);
    withhold_brokered_env(&net, &mut env, ctx);
    Ok(SandboxPolicy {
        fs,
        net,
        env,
        pid: Default::default(),
        // A generic scope, not the build jail; `compile_build_jail` sets the marker itself.
        build_jail: false,
    })
}

/// Reject a broker the final net policy cannot serve. Brokering does NOT itself open
/// the network (a `brokerTo` host contributes no allow rule), so two conditions must
/// hold. First, net must actually run a fine-grained proxy to perform the marker→secret
/// swap: coarse `net: true` (unrestricted) and an all-deny `net` (`net: false` / only
/// denies) start no proxy, so a broker there could never fire. Second, every brokered
/// host must be ADMITTED by the net verdict — a host named in `brokerTo` but absent
/// from `net` (or later denied) is a hard error, never a silent drop.
///
/// Runs over the MERGED broker list (inherited + transposed) as the LAST net step, so
/// a future `$trusted` net expansion (Phase 3) is already accounted for. The error
/// path points at `secrets.<name>.brokerTo` — the field the author can act on.
fn validate_brokers(net: &NetPolicy) -> Result<(), CompileError> {
    if net.brokers.is_empty() {
        return Ok(());
    }
    let has_allow = net.rules.iter().any(|rule| rule.effect == Effect::Allow);
    if !net.enforce || !has_allow {
        return Err(CompileError::shape(
            &broker_error_path(net.brokers.first()),
            "a brokered secret needs a fine-grained `net` allowlist that names its host; `net: true` (unrestricted) and an all-deny `net` start no proxy to perform the marker→secret swap",
        ));
    }
    let matcher = crate::matcher::HostMatcher::new(net);
    for broker in &net.brokers {
        if !matcher.admits(&broker.host) {
            return Err(CompileError::shape(
                &broker_error_path(Some(broker)),
                &format!(
                    "`{}` is brokered but not allowed in `net` — add it to the net axis",
                    broker.host
                ),
            ));
        }
    }
    Ok(())
}

/// The `secrets.<name>.brokerTo` path for a broker's diagnostic (the broker's first
/// env name is the secret). Empty name is unreachable from the compiler (every
/// broker carries at least one secret) but handled for a clean fallback.
fn broker_error_path(broker: Option<&crate::policy::CredentialBroker>) -> String {
    let name = broker
        .and_then(|b| b.env.first())
        .map(String::as_str)
        .unwrap_or("");
    format!("secrets.{name}.brokerTo")
}

/// Brokered values never survive in the serializable compile result, even when
/// the ordinary env policy would otherwise pass them through. `apply()` resolves
/// the current parent value afresh and overlays a fresh marker for each run.
fn withhold_brokered_env(net: &NetPolicy, env: &mut EnvPolicy, ctx: &CompileCtx) {
    for name in net.brokers.iter().flat_map(|broker| &broker.env) {
        env.constructed.retain(|key, _| {
            if cfg!(windows) {
                !key.eq_ignore_ascii_case(name)
            } else {
                key != name
            }
        });
        for ambient_name in ctx.ambient_env.keys().filter(|key| {
            if cfg!(windows) {
                key.eq_ignore_ascii_case(name)
            } else {
                *key == name
            }
        }) {
            if !env.withheld.iter().any(|existing| {
                if cfg!(windows) {
                    existing.eq_ignore_ascii_case(ambient_name)
                } else {
                    existing == ambient_name
                }
            }) {
                env.withheld.push(ambient_name.clone());
            }
        }
    }
    env.withheld.sort();
}

/// DERIVE the proxy posture (`mode`) + inspection tier from the net axis — a pure,
/// infallible function of `enforce` / has-allow / brokers (there is no authored
/// `proxy` knob any more). The table:
///   coarse (`net: true`) or all-deny (`net: false` / only denies) → Disabled, Connection
///   fine-grained allow, no brokers                                → Auto,     Connection
///   fine-grained allow, brokers present                           → Auto,     TlsInspect
/// A fine-grained net allow now AUTO-STARTS the egress proxy (was a compile error
/// demanding an authored proxy); termination is engaged only for the brokered hosts,
/// via the TlsInspect tier + per-host broker matching. Runs AFTER [`validate_brokers`],
/// so a non-empty broker list implies enforce + has_allow.
fn finalize_net_inspection(net: &mut NetPolicy) {
    let has_allow = net.rules.iter().any(|rule| rule.effect == Effect::Allow);
    if !net.enforce || !has_allow {
        net.mode = ProxyMode::Disabled;
        net.inspection = Inspection::Connection;
        return;
    }
    net.mode = ProxyMode::Auto;
    net.inspection = if net.brokers.is_empty() {
        Inspection::Connection
    } else {
        Inspection::TlsInspect
    };
}

/// The complete-statement FLOOR for an unlisted axis — the security inversion.
/// fs: deny-all (`FsRuleSet::default` is a deny base with no entries). net:
/// deny-all, ENFORCING. env: strip-all (enforce, empty constructed, everything
/// withheld) — identical to folding the axis with `false`.
fn floor_fs() -> FsPolicy {
    FsPolicy::default()
}
fn floor_net() -> NetPolicy {
    NetPolicy {
        enforce: true,
        default_effect: Effect::Deny,
        ..Default::default()
    }
}
fn floor_env(ctx: &CompileCtx) -> EnvPolicy {
    // Strip-all withholds all user/ambient env, but still injects the minimal
    // OS-startup essentials (Windows: SystemRoot &c.) so a floored child SPAWNS
    // reliably rather than succeeding only where the OS tolerates an empty block —
    // OS mechanism, not a floor breach. Shared with `env: false`.
    defaults::strip_all_env(&ctx.ambient_env)
}

/// `sandbox: false` — every axis relaxed. The explicit escape hatch.
fn unjailed(ctx: &CompileCtx) -> SandboxPolicy {
    SandboxPolicy {
        fs: relaxed_fs(),
        net: relaxed_net(),
        env: crate::policy::EnvPolicy {
            resolved: true,
            enforce: false,
            constructed: ctx.ambient_env.clone(),
            ..Default::default()
        },
        pid: Default::default(),
        build_jail: false,
    }
}

/// A relaxed fs axis: allow-all base, no entries.
fn relaxed_fs() -> FsPolicy {
    let mut fs = FsPolicy::default();
    fs.rules.default_effect = Effect::Allow;
    fs
}

/// A relaxed net axis: not enforcing.
fn relaxed_net() -> NetPolicy {
    NetPolicy {
        enforce: false,
        ..Default::default()
    }
}

/// `sandbox: true` — secure defaults per axis. PROVISIONAL posture (documented):
/// the exact runtime secure-default is the deferred runtime-frontend's product
/// call; the frontend-less engine only needs a safe, explicit baseline since the
/// conformance fixtures drive explicit policies. Today: generous read minus
/// secrets + no write, deny-all net, stripped env.
fn secure_default(ctx: &CompileCtx) -> Result<SandboxPolicy, CompileError> {
    Ok(SandboxPolicy {
        fs: secure_default_fs(ctx),
        net: secure_default_net(),
        env: secure_default_env(ctx),
        pid: Default::default(),
        build_jail: false,
    })
}

/// `sandbox: true` env — the curated non-secret baseline (usable + secret-free).
fn secure_default_env(ctx: &CompileCtx) -> crate::policy::EnvPolicy {
    let constructed = defaults::curated_baseline_env(&ctx.ambient_env);
    let withheld = ctx
        .ambient_env
        .keys()
        .filter(|k| !constructed.contains_key(*k))
        .cloned()
        .collect();
    crate::policy::EnvPolicy {
        resolved: true,
        enforce: true,
        constructed,
        schema: Vec::new(),
        withheld,
        // The curated baseline is secret-free by construction.
        sensitive_keys: Vec::new(),
    }
}

fn secure_default_fs(ctx: &CompileCtx) -> FsPolicy {
    // `sandbox: true`'s fs base — the generous-read allow + home-secret read denies, no
    // write grant. P4 removed the naked-`...` splice that used to assemble this; the fold
    // now builds it DIRECTLY (behavior-preserving). The home-secret denies remain part of
    // the `sandbox: true` posture here — relocating them to a preset-only model is decision
    // (a), deferred to Phase 7 (build-jail v2). See `fold::secure_default_fs`.
    fold::secure_default_fs(ctx)
}

fn secure_default_net() -> NetPolicy {
    // Enforce with a deny-all base and no committed allowlist (the build-jail
    // baseline owns the trusted-host allows).
    NetPolicy {
        enforce: true,
        default_effect: Effect::Deny,
        ..Default::default()
    }
}

enum StringKind {
    Preset,
    FileRef,
}

/// Disambiguate a `sandbox` string: a path-like string (leading `./`/`../`/`/`/`~`,
/// or carrying a file extension) is a file-ref; a bare identifier is a preset. Must
/// stay byte-identical to nub-cli's `project_config::classify_sandbox_string` — the
/// two classify the same surface string and a divergence would route it differently
/// through the skeleton vs the engine.
fn classify_string(s: &str) -> StringKind {
    let path_like = s.starts_with("./")
        || s.starts_with("../")
        || s.starts_with('/')
        || s.starts_with('~')
        || std::path::Path::new(s).extension().is_some();
    if path_like {
        StringKind::FileRef
    } else {
        StringKind::Preset
    }
}
