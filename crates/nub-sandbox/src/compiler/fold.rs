//! Per-axis fold: an axis surface value (`false | true | array | object`) →
//! its resolved IR fragment. The `"..."` spread and last-match-wins ORDER are
//! discharged here into a flat ordered list; the actual last-match decision is
//! made at evaluation time by the matcher, so the fold only has to preserve
//! order and splice the defaults at the sentinel's position.

use super::defaults;
use super::env_grammar::{EnvType, parse_env_type};
use super::resolve;
use super::{CompileCtx, CompileError};
use crate::matcher::path::expand_symbolic;
use crate::policy::{
    CanonGlob, CredentialBroker, Effect, EnvFormat, EnvPolicy, EnvRule, FsAccess, FsPolicy, FsRule,
    FsRuleSet, HeaderInject, NetPolicy, NetRule, NetTarget, Secret, TmpMode,
};
use globset::{GlobBuilder, GlobMatcher};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// A `"!..."` entry — a negated inheritance sentinel — is meaningless (you cannot
/// deny "the inherited scope") and is a shape error on every axis (D-list).
const SENTINEL_NEGATE_MSG: &str =
    "`!...` is invalid — `\"...\"` is the inheritance sentinel and cannot be negated";
/// An empty / whitespace-only fs entry used to expand to `**` (a silent whole-fs
/// grant, fail-OPEN); it is now a hard shape error (D3).
const EMPTY_FS_ENTRY_MSG: &str = "an empty fs entry is not allowed (it would grant the whole filesystem) — name a path or remove it";
/// `"..."` inheritance in fs/net is positional in the ARRAY form; as an OBJECT key
/// it has no defined meaning, so it is rejected rather than silently treated as a
/// literal path/host named `...` (fail loud, parity with env-object + the array).
const OBJECT_SENTINEL_MSG: &str = "`\"...\"` inheritance is only valid in fs/net array form (e.g. [\"...\", …]), not as an object key";

// ── fs ───────────────────────────────────────────────────────────────────────

/// Fold the `fs` axis value into an [`FsPolicy`]. Array entries and object keys
/// are subtree-expanded (a bare path grants the node + `/**`); a glob-bearing
/// pattern is emitted verbatim. Access: array grants are ReadWrite (the concise
/// "these paths are fully usable" form); object values pick `"r"`/`"rw"`. A
/// `"..."` inherits the enclosing scope's fs at its position: the resolved
/// `parent` when present (cross-scope inheritance), else the built-in generous-
/// read + secret-deny base (outermost scope).
pub fn fold_fs(
    value: &Value,
    ctx: &CompileCtx,
    path: &str,
    parent: Option<&FsPolicy>,
) -> Result<FsPolicy, CompileError> {
    let mut set = FsRuleSet {
        entries: Vec::new(),
        default_effect: Effect::Deny,
    };
    // Throwaway-tmp mode. `<tmp>` (and any `<tmp>/subpath`) is a SENTINEL for the specially-
    // provisioned per-run PRIVATE dir — a subpath maps INTO that dir, never the shared system
    // tmp — so its value is a plain fs permission: a truthy grant (`"r"`/`"rw"`/`true`) →
    // `Private` (fresh per-run dir, shared tmp hidden); `false` → `Deny` (no tmp). The backend
    // owns the private-dir creation + whole-subtree grant + shared-tmp denial at spawn time (the
    // per-run path is not knowable at compile time), so a `<tmp>`-prefixed entry sets only the
    // MODE and emits no ordinary fs rule. Shared system tmp is a SEPARATE literal path, reached
    // only by granting `/tmp` — never via this sentinel; `Shared` (the default when `<tmp>` is
    // absent) means "no tmp confinement, host tmp per fs rules".
    let mut tmp = TmpMode::Shared;
    match value {
        // `true` fully relaxes the axis; `false` fully denies it.
        Value::Bool(true) => set.default_effect = Effect::Allow,
        Value::Bool(false) => set.default_effect = Effect::Deny,
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                let p = child(path, &i.to_string());
                let s = as_str(item, &p)?;
                if let Some(mode) = parse_tmp_mode_array(s, &p)? {
                    tmp = mode;
                    continue;
                }
                fold_fs_array_entry(s, ctx, parent, &p, &mut set.entries)?;
            }
        }
        Value::Object(map) => {
            for (key, val) in map {
                let p = child(path, key);
                if let Some(mode) = parse_tmp_mode(key, val, &p)? {
                    tmp = mode;
                    continue;
                }
                fold_fs_object_entry(key, val, ctx, &p, &mut set.entries)?;
            }
        }
        _ => {
            return Err(CompileError::shape(
                path,
                "fs must be a boolean, an array, or a pattern-keyed object",
            ));
        }
    }
    finalize_env_deny(&mut set);
    Ok(FsPolicy { rules: set, tmp })
}

/// A `<tmp>` sentinel malformed by a suffix that is neither empty nor a path separator
/// (`<tmp>*`, `<tmp>x`). Rejected loud rather than folded: `expand_symbolic` would otherwise
/// root it at the SHARED host tmp (`<tmp>x` → `<host-tmp>/x`), the exact leak the sentinel
/// exists to prevent — so any `<tmp>`-prefixed form that is not the sentinel is an error.
const MALFORMED_TMP_MSG: &str = "malformed `<tmp>` sentinel — use `<tmp>` (a fresh per-run private tmp dir) or `<tmp>/subpath` (a path inside it); `<tmp>` followed by anything else is not a path into the shared system tmp — grant the literal `/tmp` for that";

/// Classify a trimmed key/entry against the `<tmp>` sentinel. A `<tmp>`-prefixed string matches
/// exactly what `expand_symbolic` roots at the host tmp (`strip_prefix("<tmp>")`), so the guard
/// MUST cover the whole class or a form it misses leaks into the shared tmp. `<tmp>` or
/// `<tmp>{/,\}subpath` is the sentinel; any other `<tmp>`-prefixed form is malformed.
enum TmpKey {
    Sentinel,
    Malformed,
    NotTmp,
}
fn classify_tmp_key(k: &str) -> TmpKey {
    let Some(after) = k.strip_prefix("<tmp>") else {
        return TmpKey::NotTmp;
    };
    if after.is_empty() || after.starts_with(['/', '\\']) {
        TmpKey::Sentinel
    } else {
        TmpKey::Malformed
    }
}

/// Fold a `<tmp>`-prefixed key into a tmp MODE. `<tmp>` (and any `<tmp>/subpath`) denotes the
/// per-run PRIVATE dir — a subpath maps INTO that dir, never the shared system tmp — so the
/// value is a plain fs permission on it: a truthy grant (`"r"`/`"rw"`/`true`) → `Private`
/// (provision the fresh dir + grant it rw); `false` → `Deny` (no tmp). Read-only on a fresh
/// empty dir is degenerate, so `"r"` is treated as `"rw"` — `Private` always grants rw. The
/// whole private subtree is backend-granted, so a `<tmp>/x` key needs no path rule of its own;
/// the caller consumes a `Some` and emits nothing. `None` for a normal (non-`<tmp>`) key; a
/// malformed `<tmp>` suffix is a hard error (it would otherwise leak into the shared host tmp).
fn parse_tmp_mode(key: &str, val: &Value, path: &str) -> Result<Option<TmpMode>, CompileError> {
    match classify_tmp_key(key.trim()) {
        TmpKey::NotTmp => return Ok(None),
        TmpKey::Malformed => return Err(CompileError::shape(path, MALFORMED_TMP_MSG)),
        TmpKey::Sentinel => {}
    }
    let mode = match val {
        Value::Bool(true) => TmpMode::Private,
        Value::Bool(false) => TmpMode::Deny,
        Value::String(s) if s == "r" || s == "rw" => TmpMode::Private,
        _ => {
            return Err(CompileError::shape(
                path,
                "`<tmp>` takes an fs permission: \"r\"/\"rw\"/`true` (a fresh per-run private tmp dir, shared system tmp hidden) or `false` (no tmp) — for the shared system tmp, grant the literal path `/tmp`",
            ));
        }
    };
    Ok(Some(mode))
}

