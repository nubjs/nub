//! Filesystem path matching: symbolic-root expansion, cross-OS normalization,
//! canonicalization that survives non-existent paths, and last-match-wins
//! evaluation of an [`FsRuleSet`].

use crate::policy::{Effect, FsAccess, FsRuleSet};
use globset::{Glob, GlobBuilder, GlobMatcher};
use std::path::{Component, Path, PathBuf};

/// The per-OS home anchors symbolic roots expand against. Host-provided
/// (Boundary B) — the engine never discovers these itself.
#[derive(Debug, Clone)]
pub struct Homes {
    pub home: PathBuf,
    pub tmp: PathBuf,
    pub cache: PathBuf,
    /// The project root, for `./`-relative patterns.
    pub project: PathBuf,
}

/// Case-insensitivity target: Windows + macOS filesystems fold case, so a
/// `!~/.ssh` deny must also block `~/.SSH`. Applied at the glob level.
const CASE_INSENSITIVE: bool = cfg!(any(target_os = "windows", target_os = "macos"));

/// Expand a surface path pattern's symbolic roots and normalize its separators to
/// forward slashes. Does NOT canonicalize (globs may contain `*`/`**` that a path
/// canonicalizer would mangle); canonicalization is applied to the CANDIDATE path
/// at match time. Recognized roots:
///   `$tmp` `$cache` → the corresponding home (the closed `$name` sentinel set);
///   `~` / `~/` → home; `./` `../` / a bare relative → under the project root.
/// A literal absolute path (`/x`, `C:\x`) passes through (only slash-normalized).
/// An unrecognized `$name` is NOT rejected here (the fold rejects it before match
/// time); it passes through so this stays a pure transform.
pub fn expand_symbolic(pattern: &str, homes: &Homes) -> String {
    let p = pattern.trim();
    let expanded = if let Some((name, rest)) = split_fs_sentinel(p) {
        match name {
            "tmp" => join_root(&homes.tmp, rest),
            "cache" => join_root(&homes.cache, rest),
            _ => p.to_string(),
        }
    } else if p == "~" {
        homes.home.to_string_lossy().into_owned()
    } else if let Some(rest) = p.strip_prefix("~/") {
        join_root(&homes.home, rest)
    } else if p.starts_with("./") || p.starts_with("../") || is_bare_relative(p) {
        // Strip a single leading `./` (noise); keep `../` (meaningful — the glob
        // prefix canonicalizer collapses it against the project root).
        join_root(&homes.project, p.strip_prefix("./").unwrap_or(p))
    } else {
        p.to_string()
    };
    normalize_slashes(&expanded)
}

/// The closed set of `$name` filesystem sentinels. `$tmp` (the private per-run tmp
/// dir) is consumed by the fold as a MODE before it ever reaches `expand_symbolic`;
/// `$cache` expands to the platform cache dir. Any other `$name` is rejected by the
/// fold (see `reject_unknown_fs_sentinel`).
pub const FS_SENTINEL_NAMES: &[&str] = &["cache", "tmp"];

/// Split a leading `$name` sentinel into `(name, remainder)`, or `None` when `p` is
/// not a `$`-sentinel. `$(` is command substitution (resolved by the fold BEFORE any
/// `$name` check — the paren disambiguation), so it returns `None`. The name is a
/// shell-style identifier (`[A-Za-z0-9_]+` after the `$`); the remainder is the rest,
/// so `$cache/x` → `("cache", "/x")` and `$cache` → `("cache", "")`. A bare `$`, `$(`,
/// or `$` followed by a non-identifier returns `None`.
pub fn split_fs_sentinel(p: &str) -> Option<(&str, &str)> {
    let rest = p.strip_prefix('$')?;
    if rest.starts_with('(') {
        return None;
    }
    let name_len = rest
        .bytes()
        .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_')
        .count();
    if name_len == 0 {
        return None;
    }
    Some((&rest[..name_len], &rest[name_len..]))
}

