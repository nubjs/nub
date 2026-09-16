//! Per-axis fold: an axis surface value (`false | true | array | object`) →
//! its resolved IR fragment. A `...:#/pointer` reuse token (see [`reuse`]) — an array
//! entry OR an object key — and last-match-wins ORDER are discharged here into a flat
//! ordered list; the actual last-match decision is made at evaluation time by the
//! matcher, so the fold only has to preserve order and splice a reused node's contents at
//! the token's position.

use super::builtin_sets;
use super::defaults;
use super::env_grammar::{EnvType, parse_env_type};
use super::resolve;
use super::reuse;
use super::{CompileCtx, CompileError, ScopeCapabilities};
use crate::matcher::path::expand_symbolic;
use crate::policy::{
    CanonGlob, CredentialBroker, Effect, EnvFormat, EnvPolicy, EnvRule, FsAccess, FsOrigin,
    FsPolicy, FsRule, FsRuleSet, NetPolicy, NetRule, NetTarget, SelfProcFile, TmpMode,
};
use globset::{GlobBuilder, GlobMatcher};
use serde_json::Value;
use std::cell::OnceCell;
use std::collections::BTreeMap;

/// Naked `...` / `!...` inheritance was REMOVED in v2 (a policy is a complete
/// statement). Any surviving `...`/`!...` — array entry OR object key, on any axis — is
/// a HARD shape error carrying the migration hint, never silently folded to a literal
/// path/host named `...`. To reuse another list, reference it with a `...:#/pointer`
/// array entry (see [`super::reuse`]).
const NAKED_SENTINEL_MSG: &str = "`...` inheritance was removed — a policy is a complete statement; to reuse a list, reference it with `...:#/pointer` (e.g. [\"...:#/shared/fs\", …])";
/// `!...:#/pointer` (negated reuse) is rejected (OQ1) in BOTH container forms — array
/// entry and object key: a spliced list/object carries its own allow/deny entries, so
/// negating the whole splice is ill-defined; reuse it, then deny specific members after.
const NEGATED_REUSE_MSG: &str = "`!...:#/pointer` (negated reuse) is not supported — reuse with `...:#/pointer`, then deny specific members after the splice (`!host` array entries / `false` object values)";
/// An empty / whitespace-only fs entry used to expand to `**` (a silent whole-fs
/// grant, fail-OPEN); it is now a hard shape error (D3).
const EMPTY_FS_ENTRY_MSG: &str = "an empty fs entry is not allowed (it would grant the whole filesystem) — name a path or remove it";
const USER_FS_DENY_MSG: &str =
    "filesystem deny entries are not part of the public policy grammar — omit the path to deny it";
/// `$tooldirs` is a SET (the built-in tool-cache dirs), not a directory root, so it
/// takes no `/subpath` — a `$tooldirs/x` would silently name nothing. Fail loud.
const TOOLDIRS_SUBPATH_MSG: &str = "`$tooldirs` is a built-in set (the package-manager / toolchain cache dirs) and takes no subpath — use it bare (`$tooldirs`) or with a permission (`{ \"$tooldirs\": \"r\" }`)";
/// `$trusted` is the NET host set; as a `net` OBJECT key it has no bool value to carry
/// (a set expands to many rules), so it is only valid in the array form.
const TRUSTED_OBJECT_MSG: &str = "`$trusted` is a built-in host set — use it in the `net` array form (e.g. [\"$trusted\", …]), not as an object key";
/// Same rule as [`TRUSTED_OBJECT_MSG`] for the other net set.
const DOWNLOADS_OBJECT_MSG: &str = "`$downloads` is a built-in host set — use it in the `net` array form (e.g. [\"$downloads\", …]), not as an object key";

// ── fs ───────────────────────────────────────────────────────────────────────

struct FsFoldCtx<'a> {
    compile: &'a CompileCtx,
    env: &'a EnvPolicy,
    tool_env: OnceCell<BTreeMap<String, String>>,
}

impl FsFoldCtx<'_> {
    fn tool_env(&self) -> &BTreeMap<String, String> {
        self.tool_env.get_or_init(|| {
            // Supplied locations remain available without inheriting their variables.
            // Resolved child values override them without rerunning substitutions.
            let mut env = self.compile.ambient_env.clone();
            for (key, value) in &self.env.constructed {
                defaults::insert_env(&mut env, key.clone(), value.clone());
            }
            env
        })
    }
}

/// Fold the `fs` axis value into an [`FsPolicy`]. Array entries and object keys
/// are subtree-expanded (a bare path grants the node + `/**`); a glob-bearing
/// pattern is emitted verbatim. Access: array grants are ReadWrite (the concise
/// "these paths are fully usable" form); object values pick `"r"`/`"rw"`. A
/// `...:#/pointer` array entry splices another list's raw entries at its position, and
/// (uniformly) a `...:#/pointer` OBJECT KEY splices another object's entries (see
/// [`reuse`]); there is no implicit inheritance (naked `...` was removed).
pub fn fold_fs(
    value: &Value,
    ctx: &CompileCtx,
    env: &EnvPolicy,
    path: &str,
) -> Result<FsPolicy, CompileError> {
    let ctx = &FsFoldCtx {
        compile: ctx,
        env,
        tool_env: OnceCell::new(),
    };
    let mut set = FsRuleSet {
        entries: Vec::new(),
        default_effect: Effect::Deny,
    };
    // `$tmp` selects session-owned private storage, not a path known at compilation.
    // It accepts rw/true or false; read-only access and suffixes are rejected because
    // the backend grants the whole private subtree. Shared (when the token is absent)
    // leaves the host temp location subject to the ordinary filesystem rules.
    let mut tmp = TmpMode::Shared;
    match value {
        // `true` fully relaxes the axis; `false` fully denies it.
        Value::Bool(true) => set.default_effect = Effect::Allow,
        Value::Bool(false) => set.default_effect = Effect::Deny,
        Value::Array(items) => {
            // The reuse resolution stack (cycle detection) — seeded empty per axis fold.
            let mut stack = Vec::new();
            for (i, item) in items.iter().enumerate() {
                let p = child(path, &i.to_string());
                let s = as_str(item, &p)?;
                fold_fs_array_item(s, ctx, &p, &mut set.entries, &mut tmp, &mut stack)?;
            }
        }
        Value::Object(map) => {
            // The reuse resolution stack (cycle detection) — seeded empty per axis fold,
            // as the array branch does; an object-key `...:#/pointer` spread threads it.
            let mut stack = Vec::new();
            for (key, val) in map {
                let p = child(path, key);
                fold_fs_object_item(key, val, ctx, &p, &mut set.entries, &mut tmp, &mut stack)?;
            }
        }
        _ => {
            return Err(CompileError::shape(
                path,
                "fs must be a boolean, an array, or a pattern-keyed object",
            ));
        }
    }
    let mut self_proc = std::collections::BTreeSet::new();
    for rule in &set.entries {
        if let Some(file) = SelfProcFile::from_path(rule.matcher.as_str()) {
            if rule.effect != Effect::Allow || rule.access != FsAccess::Read {
                return Err(CompileError::shape(
                    path,
                    "self-process metadata accepts only read-only grants (\"r\")",
                ));
            }
            self_proc.insert(file);
        }
    }
    set.entries
        .retain(|rule| SelfProcFile::from_path(rule.matcher.as_str()).is_none());
    // The only two non-positive bands on the fs axis. Everything above is a literal grant;
    // these subtract, in this order, because a policy file is never named `.env*` so the two are
    // disjoint and the fixed order just keeps the emission stable for the tests that pin it.
    //
    // AFTER the self-process extraction above, not before. `fs: {"/proc/self/maps": "r"}` folds
    // to a `self_proc` capability and NO fs rule, so running the floor first would see an allow
    // that is about to be removed, append nine denies to a policy that ends up granting no files
    // at all, and — because a deny is what arms the broker — trap every open for a policy with
    // no filesystem grants to enforce.
    finalize_policy_file_deny(&mut set, ctx.compile);
    finalize_env_deny(&mut set);
    Ok(FsPolicy {
        rules: set,
        tmp,
        self_proc,
    })
}