/// Array-form `<tmp>` sentinel → tmp MODE. A `<tmp>` / `<tmp>/subpath` entry is `Private`
/// (array grants are rw); a `!`-negated one is `Deny`. `None` for a normal entry; a malformed
/// `<tmp>` suffix errors, same as the object [`parse_tmp_mode`], so the two agree on the class.
fn parse_tmp_mode_array(entry: &str, path: &str) -> Result<Option<TmpMode>, CompileError> {
    let (body, deny) = match entry.trim().strip_prefix('!') {
        Some(rest) => (rest.trim_start(), true),
        None => (entry.trim(), false),
    };
    match classify_tmp_key(body) {
        TmpKey::NotTmp => Ok(None),
        TmpKey::Malformed => Err(CompileError::shape(path, MALFORMED_TMP_MSG)),
        TmpKey::Sentinel => Ok(Some(if deny {
            TmpMode::Deny
        } else {
            TmpMode::Private
        })),
    }
}

/// Inject the default `.env*` READ-deny at a precedence a broad dir/glob allow can NOT
/// override but an EXPLICIT exact-file allow can. `.env*` files hold the exact secrets
/// the sandbox scrubs, so reading them is denied by default on any read-granting fs
/// policy — including the OBJECT form, which never spliced the `"..."` secret set. The
/// rule composes with the last-match-wins fs algebra as FOUR ordered bands (backends
/// stay pure IR replicators — every one evaluates last-match-wins over these entries):
///   band 1  — the folded user + default entries, unchanged;
///   band 2a — the `.env*` LEAF deny, appended so it beats every band-1 broad/glob
///             allow (the `["...", "./"]` footgun where a trailing dir-allow re-exposed
///             `<proj>/.env` is closed here);
///   band 3  — the user's EXACT-FILE `.env*` allows re-emitted so they beat band 2a
///             (informed consent: `{ "./.env.production": "r" }` grants that one file);
///   band 2c — the `.env*` SUBTREE deny (`**/.env*/**`), appended LAST so it is
///             unconditionally authoritative for a `.env*`-NAMED DIRECTORY's CONTENTS.
/// Why the subtree deny goes AFTER the exact-file allows (the sub-directory-bypass fix):
/// a bare-path allow expands to a leaf grant + a `/**` subtree twin, and a subtree grant
/// is a SUBPATH on macOS / a ReadSubtree on Linux — so re-emitting the twin of a
/// `.env*`-prefixed DIRECTORY allow (`{ "./.env.d": "r" }`) would re-expose every secret
/// under `.env.d/` if the subtree deny preceded it. Ordering the subtree deny last means
/// an exact-file allow can only re-grant the `.env*` LEAF (a file), never a directory's
/// denied children — closing the bypass identically on every backend (pure last-match).
/// A band-1 entry the user later DENIES for that exact path is NOT re-emitted (its
/// band-1 verdict is Deny), so an explicit `!./.env` stays denied.
///
/// Skipped only for a FULLY-relaxed axis (`fs: true` / `sandbox: false` — the explicit
/// escape hatch) and for a policy that grants no reads at all (a deny-all fs), where the
/// deny would be inert noise.
fn finalize_env_deny(set: &mut FsRuleSet) {
    let fully_relaxed = set.default_effect == Effect::Allow && set.entries.is_empty();
    let grants_read = set.default_effect == Effect::Allow
        || set.entries.iter().any(|e| e.effect == Effect::Allow);
    if fully_relaxed || !grants_read {
        return;
    }
    // Compute band 3 from band 1 BEFORE appending any deny, so the survivor test reflects
    // the user's own last-match verdict (not the denies we are about to add).
    let band3 = exact_env_allows_surviving(set);
    set.entries.extend(defaults::env_deny_leaf_rules()); // band 2a: leaf deny
    set.entries.extend(band3); // band 3: exact-file allows
    set.entries.extend(defaults::env_deny_subtree_rules()); // band 2c: subtree deny (LAST)
}

/// The band-3 survivors: each EXACT-FILE `.env*` allow entry whose band-1 (user)
/// verdict for its own path is Allow. An exact-file entry is a literal path (or its
/// `/**` subtree twin) whose basename starts with `.env`; a glob-bearing allow
/// (`**/.env*`, `./*`) is NOT an exact-file allow and does not override the deny.
fn exact_env_allows_surviving(set: &FsRuleSet) -> Vec<FsRule> {
    let mut out = Vec::new();
    for r in &set.entries {
        if r.effect != Effect::Allow {
            continue;
        }
        // The literal file identity is the matcher minus a `/**` subtree twin.
        let literal = r
            .matcher
            .as_str()
            .strip_suffix("/**")
            .unwrap_or(r.matcher.as_str());
        if is_glob(literal) || !basename_is_dotenv(literal) {
            continue;
        }
        if band1_verdict_allows(set, literal) {
            out.push(r.clone());
        }
    }
    out
}

/// Whether a path's final component's basename starts with `.env` (the deny target).
fn basename_is_dotenv(path: &str) -> bool {
    path.rsplit('/').next().unwrap_or(path).starts_with(".env")
}

/// The band-1 last-match-wins verdict for the concrete `candidate` path over the set's
/// current (pre-band-2) entries. Pure string glob matching over the canonical IR globs
/// — deterministic, no filesystem touch (the candidate is already a canonical IR path).
fn band1_verdict_allows(set: &FsRuleSet, candidate: &str) -> bool {
    let mut effect = set.default_effect;
    for r in &set.entries {
        if let Ok(m) = crate::matcher::path::compile_glob(r.matcher.as_str())
            && m.is_match(candidate)
        {
            effect = r.effect;
        }
    }
    effect == Effect::Allow
}

fn fold_fs_array_entry(
    s: &str,
    ctx: &CompileCtx,
    parent: Option<&FsPolicy>,
    path: &str,
    out: &mut Vec<FsRule>,
) -> Result<(), CompileError> {
    if s == "!..." {
        return Err(CompileError::shape(path, SENTINEL_NEGATE_MSG));
    }
    if s == "..." {
        splice_fs_inherit(ctx, parent, out);
        return Ok(());
    }
    if s.trim().is_empty() {
        return Err(CompileError::shape(path, EMPTY_FS_ENTRY_MSG));
    }
    let (pattern, effect) = match s.strip_prefix('!') {
        Some(rest) => (rest, Effect::Deny),
        None => (s, Effect::Allow),
    };
    // `$(…)` resolves AFTER the `!` strip so a command's stdout is a path, never a
    // deny operator it could smuggle in. Array grants are ReadWrite; denies deny both.
    let pattern = resolve_fs_path(pattern, ctx, path)?;
    push_fs_rules(&pattern, effect, FsAccess::ReadWrite, ctx, out);
    Ok(())
}