/// True for a pattern with no leading root marker and no absolute anchor — a bare
/// relative like `data/**` that resolves under the project root.
fn is_bare_relative(p: &str) -> bool {
    if p.is_empty() {
        return false;
    }
    // Absolute POSIX (`/x`), Windows drive (`C:\`), UNC (`\\`), or a symbolic
    // root already handled by the caller — none are bare-relative. `$` covers both
    // a `$name` sentinel and `$(…)`; neither is ever project-joined here.
    let b = p.as_bytes();
    let posix_abs = b[0] == b'/';
    let win_drive = p.len() >= 2 && b[1] == b':';
    let unc = p.starts_with("\\\\");
    !(posix_abs || win_drive || unc || p.starts_with(['<', '~', '$']))
}

/// Join a symbolic root's remainder onto the resolved base directory, tolerating
/// a leading slash on the remainder.
fn join_root(base: &Path, rest: &str) -> String {
    let rest = rest.trim_start_matches(['/', '\\']);
    if rest.is_empty() {
        base.to_string_lossy().into_owned()
    } else {
        // Manual join keeps forward slashes (Path::join would insert `\` on
        // Windows, which normalize_slashes then has to undo anyway).
        format!(
            "{}/{}",
            base.to_string_lossy().trim_end_matches(['/', '\\']),
            rest
        )
    }
}

/// Normalize every backslash to a forward slash (gitignore/tsconfig convention).
/// The matcher works entirely in forward-slash space; the candidate path is
/// normalized the same way before matching.
pub fn normalize_slashes(s: &str) -> String {
    s.replace('\\', "/")
}

/// Canonicalize a path INCLUDING components that do not yet exist.
///
/// `std::fs::canonicalize` (and `Path::canonicalize`) Err on a non-existent path
/// — the disavowed backend's fail-closed bug: a write-allow for a not-yet-created
/// dir silently denied. This resolves the longest existing prefix via the OS
/// (collapsing symlinks / `/var`→`/private/var` firmlinks / Windows 8.3 names)
/// and then appends the remaining components with `.`/`..` collapsed LEXICALLY.
/// So `/tmp/does/not/exist/../ok` canonicalizes correctly even though nothing past
/// `/tmp` exists — closing the symlink-dodge without the fail-closed trap.
pub fn canonicalize_including_nonexistent(path: &Path) -> PathBuf {
    // Fast path: the whole thing exists.
    if let Ok(real) = std::fs::canonicalize(path) {
        return strip_verbatim_prefix(real);
    }
    // Find the longest existing ancestor (ancestors() yields longest → shortest).
    let Some(base) = path
        .ancestors()
        .find(|p| !p.as_os_str().is_empty() && p.exists())
    else {
        // No existing ancestor at all — purely lexical normalization.
        return lexical_normalize(path);
    };
    // Canonicalize the existing prefix (resolves symlinks / firmlinks / 8.3
    // names), then re-apply the non-existent tail with lexical `..`/`.` collapse.
    let mut out = std::fs::canonicalize(base)
        .map(strip_verbatim_prefix)
        .unwrap_or_else(|_| base.to_path_buf());
    if let Ok(tail) = path.strip_prefix(base) {
        for comp in tail.components() {
            match comp {
                Component::ParentDir => {
                    out.pop();
                }
                Component::CurDir => {}
                other => out.push(other.as_os_str()),
            }
        }
    }
    out
}