/// `$tmp` is managed session storage. Suffixes do not define separate grants.
const MALFORMED_TMP_MSG: &str = "`$tmp` is managed session storage and takes no suffix — use bare `$tmp`; grant a literal path for a specific shared-temp location";

/// Classify a trimmed key/entry against the `$tmp` sentinel. Identifier-boundary aware
/// (via [`split_fs_sentinel`]) so `$tmpx` remains the different `$name` `tmpx`; only bare
/// `$tmp` is the sentinel and every suffix is malformed rather than silently ignored.
enum TmpKey {
    Sentinel,
    Malformed,
    NotTmp,
}
fn classify_tmp_key(k: &str) -> TmpKey {
    match crate::matcher::path::split_fs_sentinel(k) {
        Some(("tmp", "")) => TmpKey::Sentinel,
        Some(("tmp", _)) => TmpKey::Malformed,
        _ => TmpKey::NotTmp,
    }
}

/// Fold a bare `$tmp` key into a tmp MODE. `"rw"`/`true` provisions a fresh private dir and
/// `false` disables it. Read-only is invalid because a fresh scratch directory is writable;
/// the backend grants the whole managed subtree, so the caller emits no path rule.
fn parse_tmp_mode(key: &str, val: &Value, path: &str) -> Result<Option<TmpMode>, CompileError> {
    match classify_tmp_key(key.trim()) {
        TmpKey::NotTmp => return Ok(None),
        TmpKey::Malformed => return Err(CompileError::shape(path, MALFORMED_TMP_MSG)),
        TmpKey::Sentinel => {}
    }
    let mode = match val {
        Value::Bool(true) => TmpMode::Private,
        Value::Bool(false) => TmpMode::Deny,
        Value::String(s) if s == "rw" => TmpMode::Private,
        _ => {
            return Err(CompileError::shape(
                path,
                "`$tmp` takes only \"rw\"/`true` (private session storage) or `false` (no tmp); read-only cannot provide writable scratch storage",
            ));
        }
    };
    Ok(Some(mode))
}

/// Array-form bare `$tmp` selects private mode. User deny spelling and every suffix reject;
/// object-form `false` is the explicit no-tmp form.
fn parse_tmp_mode_array(entry: &str, path: &str) -> Result<Option<TmpMode>, CompileError> {
    let (body, deny) = match entry.trim().strip_prefix('!') {
        Some(rest) => (rest.trim_start(), true),
        None => (entry.trim(), false),
    };
    if deny && !matches!(classify_tmp_key(body), TmpKey::NotTmp) {
        return Err(CompileError::shape(path, USER_FS_DENY_MSG));
    }
    match classify_tmp_key(body) {
        TmpKey::NotTmp => Ok(None),
        TmpKey::Malformed => Err(CompileError::shape(path, MALFORMED_TMP_MSG)),
        TmpKey::Sentinel => Ok(Some(TmpMode::Private)),
    }
}

/// Classify a trimmed key/entry against the `$tooldirs` SET sentinel, identifier-boundary
/// aware (via [`crate::matcher::path::split_fs_sentinel`]) so `$tooldirsx` is a different
/// `$name` (NotTooldirs — the unrecognized-sentinel path rejects it), the bare `$tooldirs`
/// is the set, and any remainder (`$tooldirs/x`, `$tooldirs.bak`) is a subpath error — a
/// set names no path of its own.
enum ToolsKey {
    Set,
    WithSubpath,
    NotTooldirs,
}
fn classify_tooldirs_key(k: &str) -> ToolsKey {
    match crate::matcher::path::split_fs_sentinel(k) {
        Some(("tooldirs", "")) => ToolsKey::Set,
        Some(("tooldirs", _)) => ToolsKey::WithSubpath,
        _ => ToolsKey::NotTooldirs,
    }
}

/// Array-form `$tooldirs` set → its fs rules, emitted IN-PLACE (last-match order
/// preserved). A bare `$tooldirs` grants ReadWrite (array grants are rw, like a bare
/// path). Returns `Ok(true)` when the entry was a `$tooldirs` set (consumed),
/// `Ok(false)` for a normal entry the caller then folds itself.
fn fold_tooldirs_array_entry(
    entry: &str,
    ctx: &FsFoldCtx<'_>,
    path: &str,
    out: &mut Vec<FsRule>,
) -> Result<bool, CompileError> {
    let (body, deny) = match entry.trim().strip_prefix('!') {
        Some(rest) => (rest.trim_start(), true),
        None => (entry.trim(), false),
    };
    match classify_tooldirs_key(body) {
        ToolsKey::NotTooldirs => Ok(false),
        ToolsKey::WithSubpath => Err(CompileError::shape(path, TOOLDIRS_SUBPATH_MSG)),
        ToolsKey::Set => {
            if deny {
                return Err(CompileError::shape(path, USER_FS_DENY_MSG));
            }
            out.extend(
                builtin_sets::tooldirs_fs_rules_with_env(
                    &ctx.compile.homes,
                    ctx.tool_env(),
                    Effect::Allow,
                    FsAccess::ReadWrite,
                )
                .map_err(|message| CompileError::shape(path, &message))?,
            );
            #[cfg(target_os = "linux")]
            out.extend(builtin_sets::tool_metadata_rules());
            Ok(true)
        }
    }
}

/// Object-form `$tooldirs` set → its fs rules. The value uses the SAME access ladder as
/// an ordinary path key ([`parse_fs_object_access`]): `"r"`→Read and `"rw"`/`true`→ReadWrite.
/// User `false` deny grammar is rejected. Returns `Ok(true)` when consumed.
fn fold_tooldirs_object_entry(
    key: &str,
    val: &Value,
    ctx: &FsFoldCtx<'_>,
    path: &str,
    out: &mut Vec<FsRule>,
) -> Result<bool, CompileError> {
    match classify_tooldirs_key(key.trim()) {
        ToolsKey::NotTooldirs => return Ok(false),
        ToolsKey::WithSubpath => return Err(CompileError::shape(path, TOOLDIRS_SUBPATH_MSG)),
        ToolsKey::Set => {}
    }
    let (effect, access) = parse_fs_object_access(val, path)?;
    if effect == Effect::Deny {
        return Err(CompileError::shape(path, USER_FS_DENY_MSG));
    }
    out.extend(
        builtin_sets::tooldirs_fs_rules_with_env(
            &ctx.compile.homes,
            ctx.tool_env(),
            effect,
            access,
        )
        .map_err(|message| CompileError::shape(path, &message))?,
    );
    #[cfg(target_os = "linux")]
    out.extend(builtin_sets::tool_metadata_rules());
    Ok(true)
}