fn fold_fs_object_entry(
    key: &str,
    val: &Value,
    ctx: &CompileCtx,
    path: &str,
    out: &mut Vec<FsRule>,
) -> Result<(), CompileError> {
    if key == "!..." {
        return Err(CompileError::shape(path, SENTINEL_NEGATE_MSG));
    }
    if key == "..." {
        return Err(CompileError::shape(path, OBJECT_SENTINEL_MSG));
    }
    if key.trim().is_empty() {
        return Err(CompileError::shape(path, EMPTY_FS_ENTRY_MSG));
    }
    let (effect, access) = match val {
        Value::Bool(true) => (Effect::Allow, FsAccess::ReadWrite),
        Value::Bool(false) => (Effect::Deny, FsAccess::Read),
        Value::String(s) => match s.as_str() {
            "r" => (Effect::Allow, FsAccess::Read),
            "rw" => (Effect::Allow, FsAccess::ReadWrite),
            other => {
                return Err(CompileError::shape(
                    path,
                    &format!("fs value `{other}` — expected \"r\", \"rw\", true, or false"),
                ));
            }
        },
        _ => {
            return Err(CompileError::shape(
                path,
                "fs value must be \"r\", \"rw\", true, or false",
            ));
        }
    };
    // Resolve `$(…)` in the path key AFTER validating the access value, so an
    // invalid `val` errors before any command runs (no wasted exec side effect).
    let pattern = resolve_fs_path(key, ctx, path)?;
    push_fs_rules(&pattern, effect, access, ctx, out);
    Ok(())
}

/// Expand a surface fs pattern into its canonical subtree globs and push a rule
/// per glob (so `~/.ssh` covers both `~/.ssh` and `~/.ssh/**`).
fn push_fs_rules(
    pattern: &str,
    effect: Effect,
    access: FsAccess,
    ctx: &CompileCtx,
    out: &mut Vec<FsRule>,
) {
    // Normalize a deny's access to the canonical inert value (D20): the array
    // form grants ReadWrite even to a `!`-deny, the object form emits Read — same
    // enforcement (a deny removes read+write), divergent IR. Fold both to one here,
    // the single funnel for user fs rules, so the IR carries a uniform deny.
    let access = if effect == Effect::Deny {
        FsAccess::DENY
    } else {
        access
    };
    let expanded = expand_symbolic(pattern, &ctx.homes);
    for g in defaults::subtree_globs(&expanded) {
        out.push(FsRule {
            matcher: CanonGlob(crate::matcher::canonicalize_glob_prefix(&g)),
            effect,
            access,
        });
    }
}

/// Resolve any `$(…)` command substitution in an fs PATH at config-LOAD time, via
/// the shared [`resolve`] machinery: the command runs once and its stdout becomes
/// the path, whole or embedded (`"$(pnpm store path)/v3"`), then flows into
/// [`push_fs_rules`] exactly as a literal would (symbolic-expand + subtree-glob +
/// canonicalize).
///
/// Fail-CLOSED corners — a resolved grant must never silently surprise. A command
/// FAILURE surfaces via `resolve_with` as a hard [`CompileError::Substitution`]
/// naming it. EMPTY output errors: an empty path would expand to a whole-fs `**`
/// grant (fail-OPEN). MULTI-LINE output errors rather than silently truncating to a
/// line that could grant the wrong subtree. Trailing whitespace is trimmed so a
/// path is clean; interior whitespace is a legitimate path character and preserved.
fn resolve_fs_path(raw: &str, ctx: &CompileCtx, path: &str) -> Result<String, CompileError> {
    if resolve::has_substitution(raw) {
        let resolved = resolve::resolve_with(raw, ctx.runner.as_ref())
            .map_err(|e| CompileError::substitution(path, &e))?;
        let resolved = resolved.trim_end().to_string();
        if resolved.is_empty() {
            return Err(CompileError::substitution(
                path,
                "`$(…)` produced empty output — expected a filesystem path",
            ));
        }
        if resolved.contains(['\n', '\r']) {
            return Err(CompileError::substitution(
                path,
                "`$(…)` produced multi-line output — a filesystem path must be a single line",
            ));
        }
        Ok(resolved)
    } else if resolve::has_open_substitution(raw) {
        // A `$(` with no balanced close — name it rather than ship shell-looking
        // text as a literal path (the same footgun the env path guards against).
        Err(CompileError::substitution(
            path,
            resolve::UNTERMINATED_SUBST_MSG,
        ))
    } else {
        Ok(raw.to_string())
    }
}

/// The fs `"..."` payload: at an inner scope splice the resolved parent's fs
/// entries (cross-scope inheritance); at the outermost scope (no parent) splice
/// the built-in generous-read + secret-deny base — the degenerate outermost case.
fn splice_fs_inherit(ctx: &CompileCtx, parent: Option<&FsPolicy>, out: &mut Vec<FsRule>) {
    match parent {
        Some(p) => out.extend(p.rules.entries.iter().cloned()),
        None => splice_fs_defaults(ctx, out),
    }
}

/// Splice the generous-read base + secret-deny defaults (the built-in fs base).
fn splice_fs_defaults(ctx: &CompileCtx, out: &mut Vec<FsRule>) {
    out.push(defaults::generous_read_allow());
    out.extend(defaults::secret_read_denies(&ctx.homes));
}

// ── net ──────────────────────────────────────────────────────────────────────

/// Fold the `net` axis into a [`NetPolicy`]. Entries are host globs or CIDRs;
/// `!` denies; `"..."` inherits the enclosing scope's net (the resolved `parent`
/// when present; nothing at the outermost scope — the built-in net base is
/// deny-all with no committed allowlist). `net: true` disables enforcement;
/// `net: false` denies all egress.
pub fn fold_net(
    value: &Value,
    ctx: &CompileCtx,
    path: &str,
    parent: Option<&NetPolicy>,
) -> Result<NetPolicy, CompileError> {
    let mut policy = NetPolicy {
        enforce: true,
        default_effect: Effect::Deny,
        ..Default::default()
    };
    match value {
        Value::Bool(true) => policy.enforce = false,
        Value::Bool(false) => {} // enforce, deny-all base, no rules
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                let p = child(path, &i.to_string());
                let s = as_str(item, &p)?;
                fold_net_entry(s, parent, &p, &mut policy.rules)?;
            }
        }
        Value::Object(map) => {
            for (key, val) in map {
                let p = child(path, key);
                if key == "!..." {
                    return Err(CompileError::shape(&p, SENTINEL_NEGATE_MSG));
                }
                if key == "..." {
                    return Err(CompileError::shape(&p, OBJECT_SENTINEL_MSG));
                }
                fold_net_object_value(key, val, ctx, &p, &mut policy)?;
            }
        }
        _ => {
            return Err(CompileError::shape(
                path,
                "net must be a boolean, an array, or a pattern-keyed object",
            ));
        }
    }
    Ok(policy)
}

fn fold_net_entry(
    s: &str,
    parent: Option<&NetPolicy>,
    path: &str,
    out: &mut Vec<NetRule>,
) -> Result<(), CompileError> {
    if s == "!..." {
        return Err(CompileError::shape(path, SENTINEL_NEGATE_MSG));
    }
    if s == "..." {
        // Inner scope: inherit the resolved parent's rules. Outermost (no parent):
        // the built-in net base is deny-all with no committed allowlist (the
        // build-jail baseline owns trusted-host allows), so splice nothing.
        if let Some(p) = parent {
            out.extend(p.rules.iter().cloned());
        }
        return Ok(());
    }
    let (pattern, effect) = match s.strip_prefix('!') {
        Some(rest) => (rest, Effect::Deny),
        None => (s, Effect::Allow),
    };
    push_net_rule(pattern, effect, path, out)
}

