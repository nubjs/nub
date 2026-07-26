//! Per-axis fold: an axis surface value (`false | true | array | object`) →
//! its resolved IR fragment. The `"..."` spread and last-match-wins ORDER are
//! discharged here into a flat ordered list; the actual last-match decision is
//! made at evaluation time by the matcher, so the fold only has to preserve
//! order and splice the defaults at the sentinel's position.

use super::defaults;
use super::env_grammar::{EnvType, parse_env_type};
use super::resolve;
use super::{CompileCtx, CompileError, ScopeCapabilities};
use crate::matcher::path::expand_symbolic;
use crate::policy::{
    CanonGlob, CredentialBroker, Effect, EnvFormat, EnvPolicy, EnvRule, FsAccess, FsPolicy, FsRule,
    FsRuleSet, NetPolicy, NetRule, NetTarget, TmpMode,
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
    // Throwaway-tmp mode. `$tmp` (and any `$tmp/subpath`) is a SENTINEL for the specially-
    // provisioned per-run PRIVATE dir — a subpath maps INTO that dir, never the shared system
    // tmp — so its value is a plain fs permission: a truthy grant (`"r"`/`"rw"`/`true`) →
    // `Private` (fresh per-run dir, shared tmp hidden); `false` → `Deny` (no tmp). The backend
    // owns the private-dir creation + whole-subtree grant + shared-tmp denial at spawn time (the
    // per-run path is not knowable at compile time), so a `$tmp`-prefixed entry sets only the
    // MODE and emits no ordinary fs rule. Shared system tmp is a SEPARATE literal path, reached
    // only by granting `/tmp` — never via this sentinel; `Shared` (the default when `$tmp` is
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

/// A `$tmp` sentinel malformed by a suffix that is neither empty nor a path separator
/// (`$tmp*`, `$tmp.bak`). Rejected loud rather than folded: `expand_symbolic` would otherwise
/// root it at the SHARED host tmp, the exact leak the sentinel exists to prevent — so any
/// `$tmp`-named form that is not the bare sentinel or a `$tmp/subpath` is an error. (`$tmpx`
/// is a DIFFERENT `$name`, not this — it is caught as an unrecognized sentinel instead.)
const MALFORMED_TMP_MSG: &str = "malformed `$tmp` sentinel — use `$tmp` (a fresh per-run private tmp dir) or `$tmp/subpath` (a path inside it); `$tmp` followed by anything else is not a path into the shared system tmp — grant the literal `/tmp` for that";

/// Classify a trimmed key/entry against the `$tmp` sentinel. Identifier-boundary aware
/// (via [`split_fs_sentinel`]) so `$tmpx` is the `$name` `tmpx` (NotTmp — the unrecognized-
/// sentinel path rejects it), while `$tmp` / `$tmp{/,\}subpath` is the sentinel and any other
/// remainder after the `tmp` name (`$tmp*`, `$tmp.bak`) is malformed — the leak guard.
enum TmpKey {
    Sentinel,
    Malformed,
    NotTmp,
}
fn classify_tmp_key(k: &str) -> TmpKey {
    match crate::matcher::path::split_fs_sentinel(k) {
        Some(("tmp", rest)) if rest.is_empty() || rest.starts_with(['/', '\\']) => TmpKey::Sentinel,
        Some(("tmp", _)) => TmpKey::Malformed,
        _ => TmpKey::NotTmp,
    }
}

/// Fold a `$tmp`-prefixed key into a tmp MODE. `$tmp` (and any `$tmp/subpath`) denotes the
/// per-run PRIVATE dir — a subpath maps INTO that dir, never the shared system tmp — so the
/// value is a plain fs permission on it: a truthy grant (`"r"`/`"rw"`/`true`) → `Private`
/// (provision the fresh dir + grant it rw); `false` → `Deny` (no tmp). Read-only on a fresh
/// empty dir is degenerate, so `"r"` is treated as `"rw"` — `Private` always grants rw. The
/// whole private subtree is backend-granted, so a `$tmp/x` key needs no path rule of its own;
/// the caller consumes a `Some` and emits nothing. `None` for a normal (non-`$tmp`) key; a
/// malformed `$tmp` suffix is a hard error (it would otherwise leak into the shared host tmp).
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
                "`$tmp` takes an fs permission: \"r\"/\"rw\"/`true` (a fresh per-run private tmp dir, shared system tmp hidden) or `false` (no tmp) — for the shared system tmp, grant the literal path `/tmp`",
            ));
        }
    };
    Ok(Some(mode))
}