/// Append the `.env*` / `.npmrc` secret floor as the last two bands of the fs axis.
///
/// WHY THIS EXISTS, because it is the one place the fs axis is not purely positive. The
/// sandbox's contract is generous-read-MINUS-SECRETS: a `nub sandbox` scope that grants the
/// project tree is meant to hand over the source, not the credentials sitting in it. Without
/// this band, `fs: ["."]` reads `./.env` and `./.npmrc`, which is the single most likely way a
/// confined command walks off with a token. Restored 2026-09-16: a refactor narrowed the floor
/// from "every read-granting policy" to "the secure preset only", and deleting the build jail
/// then took the preset — and with it the floor — away entirely, which no decision asked for.
///
/// It is also what makes deny-inside-allow REACHABLE. The public grammar has no deny form
/// (`fold_fs_array_entry` / `fold_fs_object_entry` reject one outright), so these two bands and
/// `finalize_policy_file_deny` below are the only producers of an `Effect::Deny` fs rule in the
/// crate. With no producer, `has_explicit_fs_deny` is permanently false and the supervisor's
/// write broker never arms.
///
/// Skipped in exactly two cases, both of which make the band meaningless rather than unsafe: a
/// FULLY-relaxed axis (`fs: true` / `sandbox: false` — the explicit escape hatch, where the
/// author has asked for no filesystem confinement at all), and a policy that grants no reads,
/// where there is nothing for a deny to sit inside.
fn finalize_env_deny(set: &mut FsRuleSet) {
    if !grants_read(set) {
        return;
    }
    set.entries.extend(defaults::env_deny_leaf_rules());
    set.entries.extend(defaults::env_deny_subtree_rules());
    // The cross-process /proc secret band rides the SAME injection: it is a secret floor the
    // broker carries under a whole-root grant, and it is meaningless under the same two skips
    // (`fs: true` opts out of fs confinement entirely; a no-read policy has nothing to sit inside).
    set.entries.extend(defaults::proc_secret_deny_rules());
}

/// Deny the file(s) the policy was read from, read AND write, so a confined command can
/// neither learn the rules confining it nor edit them for the next run. Exact paths, so unlike
/// the `.env*` band this needs no glob. Same two skips, for the same reasons.
fn finalize_policy_file_deny(set: &mut FsRuleSet, ctx: &CompileCtx) {
    if ctx.policy_files.is_empty() || !grants_read(set) {
        return;
    }
    set.entries.extend(
        ctx.policy_files
            .iter()
            .map(|file| defaults::policy_file_deny_rule(file)),
    );
}

/// Whether a floor band would mean anything on this axis: the policy must grant SOMETHING to
/// read, and must not be the whole-disk escape hatch (an allow base with no entries at all).
fn grants_read(set: &FsRuleSet) -> bool {
    let fully_relaxed = set.default_effect == Effect::Allow && set.entries.is_empty();
    let grants = set.default_effect == Effect::Allow
        || set.entries.iter().any(|rule| rule.effect == Effect::Allow);
    !fully_relaxed && grants
}

/// One entry of the fs Array form — the per-item body, shared by direct entries and
/// each entry of a `...:#/pointer`-spliced list so reuse composes with `$tmp` mode and
/// `$tooldirs`. Reuse is checked FIRST; a naked `...`/`!...` is a migration error; then
/// `$tmp` mode, `$tooldirs`, and the ordinary path. `tmp`/`out` are the OUTER accumulators.
/// A spliced `$tmp` sets the outer mode. `stack` is the reuse resolution stack for cycle detection.
fn fold_fs_array_item(
    s: &str,
    ctx: &FsFoldCtx<'_>,
    path: &str,
    out: &mut Vec<FsRule>,
    tmp: &mut TmpMode,
    stack: &mut Vec<String>,
) -> Result<(), CompileError> {
    match reuse::parse_reuse_token(s) {
        reuse::ReuseToken::Negated => return Err(CompileError::shape(path, NEGATED_REUSE_MSG)),
        reuse::ReuseToken::Pointer(ptr) => {
            let arr = reuse::resolve_reuse_array(ctx.compile, ptr, path, stack)?;
            stack.push(ptr.to_string());
            for (j, item) in arr.iter().enumerate() {
                // A reused entry's diagnostics point at the SOURCE list, not the splice
                // site, so a bad reused list blames the list.
                let cp = format!("{ptr}.{j}");
                fold_fs_array_item(as_str(item, &cp)?, ctx, &cp, out, tmp, stack)?;
            }
            stack.pop();
            return Ok(());
        }
        reuse::ReuseToken::None => {}
    }
    reject_naked_sentinel(s, path)?;
    if let Some(mode) = parse_tmp_mode_array(s, path)? {
        *tmp = mode;
        return Ok(());
    }
    if fold_tooldirs_array_entry(s, ctx, path, out)? {
        return Ok(());
    }
    fold_fs_array_entry(s, ctx.compile, path, out)
}

fn fold_fs_array_entry(
    s: &str,
    ctx: &CompileCtx,
    path: &str,
    out: &mut Vec<FsRule>,
) -> Result<(), CompileError> {
    if s.trim().is_empty() {
        return Err(CompileError::shape(path, EMPTY_FS_ENTRY_MSG));
    }
    let (pattern, effect) = match s.strip_prefix('!') {
        Some(_) => return Err(CompileError::shape(path, USER_FS_DENY_MSG)),
        None => (s, Effect::Allow),
    };
    // `$(…)` resolves AFTER the `!` strip so a command's stdout is a path, never a
    // deny operator it could smuggle in. Array grants are ReadWrite; denies deny both.
    let pattern = resolve_fs_path(pattern, ctx, path)?;
    push_fs_rules(&pattern, effect, FsAccess::ReadWrite, ctx, out);
    Ok(())
}

/// One entry of the fs Object form — the per-entry body, shared by direct entries and
/// each entry of a `...:#/pointer`-spliced OBJECT, so an object-key spread composes with
/// `$tmp` mode and `$tooldirs` (the object twin of [`fold_fs_array_item`]). Reuse is
/// checked FIRST; then `$tmp` mode, `$tooldirs`, and the
/// ordinary path key. `tmp`/`out` are the OUTER accumulators (a spliced `$tmp` sets the
/// outer mode); `stack` is the reuse resolution stack for cycle detection.
fn fold_fs_object_item(
    key: &str,
    val: &Value,
    ctx: &FsFoldCtx<'_>,
    path: &str,
    out: &mut Vec<FsRule>,
    tmp: &mut TmpMode,
    stack: &mut Vec<String>,
) -> Result<(), CompileError> {
    match reuse::parse_reuse_token(key) {
        reuse::ReuseToken::Negated => return Err(CompileError::shape(path, NEGATED_REUSE_MSG)),
        reuse::ReuseToken::Pointer(ptr) => {
            reject_spread_value(val, path)?;
            let obj = reuse::resolve_reuse_object(ctx.compile, ptr, path, stack)?;
            stack.push(ptr.to_string());
            for (k, v) in obj {
                // A spliced entry's diagnostics point at the SOURCE object, not the
                // splice site, so a bad reused object blames the object.
                let cp = format!("{ptr}.{k}");
                fold_fs_object_item(k, v, ctx, &cp, out, tmp, stack)?;
            }
            stack.pop();
            return Ok(());
        }
        reuse::ReuseToken::None => {}
    }
    if let Some(mode) = parse_tmp_mode(key, val, path)? {
        *tmp = mode;
        return Ok(());
    }
    if fold_tooldirs_object_entry(key, val, ctx, path, out)? {
        return Ok(());
    }
    fold_fs_object_entry(key, val, ctx.compile, path, out)
}

