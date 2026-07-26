//! The config compiler (Boundary A): the ONLY code that understands surface
//! syntax. Input is already-parsed data (a `serde_json::Value` for the `sandbox`
//! block + a [`CompileCtx`] of host-provided paths/env); output is a fully
//! resolved [`SandboxPolicy`]. The compiler NEVER reads config files — it may
//! canonicalize fs paths (a filesystem read, not a config read).
//!
//! Pipeline (design.md §2.2): wrapper trichotomy → preset expansion → per-axis
//! fold (with `"..."` spread + last-match-wins order) → env grammar + `$(…)` →
//! emit. Scope resolution and tighten-only layering live in sibling modules for
//! the future project frontend; the `--sandbox` entry is single-block.

mod clobber;
mod defaults;
mod env_grammar;
mod fold;
pub mod layering;
mod preset;
mod resolve;
pub mod scope;

pub use resolve::{CommandRunner, ShellRunner};

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
    /// Exact-host credential brokering (`net.<host>.env`) — forcing TLS termination
    /// and releasing named parent credentials only where their opaque markers occur
    /// in request headers. Denied to dependency-controlled config.
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
    /// The capabilities of a SINGLE-BLOCK compile — the `compile`/`compile_with_warnings`
    /// entry, whose one scope IS this whole ctx (the `--sandbox <file>` / `run_sandboxed`
    /// path, always approved user config). The chain resolver ([`scope::resolve_chain`])
    /// assigns each scope its OWN capabilities via [`scope::ChainScope::caps`] and NEVER
    /// reads this field — capability is decided per scope, so the fold gates read the
    /// threaded per-scope `caps`, not the ctx.
    pub caps: ScopeCapabilities,
    /// The ambient env snapshot the child env is constructed from.
    pub ambient_env: BTreeMap<String, String>,
    /// The `$(…)` command runner (production shells out; tests inject a stub).
    pub runner: Box<dyn CommandRunner>,
}

impl CompileCtx {
    /// A ctx with the real shell runner and the given homes/env and single-block caps.
    pub fn new(
        homes: Homes,
        cwd: std::path::PathBuf,
        caps: ScopeCapabilities,
        ambient_env: BTreeMap<String, String>,
    ) -> Self {
        Self {
            homes,
            cwd,
            caps,
            ambient_env,
            runner: Box::new(ShellRunner),
        }
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
/// non-fatal warnings (the clobber smell). Single-term entry: the `"..."` payload
/// resolves against the built-in base (there is no parent scope).
pub fn compile_with_warnings(
    surface: &Value,
    ctx: &CompileCtx,
) -> Result<(SandboxPolicy, Vec<CompileWarning>), CompileError> {
    let mut warnings = Vec::new();
    // A single-block compile is one scope; its capability is the ctx's (the entry
    // owns the only scope). The chain path instead assigns each scope its own.
    let policy = compile_scope(surface, None, ctx, ctx.caps, &mut warnings)?;
    Ok((policy, warnings))
}

/// Resolve ONE scope's surface against its resolved `parent` (the enclosing
/// scope's policy; `None` at the outermost scope → the built-in base). The
/// wrapper trichotomy; a granular object goes to [`compile_object`]. This is the
/// per-scope primitive the chain resolver ([`scope::resolve_chain`]) drives.
pub(crate) fn compile_scope(
    surface: &Value,
    parent: Option<&SandboxPolicy>,
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
                let mut policy = compile_object(&expanded, parent, ctx, caps, warnings)?;
                // A preset's broad grants (build-jail's `"./"`) re-open the built-in
                // secret floor under last-match-wins; re-assert it post-fold.
                preset::reassert_secret_floor(s, &mut policy, ctx);
                Ok(policy)
            }
            StringKind::FileRef => Err(CompileError::FileRefUnresolved {
                path: String::new(),
                reference: s.clone(),
            }),
        },
        Value::Object(_) => compile_object(surface, parent, ctx, caps, warnings),
        _ => Err(CompileError::shape(
            "",
            "sandbox must be a boolean, a preset name, a file-ref, or a { fs, net, vars, secrets } object",
        )),
    }
}