/// One entry of the net OBJECT form: `"<host>": true | false | { rule object }`. A
/// bool is a plain allow/deny (host-only, connection-level). A rule OBJECT is the
/// per-host capability grammar; cut-1's ONLY key is `inject` (credential brokering),
/// which implicitly ALLOWS the host and forces it to the TlsInspect tier.
fn fold_net_object_value(
    host: &str,
    val: &Value,
    ctx: &CompileCtx,
    path: &str,
    policy: &mut NetPolicy,
) -> Result<(), CompileError> {
    match val {
        Value::Bool(true) => push_net_rule(host, Effect::Allow, path, &mut policy.rules),
        Value::Bool(false) => push_net_rule(host, Effect::Deny, path, &mut policy.rules),
        Value::Object(rule) => fold_net_rule_object(host, rule, ctx, path, policy),
        _ => Err(CompileError::shape(
            path,
            "net host value must be true, false, or a rule object (e.g. { \"inject\": { … } })",
        )),
    }
}

/// Parse a per-host net rule OBJECT. Cut-1 vocabulary is credential brokering ONLY;
/// the request-filter fields the proposal reserves (`methods`/`paths`/`headers`) are
/// DEFERRED to a future request filter, so naming one fails loud rather than silently
/// under-enforcing. The brokered host is recorded both as an ordinary Allow rule (a
/// later explicit `!deny` can still override it — last-match-wins is preserved) and as
/// a [`CredentialBroker`] (→ TlsInspect tier for that host).
fn fold_net_rule_object(
    host: &str,
    rule: &serde_json::Map<String, Value>,
    ctx: &CompileCtx,
    path: &str,
    policy: &mut NetPolicy,
) -> Result<(), CompileError> {
    for key in rule.keys() {
        if key != "inject" {
            return Err(CompileError::shape(
                &child(path, key),
                &format!(
                    "`{key}` is not supported in this cut — the only per-host net capability is `inject` (credential brokering); verb/path/header filtering is deferred to a future request filter"
                ),
            ));
        }
    }
    // Credential brokering is a TRUSTED-ONLY capability. An untrusted grant
    // (`dependenciesMeta`) must not be able to force TLS termination of an allowed host
    // or inject ANY header (a literal value would otherwise slip past the `$(…)` gate).
    if !ctx.trusted {
        return Err(CompileError::shape(
            path,
            "credential brokering (`inject`) is a trusted-only capability — it is not permitted in an untrusted (dependenciesMeta) grant",
        ));
    }
    let inject = rule.get("inject").ok_or_else(|| {
        CompileError::shape(
            path,
            "a net rule object must contain `inject` (credential brokering) — it is the only per-host capability in this cut",
        )
    })?;
    // Reject a CIDR BEFORE it becomes an allow rule — it can't carry HTTP header
    // injection. Wildcard/glob broker hosts ARE accepted and match via the same
    // universal host-glob matcher as net allow/deny (maintainer decision): a
    // `*.example.com` broker brokers exactly the hosts it would allow, at the user's
    // own risk (see `validate_broker_host`).
    validate_broker_host(host, path)?;
    let injects = parse_inject(inject, ctx, &child(path, "inject"))?;
    push_net_rule(host, Effect::Allow, path, &mut policy.rules)?;
    policy.brokers.push(CredentialBroker {
        host: crate::matcher::host::strip_trailing_dot(host).to_string(),
        injects,
    });
    Ok(())
}

/// A broker host is any valid net host pattern — a literal, or the SAME universal
/// host-glob syntax used for net allow/deny (`*.example.com`, bare `*`). Only a CIDR is
/// rejected: brokering is an HTTP-header capability with no CIDR form. Wildcards are
/// intentionally permitted (maintainer decision): a `*.example.com` broker mints +
/// injects to the CLIENT-SUPPLIED SNI, so it brokers exactly the hosts the same pattern
/// would allow as a net rule — including, if the user points it at a too-broad wildcard,
/// an attacker-owned subdomain. That laundering-to-a-misconfigured-wildcard is the user's
/// own risk (identical to any wildcard net allow), out of the threat model — no warning.
fn validate_broker_host(pattern: &str, path: &str) -> Result<(), CompileError> {
    if pattern.contains('/') {
        return Err(CompileError::shape(
            path,
            "credential brokering targets a host, not a CIDR — name a hostname (a literal or a `*.` wildcard)",
        ));
    }
    if !crate::matcher::host::host_pattern_is_valid(pattern) {
        return Err(CompileError::shape(
            path,
            &format!("`{pattern}` is not a valid host for a credential broker"),
        ));
    }
    Ok(())
}

/// Parse the `inject` map (header-name → value string) into resolved [`HeaderInject`]s.
/// A `$(…)` value resolves through the SAME trusted gate as env values — an untrusted
/// (`dependenciesMeta`) `$(…)` is a hard error, never a silent exec. The resolved
/// secret is wrapped in [`Secret`] (redacting Debug, serde-skipped) so it never reaches
/// a dump, a log, or the child env.
fn parse_inject(
    value: &Value,
    ctx: &CompileCtx,
    path: &str,
) -> Result<Vec<HeaderInject>, CompileError> {
    let map = value.as_object().ok_or_else(|| {
        CompileError::shape(
            path,
            "`inject` must be a header-name → value object (e.g. { \"Authorization\": \"Bearer $(op read …)\" })",
        )
    })?;
    if map.is_empty() {
        return Err(CompileError::shape(
            path,
            "`inject` must name at least one header to set",
        ));
    }
    let mut out = Vec::with_capacity(map.len());
    for (header, raw) in map {
        let p = child(path, header);
        validate_header_name(header, &p)?;
        let raw_str = as_str(raw, &p)?;
        let resolved = resolve_credential_value(raw_str, ctx, &p)?;
        if resolved.is_empty() {
            return Err(CompileError::shape(
                &p,
                "an inject header value must not be empty",
            ));
        }
        // CRLF / NUL guard: a resolved value carrying a line break would smuggle a header
        // (request splitting) into the reconstructed request. Reject at compile time — the
        // proxy's serializer trusts values to be single-line.
        if resolved.bytes().any(|b| matches!(b, b'\r' | b'\n' | b'\0')) {
            return Err(CompileError::shape(
                &p,
                "an inject header value must not contain a newline or NUL (request-splitting guard)",
            ));
        }
        out.push(HeaderInject {
            header: header.clone(),
            value: Secret::new(resolved),
        });
    }
    Ok(out)
}

/// Resolve a credential value's `$(…)` through the trusted gate, mirroring env values.
fn resolve_credential_value(
    raw: &str,
    ctx: &CompileCtx,
    path: &str,
) -> Result<String, CompileError> {
    if resolve::has_substitution(raw) {
        if !ctx.trusted {
            return Err(CompileError::untrusted_substitution(path));
        }
        resolve::resolve_with(raw, ctx.runner.as_ref())
            .map_err(|e| CompileError::substitution(path, &e))
    } else if resolve::has_open_substitution(raw) {
        Err(CompileError::substitution(
            path,
            resolve::UNTERMINATED_SUBST_MSG,
        ))
    } else {
        Ok(raw.to_string())
    }
}