/// Strip a Windows `\\?\` / `\\?\UNC\` verbatim (extended-length) prefix that
/// `std::fs::canonicalize` prepends. An IR path MUST be a plain path: the verbatim
/// prefix is not merely cosmetic — after `normalize_slashes` its `?` reads as a glob
/// metacharacter, so `has_glob_meta`/`literal_subtree` mis-classify a fully-literal
/// grant as an unenforceable embedded-glob and DROP it (the Windows AppContainer
/// backend then denies the project its own dir). No-op on a non-verbatim path (the
/// prefix never appears off Windows). Bounded to normal-length paths, which is all a
/// project/work dir is; a genuine >MAX_PATH path that needs the prefix is out of scope.
///
/// Only the two forms that stay ABSOLUTE after stripping are unwrapped: a drive path
/// (`\\?\C:\…` → `C:\…`) and a UNC path (`\\?\UNC\server\share` → `\\server\share`).
/// A DRIVELESS volume-GUID path (`\\?\Volume{…}\…`, a folder on a volume mounted with no
/// letter) is left VERBATIM: stripping it would yield a non-absolute remainder AND expose
/// its `{`/`}` as glob metachars — dropping the grant. Leaving it verbatim is no worse
/// than today (that rare shape isn't a normal project dir); mangling it would be.
fn strip_verbatim_prefix(p: PathBuf) -> PathBuf {
    let Some(s) = p.to_str() else { return p };
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        // Unwrap only a real drive path (`X:\…`); leave `Volume{GUID}\…` (and any other
        // non-drive shape) verbatim rather than produce a broken relative path.
        let b = rest.as_bytes();
        if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
            return PathBuf::from(rest);
        }
    }
    p
}

/// Canonicalize the LITERAL directory prefix of an (already symbol-expanded,
/// slash-normalized) glob so it matches the canonicalized CANDIDATE path. Without
/// this a glob like `/tmp/**` never matches `/private/tmp/foo` on macOS (`/tmp` is
/// a firmlink), silently dropping a grant. Only the portion up to the last `/`
/// before the first glob metachar is a real path; the rest stays verbatim glob.
/// Relative globs (no absolute anchor) are returned unchanged.
pub fn canonicalize_glob_prefix(pattern: &str) -> String {
    let meta = pattern.find(['*', '?', '[', '{']);
    let dir_end = match meta {
        Some(i) => match pattern[..i].rfind('/') {
            Some(slash) => slash + 1,           // include the slash
            None => return pattern.to_string(), // metachar in the first segment
        },
        None => pattern.len(), // fully literal
    };
    let prefix = &pattern[..dir_end];
    let tail = &pattern[dir_end..];
    if prefix.is_empty() || !Path::new(prefix).is_absolute() {
        return pattern.to_string();
    }
    let canon = canonicalize_including_nonexistent(Path::new(prefix));
    let canon = normalize_slashes(&canon.to_string_lossy());
    let canon = canon.trim_end_matches('/');
    if tail.is_empty() {
        canon.to_string()
    } else {
        format!("{canon}/{tail}")
    }
}

/// Lexically collapse `.`/`..` without touching the filesystem. Used only when a
/// path has no existing ancestor (e.g. under a chroot in tests).
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// A compiled last-match-wins matcher over an [`FsRuleSet`]. Compiles every glob
/// once at construction; `decide()` walks the entries and returns the LAST match
/// (or the ruleset's `default_effect`).
pub struct PathMatcher {
    /// One per COMPILABLE ruleset entry: (compiled glob, effect, access, source index).
    /// The source index is the position in the original `FsRuleSet`, which a malformed
    /// glob makes non-identical to the position here — the Linux emitter orders its
    /// mount operations by that authored position, so it must be the ruleset's.
    entries: Vec<(GlobMatcher, Effect, FsAccess, usize)>,
    default_effect: Effect,
}

/// A decision for a candidate path: the winning effect and, when allowed, the
/// access granted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsDecision {
    pub effect: Effect,
    /// LAST-MATCH-WINS, which models the EFFECT axis and NOT the write axis —
    /// no production code reads this field, and a test that treats it as the
    /// access a backend would grant is asserting a model none of them share.
    /// Since `f43aab575f` an Allow never subtracts: every backend UNIONS write
    /// (Seatbelt was the last holdout, synthesizing a `(deny file-write*)` out
    /// of an allow). Kept last-match because `tests/compiler.rs` uses it as a
    /// strict IR-ordering check, which a union would silently weaken.
    pub access: FsAccess,
}

