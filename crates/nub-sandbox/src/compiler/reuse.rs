//! `...:#/pointer` reuse (v2 grammar): a `...:`-prefixed token — an ARRAY entry OR an
//! OBJECT KEY, on any axis — that references another node in the SAME policy document by
//! RFC-6901 JSON pointer and splices its RAW contents at that position, re-folding through
//! the ordinary per-entry path (NOT a copy of compiled IR). This is the ONLY reuse
//! mechanism — a policy is otherwise a complete, self-contained statement (the naked-`...`
//! splice and the cross-scope inheritance chain were removed in Phase 4).
//!
//! The spread is GENERAL over both container shapes, resolved by ONE pointer core
//! ([`resolve_reuse_node`]) + ONE cycle stack: an ARRAY-context token must resolve to an
//! array (its entries splice at the slot); an OBJECT-KEY token must resolve to an object
//! (its key→value entries splice, then a LOCAL key after the spread overrides a spliced
//! one by last-match, mirroring JS `{...shared, "./dist": "rw"}`). A type mismatch either
//! way is a fail-loud [`CompileError::Shape`].
//!
//! `#` in a pointer is the DOCUMENT root (threaded in via [`CompileCtx::document`]), not
//! the compiled `sandbox` block — a reused node is typically a *sibling* of `sandbox`
//! (see sandbox.mdx "Reuse another policy's rules"). Because spliced contents re-fold
//! through the ordinary path, every in-place expander (`$trusted`/`$downloads`/`$tooldirs`/`$tmp`) and
//! the `.env*` floor fire at the spliced position for free, and last-match-wins order is
//! preserved (spliced contents occupy exactly the token's slot).

use super::{CompileCtx, CompileError};
use serde_json::{Map, Value};

/// Depth belt beyond the cycle-set guard: the visited-pointer stack already bounds a
/// finite document (a pointer active on its own resolution stack is a cycle), so this
/// only guards a pathological deeply-nested ACYCLIC chain.
pub(super) const MAX_REUSE_DEPTH: usize = 64;

const REUSE_PREFIX: &str = "...:";
const NEGATED_REUSE_PREFIX: &str = "!...:";

/// Classification of a `...:`-prefixed string (array entry OR object key) against the
/// reuse token.
pub(super) enum ReuseToken<'a> {
    /// A `...:#/pointer` reuse — the text AFTER `...:` (still carrying its `#`).
    Pointer(&'a str),
    /// A `!...:#/pointer` negated reuse — a shape error (OQ1) in BOTH container forms: a
    /// spliced list/object carries its own allow/deny entries, so negating the whole
    /// splice is ill-defined; expand-then-negate is deferred.
    Negated,
    /// An ordinary entry (not a reuse token).
    None,
}

/// Detect a `...:#/pointer` (or `!...:` negated) reuse token. Whitespace-tolerant
/// (mirrors the fold's `.trim()` handling of surface entries). Container-agnostic — the
/// same classifier serves an array entry and an object key (the general spread).
pub(super) fn parse_reuse_token(s: &str) -> ReuseToken<'_> {
    let t = s.trim();
    if t.strip_prefix(NEGATED_REUSE_PREFIX).is_some() {
        return ReuseToken::Negated;
    }
    match t.strip_prefix(REUSE_PREFIX) {
        Some(ptr) => ReuseToken::Pointer(ptr),
        None => ReuseToken::None,
    }
}

/// The shared pointer-resolution core for BOTH container contexts (list reuse and
/// object-key spread): validate the `#`-rooted RFC-6901 pointer shape, the cycle set, the
/// depth belt, and dangling targets, then return the resolved document NODE. Its callers
/// ([`resolve_reuse_array`] / [`resolve_reuse_object`]) type-check that node for their
/// context, so ONE resolver + ONE cycle-stack governs every splice — the general spread,
/// no per-context special-casing. Every failure is a fail-loud [`CompileError::Shape`]
/// pointed at the splice-site `path`.
///
/// This does the cycle CHECK; the caller owns the push/pop of `stack` around the fold
/// recursion, so the same pointer reused twice on DISJOINT paths (a DAG) is allowed —
/// only a pointer active on its own resolution stack is a cycle.
fn resolve_reuse_node<'a>(
    ctx: &'a CompileCtx,
    ptr: &str,
    path: &str,
    stack: &[String],
) -> Result<&'a Value, CompileError> {
    let rfc = ptr.strip_prefix('#').ok_or_else(|| {
        CompileError::shape(
            path,
            &format!(
                "a reuse pointer must be a same-document JSON pointer beginning with `#`, e.g. `...:#/shared/fs` — got `...:{ptr}`"
            ),
        )
    })?;
    if rfc.is_empty() {
        return Err(CompileError::shape(
            path,
            "`...:#` must name a list or object — a bare `#` (the whole document) cannot be spliced",
        ));
    }
    if stack.iter().any(|p| p == ptr) {
        let mut chain = stack.to_vec();
        chain.push(ptr.to_string());
        return Err(CompileError::shape(
            path,
            &format!("reuse cycle detected: {}", chain.join(" → ")),
        ));
    }
    if stack.len() >= MAX_REUSE_DEPTH {
        return Err(CompileError::shape(
            path,
            "reuse nested too deeply (>64) — check for an unintended chain",
        ));
    }
    // `Value::pointer` handles RFC-6901 `~0`/`~1` escapes; `#` is stripped above.
    ctx.document.pointer(rfc).ok_or_else(|| {
        CompileError::shape(
            path,
            &format!("reuse pointer `#{rfc}` does not resolve to any node in the policy document"),
        )
    })
}

/// Resolve a reuse pointer used as an ARRAY entry to its referenced RAW array (its entries
/// then re-fold in place). The pointer naming a non-array node is a fail-loud type
/// mismatch — an array-context token splices a list.
pub(super) fn resolve_reuse_array<'a>(
    ctx: &'a CompileCtx,
    ptr: &str,
    path: &str,
    stack: &[String],
) -> Result<&'a Vec<Value>, CompileError> {
    match resolve_reuse_node(ctx, ptr, path, stack)? {
        Value::Array(arr) => Ok(arr),
        other => Err(CompileError::shape(
            path,
            &format!(
                "reuse pointer `{ptr}` must reference a list, not {} — a `...:#/pointer` used as an array entry splices a list",
                node_kind(other)
            ),
        )),
    }
}

/// Resolve a reuse pointer used as an OBJECT KEY to its referenced RAW object (its
/// key→value entries then re-fold in place). The pointer naming a non-object node is a
/// fail-loud type mismatch — an object-key spread splices an object.
pub(super) fn resolve_reuse_object<'a>(
    ctx: &'a CompileCtx,
    ptr: &str,
    path: &str,
    stack: &[String],
) -> Result<&'a Map<String, Value>, CompileError> {
    match resolve_reuse_node(ctx, ptr, path, stack)? {
        Value::Object(obj) => Ok(obj),
        other => Err(CompileError::shape(
            path,
            &format!(
                "reuse pointer `{ptr}` must reference an object, not {} — a `...:#/pointer` used as an object key splices an object's entries",
                node_kind(other)
            ),
        )),
    }
}

fn node_kind(v: &Value) -> &'static str {
    match v {
        Value::Object(_) => "an object",
        Value::String(_) => "a string",
        Value::Number(_) => "a number",
        Value::Bool(_) => "a boolean",
        Value::Null => "null",
        Value::Array(_) => "a list",
    }
}