/// Conservative HTTP header-name validation (RFC 7230 token subset): ASCII
/// alphanumerics plus `-`/`_`. Fail-closed on anything exotic so a crafted name can't
/// smuggle a CRLF or a `:` into the reconstructed request line.
fn validate_header_name(name: &str, path: &str) -> Result<(), CompileError> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(CompileError::shape(
            path,
            &format!("`{name}` is not a valid HTTP header name (letters, digits, `-`, `_`)"),
        ));
    }
    Ok(())
}

/// Classify a net target as a CIDR (contains `/` and parses as one) or a host
/// pattern, and push the rule.
fn push_net_rule(
    target: &str,
    effect: Effect,
    path: &str,
    out: &mut Vec<NetRule>,
) -> Result<(), CompileError> {
    // Brace alternation is not part of the host OR CIDR grammar (only a bare `*` /
    // leading `*.` wildcard) — reject a `{`/`}` the same class as the mid-host glob
    // (D11): the matcher would treat it as a literal host and silently match nothing,
    // so a `!{evil,bad}.com` deny would be inert. Checked BEFORE the CIDR split so a
    // brace CIDR-lookalike (`{a,b}.com/24`) gets the brace message, not a CIDR one.
    if target.contains(['{', '}']) {
        return Err(CompileError::shape(
            path,
            &format!(
                "`{target}` is not a valid host pattern — brace alternation `{{a,b}}` is not supported; list hosts separately (a wildcard is only a bare `*` or a leading `*.` subdomain)"
            ),
        ));
    }
    // The symbolic private-range opt-in (`<private>`, alias `<local>`): the ONLY way to
    // re-permit the RFC1918 / IPv6-ULA ranges the egress proxy blocks by default. Matched
    // BEFORE the CIDR/host split so the angle-bracket token is not mistaken for a literal
    // host (which would silently match nothing).
    if target == "<private>" || target == "<local>" {
        out.push(NetRule {
            target: NetTarget::Private,
            effect,
        });
        return Ok(());
    }
    // Reject an unknown angle-bracket token loudly — `<...>` is not a legal hostname
    // char, so a `<privat>` typo must error, not silently fold to a literal host that
    // matches nothing (which would fail-OPEN a private-range deny the author intended).
    if target.starts_with('<') || target.ends_with('>') {
        return Err(CompileError::shape(
            path,
            &format!(
                "`{target}` is not a recognized net target — the only symbolic net target is `<private>` (alias `<local>`), which re-permits the RFC1918 / IPv6-ULA private ranges"
            ),
        ));
    }
    let net_target = if target.contains('/') {
        match target.parse::<ipnet::IpNet>() {
            Ok(net) => NetTarget::Cidr(net),
            Err(e) => {
                return Err(CompileError::shape(
                    path,
                    &format!("`{target}` looks like a CIDR but did not parse: {e}"),
                ));
            }
        }
    } else {
        // D11: validate the SURFACE form before the trailing-dot strip — only a
        // bare `*` or a leading `*.suffix` wildcard is honored by the matcher; a
        // mid-host glob would silently match nothing. Validating pre-strip also
        // keeps a degenerate `*.`/`*..` from collapsing to a bare `*` allow-all.
        if !crate::matcher::host::host_pattern_is_valid(target) {
            return Err(CompileError::shape(
                path,
                &format!(
                    "`{target}` is not a valid host pattern — a `*` is only allowed as a bare `*` or a leading `*.` subdomain wildcard (e.g. `*.example.com`), not mid-host"
                ),
            ));
        }
        // D12: normalize a single FQDN trailing dot away so `example.com.` and
        // `example.com` are the same rule in the IR.
        NetTarget::Host(crate::matcher::host::strip_trailing_dot(target).to_string())
    };
    out.push(NetRule {
        target: net_target,
        effect,
    });
    Ok(())
}

// ── env ──────────────────────────────────────────────────────────────────────

/// Fold the `env` axis into an [`EnvPolicy`], building the actual child env map.
/// Base is default-DENY (env is constructed, not inherited): a key survives only
/// if the LAST matching entry allows it. `true` passes the whole ambient env;
/// `false` strips everything.
pub fn fold_env(
    value: &Value,
    ctx: &CompileCtx,
    path: &str,
    parent: Option<&EnvPolicy>,
) -> Result<EnvPolicy, CompileError> {
    // An explicit env axis always enforces (constructs the child env exactly).
    let mut policy = EnvPolicy {
        resolved: true,
        enforce: true,
        ..Default::default()
    };
    match value {
        Value::Bool(true) => {
            policy.constructed = ctx.ambient_env.clone();
            return Ok(policy);
        }
        Value::Bool(false) => {
            // Explicit strip-all — same floor as an unlisted axis: withhold all
            // user/ambient env but inject the minimal OS-startup essentials so the
            // child spawns reliably. Single source of truth with `floor_env`.
            return Ok(defaults::strip_all_env(&ctx.ambient_env));
        }
        Value::Array(items) => {
            let entries = parse_env_array(items, parent, path)?;
            construct_env(&entries, ctx, parent, &mut policy)?;
        }
        Value::Object(map) => {
            let entries = parse_env_object(map, ctx, parent, path)?;
            construct_env(&entries, ctx, parent, &mut policy)?;
        }
        _ => {
            return Err(CompileError::shape(
                path,
                "env must be a boolean, an array, or a pattern-keyed object",
            ));
        }
    }
    Ok(policy)
}

/// One parsed env entry, in surface order.
struct EnvEntry {
    /// The key or glob key the entry governs.
    pattern: String,
    action: EnvAction,
    sensitive: bool,
    optional: bool,
    format: Option<EnvFormat>,
    /// How `pattern` matches an ambient key. User patterns are case-sensitive
    /// globs; the built-in secret defaults are case-insensitive (glob or
    /// boundary-token) so an uppercase `MY_TOKEN` cannot slip past them.
    key_match: KeyMatch,
    /// A compiler-spliced default entry (the `"..."` curated baseline / inherited
    /// keys / secret denies), NOT user-authored: excluded from the emitted
    /// `schema` (which carries user validation + redaction marks only).
    builtin: bool,
}

/// How an [`EnvEntry`]'s pattern is matched against an ambient env key.
#[derive(Clone, Copy)]
enum KeyMatch {
    /// A user-authored glob/exact key. Matched OS-mirrored (D16): case-SENSITIVE
    /// on POSIX (env names are), case-INSENSITIVE on Windows (env names are one
    /// var regardless of case by OS contract — a `PATH` rule must catch an ambient
    /// `Path`). Toggled by [`ENV_KEYS_CASE_INSENSITIVE`].
    User,
    /// A built-in secret-KEY guard (`AWS_*`, `NPM_TOKEN`), matched as a
    /// case-INsensitive glob.
    SecretGlob,
    /// A built-in unambiguous secret token (`token`, `credential`), matched
    /// case-insensitively as a SUBSTRING (via `defaults::word_in_substr`).
    SecretSubstr,
    /// A built-in short/ambiguous secret token (`pat`, `pwd`, `auth`), matched
    /// case-insensitively as a whole SEGMENT (via `defaults::word_is_segment`).
    SecretSegment,
    /// The built-in curated baseline (the env `"..."` payload at the OUTERMOST
    /// scope): matches a key iff `defaults::baseline_allows` admits it. One such
    /// allow entry reproduces `sandbox: true`'s curated env exactly.
    CuratedBaseline,
    /// Cross-scope inheritance (the env `"..."` payload at an INNER scope):
    /// matches a key iff it is in the resolved parent's constructed env.
    InheritedKeys,
}