/// Fold a granular `{ fs, net, vars, secrets }` object. A present block is a COMPLETE
/// statement: an axis it does NOT list FLOORS (deny fs, deny-all-enforcing net,
/// strip env) — least-exposure, fails closed. An object-level `"..."` key
/// (`{ "...": true }`) opts every UNLISTED axis into inheriting the enclosing
/// scope's base instead of flooring; a LISTED axis's own `"..."` inherits that
/// axis. So `{}` = deny-all; `{ "fs": [...] }` floors net + env (both `vars` and
/// `secrets`); `{ "...": true }` ≡ the enclosing base for all axes.
fn compile_object(
    surface: &Value,
    parent: Option<&SandboxPolicy>,
    ctx: &CompileCtx,
    caps: ScopeCapabilities,
    warnings: &mut Vec<CompileWarning>,
) -> Result<SandboxPolicy, CompileError> {
    let obj = surface
        .as_object()
        .ok_or_else(|| CompileError::shape("", "expected a { fs, net, vars, secrets } object"))?;
    fold::reject_unknown_keys(obj, &["fs", "net", "vars", "secrets", "proxy", "..."], "")?;
    let inherit_base = object_spread(obj.get("..."))?;

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
        Some(v) => fold::fold_fs(v, ctx, "fs", parent.map(|p| &p.fs))?,
        None if inherit_base => inherit_fs(parent, ctx),
        None => floor_fs(),
    };
    let mut net = match obj.get("net") {
        Some(v) => fold::fold_net(v, ctx, caps, "net", parent.map(|p| &p.net))?,
        None if inherit_base => inherit_net(parent),
        None => floor_net(),
    };
    // The `proxy` knob is authored at the wrapper level (sibling of net) but governs the
    // net axis. A child that inherits a parent net array through `"..."` also inherits
    // the parent's explicit proxy posture; otherwise omission remains Disabled. That
    // means a parent author can grant a proxy once without a nested scope silently
    // widening it, while a fresh fine-grained allow still needs its own opt-in.
    if obj.contains_key("proxy") {
        net.mode = parse_proxy_mode(obj.get("proxy"))?;
    } else if net_array_inherits_parent_mode(obj.get("net")) {
        net.mode = parent
            .map(|policy| policy.net.mode)
            .unwrap_or(ProxyMode::Disabled);
    }
    reconcile_brokers(&mut net);
    finalize_net_inspection(&mut net, "net")?;
    // The env-family axes (`vars` + `secrets`) fold together into one EnvPolicy.
    // Both absent: inherit (under `"..."`) or floor, exactly as the old single `env`
    // axis did. Either present: an explicit, complete env statement — the other axis
    // defaults to no entries.
    let vars = obj.get("vars");
    let secrets = obj.get("secrets");
    let mut env = match (vars, secrets) {
        (None, None) if inherit_base => inherit_env(parent, ctx),
        (None, None) => floor_env(ctx),
        _ => fold::fold_env_axes(vars, secrets, ctx, caps, parent.map(|p| &p.env))?,
    };
    withhold_brokered_env(&net, &mut env, ctx);
    Ok(SandboxPolicy {
        fs,
        net,
        env,
        pid: Default::default(),
    })
}

