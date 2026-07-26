//! Clobber detection (D2b/D6): warn when a later ARRAY entry TOTALLY shadows an
//! earlier one, so the earlier entry can never be the deciding rule (dead code).
//! Partial overrides — allow a block, then deny one key inside it — are the
//! intended granular idiom and stay SILENT; a total shadow is the smell.
//!
//! SOUND by construction: a warning fires only when the later entry's match-set is
//! PROVABLY a superset of the earlier's (equal, universal `*`/`**`, or subtree /
//! prefix containment). It tolerates false NEGATIVES (an exotic shadow it can't
//! prove goes unwarned) but never false POSITIVES — a false-positive warning on a
//! valid config erodes trust as much as a false finding. Polarity is IRRELEVANT:
//! if a later entry's set covers an earlier's, the earlier is dead whether the two
//! agree or conflict, so `!` is stripped before comparison. The `"..."` / `"!..."`
//! sentinels are excluded (they expand to many rules; clobber judges only the
//! explicit user entries).

use super::CompileWarning;
use crate::matcher::path::{Homes, expand_symbolic};
use serde_json::Value;

/// fs array clobber. Patterns are symbolic-expanded + trailing-slash-normalized so
/// `~/.ssh` and `~/.ssh/config` compare on the same footing.
pub fn detect_fs(items: &[Value], homes: &Homes, path: &str, out: &mut Vec<CompileWarning>) {
    let norm: Vec<Option<String>> = items
        .iter()
        .map(|v| coverable(v).map(|p| expand_symbolic(p, homes).trim_end_matches('/').to_string()))
        .collect();
    detect(&norm, path, "fs", fs_covers, out);
}

/// net array clobber (host globs / CIDRs, compared as raw strings).
pub fn detect_net(items: &[Value], path: &str, out: &mut Vec<CompileWarning>) {
    let norm: Vec<Option<String>> = items
        .iter()
        .map(|v| coverable(v).map(str::to_string))
        .collect();
    detect(&norm, path, "net", net_covers, out);
}

/// env-family array clobber (exact keys / prefix globs, compared as raw strings).
/// Shared by the `vars` and `secrets` axes; the axis label in the warning is the
/// surface `path` so the message names the axis the author actually wrote.
pub fn detect_env(items: &[Value], path: &str, out: &mut Vec<CompileWarning>) {
    let norm: Vec<Option<String>> = items
        .iter()
        .map(|v| coverable(v).map(str::to_string))
        .collect();
    detect(&norm, path, path, env_covers, out);
}

/// A coverable entry: its polarity-stripped pattern, or `None` for a non-string /
/// sentinel entry (excluded from analysis).
fn coverable(v: &Value) -> Option<&str> {
    let s = v.as_str()?;
    if s == "..." || s == "!..." {
        return None;
    }
    let body = s.strip_prefix('!').unwrap_or(s);
    // Multi-rule sentinels are not a single comparable path/host: `$tmp` is a tmp-MODE
    // (emits no ordinary rule), `$tooldirs`/`$trusted` are SETS that expand to many
    // rules, and a `...:#/pointer` reuse token splices another list's entries. Exclude
    // them from shadow analysis (same reasoning as `"..."`) so the analyzer never
    // compares a multi-rule token's opaque literal as if it were one entry.
    let head = body.trim_start();
    if head.starts_with("$tmp")
        || head.starts_with("$tooldirs")
        || head.starts_with("$trusted")
        || head.starts_with("...:")
    {
        return None;
    }
    Some(body)
}

/// Emit one warning per DEAD entry: an entry `i` a later entry `j` fully covers.
fn detect(
    norm: &[Option<String>],
    path: &str,
    axis: &str,
    covers: impl Fn(&str, &str) -> bool,
    out: &mut Vec<CompileWarning>,
) {
    for (i, ei) in norm.iter().enumerate() {
        let Some(ei) = ei else { continue };
        let shadow = norm
            .iter()
            .enumerate()
            .skip(i + 1)
            .find_map(|(j, ej)| ej.as_ref().filter(|ej| covers(ej, ei)).map(|ej| (j, ej)));
        if let Some((j, ej)) = shadow {
            out.push(CompileWarning {
                path: path.to_string(),
                message: format!(
                    "{axis} entry #{i} `{ei}` is fully shadowed by later entry #{j} `{ej}` — it can never take effect"
                ),
            });
        }
    }
}

/// fs: `outer` covers `inner` iff equal, universal (`**`), a `P/**` subtree glob
/// whose prefix `inner` is / lives under, or a bare (non-glob) subtree prefix of
/// `inner`. A mid-glob `outer` (e.g. `packages/*/dist`) is not reasoned about → no
/// warning (sound). Note a bare surface path expands to a project-anchored root
/// (`**` → `<proj>/**`), so the `/**` case is what makes a later `**` cover an
/// earlier grant.
fn fs_covers(outer: &str, inner: &str) -> bool {
    if outer == inner || outer == "**" || outer == "/**" {
        return true;
    }
    // A subtree glob covers its prefix and everything under it.
    if let Some(prefix) = outer.strip_suffix("/**") {
        return inner == prefix || inner.starts_with(&format!("{prefix}/"));
    }
    if outer.contains(['*', '?', '[', '{']) {
        return false;
    }
    inner.starts_with(&format!("{outer}/"))
}

/// net: `outer` covers `inner` iff equal, `*` (all hosts), or a `*.suffix` glob
/// whose suffix `inner` ends under. A `*.suffix` does NOT cover the apex `suffix`
/// (subdomains-only, matching `host_glob_matches`), so `["example.com",
/// "*.example.com"]` is two disjoint rules — no shadow, no warning.
fn net_covers(outer: &str, inner: &str) -> bool {
    if outer == inner {
        return true;
    }
    // A symbolic target (`<private>` / `<local>`) is its own class: no host glob,
    // including a bare `*`, covers it — that is precisely why `*` does not re-open the
    // private ranges. Only an equal token (handled above) shadows it.
    if inner.starts_with('<') {
        return false;
    }
    if outer == "*" {
        return true;
    }
    if let Some(suffix) = outer.strip_prefix("*.") {
        return inner.ends_with(&format!(".{suffix}"));
    }
    false
}

/// env: `outer` covers `inner` iff equal, `*` (all keys), or a trailing-`*` prefix
/// glob (`VITE_*`) whose prefix `inner` starts with.
fn env_covers(outer: &str, inner: &str) -> bool {
    if outer == inner {
        return true;
    }
    if let Some(prefix) = outer.strip_suffix('*') {
        // `*` → prefix "" → covers all; `VITE_*` → prefix "VITE_". A mid-glob
        // prefix is not reasoned about.
        if !prefix.contains(['*', '?', '[', '{']) {
            return inner.starts_with(prefix);
        }
    }
    false
}