enum EnvAction {
    /// Pass the ambient value through; validate against the type if present.
    Allow(Option<EnvType>),
    /// Construct the key out of the child env.
    Deny,
    /// A literal value (object `value:` or a resolved `$(…)`) — set directly,
    /// independent of the ambient env.
    Literal(String),
}

fn parse_env_array(
    items: &[Value],
    parent: Option<&EnvPolicy>,
    path: &str,
) -> Result<Vec<EnvEntry>, CompileError> {
    let mut out = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let p = child(path, &i.to_string());
        let s = as_str(item, &p)?;
        if s == "!..." {
            return Err(CompileError::shape(&p, SENTINEL_NEGATE_MSG));
        }
        if s == "..." {
            splice_env_inherit(parent, &mut out);
            continue;
        }
        let (pattern, deny) = match s.strip_prefix('!') {
            Some(rest) => (rest.to_string(), true),
            None => (s.to_string(), false),
        };
        reject_env_key_braces(&pattern, &p)?;
        // A `$(…)` in array form would have no key to bind to — array entries are
        // key/glob selectors, not values. Reject to avoid silent misuse.
        if resolve::has_substitution(&pattern) {
            return Err(CompileError::shape(
                &p,
                "`$(…)` is only valid as an object-form env value, not an array entry",
            ));
        }
        out.push(EnvEntry {
            pattern,
            action: if deny {
                EnvAction::Deny
            } else {
                EnvAction::Allow(None)
            },
            sensitive: !deny, // an allow defaults sensitive; a deny mark is irrelevant
            // The array form is a concise ALLOWLIST (pass-through-if-present),
            // never a required-var declaration — an exact key here means "permit
            // it", not "demand it" (required/optional is an object-form concept
            // via the `?` suffix). So array entries are always optional; without
            // this the canonical `["FOO", "BAR", "!*_TOKEN"]` would hard-error
            // whenever FOO is unset. Object plain-keys stay required.
            optional: true,
            format: None,
            key_match: KeyMatch::User,
            builtin: false,
        });
    }
    Ok(out)
}

fn parse_env_object(
    map: &serde_json::Map<String, Value>,
    ctx: &CompileCtx,
    parent: Option<&EnvPolicy>,
    path: &str,
) -> Result<Vec<EnvEntry>, CompileError> {
    let mut out = Vec::new();
    for (raw_key, val) in map {
        let p = child(path, raw_key);
        if raw_key == "!..." {
            return Err(CompileError::shape(&p, SENTINEL_NEGATE_MSG));
        }
        // `"..."` as an env-object key inherits the enclosing scope's env keys at
        // this position (positional last-match). `true` = inherit; a string is a
        // file-extends (frontend-resolved — deferred here, as elsewhere).
        if raw_key == "..." {
            match val {
                Value::Bool(true) => {
                    splice_env_inherit(parent, &mut out);
                    continue;
                }
                // Only a path-like string is a (frontend-deferred) file-ref; a bare
                // scalar (`{"...": "port"}`) is a malformed sentinel value, not a
                // file to resolve — reject it with the same message as every axis.
                Value::String(reference) if super::is_file_ref_value(reference) => {
                    return Err(CompileError::FileRefUnresolved {
                        path: p,
                        reference: reference.clone(),
                    });
                }
                other => {
                    return Err(CompileError::shape(&p, &super::sentinel_value_error(other)));
                }
            }
        }
        // A trailing `?` on the key marks it optional; a glob key is inherently
        // optional (D9 — a glob matches however many keys, zero included), so it is
        // never a required-var declaration and reports optional in the schema.
        let (key, optional) = match raw_key.strip_suffix('?') {
            Some(k) => (k.to_string(), true),
            None => (raw_key.clone(), false),
        };
        reject_env_key_braces(&key, &p)?;
        let optional = optional || is_glob(&key);
        let entry = parse_env_object_value(key, optional, val, ctx, &p)?;
        out.push(entry);
    }
    Ok(out)
}

fn parse_env_object_value(
    key: String,
    optional: bool,
    val: &Value,
    ctx: &CompileCtx,
    path: &str,
) -> Result<EnvEntry, CompileError> {
    match val {
        Value::Bool(true) => Ok(EnvEntry {
            pattern: key,
            action: EnvAction::Allow(None),
            sensitive: true,
            optional,
            format: None,
            key_match: KeyMatch::User,
            builtin: false,
        }),
        Value::Bool(false) => Ok(EnvEntry {
            pattern: key,
            action: EnvAction::Deny,
            sensitive: true,
            optional,
            format: None,
            key_match: KeyMatch::User,
            builtin: false,
        }),
        Value::String(s) => parse_env_string_value(key, optional, s, ctx, path),
        Value::Object(extras) => parse_env_extras(key, optional, extras, ctx, path),
        _ => Err(CompileError::shape(
            path,
            "env value must be a boolean, a type string, \"$(…)\", or an object",
        )),
    }
}

fn parse_env_string_value(
    key: String,
    optional: bool,
    s: &str,
    ctx: &CompileCtx,
    path: &str,
) -> Result<EnvEntry, CompileError> {
    // `$(…)` resolver — trusted homes only.
    if resolve::has_substitution(s) {
        // Reject a glob-key literal BEFORE running the command (a glob key has no
        // single value to bind; without this the exec fires, then construct_env
        // rejects it — a wasted, surprising side effect).
        if is_glob(&key) {
            return Err(CompileError::shape(
                path,
                "`$(…)` cannot be bound to a glob key",
            ));
        }
        if !ctx.trusted {
            return Err(CompileError::untrusted_substitution(path));
        }
        let resolved = resolve::resolve_with(s, ctx.runner.as_ref())
            .map_err(|e| CompileError::substitution(path, &e))?;
        return Ok(EnvEntry {
            pattern: key,
            action: EnvAction::Literal(resolved),
            sensitive: true,
            optional,
            format: None,
            key_match: KeyMatch::User,
            builtin: false,
        });
    }
    // Otherwise a type from the grammar. A string that fails to parse as a type yet
    // carries a `$(` opener is an unterminated substitution (never a valid type) — a
    // valid `/regex/` or `'union'` parses cleanly first, so this never mis-flags one,
    // and an unterminated `$(op read 'x'` / `/$(x` gets the substitution-shaped error
    // rather than a confusing "unknown env type" (D18).
    let ty = match parse_env_type(s) {
        Ok(ty) => ty,
        Err(e) => {
            return Err(if resolve::has_open_substitution(s) {
                CompileError::substitution(path, resolve::UNTERMINATED_SUBST_MSG)
            } else {
                CompileError::shape(path, &e)
            });
        }
    };
    let format = ty.format();
    Ok(EnvEntry {
        pattern: key,
        action: EnvAction::Allow(Some(ty)),
        sensitive: true,
        optional,
        format,
        key_match: KeyMatch::User,
        builtin: false,
    })
}