fn reconcile_brokers(net: &mut NetPolicy) {
    let removed_hosts = net
        .brokers
        .iter()
        .filter(|broker| !crate::matcher::HostMatcher::new(net).admits(&broker.host))
        .map(|broker| broker.host.clone())
        .collect::<Vec<_>>();
    if removed_hosts.is_empty() {
        return;
    }
    net.brokers.retain(|broker| {
        !removed_hosts
            .iter()
            .any(|host| host.eq_ignore_ascii_case(&broker.host))
    });
    // The object-form broker contributed an implicit exact-host Allow. Once a
    // later rule denies that host, remove the now-shadowed grant with the broker;
    // otherwise the syntactic Allow would spuriously require a proxy even though
    // the final last-match verdict admits no brokered traffic.
    net.rules.retain(|rule| {
        !(rule.effect == Effect::Allow
            && matches!(
                &rule.target,
                crate::policy::NetTarget::Host(host)
                    if removed_hosts
                        .iter()
                        .any(|removed| removed.eq_ignore_ascii_case(host))
            ))
    });
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

/// Whether a net array splices the enclosing policy at least once. The fold copies
/// inherited rules but not wrapper metadata, so this preserves the parent proxy mode
/// across an explicit `"..."` splice. Object-form net deliberately has no sentinel.
fn net_array_inherits_parent_mode(value: Option<&Value>) -> bool {
    matches!(
        value,
        Some(Value::Array(items))
            if items.iter().any(|item| matches!(item, Value::String(entry) if entry == "..."))
    )
}

/// Parse the wrapper-level `proxy` knob into a [`ProxyMode`]. Omission leaves the
/// proxy disabled. A fine-grained allow must opt into one of the explicit modes;
/// a credential broker is the sole automatic-proxy capability.
fn parse_proxy_mode(v: Option<&Value>) -> Result<ProxyMode, CompileError> {
    match v {
        None => Ok(ProxyMode::Disabled),
        Some(Value::String(s)) => match s.as_str() {
            "auto" => Ok(ProxyMode::Auto),
            "passthrough" => Ok(ProxyMode::Passthrough),
            "terminate" => Ok(ProxyMode::Terminate),
            other => Err(CompileError::shape(
                "proxy",
                &format!(
                    "`proxy` must be \"auto\", \"passthrough\", or \"terminate\" (got `{other}`)"
                ),
            )),
        },
        Some(_) => Err(CompileError::shape(
            "proxy",
            "`proxy` must be a string: \"auto\", \"passthrough\", or \"terminate\"",
        )),
    }
}

/// Derive the net enforcement tier and reject a policy whose requested net allows
/// could not be enforced. Coarse `net: true` / `net: false` never need a proxy. An
/// ordinary hostname/CIDR allow needs an explicit proxy mode; a credential broker is
/// the narrow exception because it necessarily needs a terminating proxy to inject its
/// headers. `passthrough` still forbids that termination.
fn finalize_net_inspection(net: &mut NetPolicy, path: &str) -> Result<(), CompileError> {
    let has_allow = net.rules.iter().any(|rule| rule.effect == Effect::Allow);
    let needs_tls = !net.brokers.is_empty();

    if !net.enforce {
        if net.mode != ProxyMode::Disabled {
            return Err(CompileError::shape(
                "proxy",
                "`proxy` requires a fine-grained net allow; `net: true` is unrestricted and does not start a proxy",
            ));
        }
        net.inspection = Inspection::Connection;
        return Ok(());
    }

    if !has_allow {
        if net.mode != ProxyMode::Disabled {
            return Err(CompileError::shape(
                "proxy",
                "`proxy` requires a fine-grained net allow; `net: false` denies all egress and does not start a proxy",
            ));
        }
        net.inspection = Inspection::Connection;
        return Ok(());
    }

    if !needs_tls && net.mode == ProxyMode::Disabled {
        return Err(CompileError::shape(
            path,
            "a fine-grained net allow requires an explicit proxy — set `proxy` to \"auto\", \"passthrough\", or \"terminate\"",
        ));
    }

    net.inspection = match net.mode {
        ProxyMode::Passthrough => {
            if needs_tls {
                return Err(CompileError::shape(
                    path,
                    "a credential-broker rule requires TLS termination, but `proxy` is \"passthrough\" — remove the broker rule or set `proxy` to \"auto\"/\"terminate\"",
                ));
            }
            Inspection::Connection
        }
        ProxyMode::Terminate => Inspection::TlsInspect,
        ProxyMode::Disabled if needs_tls => Inspection::TlsInspect,
        ProxyMode::Auto if needs_tls => Inspection::TlsInspect,
        ProxyMode::Disabled | ProxyMode::Auto => Inspection::Connection,
    };
    Ok(())
}

/// Parse a top-level object `"..."` key. `true` = inherit the enclosing base for
/// unlisted axes; a string = a file-extends (frontend-resolved — deferred here);
/// anything else is a shape error. Absent = complete statement (floor unlisted).
fn object_spread(v: Option<&Value>) -> Result<bool, CompileError> {
    match v {
        None => Ok(false),
        Some(Value::Bool(true)) => Ok(true),
        // Only a path-like string is a (frontend-deferred) file-ref; a bare scalar
        // (`{"...": "r"}`) is a malformed sentinel value, not a file to resolve.
        Some(Value::String(reference)) if is_file_ref_value(reference) => {
            Err(CompileError::FileRefUnresolved {
                path: "...".to_string(),
                reference: reference.clone(),
            })
        }
        Some(v) => Err(CompileError::shape("...", &sentinel_value_error(v))),
    }
}

/// An unlisted axis under an object-level `"..."`: inherit the resolved parent's
/// axis at an inner scope, or the built-in base (≡ `sandbox: true`'s axis) at the
/// outermost scope.
fn inherit_fs(parent: Option<&SandboxPolicy>, ctx: &CompileCtx) -> FsPolicy {
    parent
        .map(|p| p.fs.clone())
        .unwrap_or_else(|| secure_default_fs(ctx))
}
fn inherit_net(parent: Option<&SandboxPolicy>) -> NetPolicy {
    parent
        .map(|p| p.net.clone())
        .unwrap_or_else(secure_default_net)
}
fn inherit_env(parent: Option<&SandboxPolicy>, ctx: &CompileCtx) -> EnvPolicy {
    parent
        .map(|p| p.env.clone())
        .unwrap_or_else(|| secure_default_env(ctx))
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
    }
}