impl PathMatcher {
    /// Compile the ruleset. An individual malformed glob is skipped with a
    /// `tracing` warning rather than failing the whole matcher — the compiler
    /// validates globs up front, so this only guards a corrupt deserialized IR.
    pub fn new(set: &FsRuleSet) -> Self {
        let mut entries = Vec::with_capacity(set.entries.len());
        for (index, rule) in set.entries.iter().enumerate() {
            match compile_glob(rule.matcher.as_str()) {
                Ok(m) => entries.push((m, rule.effect, rule.access, index)),
                Err(e) => {
                    tracing::warn!(glob = rule.matcher.as_str(), error = %e, "skipping malformed fs glob");
                }
            }
        }
        Self {
            entries,
            default_effect: set.default_effect,
        }
    }

    /// Decide the verdict for a candidate path. The candidate is canonicalized
    /// (incl. non-existent) and slash-normalized before matching, so a symlink /
    /// `..` / short-name spelling cannot dodge a deny.
    pub fn decide(&self, candidate: &Path) -> FsDecision {
        let canon = canonicalize_including_nonexistent(candidate);
        let norm = normalize_slashes(&canon.to_string_lossy());
        self.decide_normalized(&norm, None)
    }

    /// Evaluate one logical directory-entry spelling and its resolved object as a
    /// single candidate. A deny matching either spelling wins at its authored
    /// position, which prevents a symlink named `.env` from losing its logical-name
    /// deny when canonicalization follows the link.
    #[cfg(target_os = "linux")]
    pub(crate) fn decide_logical_or_resolved(&self, logical: &Path, resolved: &Path) -> FsDecision {
        let logical = normalize_slashes(&logical.to_string_lossy());
        let resolved = normalize_slashes(&resolved.to_string_lossy());
        self.decide_normalized(&logical, Some(&resolved))
    }

    /// Last matching effect among entries before `end`. Used by the Linux mask
    /// planner to distinguish an explicit user deny from compiler-injected dotenv
    /// defaults without adding backend provenance to the public policy IR.
    #[cfg(target_os = "linux")]
    pub(crate) fn last_matching_effect_before(
        &self,
        logical: &Path,
        resolved: &Path,
        end: usize,
    ) -> Option<Effect> {
        let logical = normalize_slashes(&logical.to_string_lossy());
        let resolved = normalize_slashes(&resolved.to_string_lossy());
        self.entries
            .iter()
            .take(end)
            .filter(|(glob, _, _, _)| glob.is_match(&logical) || glob.is_match(&resolved))
            .map(|(_, effect, _, _)| *effect)
            .next_back()
    }

    /// Last matching effect among entries AT OR AFTER `start`, i.e. does anything the
    /// policy says LATER override this one. The Linux mount-plan compiler asks this of
    /// each allow before turning it into a bind.
    ///
    /// Deliberately not `decide`: an allow's own glob need not match its own compiled
    /// path — `dir/**` compiles to a bind on `dir`, which `dir/**` does not match — so
    /// `decide` would fall through to the ruleset default and report Deny for a grant
    /// nothing actually denies.
    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn last_matching_effect_after(
        &self,
        logical: &Path,
        resolved: &Path,
        start: usize,
    ) -> Option<Effect> {
        let logical = normalize_slashes(&logical.to_string_lossy());
        let resolved = normalize_slashes(&resolved.to_string_lossy());
        self.entries
            .iter()
            .filter(|(_, _, _, index)| *index >= start)
            .filter(|(glob, _, _, _)| glob.is_match(&logical) || glob.is_match(&resolved))
            .map(|(_, effect, _, _)| *effect)
            .next_back()
    }