/// Array-form `$tmp` sentinel → tmp MODE. A `$tmp` / `$tmp/subpath` entry is `Private`
/// (array grants are rw); a `!`-negated one is `Deny`. `None` for a normal entry; a malformed
/// `$tmp` suffix errors, same as the object [`parse_tmp_mode`], so the two agree on the class.
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

/// Inject the default `.env*` READ-deny as an UNCONDITIONAL floor — the highest-precedence
/// rule on the fs axis, which no directory grant, glob, or exact path can reopen (sandbox.mdx
/// "`.env` files are always blocked"). `.env*` files hold the exact secrets the sandbox scrubs,
/// so reading them is denied by default on any read-granting fs policy — including the OBJECT
/// form, which never spliced the `"..."` secret set. The rule composes with the last-match-wins
/// fs algebra as two trailing DENY bands appended after all user + default entries (backends
/// stay pure IR replicators — every one evaluates last-match-wins over these entries):
///   band 1  — the folded user + default entries, unchanged;
///   band 2a — the `.env*` LEAF deny (`**/.env*`, `.env*`), so it beats every band-1
///             broad/glob/exact allow (the `["...", "./"]` footgun where a trailing dir-allow
///             re-exposed `<proj>/.env`, AND a `{ "./.env": "r" }` exact allow, are BOTH closed);
///   band 2c — the `.env*` SUBTREE deny (`**/.env*/**`, `.env*/**`), so a `.env*`-NAMED
///             DIRECTORY's CONTENTS are denied too.
/// The two bands are always the LAST entries in that fixed order — the Linux backend's
/// `builtin_env_band_start`/`is_builtin_env_glob` recognize them positionally, so this
/// emission and that matcher are COUPLED and must change together.
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
    set.entries.extend(defaults::env_deny_leaf_rules()); // band 2a: leaf deny
    set.entries.extend(defaults::env_deny_subtree_rules()); // band 2c: subtree deny (LAST)
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
    reject_unknown_fs_sentinel(raw, path)?;
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

/// Reject a leading `$name` that is not a recognized filesystem sentinel (per the v2
/// grammar: "An unrecognized `$name` is an error"). `$( … )` command substitution is
/// recognized BEFORE `$name` — the paren disambiguation — so a leading `$(` returns Ok
/// and is handled by the substitution branches. Validated on the RAW pattern's leading
/// token so an unrecognized sentinel is rejected even when a `$(…)` also appears later
/// (`$foo/$(cmd)`). `$tmp` never reaches here (the fold consumes it as a mode first),
/// leaving `$cache` as the sole recognized sentinel that flows through.
fn reject_unknown_fs_sentinel(raw: &str, path: &str) -> Result<(), CompileError> {
    let p = raw.trim_start();
    // P0-F1: the pre-v2 angle-bracket fs sentinels (`<tmp>`/`<cache>`/`<home>`) were
    // renamed to `$tmp`/`$cache`/`~`. A leading `<…>` is not a valid fs path, and left
    // unrejected it degrades SILENTLY to an inert literal rule — `{"fs":{"<tmp>":"rw"}}`
    // then leaves `tmp_mode = Shared` (not the private per-run dir), so a broad read
    // re-exposes the host tmp. Fail loud like `$data` does, with a migration hint. (net's
    // `<private>`/`<local>` are a separate axis handled in `push_net_rule`; this is fs-only.)
    if p.starts_with('<') {
        return Err(CompileError::shape(path, &deprecated_angle_sentinel_msg(p)));
    }
    if p.starts_with("$(") || !p.starts_with('$') {
        return Ok(());
    }
    if let Some((name, _)) = crate::matcher::path::split_fs_sentinel(p) {
        if crate::matcher::path::FS_SENTINEL_NAMES.contains(&name) {
            return Ok(());
        }
        return Err(CompileError::shape(
            path,
            &format!(
                "unrecognized `${name}` filesystem sentinel — the built-in names are `$cache` and `$tmp`; use `$( … )` for command substitution"
            ),
        ));
    }
    Err(CompileError::shape(
        path,
        "a bare `$` is not a valid filesystem path — the built-in sentinels are `$cache` and `$tmp`, and `$( … )` is command substitution",
    ))
}