fn secure_default_fs(ctx: &CompileCtx) -> FsPolicy {
    // Equivalent to `fs: ["..."]` — the generous-read + secret-deny defaults, no
    // write grant. Outermost scope → `parent = None`.
    fold::fold_fs(
        &Value::Array(vec![Value::String("...".into())]),
        ctx,
        "fs",
        None,
    )
    .expect("`[\"...\"]` fs default always folds")
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

/// Whether a `"..."` sentinel's STRING value is a file-ref (a `"./policy.json"`
/// file-extends, frontend-deferred) rather than a malformed scalar. The `"..."`
/// value grammar is `true | "<file-ref>" | [list]`; a non-path-like scalar
/// (`"r"`, `"port"`) is NOT a file-ref and must be a clear shape error, never
/// silently admitted into the file-extends branch (which the frontend would then
/// try to resolve as a file named `port`). Path-likeness is defined identically to
/// [`classify_string`]'s file-ref arm so the two never disagree.
pub(super) fn is_file_ref_value(s: &str) -> bool {
    matches!(classify_string(s), StringKind::FileRef)
}

/// The self-debugging shape message for a `"..."` sentinel carrying a value outside
/// its `true | "<file-ref>"` grammar — names the offending construct so the author
/// sees exactly what was rejected.
pub(super) fn sentinel_value_error(offending: &Value) -> String {
    let got = match offending {
        Value::String(s) => format!("the string `{s}`"),
        Value::Bool(false) => "`false`".to_string(),
        Value::Number(n) => format!("the number `{n}`"),
        Value::Array(_) => "an array".to_string(),
        Value::Object(_) => "an object".to_string(),
        Value::Null => "null".to_string(),
        Value::Bool(true) => "`true`".to_string(),
    };
    format!(
        "`\"...\"` value must be true (inherit the enclosing scope) or a file-ref (e.g. \"./policy.json\") — got {got}"
    )
}