    /// Authored position of the LAST rule matching either spelling. The Linux emitter
    /// keys a deny mask's place in the mount-operation stream off this, so a mask lands
    /// where the policy put the deny that produced it rather than after every grant.
    /// `None` means no rule matched — the candidate is backend infrastructure, not policy.
    #[cfg(target_os = "linux")]
    pub(crate) fn last_matching_index(&self, logical: &Path, resolved: &Path) -> Option<usize> {
        let logical = normalize_slashes(&logical.to_string_lossy());
        let resolved = normalize_slashes(&resolved.to_string_lossy());
        self.entries
            .iter()
            .filter(|(glob, _, _, _)| glob.is_match(&logical) || glob.is_match(&resolved))
            .map(|(_, _, _, index)| *index)
            .next_back()
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn matches_deny_entry(&self, logical: &Path, resolved: &Path) -> bool {
        let logical = normalize_slashes(&logical.to_string_lossy());
        let resolved = normalize_slashes(&resolved.to_string_lossy());
        self.entries.iter().any(|(glob, effect, _, _)| {
            *effect == Effect::Deny && (glob.is_match(&logical) || glob.is_match(&resolved))
        })
    }

    fn decide_normalized(&self, first: &str, second: Option<&str>) -> FsDecision {
        let mut winner: Option<(Effect, FsAccess)> = None;
        for (glob, effect, access, _) in &self.entries {
            if glob.is_match(first) || second.is_some_and(|path| glob.is_match(path)) {
                winner = Some((*effect, *access));
            }
        }
        match winner {
            Some((effect, access)) => FsDecision { effect, access },
            None => FsDecision {
                effect: self.default_effect,
                // Access is meaningless on a Deny; report Read as a neutral value.
                access: FsAccess::Read,
            },
        }
    }
}

/// Build a `globset` matcher with the cross-OS flags nub relies on: literal-
/// separator matching (so `*` never crosses `/`) and per-OS case-insensitivity.
pub fn compile_glob(pattern: &str) -> Result<GlobMatcher, globset::Error> {
    let glob: Glob = GlobBuilder::new(pattern)
        .literal_separator(true)
        .case_insensitive(CASE_INSENSITIVE)
        .build()?;
    Ok(glob.compile_matcher())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbatim_prefix_is_stripped_so_the_ir_path_has_no_bogus_glob_char() {
        // `std::fs::canonicalize` on Windows returns `\\?\C:\…`; unstripped, its `?`
        // reads as a glob metachar and drops the literal grant. Exhaustive over every
        // shape canonicalize can emit; strip ONLY the two that stay absolute.
        let cases: &[(&str, &str)] = &[
            // Drive verbatim → plain drive path.
            (r"\\?\C:\Users\me\proj", r"C:\Users\me\proj"),
            (r"\\?\D:\", r"D:\"),
            // UNC verbatim → plain UNC.
            (r"\\?\UNC\server\share\proj", r"\\server\share\proj"),
            // Driveless volume-GUID: MUST stay verbatim (stripping → non-absolute + `{}`
            // glob metachars → dropped grant). Left unchanged is the correct behavior.
            (
                r"\\?\Volume{b75e2c83-0000-0000-0000-602f00000000}\proj",
                r"\\?\Volume{b75e2c83-0000-0000-0000-602f00000000}\proj",
            ),
            // A bare/short `\\?\x` that is not a drive path stays verbatim (no mangling).
            (r"\\?\x", r"\\?\x"),
            // Already-plain paths pass through untouched (incl. non-Windows).
            (r"C:\Users\me\proj", r"C:\Users\me\proj"),
            (r"\\server\share\proj", r"\\server\share\proj"),
            ("/private/tmp/proj", "/private/tmp/proj"),
        ];
        for (input, want) in cases {
            assert_eq!(
                strip_verbatim_prefix(PathBuf::from(input)),
                PathBuf::from(want),
                "strip_verbatim_prefix({input:?})"
            );
        }
    }
}