/// Migration message for a removed `<…>` angle-bracket fs sentinel (P0-F1). The three
/// renamed forms (`<tmp>`/`<cache>`/`<home>`, alone or with a `/subpath`) get a targeted
/// `→ $tmp`/`$cache`/`~` hint; any other `<…>` is rejected generically (the whole
/// angle-bracket syntax is gone).
fn deprecated_angle_sentinel_msg(p: &str) -> String {
    for (old, new) in [("<tmp>", "$tmp"), ("<cache>", "$cache"), ("<home>", "~")] {
        if p == old
            || p.strip_prefix(old)
                .is_some_and(|rest| rest.starts_with('/'))
        {
            return format!(
                "`{p}` — the `<…>` filesystem sentinel syntax was removed; use `{new}` instead of `{old}`"
            );
        }
    }
    format!(
        "`{p}` is not a valid filesystem path — the `<…>` sentinel syntax was removed; the built-in roots are `$tmp`, `$cache`, and `~`"
    )
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
    caps: ScopeCapabilities,
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
                fold_net_entry(s, parent, &p, &mut policy)?;
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
                fold_net_object_value(key, val, ctx, caps, &p, &mut policy)?;
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
    policy: &mut NetPolicy,
) -> Result<(), CompileError> {
    if s == "!..." {
        return Err(CompileError::shape(path, SENTINEL_NEGATE_MSG));
    }
    if s == "..." {
        // Inner scope: inherit the resolved parent's rules. Outermost (no parent):
        // the built-in net base is deny-all with no committed allowlist (the
        // build-jail baseline owns trusted-host allows), so splice nothing.
        if let Some(p) = parent {
            policy.rules.extend(p.rules.iter().cloned());
            policy.brokers.extend(p.brokers.iter().cloned());
        }
        return Ok(());
    }
    let (pattern, effect) = match s.strip_prefix('!') {
        Some(rest) => (rest, Effect::Deny),
        None => (s, Effect::Allow),
    };
    push_net_rule(pattern, effect, path, &mut policy.rules)
}

/// One entry of the net OBJECT form: `"<host>": true | false | { rule object }`. A
/// bool is a plain allow/deny (host-only, connection-level). A rule object with
/// `env` enables credential brokering for an exact hostname, implicitly allowing
/// that host and forcing the TlsInspect tier.
fn fold_net_object_value(
    host: &str,
    val: &Value,
    ctx: &CompileCtx,
    caps: ScopeCapabilities,
    path: &str,
    policy: &mut NetPolicy,
) -> Result<(), CompileError> {
    match val {
        Value::Bool(true) => push_net_rule(host, Effect::Allow, path, &mut policy.rules),
        Value::Bool(false) => push_net_rule(host, Effect::Deny, path, &mut policy.rules),
        Value::Object(rule) => fold_net_rule_object(host, rule, ctx, caps, path, policy),
        _ => Err(CompileError::shape(
            path,
            "net host value must be true, false, or a credential rule object (e.g. { \"env\": [\"TOKEN\"] })",
        )),
    }
}

/// Parse an exact-host credential rule. The brokered host is also an ordinary
/// Allow rule; a later explicit deny can still override it under last-match-wins.
fn fold_net_rule_object(
    host: &str,
    rule: &serde_json::Map<String, Value>,
    _ctx: &CompileCtx,
    caps: ScopeCapabilities,
    path: &str,
    policy: &mut NetPolicy,
) -> Result<(), CompileError> {
    for key in rule.keys() {
        if key == "inject" {
            return Err(CompileError::shape(
                &child(path, key),
                "the old `inject` credential syntax is not supported; name parent environment variables with `{ \"env\": [\"TOKEN\"] }`",
            ));
        }
        if key != "env" {
            return Err(CompileError::shape(
                &child(path, key),
                &format!("unknown credential rule option `{key}` (allowed: env)"),
            ));
        }
    }
    // Gated on THIS scope's capability, not a compile-wide trust flag.
    if !caps.credential_broker {
        return Err(CompileError::shape(
            path,
            "credential brokering (`env`) is a trusted-only capability — it is not permitted in an untrusted (dependenciesMeta) grant",
        ));
    }
    let env = rule.get("env").ok_or_else(|| {
        CompileError::shape(
            path,
            "a credential rule object must contain `env` (for example `{ \"env\": [\"STRIPE_TOKEN\"] }`)",
        )
    })?;
    validate_broker_host(host, path)?;
    let env = parse_broker_env(env, &child(path, "env"))?;
    push_net_rule(host, Effect::Allow, path, &mut policy.rules)?;
    policy.brokers.push(CredentialBroker {
        host: crate::matcher::host::strip_trailing_dot(host).to_string(),
        env,
    });
    Ok(())
}

