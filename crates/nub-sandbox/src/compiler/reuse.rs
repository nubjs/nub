//! `...:#/pointer` list reuse (v2 grammar): an fs/net array-entry token that
//! references another list in the SAME policy document by RFC-6901 JSON pointer and
//! splices its RAW entries at that position, so they re-fold through the ordinary
//! per-entry path (NOT a copy of compiled IR). This is the ONLY reuse mechanism — a
//! policy is otherwise a complete, self-contained statement (the naked-`...` splice
//! and the cross-scope inheritance chain were removed in Phase 4).
//!
//! `#` in a pointer is the DOCUMENT root (threaded in via [`CompileCtx::document`]),
//! not the compiled `sandbox` block — a reused list is typically a *sibling* of
//! `sandbox` (see sandbox.mdx "Reuse another policy's rules"). Because a reused
//! array's entries re-fold through the ordinary path, every in-place expander
//! (`$trusted`/`$tooldirs`/`$tmp`) and the `.env*` floor fire at the spliced position
//! for free, and last-match-wins order is preserved (spliced entries occupy exactly
//! the token's slot).

use super::{CompileCtx, CompileError};
use serde_json::Value;

/// Depth belt beyond the cycle-set guard: the visited-pointer stack already bounds a
/// finite document (a pointer active on its own resolution stack is a cycle), so this
/// only guards a pathological deeply-nested ACYCLIC chain.
pub(super) const MAX_REUSE_DEPTH: usize = 64;

const REUSE_PREFIX: &str = "...:";
const NEGATED_REUSE_PREFIX: &str = "!...:";

/// Classification of an fs/net array string entry against the reuse token.
pub(super) enum ReuseToken<'a> {
    /// A `...:#/pointer` reuse — the text AFTER `...:` (still carrying its `#`).
    Pointer(&'a str),
    /// A `!...:#/pointer` negated reuse — rejected in P4 (a shape error, OQ1): you
    /// cannot deny a spliced list, and expand-then-negate is deferred.
    Negated,
    /// An ordinary entry (not a reuse token).
    None,
}

/// Detect a `...:#/pointer` (or `!...:` negated) reuse token. Whitespace-tolerant
/// (mirrors the fold's `.trim()` handling of surface entries).
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

/// Whether an OBJECT key is a `...:`-prefixed reuse token. Reuse is array-only (OQ2),
/// so a `...:`/`!...:` object key is rejected loud by the caller rather than folded to
/// a literal path/host named `...:#/…`.
pub(super) fn is_reuse_object_key(key: &str) -> bool {
    let t = key.trim();
    t.starts_with(REUSE_PREFIX) || t.starts_with(NEGATED_REUSE_PREFIX)
}

/// Resolve a reuse pointer to its referenced RAW array in [`CompileCtx::document`],
/// validating pointer shape, dangling/non-array targets, and cycles. Returns a borrow
/// of the referenced array (its entries then re-fold in place). Every failure is a
/// fail-loud [`CompileError::Shape`] pointed at the splice-site `path`.
///
/// This does the cycle CHECK; the caller owns the push/pop of `stack` around the fold
/// recursion, so the same pointer reused twice on DISJOINT paths (a DAG) is allowed —
/// only a pointer active on its own resolution stack is a cycle.
pub(super) fn resolve_reuse_array<'a>(
    ctx: &'a CompileCtx,
    ptr: &str,
    path: &str,
    stack: &[String],
) -> Result<&'a Vec<Value>, CompileError> {
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
            "`...:#` must name a list — a bare `#` (the whole document) cannot be spliced",
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
    let node = ctx.document.pointer(rfc).ok_or_else(|| {
        CompileError::shape(
            path,
            &format!("reuse pointer `#{rfc}` does not resolve to any node in the policy document"),
        )
    })?;
    match node {
        Value::Array(arr) => Ok(arr),
        other => Err(CompileError::shape(
            path,
            &format!(
                "reuse pointer `#{rfc}` must reference a list, not {}",
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