fn fold_fs_object_entry(
    key: &str,
    val: &Value,
    ctx: &CompileCtx,
    path: &str,
    out: &mut Vec<FsRule>,
) -> Result<(), CompileError> {
    reject_naked_sentinel(key, path)?;
    if key.trim().is_empty() {
        return Err(CompileError::shape(path, EMPTY_FS_ENTRY_MSG));
    }
    let (effect, access) = parse_fs_object_access(val, path)?;
    if effect == Effect::Deny {
        return Err(CompileError::shape(path, USER_FS_DENY_MSG));
    }
    // Resolve `$(…)` in the path key AFTER validating the access value, so an
    // invalid `val` errors before any command runs (no wasted exec side effect).
    let pattern = resolve_fs_path(key, ctx, path)?;
    push_fs_rules(&pattern, effect, access, ctx, out);
    Ok(())
}

/// The fs object-value access ladder, shared by an ordinary path key and the
/// `$tooldirs` set. `false` is represented briefly so its caller can reject the removed
/// public deny grammar with the same diagnostic as every other path form.
fn parse_fs_object_access(val: &Value, path: &str) -> Result<(Effect, FsAccess), CompileError> {
    match val {
        Value::Bool(true) => Ok((Effect::Allow, FsAccess::ReadWrite)),
        Value::Bool(false) => Ok((Effect::Deny, FsAccess::Read)),
        Value::String(s) => match s.as_str() {
            "r" => Ok((Effect::Allow, FsAccess::Read)),
            "rw" => Ok((Effect::Allow, FsAccess::ReadWrite)),
            other => Err(CompileError::shape(
                path,
                &format!("fs value `{other}` — expected \"r\", \"rw\", true, or false"),
            )),
        },
        _ => Err(CompileError::shape(
            path,
            "fs value must be \"r\", \"rw\", true, or false",
        )),
    }
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
    // Keep the IR canonical for internal callers: a deny has no access mode. Authored
    // filesystem grammar reaches this funnel only with an Allow effect.
    let access = if effect == Effect::Deny {
        FsAccess::DENY
    } else {
        access
    };
    let expanded = expand_symbolic(pattern, &ctx.homes);
    // Keep dynamic process identity out of the ordinary canonical-path ruleset.
    // fold_fs extracts this marker into its resolved metadata capability.
    if SelfProcFile::from_path(&expanded).is_some() {
        out.push(FsRule {
            matcher: CanonGlob(expanded),
            effect,
            access,
            origin: FsOrigin::Authored,
        });
        return;
    }
    for g in defaults::subtree_globs(&expanded) {
        out.push(FsRule {
            matcher: CanonGlob(crate::matcher::canonicalize_glob_prefix(&g)),
            effect,
            access,
            origin: FsOrigin::Authored,
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
    let raw = normalize_home_alias(raw, path)?;
    reject_unknown_fs_sentinel(&raw, path)?;
    if resolve::has_substitution(&raw) {
        let resolved = resolve::resolve_with(&raw, ctx.runner.as_ref())
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
    } else if resolve::has_open_substitution(&raw) {
        // A `$(` with no balanced close — name it rather than ship shell-looking
        // text as a literal path (the same footgun the env path guards against).
        Err(CompileError::substitution(
            path,
            resolve::UNTERMINATED_SUBST_MSG,
        ))
    } else {
        Ok(raw)
    }
}

/// `$home` is the public spelling of the existing `~` anchor. Normalize before
/// `matcher::path` sees the token, keeping that module a pure path expander.
fn normalize_home_alias(raw: &str, path: &str) -> Result<String, CompileError> {
    let trimmed = raw.trim_start();
    let Some(("home", rest)) = crate::matcher::path::split_fs_sentinel(trimmed) else {
        return Ok(raw.to_string());
    };
    if rest.is_empty() || rest.starts_with(['/', '\\']) {
        return Ok(raw.replacen("$home", "~", 1));
    }
    Err(CompileError::shape(
        path,
        "malformed `$home` sentinel — use `$home` or `$home/subpath`",
    ))
}

/// Reject a leading `$name` that is not a recognized filesystem sentinel (per the v2
/// grammar: "An unrecognized `$name` is an error"). `$( … )` command substitution is
/// recognized BEFORE `$name` — the paren disambiguation — so a leading `$(` returns Ok
/// and is handled by the substitution branches. Validated on the RAW pattern's leading
/// token so an unrecognized sentinel is rejected even when a `$(…)` also appears later
/// (`$foo/$(cmd)`). `$tmp` never reaches here and `$home` is normalized to `~`; `$cache`
/// remains the sole `$name` sentinel that reaches the matcher.
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
        // Both net host sets are misuses HERE, not generic unknown fs names — point the
        // author at the right axis.
        if name == "trusted" || name == "downloads" {
            return Err(CompileError::shape(
                path,
                &format!(
                    "`${name}` is a network host set — use it on the `net` axis; the filesystem sentinels are `$home`, `$cache`, and `$tmp`"
                ),
            ));
        }
        return Err(CompileError::shape(
            path,
            &format!(
                "unrecognized `${name}` filesystem sentinel — the built-in names are `$home`, `$cache`, and `$tmp`; use `$( … )` for command substitution"
            ),
        ));
    }
    Err(CompileError::shape(
        path,
        "a bare `$` is not a valid filesystem path — the built-in sentinels are `$home`, `$cache`, and `$tmp`, and `$( … )` is command substitution",
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

/// `sandbox: true`'s positive filesystem default: its project tree is readable,
/// writes remain denied, and a managed private tmp supplies scratch space. The default
/// deny applies everywhere else.
///
/// It carries the same secret floor an authored policy gets, and needs it MORE, not less: this
/// is the posture someone selects by typing `true`, so the project tree it grants is exactly
/// where a committed `.env` lives.
pub(super) fn secure_default_fs(ctx: &CompileCtx) -> FsPolicy {
    let mut set = FsRuleSet {
        entries: Vec::new(),
        default_effect: Effect::Deny,
    };
    set.entries.extend(
        defaults::subtree_globs(&ctx.homes.project.to_string_lossy())
            .into_iter()
            .map(|matcher| FsRule {
                matcher: CanonGlob(matcher),
                effect: Effect::Allow,
                access: FsAccess::Read,
                origin: FsOrigin::Authored,
            }),
    );
    finalize_policy_file_deny(&mut set, ctx);
    finalize_env_deny(&mut set);
    FsPolicy {
        rules: set,
        tmp: TmpMode::Private,
        ..Default::default()
    }
}

// ── net ──────────────────────────────────────────────────────────────────────

/// Fold the `net` axis into a [`NetPolicy`]. Entries are host globs or CIDRs;
/// `!` denies; a `...:#/pointer` array entry splices another list's raw entries at its
/// position, and (uniformly) a `...:#/pointer` OBJECT KEY splices another object's
/// `"<host>": bool` entries (see [`reuse`]). There is no implicit inheritance (naked
/// `...` was removed). `net: true` disables enforcement; `net: false` denies all egress.
pub fn fold_net(value: &Value, ctx: &CompileCtx, path: &str) -> Result<NetPolicy, CompileError> {
    let mut policy = NetPolicy {
        enforce: true,
        default_effect: Effect::Deny,
        ..Default::default()
    };
    match value {
        Value::Bool(true) => policy.enforce = false,
        Value::Bool(false) => {} // enforce, deny-all base, no rules
        Value::Array(items) => {
            // The reuse resolution stack (cycle detection) — seeded empty per axis fold.
            let mut stack = Vec::new();
            for (i, item) in items.iter().enumerate() {
                let p = child(path, &i.to_string());
                let s = as_str(item, &p)?;
                fold_net_array_item(s, ctx, &p, &mut policy, &mut stack)?;
            }
        }
        Value::Object(map) => {
            // The reuse resolution stack (cycle detection) — seeded empty per axis fold.
            let mut stack = Vec::new();
            for (key, val) in map {
                let p = child(path, key);
                fold_net_object_item(key, val, ctx, &p, &mut policy, &mut stack)?;
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

/// One entry of the net Array form — shared by direct entries and each entry of a
/// `...:#/pointer`-spliced list, so a reused net list's hosts / `$trusted` land in
/// `net.rules` at the splice position (before broker validation runs). Reuse first; a
/// naked `...`/`!...` is a migration error; then the ordinary host/CIDR/`$trusted` path.
fn fold_net_array_item(
    s: &str,
    ctx: &CompileCtx,
    path: &str,
    policy: &mut NetPolicy,
    stack: &mut Vec<String>,
) -> Result<(), CompileError> {
    match reuse::parse_reuse_token(s) {
        reuse::ReuseToken::Negated => return Err(CompileError::shape(path, NEGATED_REUSE_MSG)),
        reuse::ReuseToken::Pointer(ptr) => {
            let arr = reuse::resolve_reuse_array(ctx, ptr, path, stack)?;
            stack.push(ptr.to_string());
            for (j, item) in arr.iter().enumerate() {
                let cp = format!("{ptr}.{j}");
                fold_net_array_item(as_str(item, &cp)?, ctx, &cp, policy, stack)?;
            }
            stack.pop();
            return Ok(());
        }
        reuse::ReuseToken::None => {}
    }
    reject_naked_sentinel(s, path)?;
    fold_net_entry(s, path, policy)
}

fn fold_net_entry(s: &str, path: &str, policy: &mut NetPolicy) -> Result<(), CompileError> {
    let (pattern, effect) = match s.strip_prefix('!') {
        Some(rest) => (rest, Effect::Deny),
        None => (s, Effect::Allow),
    };
    // `$trusted` expands the curated trusted-host set IN-PLACE (last-match order
    // preserved), like a reused list's entries. `!$trusted` denies each host
    // (expand-then-negate — grammar-uniform with `!host`, enables `["*", "!$trusted"]`).
    if pattern == "$trusted" {
        policy.rules.extend(builtin_sets::trusted_net_rules(effect));
        return Ok(());
    }
    // `$downloads` is the install-scoped artifact set — same expansion contract as
    // `$trusted`, a deliberately separate membership (see `builtin_sets`).
    if pattern == "$downloads" {
        policy
            .rules
            .extend(builtin_sets::download_net_rules(effect));
        return Ok(());
    }
    push_net_rule(pattern, effect, path, &mut policy.rules)
}

/// One entry of the net Object form — the per-entry body, shared by direct entries and
/// each entry of a `...:#/pointer`-spliced OBJECT (the object twin of
/// [`fold_net_array_item`]). Reuse first; then a naked-`...` migration error, the
/// `$trusted`-as-object rejection, and the ordinary `"<host>": bool` path. `stack` is the
/// reuse resolution stack for cycle detection.
fn fold_net_object_item(
    key: &str,
    val: &Value,
    ctx: &CompileCtx,
    path: &str,
    policy: &mut NetPolicy,
    stack: &mut Vec<String>,
) -> Result<(), CompileError> {
    match reuse::parse_reuse_token(key) {
        reuse::ReuseToken::Negated => return Err(CompileError::shape(path, NEGATED_REUSE_MSG)),
        reuse::ReuseToken::Pointer(ptr) => {
            reject_spread_value(val, path)?;
            let obj = reuse::resolve_reuse_object(ctx, ptr, path, stack)?;
            stack.push(ptr.to_string());
            for (k, v) in obj {
                let cp = format!("{ptr}.{k}");
                fold_net_object_item(k, v, ctx, &cp, policy, stack)?;
            }
            stack.pop();
            return Ok(());
        }
        reuse::ReuseToken::None => {}
    }
    reject_naked_sentinel(key, path)?;
    if key == "$trusted" {
        return Err(CompileError::shape(path, TRUSTED_OBJECT_MSG));
    }
    if key == "$downloads" {
        return Err(CompileError::shape(path, DOWNLOADS_OBJECT_MSG));
    }
    fold_net_object_value(key, val, path, policy)
}

/// One entry of the net OBJECT form: `"<host>": true | false`. A bool is a plain
/// allow/deny (host-only, connection-level). Credential brokering moved to the
/// `secrets` axis in Phase 2 (`secrets.<name>.brokerTo`), so a net host value is
/// bool-only — an object (the old `{ "env": [...] }` broker rule) is a shape error
/// pointing at the new form.
fn fold_net_object_value(
    host: &str,
    val: &Value,
    path: &str,
    policy: &mut NetPolicy,
) -> Result<(), CompileError> {
    match val {
        Value::Bool(true) => push_net_rule(host, Effect::Allow, path, &mut policy.rules),
        Value::Bool(false) => push_net_rule(host, Effect::Deny, path, &mut policy.rules),
        _ => Err(CompileError::shape(
            path,
            "net host value must be true or false — broker a credential on the secrets axis: `secrets: { \"<NAME>\": { \"brokerTo\": [\"<host>\"] } }`",
        )),
    }
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

/// Validate the exact env NAME a secret is brokered under (the `secrets` key). One
/// exact, non-empty name — no glob / optional-key syntax — and not a name owned by
/// sandbox runtime plumbing (a marker written there would be overwritten before spawn
/// while the real secret stayed live in proxy state). Mirrors the per-name checks the
/// old net-axis `env` list ran, now applied to the secret key.
fn validate_broker_env_name(name: &str, path: &str) -> Result<(), CompileError> {
    if name.is_empty()
        || name.contains(['=', '\0', '*', '?', '[', ']', '{', '}'])
        || name.ends_with('?')
    {
        return Err(CompileError::shape(
            path,
            "a brokered secret must be one exact, non-empty name without glob or optional-key syntax",
        ));
    }
    if crate::policy::credential_env_name_is_reserved(name) {
        return Err(CompileError::shape(
            path,
            &format!("`{name}` is owned by sandbox runtime plumbing and cannot be brokered"),
        ));
    }
    Ok(())
}

/// Parse a `secrets.<name>.brokerTo` value: a non-empty array of exact literal
/// hostnames the secret may cross. Each host runs through [`validate_broker_host`]
/// (one exact DNS name — no wildcard / IP / CIDR / symbolic class) and duplicates
/// (trailing-dot- and case-insensitive) are rejected. Hosts are stored as authored;
/// [`transpose_brokers`] trailing-dot-normalizes them onto `net.brokers`.
fn parse_broker_hosts(value: &Value, path: &str) -> Result<Vec<String>, CompileError> {
    let items = value.as_array().ok_or_else(|| {
        CompileError::shape(path, "`brokerTo` must be a non-empty array of hostnames")
    })?;
    if items.is_empty() {
        return Err(CompileError::shape(
            path,
            "`brokerTo` must name at least one host",
        ));
    }
    let mut out: Vec<String> = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let p = child(path, &index.to_string());
        let host = as_str(item, &p)?;
        validate_broker_host(host, &p)?;
        let normalized = crate::matcher::host::strip_trailing_dot(host);
        if out.iter().any(|existing| {
            crate::matcher::host::strip_trailing_dot(existing).eq_ignore_ascii_case(normalized)
        }) {
            return Err(CompileError::shape(
                &p,
                &format!("duplicate brokered host `{host}`"),
            ));
        }
        out.push(host.to_string());
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
    // MANDATORY `$`-guard (fail-open closer): `$trusted` and `$downloads` are the only
    // `$`-tokens net accepts and both are consumed in `fold_net_entry` before reaching
    // here; net has no other `$name` set and no `$(…)` command substitution. A leading `$`
    // that survived is unrecognized — reject it loudly rather than fold it to a
    // `NetTarget::Host` that matches NOTHING (the footgun: `net: ["$tooldirs"]` or
    // `["!$foo"]` would otherwise compile to an inert rule that silently does nothing).
    // Mirrors the `<...>` reject.
    if target.starts_with('$') {
        return Err(CompileError::shape(
            path,
            &format!(
                "`{target}` is not a valid net target — the built-in net sets are `$trusted` (the curated trusted-host allowlist) and `$downloads` (the install-time artifact hosts); `$tooldirs` is a filesystem set (use it on the `fs` axis), and `$( … )` command substitution is not supported on `net`"
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
///
/// Returns the resolved env policy AND the [`BrokerIntent`]s harvested from any
/// `secrets.<name>.brokerTo` entries — the caller ([`super::compile_object`])
/// transposes these onto `net.brokers` (the env axis never sees the net policy).
pub fn fold_env_axes(
    vars: Option<&Value>,
    secrets: Option<&Value>,
    ctx: &CompileCtx,
    caps: ScopeCapabilities,
) -> Result<(EnvPolicy, Vec<BrokerIntent>), CompileError> {
    // An explicit env-family axis always enforces (constructs the child env exactly).
    let mut policy = EnvPolicy {
        resolved: true,
        enforce: true,
        ..Default::default()
    };
    let mut entries = Vec::new();
    if let Some(v) = vars {
        entries.extend(parse_env_surface(v, false, "vars", ctx, caps)?);
    }
    if let Some(v) = secrets {
        entries.extend(parse_env_surface(v, true, "secrets", ctx, caps)?);
    }
    construct_env(&entries, ctx, &mut policy)?;
    // An allowlist can legitimately omit every ambient key, but a Windows child still
    // needs the small bootstrap set for `CreateProcessW` to start it at all. POSIX receives
    // no additions because its essential set is empty. This also floors a `vars: []`
    // / `false` (no entries) to the strip-all posture (OS essentials only), matching
    // the complete-statement floor and `strip_all_env`.
    defaults::add_os_essential_env(&mut policy, &ctx.ambient_env);
    // Harvest broker intents from the ordered entries (secrets carrying `brokerTo`).
    let intents = entries
        .iter()
        .filter(|e| !e.broker_to.is_empty())
        .map(|e| BrokerIntent {
            secret: e.pattern.clone(),
            hosts: e.broker_to.clone(),
        })
        .collect();
    Ok((policy, intents))
}

/// A brokered secret harvested from a `secrets.<name>.brokerTo` entry: the secret's
/// env NAME plus the exact hosts it may cross. Transposed onto `net.brokers` by
/// [`transpose_brokers`] (the IR keeps one broker per host, not per secret).
pub(super) struct BrokerIntent {
    pub secret: String,
    pub hosts: Vec<String>,
}

/// Transpose harvested [`BrokerIntent`]s onto `net.brokers`, COALESCING by host so
/// each host yields exactly ONE [`CredentialBroker`] with a merged env list. This
/// coalescing is load-bearing, not cosmetic: every broker consumer keys a host to one
/// replacement set (the proxy's `broker_for_host` resolves a host via `.find()`, and
/// `validate_apply_inputs` rejects duplicate-host brokers), so two secrets brokered to
/// the same host MUST merge into one broker or all but the first would be silently
/// dropped. Hosts are trailing-dot-normalized to match the IR the old net form emitted.
pub(super) fn transpose_brokers(net: &mut NetPolicy, intents: &[BrokerIntent]) {
    for intent in intents {
        for raw_host in &intent.hosts {
            let host = crate::matcher::host::strip_trailing_dot(raw_host).to_string();
            match net.brokers.iter_mut().find(|b| {
                crate::matcher::host::strip_trailing_dot(&b.host).eq_ignore_ascii_case(&host)
            }) {
                Some(existing) => {
                    if !existing
                        .env
                        .iter()
                        .any(|e| broker_env_key_eq(e, &intent.secret))
                    {
                        existing.env.push(intent.secret.clone());
                    }
                }
                None => net.brokers.push(CredentialBroker {
                    host,
                    env: vec![intent.secret.clone()],
                }),
            }
        }
    }
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
        Value::Array(items) => parse_env_array(items, ctx, path, default_sensitive),
        Value::Object(map) => parse_env_object(map, ctx, caps, path, default_sensitive),
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
        broker_to: Vec::new(),
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
    /// A compiler-spliced default entry (NOT user-authored): excluded from the
    /// emitted `schema` (which carries user validation + redaction marks only). The
    /// producers of `builtin: true` were the `"..."` curated-baseline / inherited-keys
    /// splices, removed in P4; the field + its schema-exclusion seam are retained for
    /// any future compiler-spliced default (all current entries are user-authored).
    builtin: bool,
    /// The brokerTo hosts a `secrets` entry may cross (empty on every other entry).
    /// `fold_env_axes` harvests a non-empty list into a [`BrokerIntent`] and the
    /// caller transposes it onto `net.brokers`; the secret's real value is then
    /// withheld from the child, which receives an opaque marker instead.
    broker_to: Vec<String>,
}

/// How an [`EnvEntry`]'s pattern is matched against an ambient env key. Every entry is
/// now a user-authored key — the built-in secret-KEY / -substr / -segment matchers were
/// produced ONLY by the `"..."` env splice, removed in P4 (`sandbox: true`'s secret
/// filtering lives in `defaults::curated_baseline_env`, not here). Retained as the
/// classifier seam should a future phase re-introduce a compiler-spliced matcher.
#[derive(Clone, Copy)]
enum KeyMatch {
    /// A user-authored glob/exact key. Matched OS-mirrored (D16): case-SENSITIVE
    /// on POSIX (env names are), case-INSENSITIVE on Windows (env names are one
    /// var regardless of case by OS contract — a `PATH` rule must catch an ambient
    /// `Path`). Toggled by [`ENV_KEYS_CASE_INSENSITIVE`].
    User,
}

enum EnvAction {
    /// Pass the ambient value through; validate against the type if present.
    Allow(Option<EnvType>),
    /// Construct the key out of the child env.
    Deny,
    /// A resolved `$(…)` substitution — set directly, independent of the ambient env.
    Literal(String),
}

fn parse_env_array(
    items: &[Value],
    ctx: &CompileCtx,
    path: &str,
    default_sensitive: bool,
) -> Result<Vec<EnvEntry>, CompileError> {
    let mut out = Vec::new();
    // The reuse resolution stack (cycle detection) — seeded empty per axis fold, so a
    // `...:#/pointer` array entry in vars/secrets threads it like fs/net.
    let mut stack = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let p = child(path, &i.to_string());
        let s = as_str(item, &p)?;
        parse_env_array_entry(s, ctx, &p, default_sensitive, &mut out, &mut stack)?;
    }
    Ok(out)
}

/// One entry of an env-family Array form (`vars`/`secrets`), shared by direct entries and
/// each entry of a `...:#/pointer`-spliced LIST — the array twin of the fs/net item folds,
/// completing spread uniformity across every axis and both containers. Reuse is checked
/// FIRST (a spliced list re-folds in place, so last-match composes at the splice); then the
/// ordinary selector path (naked-`...` reject, `!`-deny strip, brace reject, `$(…)` reject).
/// A spliced entry's sensitivity follows the SPLICE SITE's axis (`default_sensitive`), not
/// the source — a list reused under `secrets` yields sensitive entries.
fn parse_env_array_entry(
    s: &str,
    ctx: &CompileCtx,
    path: &str,
    default_sensitive: bool,
    out: &mut Vec<EnvEntry>,
    stack: &mut Vec<String>,
) -> Result<(), CompileError> {
    match reuse::parse_reuse_token(s) {
        reuse::ReuseToken::Negated => return Err(CompileError::shape(path, NEGATED_REUSE_MSG)),
        reuse::ReuseToken::Pointer(ptr) => {
            let arr = reuse::resolve_reuse_array(ctx, ptr, path, stack)?;
            stack.push(ptr.to_string());
            for (j, item) in arr.iter().enumerate() {
                let cp = format!("{ptr}.{j}");
                parse_env_array_entry(as_str(item, &cp)?, ctx, &cp, default_sensitive, out, stack)?;
            }
            stack.pop();
            return Ok(());
        }
        reuse::ReuseToken::None => {}
    }
    reject_naked_sentinel(s, path)?;
    let (pattern, deny) = match s.strip_prefix('!') {
        Some(rest) => (rest.to_string(), true),
        None => (s.to_string(), false),
    };
    reject_env_key_braces(&pattern, path)?;
    // A `$(…)` in array form would have no key to bind to — array entries are
    // key/glob selectors, not values. Reject to avoid silent misuse.
    if resolve::has_substitution(&pattern) {
        return Err(CompileError::shape(
            path,
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
        broker_to: Vec::new(),
    });
    Ok(())
}

fn parse_env_object(
    map: &serde_json::Map<String, Value>,
    ctx: &CompileCtx,
    caps: ScopeCapabilities,
    path: &str,
    default_sensitive: bool,
) -> Result<Vec<EnvEntry>, CompileError> {
    let mut out = Vec::new();
    // The reuse resolution stack (cycle detection) — seeded empty per axis fold, so an
    // object-key `...:#/pointer` spread in vars/secrets threads it like fs/net.
    let mut stack = Vec::new();
    for (raw_key, val) in map {
        let p = child(path, raw_key);
        parse_env_object_entry(
            raw_key,
            val,
            ctx,
            caps,
            &p,
            default_sensitive,
            &mut out,
            &mut stack,
        )?;
    }
    Ok(out)
}

/// One entry of an env-family Object form (`vars`/`secrets`), shared by direct entries and
/// each entry of a `...:#/pointer`-spliced OBJECT — the object twin of the fs/net item
/// folds. Reuse is checked FIRST (a spliced object's entries re-fold in place, so
/// last-match composes at the splice); then the ordinary key path (naked-`...` reject,
/// `?`-optional suffix, brace reject, value parse). `stack` is the reuse resolution stack.
#[allow(clippy::too_many_arguments)]
fn parse_env_object_entry(
    raw_key: &str,
    val: &Value,
    ctx: &CompileCtx,
    caps: ScopeCapabilities,
    path: &str,
    default_sensitive: bool,
    out: &mut Vec<EnvEntry>,
    stack: &mut Vec<String>,
) -> Result<(), CompileError> {
    match reuse::parse_reuse_token(raw_key) {
        reuse::ReuseToken::Negated => return Err(CompileError::shape(path, NEGATED_REUSE_MSG)),
        reuse::ReuseToken::Pointer(ptr) => {
            reject_spread_value(val, path)?;
            let obj = reuse::resolve_reuse_object(ctx, ptr, path, stack)?;
            stack.push(ptr.to_string());
            for (k, v) in obj {
                let cp = format!("{ptr}.{k}");
                parse_env_object_entry(k, v, ctx, caps, &cp, default_sensitive, out, stack)?;
            }
            stack.pop();
            return Ok(());
        }
        reuse::ReuseToken::None => {}
    }
    // `...`/`!...` env-object inheritance was removed (v2: complete statement).
    reject_naked_sentinel(raw_key, path)?;
    // A trailing `?` on the key marks it optional; a glob key is inherently
    // optional (D9 — a glob matches however many keys, zero included), so it is
    // never a required-var declaration and reports optional in the schema.
    let (key, optional) = match raw_key.strip_suffix('?') {
        Some(k) => (k.to_string(), true),
        None => (raw_key.to_string(), false),
    };
    reject_env_key_braces(&key, path)?;
    let optional = optional || is_glob(&key);
    let entry = parse_env_object_value(key, optional, val, ctx, caps, path, default_sensitive)?;
    out.push(entry);
    Ok(())
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
            broker_to: Vec::new(),
        }),
        Value::Bool(false) => Ok(EnvEntry {
            pattern: key,
            action: EnvAction::Deny,
            sensitive: default_sensitive,
            optional,
            format: None,
            key_match: KeyMatch::User,
            builtin: false,
            broker_to: Vec::new(),
        }),
        Value::String(s) => {
            parse_env_string_value(key, optional, s, ctx, caps, path, default_sensitive)
        }
        Value::Object(extras) => {
            parse_env_extras(key, optional, extras, caps, path, default_sensitive)
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
            broker_to: Vec::new(),
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
        broker_to: Vec::new(),
    })
}

/// The object extras form: `{ format, optional, brokerTo }`. Sensitivity is NOT an
/// extras key — it is decided by the axis the entry came from (`vars`→false,
/// `secrets`→true), threaded in as `default_sensitive`.
fn parse_env_extras(
    key: String,
    optional_from_key: bool,
    extras: &serde_json::Map<String, Value>,
    caps: ScopeCapabilities,
    path: &str,
    default_sensitive: bool,
) -> Result<EnvEntry, CompileError> {
    const ALLOWED: &[&str] = &["format", "optional", "brokerTo"];
    for k in extras.keys() {
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
    // `brokerTo` relocates credential brokering onto the secrets axis: the secret KEY
    // is the brokered env name and the value is the exact hosts it may cross. Parsed
    // into `broker_to` on the entry so the ordered list carries the intent;
    // `fold_env_axes` harvests it and the caller transposes it onto `net.brokers`.
    let broker_to = match extras.get("brokerTo") {
        None => Vec::new(),
        Some(hosts) => {
            let bp = child(path, "brokerTo");
            // Trusted-only, gated on THIS scope's capability (mirrors the old net
            // form): dependency-controlled config never brokers a credential.
            if !caps.credential_broker {
                return Err(CompileError::shape(
                    &bp,
                    "credential brokering (`brokerTo`) is a trusted-only capability — it is not permitted in an untrusted (dependenciesMeta) grant",
                ));
            }
            // Brokering protects a sensitive value, so it is a `secrets`-only knob.
            if !default_sensitive {
                return Err(CompileError::shape(
                    &bp,
                    "`brokerTo` is only valid on a `secrets` entry — move the entry to `secrets` to broker it",
                ));
            }
            // The secret key is the brokered env name: a glob binds no single
            // credential, and an optional secret can be absent with nothing to broker.
            if is_glob(&key) {
                return Err(CompileError::shape(
                    &bp,
                    "`brokerTo` cannot be set on a glob key — name the exact secret to broker",
                ));
            }
            if optional {
                return Err(CompileError::shape(
                    &bp,
                    "a brokered secret cannot be optional — its value must be present at startup to broker",
                ));
            }
            validate_broker_env_name(&key, &bp)?;
            parse_broker_hosts(hosts, &bp)?
        }
    };
    Ok(EnvEntry {
        pattern: key,
        action: EnvAction::Allow(ty),
        sensitive,
        optional,
        format,
        key_match: KeyMatch::User,
        builtin: false,
        broker_to,
    })
}

/// Build the child env map + schema + withheld list from ordered entries.
/// Source keys are filtered last-match-wins; explicit-value entries are set
/// directly. A required exact key with no source value and no literal errors. The
/// value source is the ambient env verbatim (a single-block compile has no parent
/// scope — cross-scope env inheritance was removed in P4).
fn construct_env(
    entries: &[EnvEntry],
    ctx: &CompileCtx,
    policy: &mut EnvPolicy,
) -> Result<(), CompileError> {
    let source: &BTreeMap<String, String> = &ctx.ambient_env;

    // Compile a matcher per entry, honoring its `key_match`: user patterns are
    // case-sensitive globs, the built-in secret defaults case-insensitive / predicate.
    let matchers: Vec<KeyMatcher> = entries.iter().map(compile_key_matcher).collect();

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
        // Retain the winning ENTRY (not just its action) so a failed validation on a
        // sensitive source value can be redacted (L1) — never echo a secret's value.
        // (Deny or a literal, handled above, falls through to withhold.)
        if let Some(e) = verdict
            && let EnvAction::Allow(ty) = &e.action
        {
            if let Some(t) = ty {
                let display = if e.sensitive {
                    "<redacted>"
                } else {
                    value.as_str()
                };
                t.validate_display(value, display)
                    .map_err(|err| CompileError::validation(name, &err))?;
            }
            defaults::insert_env(&mut policy.constructed, name.clone(), value.clone());
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

    // 5. Sensitive-key set (fail-safe): a CONSTRUCTED key is sensitive iff ANY matching
    //    entry is `sensitive` — order-robust, never flipped off by a later non-sensitive
    //    rule (`vars:["*"]`+`secrets:["FOO"]` ⇒ FOO sensitive regardless of list order).
    //    Materialized per concrete key so an output redactor never re-globs; reuses the
    //    exact matchers the value verdict used. Sorted (BTreeMap key order).
    policy.sensitive_keys = policy
        .constructed
        .keys()
        .filter(|name| {
            entries
                .iter()
                .zip(&matchers)
                .any(|(e, m)| e.sensitive && m.hit(name))
        })
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
    /// A compiled glob (user key, OS-mirrored case-sensitivity).
    Glob(GlobMatcher),
    /// Exact fallback when a pattern fails to compile as a glob; `bool` carries the
    /// same OS-mirrored case-insensitivity the glob would have.
    Exact(String, bool),
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
        }
    }
}

/// Compile an entry's pattern into a [`KeyMatcher`]. Every entry is a user-authored key
/// ([`KeyMatch::User`]) — the built-in secret-key matchers were removed with the `"..."`
/// env splice in P4 — matched as an OS-mirrored glob with an exact fallback.
fn compile_key_matcher(e: &EnvEntry) -> KeyMatcher {
    let KeyMatch::User = e.key_match;
    GlobBuilder::new(&e.pattern)
        .case_insensitive(ENV_KEYS_CASE_INSENSITIVE)
        .build()
        .map(|g| KeyMatcher::Glob(g.compile_matcher()))
        .unwrap_or_else(|_| KeyMatcher::Exact(e.pattern.clone(), ENV_KEYS_CASE_INSENSITIVE))
}

// ── shared helpers ────────────────────────────────────────────────────────────

/// Reject a naked `...` / `!...` inheritance sentinel (array entry OR object key, any
/// axis). v2 removed implicit inheritance — a policy is a complete statement — so a
/// surviving `...`/`!...` is a HARD migration error, never silently folded to a literal
/// path/host named `...`. The one supported reuse form is a `...:#/pointer` reference
/// (see [`reuse`]), matched separately by [`reuse::parse_reuse_token`] before this runs.
fn reject_naked_sentinel(s: &str, path: &str) -> Result<(), CompileError> {
    // Trim to agree with `reuse::parse_reuse_token`'s trimming, so a whitespace-padded
    // `"  ...  "` (array entry OR object key) still fails loud rather than folding to a
    // literal `<proj>/...` grant.
    let s = s.trim();
    if s == "..." || s == "!..." {
        return Err(CompileError::shape(path, NAKED_SENTINEL_MSG));
    }
    Ok(())
}

/// The object-key spread `"...:#/pointer": <value>` carries a PLACEHOLDER value only —
/// the spliced object's entries bring their own values, so this value governs nothing.
/// `true` is the canonical placeholder; anything else (a mode, a host bool, an env type,
/// even `false`) is rejected fail-loud so a reader is never misled into thinking it
/// applies to the spliced entries. Provenance: JS `{...shared}` has no value; JSONC needs
/// one syntactically, so `true` stands in for "no value", and anything else is a mistake.
fn reject_spread_value(val: &Value, path: &str) -> Result<(), CompileError> {
    if matches!(val, Value::Bool(true)) {
        return Ok(());
    }
    Err(CompileError::shape(
        path,
        "a `...:#/pointer` spread key takes only the placeholder value `true` — the spliced entries carry their own values, so a mode/type/bool here would not apply to them",
    ))
}

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