/// Credential release is intentionally narrower than ordinary net matching: one
/// literal DNS hostname only. A wildcard, IP literal, CIDR, or symbolic class would
/// let one marker authorize more than one exact upstream boundary.
fn validate_broker_host(pattern: &str, path: &str) -> Result<(), CompileError> {
    let host = crate::matcher::host::strip_trailing_dot(pattern);
    if pattern.contains('/')
        || pattern.contains('*')
        || pattern.starts_with('<')
        || host.parse::<std::net::IpAddr>().is_ok()
        || crate::policy::broker_host_is_legacy_ipv4_literal(host)
    {
        return Err(CompileError::shape(
            path,
            "credential brokering requires one exact literal hostname; wildcards, IP literals, CIDRs, and symbolic host classes are not allowed",
        ));
    }
    if host.is_empty()
        || !crate::matcher::host::host_pattern_is_valid(pattern)
        || host.starts_with('.')
        || host.ends_with('.')
        || host.split('.').any(|label| {
            label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
        || host.len() > 253
    {
        return Err(CompileError::shape(
            path,
            &format!("`{pattern}` is not a valid literal hostname for a credential broker"),
        ));
    }
    Ok(())
}

fn parse_broker_env(value: &Value, path: &str) -> Result<Vec<String>, CompileError> {
    let items = value.as_array().ok_or_else(|| {
        CompileError::shape(
            path,
            "`env` must be a non-empty array of exact environment-variable names",
        )
    })?;
    if items.is_empty() {
        return Err(CompileError::shape(
            path,
            "`env` must name at least one environment variable",
        ));
    }
    let mut out = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let p = child(path, &index.to_string());
        let name = as_str(item, &p)?;
        if name.is_empty()
            || name.contains(['=', '\0', '*', '?', '[', ']', '{', '}'])
            || name.ends_with('?')
        {
            return Err(CompileError::shape(
                &p,
                "a brokered environment variable must be one exact, non-empty name without glob or optional-key syntax",
            ));
        }
        if crate::policy::credential_env_name_is_reserved(name) {
            return Err(CompileError::shape(
                &p,
                &format!("`{name}` is owned by sandbox runtime plumbing and cannot be brokered"),
            ));
        }
        if out
            .iter()
            .any(|existing: &String| broker_env_key_eq(existing, name))
        {
            return Err(CompileError::shape(
                &p,
                &format!("duplicate brokered environment variable `{name}`"),
            ));
        }
        out.push(name.to_string());
    }
    Ok(out)
}

fn broker_env_key_eq(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
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

// ── env (the `vars` + `secrets` axes) ──────────────────────────────────────────

/// Fold the `vars` + `secrets` axes into ONE [`EnvPolicy`], building the child env
/// map. The two axes are the SAME environment mechanism split by sensitivity: a
/// `vars` entry marks `sensitive:false`, a `secrets` entry `sensitive:true`. Base is
/// default-DENY (env is constructed, not inherited): a key survives only if the LAST
/// matching entry allows it. Both surfaces flow into one ordered `[EnvEntry]` —
/// vars FIRST, then secrets — so under last-match-wins a name in BOTH axes takes the
/// later secrets rule (`sensitive:true`, fail-safe toward redaction). At least one
/// axis is present (the caller floors/inherits when both are absent).
pub fn fold_env_axes(
    vars: Option<&Value>,
    secrets: Option<&Value>,
    ctx: &CompileCtx,
    caps: ScopeCapabilities,
    parent: Option<&EnvPolicy>,
) -> Result<EnvPolicy, CompileError> {
    // An explicit env-family axis always enforces (constructs the child env exactly).
    let mut policy = EnvPolicy {
        resolved: true,
        enforce: true,
        ..Default::default()
    };
    let mut entries = Vec::new();
    if let Some(v) = vars {
        entries.extend(parse_env_surface(v, false, "vars", ctx, caps, parent)?);
    }
    if let Some(v) = secrets {
        entries.extend(parse_env_surface(v, true, "secrets", ctx, caps, parent)?);
    }
    construct_env(&entries, ctx, parent, &mut policy)?;
    // An allowlist can legitimately omit every ambient key, but a Windows child still
    // needs the small bootstrap set for process/AppContainer startup. POSIX receives
    // no additions because its essential set is empty. This also floors a `vars: []`
    // / `false` (no entries) to the strip-all posture (OS essentials only), matching
    // the complete-statement floor and `strip_all_env`.
    defaults::add_os_essential_env(&mut policy, &ctx.ambient_env);
    Ok(policy)
}

/// Parse one env-family axis surface into ordered [`EnvEntry`]s. `default_sensitive`
/// both marks the entries (`vars`→false, `secrets`→true) AND selects the axis's
/// accepted shapes: `vars` takes `"*"`/`true` (pass every ambient var), `[]`/globs, or
/// an object; `secrets` takes only an array/object/`false` — it must NAME each secret,
/// so a catch-all `"*"`/`true` is a shape error (redacting the whole environment is
/// never the intent). Converting `"*"`/`true` into a real `"*"` Allow entry (rather
/// than short-circuiting) is what lets the two axes compose under one `construct_env`.
fn parse_env_surface(
    value: &Value,
    default_sensitive: bool,
    path: &str,
    ctx: &CompileCtx,
    caps: ScopeCapabilities,
    parent: Option<&EnvPolicy>,
) -> Result<Vec<EnvEntry>, CompileError> {
    let is_secrets = default_sensitive;
    match value {
        // `vars: "*"` / `vars: true` → one catch-all Allow passing every ambient key.
        Value::String(s) if s == "*" && !is_secrets => Ok(vec![env_catch_all(default_sensitive)]),
        Value::Bool(true) if !is_secrets => Ok(vec![env_catch_all(default_sensitive)]),
        // A `secrets` string/`true`, or a non-`"*"` `vars` string, is a shape error.
        Value::String(_) | Value::Bool(true) => Err(CompileError::shape(
            path,
            if is_secrets {
                "`secrets` must be an array, an object, or `false` — it must name each secret; a catch-all `\"*\"`/`true` is not allowed (use `vars` for non-secret pass-through)"
            } else {
                "the only string `vars` accepts is `\"*\"` (pass every ambient variable) — use an array or object to select variables"
            },
        )),
        // Explicit strip: no entries. construct_env withholds everything and
        // add_os_essential_env re-adds only the OS-startup essentials.
        Value::Bool(false) => Ok(Vec::new()),
        Value::Array(items) => parse_env_array(items, parent, path, default_sensitive),
        Value::Object(map) => parse_env_object(map, ctx, caps, parent, path, default_sensitive),
        _ => Err(CompileError::shape(
            path,
            &format!("{path} must be a boolean, an array, or a pattern-keyed object"),
        )),
    }
}

/// The `"*"` / back-compat `true` catch-all: one Allow entry passing every ambient
/// key. Optional (a catch-all never demands a specific var); `sensitive` per axis.
fn env_catch_all(sensitive: bool) -> EnvEntry {
    EnvEntry {
        pattern: "*".to_string(),
        action: EnvAction::Allow(None),
        sensitive,
        optional: true,
        format: None,
        key_match: KeyMatch::User,
        builtin: false,
    }
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
    default_sensitive: bool,
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
            // The axis decides sensitivity (vars→false, secrets→true); a deny mark
            // is irrelevant (denies never enter the schema).
            sensitive: default_sensitive,
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
    caps: ScopeCapabilities,
    parent: Option<&EnvPolicy>,
    path: &str,
    default_sensitive: bool,
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
        let entry = parse_env_object_value(key, optional, val, ctx, caps, &p, default_sensitive)?;
        out.push(entry);
    }
    Ok(out)
}

fn parse_env_object_value(
    key: String,
    optional: bool,
    val: &Value,
    ctx: &CompileCtx,
    caps: ScopeCapabilities,
    path: &str,
    default_sensitive: bool,
) -> Result<EnvEntry, CompileError> {
    match val {
        Value::Bool(true) => Ok(EnvEntry {
            pattern: key,
            action: EnvAction::Allow(None),
            sensitive: default_sensitive,
            optional,
            format: None,
            key_match: KeyMatch::User,
            builtin: false,
        }),
        Value::Bool(false) => Ok(EnvEntry {
            pattern: key,
            action: EnvAction::Deny,
            sensitive: default_sensitive,
            optional,
            format: None,
            key_match: KeyMatch::User,
            builtin: false,
        }),
        Value::String(s) => {
            parse_env_string_value(key, optional, s, ctx, caps, path, default_sensitive)
        }
        Value::Object(extras) => {
            parse_env_extras(key, optional, extras, ctx, caps, path, default_sensitive)
        }
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
    caps: ScopeCapabilities,
    path: &str,
    default_sensitive: bool,
) -> Result<EnvEntry, CompileError> {
    // `$(…)` resolver — the `env_substitution` capability only.
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
        if !caps.env_substitution {
            return Err(CompileError::untrusted_substitution(path));
        }
        let resolved = resolve::resolve_with(s, ctx.runner.as_ref())
            .map_err(|e| CompileError::substitution(path, &e))?;
        return Ok(EnvEntry {
            pattern: key,
            action: EnvAction::Literal(resolved),
            sensitive: default_sensitive,
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
        sensitive: default_sensitive,
        optional,
        format,
        key_match: KeyMatch::User,
        builtin: false,
    })
}

/// The object extras form: `{ format, value, optional }`. Sensitivity is NOT an
/// extras key — it is decided by the axis the entry came from (`vars`→false,
/// `secrets`→true), threaded in as `default_sensitive`.
fn parse_env_extras(
    key: String,
    optional_from_key: bool,
    extras: &serde_json::Map<String, Value>,
    ctx: &CompileCtx,
    caps: ScopeCapabilities,
    path: &str,
    default_sensitive: bool,
) -> Result<EnvEntry, CompileError> {
    const ALLOWED: &[&str] = &["format", "value", "optional"];
    for k in extras.keys() {
        // `brokerTo` is the Phase-2 secrets-brokering key; it is NOT accepted yet.
        // Point at the current net-axis broker form rather than a generic
        // unknown-option error. (Phase 2 relocates brokering onto the secrets axis.)
        if k == "brokerTo" {
            return Err(CompileError::shape(
                &child(path, k),
                "`brokerTo` is not supported yet — broker a credential on the net axis: `net: { \"<host>\": { \"env\": [\"NAME\"] } }`",
            ));
        }
        if !ALLOWED.contains(&k.as_str()) {
            return Err(CompileError::shape(
                &child(path, k),
                &format!("unknown env option `{k}` (allowed: {})", ALLOWED.join(", ")),
            ));
        }
    }
    // Sensitivity is set by the axis, not an extras key.
    let sensitive = default_sensitive;
    let optional = optional_from_key
        || match extras.get("optional") {
            Some(Value::Bool(value)) => *value,
            Some(_) => {
                return Err(CompileError::shape(
                    &child(path, "optional"),
                    "optional must be a boolean",
                ));
            }
            None => false,
        };
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
            if !caps.env_substitution {
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
                defaults::insert_env(&mut m, k.clone(), v.clone());
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
            defaults::insert_env(&mut policy.constructed, e.pattern.clone(), v.clone());
        }
    }

    // 2. Source keys: last-match-wins over allow/deny entries.
    for (name, value) in source {
        if defaults::env_contains_key(&policy.constructed, name) {
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
                defaults::insert_env(&mut policy.constructed, name.clone(), value.clone());
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
        let satisfied = defaults::env_contains_key(&policy.constructed, &e.pattern);
        if matches!(e.action, EnvAction::Allow(_)) && !satisfied {
            return Err(CompileError::missing_required(&e.pattern));
        }
    }

    // 4. Schema (one rule per non-deny, non-builtin entry) + withheld (source
    //    minus kept). Builtin baseline/inherited/secret entries carry no user
    //    validation or redaction mark, so they never enter the schema. Dedup is
    //    LAST-wins-by-key (upsert in place, first position kept): a name in BOTH
    //    axes (vars entries precede secrets) records the later secrets rule
    //    (`sensitive:true`), consistent with the value's last-match-wins verdict.
    let mut schema_index: BTreeMap<String, usize> = BTreeMap::new();
    for e in entries {
        if e.builtin || matches!(e.action, EnvAction::Deny) {
            continue;
        }
        let rule = EnvRule {
            key: e.pattern.clone(),
            sensitive: e.sensitive,
            format: e.format,
            optional: e.optional,
        };
        match schema_index.get(&e.pattern) {
            Some(&i) => policy.schema[i] = rule,
            None => {
                schema_index.insert(e.pattern.clone(), policy.schema.len());
                policy.schema.push(rule);
            }
        }
    }
    policy.withheld = source
        .keys()
        .filter(|k| !defaults::env_contains_key(&policy.constructed, k))
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