/// The object extras form: `{ sensitive, format, value, optional }`.
fn parse_env_extras(
    key: String,
    optional_from_key: bool,
    extras: &serde_json::Map<String, Value>,
    ctx: &CompileCtx,
    path: &str,
) -> Result<EnvEntry, CompileError> {
    const ALLOWED: &[&str] = &["sensitive", "format", "value", "optional"];
    for k in extras.keys() {
        if !ALLOWED.contains(&k.as_str()) {
            return Err(CompileError::shape(
                &child(path, k),
                &format!("unknown env option `{k}` (allowed: {})", ALLOWED.join(", ")),
            ));
        }
    }
    // Single `sensitive` mark (D17), default-on; `sensitive: false` opts out of
    // redaction. Collapses the old `secret`/`public` pair.
    let sensitive = extras
        .get("sensitive")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let optional = optional_from_key
        || extras
            .get("optional")
            .and_then(Value::as_bool)
            .unwrap_or(false);
    let ty = match extras.get("format") {
        Some(Value::String(f)) => {
            Some(parse_env_type(f).map_err(|e| CompileError::shape(&child(path, "format"), &e))?)
        }
        Some(_) => {
            return Err(CompileError::shape(
                &child(path, "format"),
                "format must be a string",
            ));
        }
        None => None,
    };
    let format = ty.as_ref().and_then(EnvType::format);
    // An explicit `value:` (optionally `$(…)`) overrides the ambient source.
    if let Some(v) = extras.get("value") {
        // A literal value has no single key to bind to under a glob — reject
        // before any `$(…)` runs.
        if is_glob(&key) {
            return Err(CompileError::shape(
                &child(path, "value"),
                "a literal `value` cannot be bound to a glob key",
            ));
        }
        let raw = as_str(v, &child(path, "value"))?;
        let resolved = if resolve::has_substitution(raw) {
            if !ctx.trusted {
                return Err(CompileError::untrusted_substitution(&child(path, "value")));
            }
            resolve::resolve_with(raw, ctx.runner.as_ref())
                .map_err(|e| CompileError::substitution(&child(path, "value"), &e))?
        } else if resolve::has_open_substitution(raw) {
            // An unterminated `$(` — do NOT pass it through as a literal value
            // (silently shipping shell-looking text is the footgun); name it.
            return Err(CompileError::substitution(
                &child(path, "value"),
                resolve::UNTERMINATED_SUBST_MSG,
            ));
        } else {
            raw.to_string()
        };
        if let Some(t) = &ty {
            t.validate(&resolved)
                .map_err(|e| CompileError::validation(&child(path, "value"), &e))?;
        }
        return Ok(EnvEntry {
            pattern: key,
            action: EnvAction::Literal(resolved),
            sensitive,
            optional,
            format,
            key_match: KeyMatch::User,
            builtin: false,
        });
    }
    Ok(EnvEntry {
        pattern: key,
        action: EnvAction::Allow(ty),
        sensitive,
        optional,
        format,
        key_match: KeyMatch::User,
        builtin: false,
    })
}

/// The env `"..."` payload: inherit the enclosing scope's env at this position.
/// At an INNER scope (`parent = Some`) splice one `InheritedKeys` allow so the
/// child inherits exactly the resolved parent's keys (already secret-filtered by
/// the parent). At the OUTERMOST scope (`parent = None`) splice the built-in
/// curated baseline — the degenerate outermost case, ≡ `sandbox: true`'s env.
fn splice_env_inherit(parent: Option<&EnvPolicy>, out: &mut Vec<EnvEntry>) {
    match parent {
        Some(_) => out.push(EnvEntry {
            pattern: "...".to_string(),
            action: EnvAction::Allow(None),
            sensitive: false,
            optional: true,
            format: None,
            key_match: KeyMatch::InheritedKeys,
            builtin: true,
        }),
        None => splice_env_defaults(out),
    }
}

/// The built-in env base (outermost `"..."`): the secret DENIES followed by the
/// curated-baseline ALLOW. Ordered so the baseline allow is LAST — its verdict is
/// authoritative for baseline keys (so a bare `["..."]` ≡ the curated baseline,
/// i.e. `sandbox: true`'s env), while the secret denies bind only when a LATER
/// user entry re-broadens (e.g. `["*", "..."]`, which allows all then re-strips
/// secrets). All are `builtin` → excluded from the emitted user schema.
fn splice_env_defaults(out: &mut Vec<EnvEntry>) {
    let secret_deny = |pattern: String, key_match: KeyMatch| EnvEntry {
        pattern,
        action: EnvAction::Deny,
        sensitive: true,
        optional: false,
        format: None,
        key_match,
        builtin: true,
    };
    for tok in defaults::SECRET_SUBSTR_TOKENS {
        out.push(secret_deny(tok.to_string(), KeyMatch::SecretSubstr));
    }
    for tok in defaults::SECRET_SEGMENT_TOKENS {
        out.push(secret_deny(tok.to_string(), KeyMatch::SecretSegment));
    }
    for key in defaults::SECRET_ENV_KEYS {
        let pat = if key.ends_with('_') {
            format!("{key}*")
        } else {
            key.to_string()
        };
        out.push(secret_deny(pat, KeyMatch::SecretGlob));
    }
    // The curated allowlist as ONE allow entry (matches iff `baseline_allows`),
    // placed LAST so it is the authoritative verdict for the keys it admits.
    out.push(EnvEntry {
        pattern: "...".to_string(),
        action: EnvAction::Allow(None),
        sensitive: false,
        optional: true,
        format: None,
        key_match: KeyMatch::CuratedBaseline,
        builtin: true,
    });
}

/// Build the child env map + schema + withheld list from ordered entries.
/// Source keys are filtered last-match-wins; explicit-value entries are set
/// directly. A required exact key with no source value and no literal errors.
///
/// `parent` (an inner scope's resolved parent env) contributes two things: its
/// keys become candidate SOURCE keys (with the parent's resolved value winning
/// over ambient), and an `InheritedKeys` entry (spliced by `"..."`) admits
/// exactly those keys. At the outermost scope `parent` is `None` and the source
/// is the ambient env verbatim — behavior-identical to the single-term path.
fn construct_env(
    entries: &[EnvEntry],
    ctx: &CompileCtx,
    parent: Option<&EnvPolicy>,
    policy: &mut EnvPolicy,
) -> Result<(), CompileError> {
    // The value source: ambient, overlaid with the resolved parent's keys (parent
    // value wins — it is the already-resolved truth for an inherited key). Owned
    // only when a parent actually contributes keys, else the ambient env verbatim.
    let source_owned;
    let source: &BTreeMap<String, String> = match parent.filter(|p| !p.constructed.is_empty()) {
        Some(p) => {
            let mut m = ctx.ambient_env.clone();
            for (k, v) in &p.constructed {
                m.insert(k.clone(), v.clone());
            }
            source_owned = m;
            &source_owned
        }
        None => &ctx.ambient_env,
    };
    let parent_keys: BTreeSet<String> = parent
        .map(|p| p.constructed.keys().cloned().collect())
        .unwrap_or_default();

    // Compile a matcher per entry, honoring its `key_match`: user patterns are
    // case-sensitive globs, the built-in defaults case-insensitive / predicate.
    let matchers: Vec<KeyMatcher> = entries
        .iter()
        .map(|e| compile_key_matcher(e, &parent_keys))
        .collect();

    // 1. Literal-value entries: set directly + validate + schema. (Exact keys
    //    only; a glob key has no single value to bind.)
    for e in entries {
        if let EnvAction::Literal(v) = &e.action {
            if is_glob(&e.pattern) {
                return Err(CompileError::shape(
                    &e.pattern,
                    "a literal env value cannot be bound to a glob key",
                ));
            }
            policy.constructed.insert(e.pattern.clone(), v.clone());
        }
    }

    // 2. Source keys: last-match-wins over allow/deny entries.
    for (name, value) in source {
        if policy.constructed.contains_key(name) {
            continue; // a literal already claimed this key
        }
        let mut verdict: Option<&EnvEntry> = None;
        for (e, m) in entries.iter().zip(&matchers) {
            if m.hit(name) {
                verdict = Some(e);
            }
        }
        match verdict.map(|e| &e.action) {
            Some(EnvAction::Allow(ty)) => {
                if let Some(t) = ty {
                    t.validate(value)
                        .map_err(|err| CompileError::validation(name, &err))?;
                }
                policy.constructed.insert(name.clone(), value.clone());
            }
            _ => {
                // Deny, no match, or a literal (handled above) → withhold.
            }
        }
    }

    // 3. Required-key check: an exact-key Allow entry that is not optional, has no
    //    literal, and matched no source value → missing required var.
    for e in entries {
        if e.optional || is_glob(&e.pattern) {
            continue;
        }
        // Case-mirrored (D16): on Windows a `PATH` requirement is satisfied by an
        // ambient `Path` — `constructed` is keyed by the source casing, so an
        // exact-string lookup would false-miss. Match how the key matcher matched.
        let satisfied = policy.constructed.keys().any(|k| env_key_eq(k, &e.pattern));
        if matches!(e.action, EnvAction::Allow(_)) && !satisfied {
            return Err(CompileError::missing_required(&e.pattern));
        }
    }

    // 4. Schema (one rule per non-deny, non-builtin entry) + withheld (source
    //    minus kept). Builtin baseline/inherited/secret entries carry no user
    //    validation or redaction mark, so they never enter the schema.
    let mut seen = BTreeSet::new();
    for e in entries {
        if e.builtin || matches!(e.action, EnvAction::Deny) {
            continue;
        }
        if seen.insert(e.pattern.clone()) {
            policy.schema.push(EnvRule {
                key: e.pattern.clone(),
                sensitive: e.sensitive,
                format: e.format,
                optional: e.optional,
            });
        }
    }
    policy.withheld = source
        .keys()
        .filter(|k| !policy.constructed.contains_key(*k))
        .cloned()
        .collect();
    Ok(())
}

fn is_glob(s: &str) -> bool {
    s.contains(['*', '?', '[', '{'])
}

/// Reject brace alternation in an env-var-NAME pattern. Env keys are a NARROWER
/// grammar than fs globs — `*` prefix/suffix names one variable family — so a
/// `{`/`}` is rejected the same class as the mid-host net glob (D11): fail loud on
/// the typo rather than silently expand. (fs globs DO support braces; env keys and
/// net hosts do not.)
fn reject_env_key_braces(key: &str, path: &str) -> Result<(), CompileError> {
    if key.contains(['{', '}']) {
        return Err(CompileError::shape(
            path,
            &format!(
                "`{key}` is not a valid env key — brace alternation `{{a,b}}` is not supported in env-var-name patterns; list the keys separately or use a `*` wildcard"
            ),
        ));
    }
    Ok(())
}

/// Env var NAMES are case-insensitive on Windows (OS contract: `PATH`/`Path`/
/// `path` are one var) and case-sensitive on POSIX. The user env-key matcher
/// mirrors that (D16) so a `PATH` allow/deny catches an ambient `Path` on Windows
/// but stays exact on unix. Compile-gated like the fs-matcher `CASE_INSENSITIVE`:
/// env is folded on the host it runs on, so host OS == target OS. (Env is
/// Windows-only insensitive, unlike fs which is also macOS-insensitive.)
const ENV_KEYS_CASE_INSENSITIVE: bool = cfg!(windows);

/// Compare two env keys honoring the OS case rule ([`ENV_KEYS_CASE_INSENSITIVE`]).
fn env_key_eq(a: &str, b: &str) -> bool {
    if ENV_KEYS_CASE_INSENSITIVE {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

/// A compiled env-key matcher — the runtime form of an entry's [`KeyMatch`].
enum KeyMatcher {
    /// A compiled glob (user OS-mirrored, or a secret-KEY case-insensitive).
    Glob(GlobMatcher),
    /// Exact fallback when a pattern fails to compile as a glob; `bool` carries the
    /// same case-insensitivity the glob would have (OS-mirrored user / secret-KEY).
    Exact(String, bool),
    /// A secret token matched as a case-insensitive substring.
    SecretSubstr(String),
    /// A secret token matched as a case-insensitive whole segment.
    SecretSegment(String),
    /// The curated-baseline predicate (`defaults::baseline_allows`).
    Baseline,
    /// Cross-scope inheritance: the key is in the resolved parent's env.
    InheritedKeys(BTreeSet<String>),
}

impl KeyMatcher {
    fn hit(&self, name: &str) -> bool {
        match self {
            KeyMatcher::Glob(m) => m.is_match(name),
            KeyMatcher::Exact(s, ci) => {
                if *ci {
                    s.eq_ignore_ascii_case(name)
                } else {
                    s == name
                }
            }
            KeyMatcher::SecretSubstr(word) => defaults::word_in_substr(word, name),
            KeyMatcher::SecretSegment(word) => defaults::word_is_segment(word, name),
            KeyMatcher::Baseline => defaults::baseline_allows(name),
            KeyMatcher::InheritedKeys(keys) => keys.contains(name),
        }
    }
}

/// Compile an entry's pattern into a [`KeyMatcher`] per its [`KeyMatch`] kind.
/// `parent_keys` is the resolved parent's env key set (empty at the outermost
/// scope), the match set for an `InheritedKeys` entry.
fn compile_key_matcher(e: &EnvEntry, parent_keys: &BTreeSet<String>) -> KeyMatcher {
    match e.key_match {
        KeyMatch::SecretSubstr => KeyMatcher::SecretSubstr(e.pattern.clone()),
        KeyMatch::SecretSegment => KeyMatcher::SecretSegment(e.pattern.clone()),
        KeyMatch::CuratedBaseline => KeyMatcher::Baseline,
        KeyMatch::InheritedKeys => KeyMatcher::InheritedKeys(parent_keys.clone()),
        KeyMatch::User | KeyMatch::SecretGlob => {
            // SecretGlob is always case-insensitive; a user key mirrors the OS.
            let case_insensitive =
                matches!(e.key_match, KeyMatch::SecretGlob) || ENV_KEYS_CASE_INSENSITIVE;
            GlobBuilder::new(&e.pattern)
                .case_insensitive(case_insensitive)
                .build()
                .map(|g| KeyMatcher::Glob(g.compile_matcher()))
                .unwrap_or_else(|_| KeyMatcher::Exact(e.pattern.clone(), case_insensitive))
        }
    }
}

// ── shared helpers ────────────────────────────────────────────────────────────

/// Ensure the map has no keys beyond `allowed`; used by callers folding an
/// axis-bearing object. Exposed for the pipeline's granular-object validation.
pub fn reject_unknown_keys(
    map: &serde_json::Map<String, Value>,
    allowed: &[&str],
    path: &str,
) -> Result<(), CompileError> {
    for k in map.keys() {
        if !allowed.contains(&k.as_str()) {
            return Err(CompileError::shape(
                &child(path, k),
                &format!("unknown key `{k}` (allowed: {})", allowed.join(", ")),
            ));
        }
    }
    Ok(())
}

fn as_str<'a>(v: &'a Value, path: &str) -> Result<&'a str, CompileError> {
    v.as_str()
        .ok_or_else(|| CompileError::shape(path, "expected a string"))
}

fn child(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_string()
    } else {
        format!("{path}.{key}")
    }
}
