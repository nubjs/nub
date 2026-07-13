//! OS-agnostic Landlock grant DERIVATION: resolved [`SandboxPolicy`] fs IR → a
//! concrete set of `(path, access)` grants the Linux backend installs.
//!
//! WHY THIS IS ITS OWN MODULE (the testability seam): Landlock enforcement needs a
//! real kernel (a macOS/LinuxKit host can't run it), but the SECURITY-CRITICAL
//! logic is not the syscall plumbing — it is turning an allow/deny/last-match-wins
//! ruleset into a set of grants that (a) never grants a denied path and (b) never
//! grants `/proc`/`/sys` (the env-read boundary). That logic reads only the real
//! filesystem — no landlock types — so it compiles and its unit tests RUN ON THE
//! macOS HOST over `tempfile` trees. `linux.rs` (Linux-only) maps these grant-kinds
//! to `AccessFs` bits, opens `PathFd`s, and `restrict_self`.
//!
//! THE LANDLOCK CONSTRAINT that shapes everything: Landlock is ALLOW-ONLY and
//! subtree-based — no deny primitive, and `PathBeneath` always covers a whole
//! subtree. So an allow with a nested deny cannot be one grant: the carve walk
//! grants clean subtrees whole and, only where a later rule can flip the verdict
//! inside, descends to grant the allowed children individually — the denied path is
//! never granted. Secrets OUTSIDE an allowlist need no carve: they are closed by
//! never being granted.
//!
//! CORRECTNESS HINGE — the walk must respect LAST-MATCH-WINS ORDER. A subtree can be
//! granted whole only when the directory is allowed AND no rule *after* the one that
//! decided the directory can match anything inside it (a later rule could flip
//! allow→deny or deny→allow). The blanket `**` allow/deny that forms the generous-
//! read baseline is therefore NOT a carve trigger by itself — only a rule ordered
//! after the winner and reaching inside forces a descent.

use crate::matcher::path::{canonicalize_including_nonexistent, compile_glob, normalize_slashes};
use crate::policy::{Effect, FsAccess, FsPolicy, FsRuleSet, SandboxPolicy};
use globset::GlobMatcher;
use std::path::{Component, Path, PathBuf};

/// One Landlock grant: an existing filesystem path plus what to grant on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Grant {
    pub path: PathBuf,
    pub kind: GrantKind,
}

/// What a [`Grant`] grants. Read splits into subtree / dir-list / single-file
/// because a carved directory must stay LISTABLE (`ReadDir`) without blanket-
/// granting its file children (only the allowed ones get `ReadFile`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum GrantKind {
    /// Read (file + dir) everything beneath `path`.
    ReadSubtree,
    /// Read-directory (listable) on `path` only.
    ReadDir,
    /// Read a single file.
    ReadFile,
    /// Read+write everything beneath `path`.
    WriteSubtree,
}

/// Derived fs grants plus the honesty flag.
#[derive(Debug, Default)]
pub(crate) struct DerivedGrants {
    pub grants: Vec<Grant>,
    /// A carve could not be fully enumerated within the walk budget. The unwalked
    /// remainder is left UNgranted (fail-safe — more restrictive), and this tells
    /// the caller to report `fs-read-partial` rather than over-claim enforcement.
    pub read_partial: bool,
}

/// Budget: a pathological generous-over-`/` read with a depth-independent `.env`
/// deny would otherwise walk the whole filesystem. On overflow we stop, leave the
/// remainder ungranted (safe), and flag `read_partial`.
const MAX_GRANTS: usize = 8192;
const MAX_VISITS: usize = 50_000;

/// System top-levels granted CLEAN under a generous `**` read (no user secrets live
/// here; net-deny blocks exfil regardless) — so the depth-independent `.env*` carve
/// is not walked across them. A rule ordered to reach inside one still forces its
/// carve via the normal walk. User-data roots (home/root/tmp/…) are always walked.
const SYSTEM_TOPLEVELS: &[&str] = &[
    "usr", "bin", "sbin", "lib", "lib64", "lib32", "libx32", "etc", "opt", "boot",
];

/// Whether the fs axis confines anything (mirrors `macos::fs_confines`). A relaxed
/// axis (`default_effect == Allow`, no entries) needs no Landlock at all.
pub(crate) fn fs_confines(fs: &FsPolicy) -> bool {
    fs.rules.default_effect != Effect::Allow || !fs.rules.entries.is_empty()
}

/// Whether any path is write-granted (an `Allow` carrying `ReadWrite`).
pub(crate) fn write_confines(fs: &FsPolicy) -> bool {
    fs.rules
        .entries
        .iter()
        .any(|r| r.effect == Effect::Allow && r.access == FsAccess::ReadWrite)
}

/// Derive the READ grants. Never grants `/proc`/`/sys` (env-read boundary).
pub(crate) fn derive_read_grants(policy: &SandboxPolicy) -> DerivedGrants {
    let mut out = DerivedGrants::default();
    if !fs_confines(&policy.fs) {
        return out; // relaxed read — no Landlock read-confine
    }
    let view = View::read(&policy.fs.rules);
    let mut visits = 0usize;
    let (roots, whole_fs) = view.allow_roots();
    if whole_fs {
        // Generous `**` read: seed from `/`'s top-levels. System dirs are granted
        // clean (documented threat-model call — user secrets live under home/…,
        // which ARE walked); `/proc`,`/sys` are never granted.
        for top in read_top_levels() {
            if is_proc_or_sys(&top) {
                continue;
            }
            // A system top-level is granted CLEAN only when the ONLY denies reaching
            // inside it are the BUILT-IN `.env*` secret globs — those are
            // deliberately not carved across system dirs (no user `.env` there;
            // net-deny blocks exfil regardless). A USER-authored deny reaching inside
            // (an absolute `!/etc/**`, or the whole-fs `!**`) forces the full carve so
            // an explicit deny is never silently overridden.
            if is_system_toplevel(&top)
                && view.allows(&top)
                && !view.has_nonbuiltin_deny_reaching(&top)
            {
                out.grants.push(Grant {
                    path: top,
                    kind: GrantKind::ReadSubtree,
                });
            } else {
                walk_read(&top, &view, &mut out, &mut visits);
            }
        }
    } else {
        for root in roots {
            walk_read(&root, &view, &mut out, &mut visits);
        }
    }
    dedup(&mut out.grants);
    out
}

/// Derive the WRITE grants (read+write subtrees) plus a `write_partial` honesty
/// flag. The caller pre-creates a not-yet-existing write root before installing.
#[cfg(test)]
pub(crate) fn derive_write_grants(policy: &SandboxPolicy) -> (Vec<Grant>, bool) {
    if !write_confines(&policy.fs) {
        return (Vec::new(), false);
    }
    let view = View::write(&policy.fs.rules);
    // `whole_fs` (a `/`/`**` rw spelling) is deliberately ignored for writes — it is
    // the dangerous `/` root, dropped by the guard below.
    let (roots, _whole_fs) = view.allow_roots();
    let mut out = Vec::new();
    let mut visits = 0usize;
    let mut partial = false;
    // A write grant that resolves to a dangerous top-level root is DROPPED, not
    // honored — fail-safe, mirroring the macOS/Windows `is_dangerous_write_root`
    // guard so the over-grant is closed identically on every OS. WHY: a `..` in a
    // surface path collapses lexically at compile time (`<proj>/../../..` → `/`), so
    // an accidental (or hostile) path can widen a scoped rw grant into a
    // filesystem-wide write hole; an explicit `/`/`**` rw grant is the same hole.
    // Reads are exempt (a generous `(subpath "/")` read is a legitimate posture).
    // The whole-fs spelling IS the `/` root: its grant-everything seeding is dropped
    // entirely, while any EXPLICIT non-dangerous root in the same policy still
    // applies (mirrors macOS dropping only the `/` rule, not the co-listed scoped
    // grants).
    for root in roots {
        if is_dangerous_write_root(&root) {
            continue;
        }
        walk_write(&root, &view, &mut out, &mut visits, &mut partial);
    }
    dedup(&mut out);
    (out, partial)
}

/// Derive Bubblewrap's coarse writable bind roots. Unlike Landlock's allow-only
/// rules, a mount plan can bind the authored writable root once and then layer exact
/// deny mounts over it. This keeps setup independent of the number of project files.
pub(crate) fn derive_write_mount_grants(policy: &SandboxPolicy) -> (Vec<Grant>, bool) {
    if !write_confines(&policy.fs) {
        return (Vec::new(), false);
    }

    let mut grants = Vec::new();
    let mut partial = false;
    for rule in &policy.fs.rules.entries {
        if rule.effect != Effect::Allow || rule.access != FsAccess::ReadWrite {
            continue;
        }
        let pattern = rule.matcher.as_str();
        if pattern.is_empty() || matches!(pattern, "**" | "/**" | "/") {
            continue;
        }
        let trimmed = pattern.strip_suffix("/**").unwrap_or(pattern);
        if trimmed.is_empty() || trimmed.contains(['*', '?', '[', '{']) {
            partial = true;
            continue;
        }
        let path = PathBuf::from(trimmed);
        if is_dangerous_write_root(&path) {
            continue;
        }
        grants.push(Grant {
            path,
            kind: GrantKind::WriteSubtree,
        });
    }

    grants.sort_by(|a, b| {
        a.path
            .components()
            .count()
            .cmp(&b.path.components().count())
            .then_with(|| a.path.cmp(&b.path))
    });
    let mut collapsed: Vec<Grant> = Vec::new();
    for grant in grants {
        if collapsed
            .iter()
            .any(|parent| grant.path.starts_with(&parent.path))
        {
            continue;
        }
        collapsed.push(grant);
    }
    (collapsed, partial)
}

/// Whether an explicit USER-authored (non-`.env*`-builtin) deny reaches into the
/// essential system dir `dir`. The Linux backend grants the essential loader set
/// (`/usr`,`/etc`,…) WHOLESALE so a dynamically-linked child can exec/link; but an
/// explicit deny landing inside one (`!/etc/secret`) must still carve — otherwise a
/// secret placed under `/etc` stays readable despite the user asking to deny it.
/// Built-in `.env*` globs are excluded: they never target system dirs, and skipping
/// them keeps the wholesale fast-path for the common no-deny case.
#[cfg(test)]
pub(crate) fn essential_dir_needs_carve(policy: &SandboxPolicy, dir: &Path) -> bool {
    View::essential(&policy.fs.rules).has_nonbuiltin_deny_reaching(dir)
}

/// Carve a single essential system dir under the IMPLICIT-ALLOW view: the dir would
/// otherwise be granted wholesale for the loader, so the base effect is `Allow` and
/// the policy's rules overlay it — a clean subtree is granted whole, and only a
/// subtree a user deny reaches is descended into, excluding the denied file. The
/// loader's files (`/etc/ld.so.cache`, `resolv.conf`, CA bundles) stay readable
/// while `!/etc/secret` is honored. Budget-capped like [`derive_read_grants`]; on
/// overflow the remainder is left ungranted (fail-safe) and `read_partial` is set.
#[cfg(test)]
pub(crate) fn derive_essential_dir_carve(policy: &SandboxPolicy, dir: &Path) -> DerivedGrants {
    let mut out = DerivedGrants::default();
    let view = View::essential(&policy.fs.rules);
    let mut visits = 0usize;
    walk_read(dir, &view, &mut out, &mut visits);
    dedup(&mut out.grants);
    out
}

// ── the carve walks ────────────────────────────────────────────────────────────

/// Directory entries, SORTED by path, symlinks dropped — so the walk (and thus the
/// grant set) is deterministic regardless of `read_dir` iteration order (a budget
/// overflow must not depend on FS ordering). `Err` (unreadable/nonexistent) → empty.
fn sorted_entries(dir: &Path) -> Vec<(std::fs::FileType, PathBuf)> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut v: Vec<_> = rd
        .flatten()
        .filter_map(|e| e.file_type().ok().map(|ft| (ft, e.path())))
        .filter(|(ft, _)| !ft.is_symlink())
        .collect();
    v.sort_by(|a, b| a.1.cmp(&b.1));
    v
}

/// Recursively grant read under `dir`. A subtree whose verdict no later rule can
/// flip is granted whole (`ReadSubtree`, walk stops). Where a later rule reaches
/// inside, `dir` gets a `ReadDir` (stays listable) and each child is handled
/// individually — allowed file → `ReadFile`, subdir → recurse — so a denied file is
/// never file-granted. `/proc`/`/sys` and symlinks are never followed.
fn walk_read(dir: &Path, view: &View, out: &mut DerivedGrants, visits: &mut usize) {
    if is_proc_or_sys(dir) {
        return;
    }
    *visits += 1;
    if *visits > MAX_VISITS || out.grants.len() > MAX_GRANTS {
        out.read_partial = true;
        return;
    }
    let (widx, weff) = view.decide(dir);
    let carved = view.later_reaches(dir, widx, weff);
    if !carved {
        // Uniform subtree: grant whole iff allowed; if denied, grant nothing.
        if weff == Effect::Allow {
            out.grants.push(Grant {
                path: dir.to_path_buf(),
                kind: GrantKind::ReadSubtree,
            });
        }
        return;
    }
    // A later rule can flip the verdict inside — carve. Never follow symlinks: a
    // link's target is granted (or not) on its own merits — a link to a denied
    // secret stays denied because its resolved inode is not in the grant set.
    if weff == Effect::Allow {
        out.grants.push(Grant {
            path: dir.to_path_buf(),
            kind: GrantKind::ReadDir,
        });
    }
    for (ft, child) in sorted_entries(dir) {
        if ft.is_dir() {
            walk_read(&child, view, out, visits);
        } else if ft.is_file() && view.allows(&child) {
            out.grants.push(Grant {
                path: child,
                kind: GrantKind::ReadFile,
            });
        }
    }
}

/// Write carve — grant `WriteSubtree` on a uniform allowed subtree; where a later
/// rule reaches inside, recurse and grant each allowed child (no dir-list analog for
/// write). Creating a NEW entry directly in a CARVED directory is not grantable
/// without also granting the denied sibling, so that narrow corner is left denied
/// (fail-safe) — existing allowed subtrees/files stay writable. Budget-capped like
/// the read walk: on overflow the remainder is left ungranted and `partial` is set.
#[cfg(test)]
fn walk_write(
    dir: &Path,
    view: &View,
    out: &mut Vec<Grant>,
    visits: &mut usize,
    partial: &mut bool,
) {
    if is_proc_or_sys(dir) {
        return;
    }
    *visits += 1;
    if *visits > MAX_VISITS || out.len() > MAX_GRANTS {
        *partial = true;
        return;
    }
    let (widx, weff) = view.decide(dir);
    if !view.later_reaches(dir, widx, weff) {
        // Uniform subtree (incl. a not-yet-existing write root — linux.rs pre-creates
        // it before opening) → grant whole iff allowed.
        if weff == Effect::Allow {
            out.push(Grant {
                path: dir.to_path_buf(),
                kind: GrantKind::WriteSubtree,
            });
        }
        return;
    }
    for (ft, child) in sorted_entries(dir) {
        if ft.is_dir() {
            walk_write(&child, view, out, visits, partial);
        } else if ft.is_file() && view.allows(&child) {
            out.push(Grant {
                path: child,
                kind: GrantKind::WriteSubtree,
            });
        }
    }
}

// ── the projected view (read or write) with order-aware decisions ──────────────

/// One compiled ruleset entry: its glob (compiled + raw), its literal-prefix
/// analysis, and its effect.
struct Entry {
    matcher: GlobMatcher,
    glob: String,
    effect: Effect,
}

/// A read- or write-projection of the fs ruleset, compiled for matching. Owns the
/// order-aware decisions the carve relies on.
struct View {
    entries: Vec<Entry>,
    default_effect: Effect,
}

impl View {
    /// The read view: an `Allow` of any access is readable; a `Deny` denies read.
    fn read(set: &FsRuleSet) -> Self {
        Self::project(set, set.default_effect, |r| r.effect)
    }

    /// The write view: only an `Allow` carrying `ReadWrite` grants write; a
    /// read-only allow or a deny caps it. Base is always deny (the caller gates on
    /// `write_confines`, so a relaxed axis never reaches here).
    ///
    /// The builtin `.env*` denies are a READ deny only ("writes follow the normal
    /// write-deny-default") — so they are DROPPED from the write projection. Keeping
    /// them would make a depth-independent deny "reach into" every rw subtree and force
    /// a write CARVE, which on allow-only Landlock cannot re-grant a NEW file in the
    /// carved dir — breaking new-file creation under a broad rw grant for zero write
    /// security (a `.env*` file's read-exfil is already closed by the read carve, which
    /// is rename-safe: a carved dir grants only its enumerated children, so renaming
    /// `.env` to a fresh name yields no read grant). A USER-authored `!.env` deny is
    /// NOT builtin and still caps write normally.
    #[cfg(test)]
    fn write(set: &FsRuleSet) -> Self {
        Self::project_where(
            set,
            Effect::Deny,
            |r| match (r.effect, r.access) {
                (Effect::Allow, FsAccess::ReadWrite) => Effect::Allow,
                _ => Effect::Deny,
            },
            |r| !(r.effect == Effect::Deny && is_builtin_env_glob(r.matcher.as_str())),
        )
    }

    /// The essential-dir carve view: an implicit-allow base (the loader dir would be
    /// granted wholesale) with the policy's rules overlaid, so a USER deny inside a
    /// system dir carves it while the loader keeps the rest. An `Allow` of any access
    /// is readable; a `Deny` denies read (same read projection as [`View::read`], only
    /// the default flips to `Allow`).
    #[cfg(test)]
    fn essential(set: &FsRuleSet) -> Self {
        Self::project(set, Effect::Allow, |r| r.effect)
    }

    fn project(
        set: &FsRuleSet,
        default_effect: Effect,
        effect_of: impl Fn(&crate::policy::FsRule) -> Effect,
    ) -> Self {
        Self::project_where(set, default_effect, effect_of, |_| true)
    }

    /// [`project`] with an entry FILTER — an entry for which `keep` is false is dropped
    /// from the view entirely (not merely re-effected), so it neither wins a decision
    /// nor triggers a carve. Used by the write view to drop the read-only builtin
    /// `.env*` denies.
    fn project_where(
        set: &FsRuleSet,
        default_effect: Effect,
        effect_of: impl Fn(&crate::policy::FsRule) -> Effect,
        keep: impl Fn(&crate::policy::FsRule) -> bool,
    ) -> Self {
        let entries = set
            .entries
            .iter()
            .filter(|r| keep(r))
            .filter_map(|r| {
                compile_glob(r.matcher.as_str()).ok().map(|m| Entry {
                    matcher: m,
                    glob: r.matcher.as_str().to_string(),
                    effect: effect_of(r),
                })
            })
            .collect();
        Self {
            entries,
            default_effect,
        }
    }

    /// The winning (last-matching) entry index + effect for `path`, canonicalized
    /// the same way the runtime matcher canonicalizes candidates (symlinks / `..` /
    /// firmlinks resolved) so a spelling can't dodge a deny.
    fn decide(&self, path: &Path) -> (Option<usize>, Effect) {
        let norm = normalize_slashes(&canonicalize_including_nonexistent(path).to_string_lossy());
        let mut winner = None;
        for (i, e) in self.entries.iter().enumerate() {
            if e.matcher.is_match(&norm) {
                winner = Some((i, e.effect));
            }
        }
        match winner {
            Some((i, eff)) => (Some(i), eff),
            None => (None, self.default_effect),
        }
    }

    fn allows(&self, path: &Path) -> bool {
        self.decide(path).1 == Effect::Allow
    }

    /// Whether any deny that is NOT a built-in `.env*` secret glob reaches inside
    /// `dir`. Used by the generous-`**` system-dir seeding: the built-in `.env*`
    /// carve is skipped across system dirs, but a USER-authored deny (literal or
    /// depth-independent) reaching inside must force the carve so it isn't silently
    /// overridden.
    fn has_nonbuiltin_deny_reaching(&self, dir: &Path) -> bool {
        self.entries.iter().any(|e| {
            e.effect == Effect::Deny
                && !is_builtin_env_glob(&e.glob)
                && glob_reaches_under(&e.glob, dir)
        })
    }

    /// Whether an entry ordered AFTER `after` could FLIP `dir`'s verdict somewhere
    /// inside its subtree — the trigger to carve rather than grant `dir` whole. Only
    /// a later rule of the OPPOSITE effect can flip it (a same-effect later rule,
    /// e.g. the `/x` + `/x/**` subtree twin, only re-affirms), and only if its glob
    /// could reach inside `dir`. `None` (the default won for `dir`) means every entry
    /// is "after". Conservative on the glob side (a possible-match) so it never
    /// under-carves; may over-carve, which stays correct (children re-decided).
    fn later_reaches(&self, dir: &Path, after: Option<usize>, current: Effect) -> bool {
        let threshold = after.map(|i| i as isize).unwrap_or(-1);
        self.entries.iter().enumerate().any(|(i, e)| {
            (i as isize) > threshold && e.effect != current && glob_reaches_under(&e.glob, dir)
        })
    }

    /// The concrete directory roots to seed the read walk from, plus `whole_fs` when
    /// a generous `**`/`/` allow means "start at the top-levels of `/`".
    fn allow_roots(&self) -> (Vec<PathBuf>, bool) {
        let mut roots = Vec::new();
        let mut whole_fs = false;
        for e in &self.entries {
            if e.effect != Effect::Allow {
                continue;
            }
            match literal_prefix(&e.glob) {
                Prefix::WholeFs => whole_fs = true,
                Prefix::Dir(p) => roots.push(p),
                Prefix::Relative => {}
            }
        }
        roots.sort();
        roots.dedup();
        (roots, whole_fs)
    }
}

// ── glob prefix analysis ───────────────────────────────────────────────────────

enum Prefix {
    /// A whole-filesystem glob (`**`, `/**`, `/`).
    WholeFs,
    /// An absolute literal directory prefix (before the first glob metachar).
    Dir(PathBuf),
    /// A relative / rootless glob — no concrete seed (matches by basename anywhere).
    Relative,
}

/// The literal absolute directory prefix of a canonical glob: `/a/b/**` → `/a/b`,
/// `/a/b` → `/a/b`, `/a/*.pem` → `/a`, `**` → whole-fs, `**/.env` → relative.
fn literal_prefix(glob: &str) -> Prefix {
    if glob == "**" || glob == "/**" || glob == "/" {
        return Prefix::WholeFs;
    }
    if !glob.starts_with('/') {
        return Prefix::Relative;
    }
    let end = match glob.find(['*', '?', '[', '{']) {
        Some(i) => glob[..i].rfind('/').map(|s| s + 1).unwrap_or(0),
        None => glob.len(),
    };
    let literal = glob[..end].trim_end_matches('/');
    if literal.is_empty() {
        Prefix::WholeFs
    } else {
        Prefix::Dir(PathBuf::from(literal))
    }
}

/// The built-in `.env*` deny globs the compiler injects on every read-granting policy
/// (the shared `crate::compiler::ENV_DENY_GLOBS` — single source of truth, no drift).
/// These are the ONLY denies the generous-`**` system-dir seeding is allowed to skip —
/// user secrets live in home/project (always walked), not under `/usr`,`/etc`,…; a
/// USER-authored deny is never skipped.
fn is_builtin_env_glob(glob: &str) -> bool {
    crate::compiler::ENV_DENY_GLOBS.contains(&glob)
}

/// Whether a glob could match some path inside `dir`'s subtree. Depth-independent
/// (rootless) and whole-fs globs can match anywhere → true; a literal-anchored glob
/// reaches `dir` iff their subtrees overlap. Conservative (possible-match) so the
/// carve never misses a deny.
fn glob_reaches_under(glob: &str, dir: &Path) -> bool {
    match literal_prefix(glob) {
        Prefix::WholeFs | Prefix::Relative => true,
        Prefix::Dir(p) => p.starts_with(dir) || dir.starts_with(&p),
    }
}

// ── filesystem roots / classification ──────────────────────────────────────────

/// `/proc` and `/sys` are NEVER granted: `/proc/<pid>/environ` is the ascendant-env
/// leak the env-read boundary closes (design.md §2.4), and `/sys` is refused as
/// defense-in-depth (no build needs it read). Hard filter, not policy-overridable.
fn is_proc_or_sys(path: &Path) -> bool {
    path.starts_with("/proc") || path.starts_with("/sys")
}

/// Top-levels of `/` EXCLUDING `/proc`,`/sys` — the "relaxed fs but close /proc"
/// grant set for an env-scrub-only policy. The env-read boundary requires `/proc`
/// be unreadable whenever env is scrubbed (else a scrubbed child recovers the
/// ancestor's env via `/proc/<ppid>/environ`), so even a policy that does NOT
/// confine fs installs this ruleset: everything readable/writable except `/proc`,
/// `/sys` — preserving "fs relaxed" while closing the ancestor-env file vector.
#[cfg(test)]
pub(crate) fn relaxed_top_levels_except_proc_sys() -> Vec<PathBuf> {
    read_top_levels()
        .into_iter()
        .filter(|p| !is_proc_or_sys(p))
        .collect()
}

/// Top-level directories of `/`, for the generous `**` seeding. Empty if `/` can't
/// be read; symlinked top-levels are skipped.
fn read_top_levels() -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir("/") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.path())
        .collect()
}

/// Top-level roots a WRITE grant must never cover — the Linux twin of the macOS /
/// Windows `is_dangerous_write_root`. A `..`-collapsed surface path can resolve to a
/// system root (`/`, `/etc`, `/usr`, …) or the all-users home container (`/home`),
/// which would turn a scoped rw grant into a filesystem-wide write hole; drop it
/// fail-safe instead of emitting it. The entries are the CANONICAL forms the grant
/// derivation sees — the matcher prefix is already run through
/// `canonicalize_glob_prefix`, so a usr-merged distro's `/bin`→`/usr/bin` symlink is
/// resolved here (both spellings listed). `/tmp` is deliberately ABSENT: it is the
/// legitimate broad temp write target (mirrors macOS excluding `/private/tmp`). An
/// EXACT top-level match only — a nested grant like `/home/<user>/proj` is fine, just
/// as macOS drops `/Users` but not `/Users/<user>`.
fn is_dangerous_write_root(path: &Path) -> bool {
    let Some(s) = path.to_str() else { return false };
    let s = normalize_slashes(s);
    let s = s.trim_end_matches('/');
    matches!(
        s,
        ""  // an empty trimmed string is `/` itself
            | "/"
            | "/usr"
            | "/usr/bin"
            | "/usr/sbin"
            | "/usr/lib"
            | "/usr/lib32"
            | "/usr/lib64"
            | "/usr/libx32"
            | "/usr/local"
            | "/bin"
            | "/sbin"
            | "/lib"
            | "/lib32"
            | "/lib64"
            | "/libx32"
            | "/etc"
            | "/opt"
            | "/boot"
            | "/var"
            | "/home"
            | "/root"
            | "/dev"
            | "/proc"
            | "/sys"
            | "/run"
            | "/srv"
            | "/mnt"
            | "/media"
    )
}

/// A system top-level (no user secrets) — granted clean under a generous read.
fn is_system_toplevel(path: &Path) -> bool {
    // Compare on the final path component (a top-level of `/`).
    matches!(
        path.components().next_back(),
        Some(Component::Normal(seg)) if SYSTEM_TOPLEVELS.iter().any(|s| *s == seg)
    )
}

/// Drop exact-duplicate grants (overlapping allow roots are common; Landlock unions
/// rules anyway, this just trims the rule count).
fn dedup(grants: &mut Vec<Grant>) {
    let mut seen = std::collections::HashSet::new();
    grants.retain(|g| seen.insert((g.path.clone(), g.kind)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler::{CompileCtx, ShellRunner};
    use crate::matcher::Homes;
    use serde_json::Value;
    use std::collections::BTreeMap;
    use std::fs;
    use tempfile::TempDir;

    // A fixture tree: project (with .env + nested .env + a writable subdir) and a
    // fake home (with .ssh) — so secret denies target fixture paths, never real ~.
    struct Fixture {
        _tmp: TempDir,
        root: PathBuf,
        proj: PathBuf,
        home: PathBuf,
    }

    fn fixture() -> Fixture {
        let tmp = TempDir::new().unwrap();
        // Canonicalize so grant paths compare against the canonical form the matcher
        // produces (macOS /var→/private/var etc.).
        let root = fs::canonicalize(tmp.path()).unwrap();
        let proj = root.join("proj");
        let home = root.join("home");
        fs::create_dir_all(proj.join("sub")).unwrap();
        fs::create_dir_all(proj.join("writable")).unwrap();
        fs::create_dir_all(home.join(".ssh")).unwrap();
        fs::write(proj.join("pub.txt"), "PUBLIC").unwrap();
        fs::write(proj.join("sub/nested.txt"), "N").unwrap();
        fs::write(proj.join(".env"), "SECRET").unwrap();
        fs::write(proj.join("sub/.env"), "SECRET").unwrap();
        fs::write(home.join(".ssh/id_rsa"), "KEY").unwrap();
        Fixture {
            _tmp: tmp,
            root,
            proj,
            home,
        }
    }

    impl Fixture {
        fn homes(&self) -> Homes {
            Homes {
                home: self.home.clone(),
                tmp: std::env::temp_dir(),
                cache: self.home.join(".cache"),
                project: self.proj.clone(),
            }
        }
        fn compile(&self, surface: Value) -> SandboxPolicy {
            let ctx = CompileCtx {
                homes: self.homes(),
                cwd: self.proj.clone(),
                trusted: true,
                ambient_env: BTreeMap::new(),
                runner: Box::new(ShellRunner),
            };
            crate::compile(&surface, &ctx).expect("compiles")
        }
    }

    /// Whether a grant-set makes `path` file-READABLE (a ReadSubtree covering it or a
    /// ReadFile on it; a ReadDir alone only makes a directory listable).
    fn read_reachable(grants: &[Grant], path: &Path) -> bool {
        grants.iter().any(|g| match g.kind {
            GrantKind::ReadSubtree => path.starts_with(&g.path),
            GrantKind::ReadFile => g.path == path,
            _ => false,
        })
    }
    fn write_reachable(grants: &[Grant], path: &Path) -> bool {
        grants
            .iter()
            .any(|g| g.kind == GrantKind::WriteSubtree && path.starts_with(&g.path))
    }

    #[test]
    fn allowlist_read_confine_grants_project_excludes_outside() {
        let f = fixture();
        let d = derive_read_grants(&f.compile(serde_json::json!({ "fs": ["./"] })));
        assert!(!d.read_partial);
        assert!(read_reachable(&d.grants, &f.proj.join("pub.txt")));
        assert!(read_reachable(&d.grants, &f.proj.join("sub/nested.txt")));
        // Outside the allowlist is closed by EXCLUSION — no grant covers it.
        assert!(!read_reachable(&d.grants, &f.home.join(".ssh/id_rsa")));
        assert!(d.grants.iter().all(|g| !g.path.starts_with("/proc")));
    }

    #[test]
    fn bounded_generous_read_carves_dotenv_and_ssh() {
        // A BOUNDED generous read (allow the fixture root, deny .env at any depth +
        // ~/.ssh) exercises the carve on a small tree — the whole-fs `["..."]`
        // seeding walks the real `/` and is proven in the VM enforcement suite.
        let f = fixture();
        let surface = serde_json::json!({
            "fs": [f.root.to_str().unwrap(), "!**/.env", "!~/.ssh"]
        });
        let d = derive_read_grants(&f.compile(surface));
        assert!(read_reachable(&d.grants, &f.proj.join("pub.txt")));
        assert!(read_reachable(&d.grants, &f.proj.join("sub/nested.txt")));
        assert!(
            !read_reachable(&d.grants, &f.proj.join(".env")),
            "project .env must not be read-granted"
        );
        assert!(
            !read_reachable(&d.grants, &f.proj.join("sub/.env")),
            "nested .env must not be read-granted"
        );
        assert!(
            !read_reachable(&d.grants, &f.home.join(".ssh/id_rsa")),
            "~/.ssh key must not be read-granted"
        );
        // The project dir stays LISTABLE even though .env is carved out of it.
        assert!(
            d.grants
                .iter()
                .any(|g| g.kind == GrantKind::ReadDir && g.path == f.proj),
            "carved project dir keeps a ReadDir (listable)"
        );
    }

    #[test]
    fn write_confine_grants_only_writable_subdir() {
        let f = fixture();
        // Generous read + write only to ./writable. The generous `**` read is a
        // baseline (ordered first), so the later rw grant is uniform → whole subtree.
        let (g, _partial) =
            derive_write_grants(&f.compile(serde_json::json!({ "fs": ["...", "./writable"] })));
        assert!(write_reachable(&g, &f.proj.join("writable/out.txt")));
        assert!(!write_reachable(&g, &f.proj.join("pub.txt")));
        assert!(!write_reachable(&g, &f.home.join("x")));
        assert!(g.iter().all(|x| !x.path.starts_with("/proc")));
    }

    #[test]
    fn dangerous_write_root_yields_no_write_grant() {
        let f = fixture();
        // An explicit `/` rw grant must not become a write-everything grant: the
        // whole-fs spelling is the dangerous `/` root, dropped fail-safe.
        let (g, _) = derive_write_grants(&f.compile(serde_json::json!({ "fs": ["...", "/"] })));
        assert!(g.is_empty(), "`/` rw must yield no write grant");

        // A dangerous top-level (`/etc`) is dropped while a co-listed SCOPED grant
        // survives — the guard filters only the dangerous root, not the whole policy.
        let (g, _) = derive_write_grants(
            &f.compile(serde_json::json!({ "fs": ["...", "/etc", "./writable"] })),
        );
        assert!(
            write_reachable(&g, &f.proj.join("writable/out.txt")),
            "scoped grant survives alongside a dropped dangerous root"
        );
        assert!(
            g.iter().all(|x| !x.path.starts_with("/etc")),
            "no write grant under the dropped `/etc` root"
        );
    }

    #[test]
    fn nested_write_deny_is_carved_out() {
        let f = fixture();
        // Allow rw the whole project, deny ./sub — sub must be excluded from write.
        let (g, _partial) =
            derive_write_grants(&f.compile(serde_json::json!({ "fs": ["./", "!./sub"] })));
        assert!(write_reachable(&g, &f.proj.join("pub.txt")));
        assert!(
            !write_reachable(&g, &f.proj.join("sub/x")),
            "a nested write-deny must not be write-granted"
        );
    }

    #[test]
    fn mount_write_plan_keeps_one_project_bind_and_leaves_denies_to_masks() {
        let f = fixture();
        let denied = format!("!{}", f.proj.join("policy.sandbox.json").display());
        let (grants, partial) = derive_write_mount_grants(
            &f.compile(serde_json::json!({ "fs": ["...", "./", denied] })),
        );
        assert!(!partial);
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].path, f.proj);
    }

    #[test]
    fn mount_write_plan_rejects_wildcard_widening() {
        let f = fixture();
        let (_grants, partial) = derive_write_mount_grants(
            &f.compile(serde_json::json!({ "fs": { "./packages/*": "rw" } })),
        );
        assert!(
            partial,
            "a wildcard write cannot become a parent-directory bind"
        );
    }

    #[test]
    fn mount_write_plan_drops_whole_filesystem_without_partial_failure() {
        let f = fixture();
        let policy = f.compile(serde_json::json!({ "fs": ["...", "/"] }));
        let (grants, partial) = derive_write_mount_grants(&policy);
        assert!(
            !partial,
            "whole-fs spellings should be dropped, not treated as wildcard widening: {:?}",
            policy
                .fs
                .rules
                .entries
                .iter()
                .map(|r| (r.matcher.as_str(), r.effect, r.access))
                .collect::<Vec<_>>()
        );
        assert!(grants.is_empty());
    }

    #[test]
    fn relaxed_fs_yields_no_grants() {
        let f = fixture();
        let d = derive_read_grants(&f.compile(serde_json::json!({ "fs": true })));
        assert!(d.grants.is_empty(), "relaxed read needs no Landlock grants");
        assert!(
            derive_write_grants(&f.compile(serde_json::json!({ "fs": true })))
                .0
                .is_empty()
        );
    }

    #[test]
    fn explicit_proc_allow_is_never_granted() {
        let f = fixture();
        // Even an explicit allow of /proc must not produce a /proc grant.
        let d = derive_read_grants(&f.compile(serde_json::json!({ "fs": ["/proc"] })));
        assert!(d.grants.iter().all(|g| !g.path.starts_with("/proc")));
    }

    #[test]
    fn trailing_deny_all_overrides_earlier_allow() {
        // `[allow root, !**]` — the trailing deny-all wins for every path, so nothing
        // under root is read-granted (order-aware last-match-wins).
        let f = fixture();
        let surface = serde_json::json!({ "fs": [f.root.to_str().unwrap(), "!**"] });
        let d = derive_read_grants(&f.compile(surface));
        assert!(
            !read_reachable(&d.grants, &f.proj.join("pub.txt")),
            "a trailing !** must deny even an earlier allow"
        );
    }

    #[test]
    fn read_grant_derivation_is_deterministic() {
        // Sorted walk → the grant set is identical across runs (a budget overflow
        // must not depend on read_dir iteration order).
        let f = fixture();
        let surface = serde_json::json!({ "fs": [f.root.to_str().unwrap(), "!**/.env"] });
        let a = derive_read_grants(&f.compile(surface.clone())).grants;
        let b = derive_read_grants(&f.compile(surface)).grants;
        assert_eq!(a, b, "derivation must be deterministic");
    }

    #[test]
    fn relaxed_top_levels_exclude_proc_and_sys() {
        // The env-scrub-only relaxed grant set never includes /proc or /sys.
        let tops = relaxed_top_levels_except_proc_sys();
        assert!(
            tops.iter()
                .all(|p| !p.starts_with("/proc") && !p.starts_with("/sys"))
        );
    }

    #[test]
    fn builtin_env_glob_recognition() {
        // The compiler-injected `.env*` deny set (basename-prefix, leaf + subtree) must
        // be recognized as builtin, or the generous-`**` seeding would treat it as a
        // user deny and over-carve every system dir (a perf regression). Kept in sync
        // via the shared `compiler::ENV_DENY_GLOBS` const, not a local copy.
        assert!(is_builtin_env_glob("**/.env*"));
        assert!(is_builtin_env_glob("**/.env*/**"));
        assert!(is_builtin_env_glob(".env*"));
        // A user-authored deny (even a `.env`-shaped literal) is NOT the builtin set.
        assert!(!is_builtin_env_glob("**/.env"));
        assert!(!is_builtin_env_glob("**/*.pem"));
        assert!(!is_builtin_env_glob("/etc/secret"));
    }

    #[test]
    fn essential_dir_carve_honors_user_deny_keeps_rest() {
        // The essential loader dirs (/usr,/etc,…) are granted wholesale, but an
        // explicit USER deny landing inside one must CARVE — a secret placed under
        // /etc is not silently readable despite the deny. Exercised on a fixture
        // "etc" dir (the derivation is dir-agnostic; the real /etc close is proven by
        // an ad-hoc VM run). Regression guard for the LIMITATIONS.md "/etc granted
        // wholesale (no deny-inside carve)" residual now that the carve exists.
        let f = fixture();
        let etc = f.root.join("etc");
        fs::create_dir_all(&etc).unwrap();
        fs::write(etc.join("resolv.conf"), "nameserver 1.1.1.1").unwrap();
        fs::write(etc.join("secret.txt"), "ETCSECRET").unwrap();
        let deny = format!("!{}", etc.join("secret.txt").to_str().unwrap());
        let policy = f.compile(serde_json::json!({ "fs": [f.root.to_str().unwrap(), deny] }));
        assert!(
            essential_dir_needs_carve(&policy, &etc),
            "a user deny reaching an essential dir must trigger the carve"
        );
        let d = derive_essential_dir_carve(&policy, &etc);
        assert!(
            read_reachable(&d.grants, &etc.join("resolv.conf")),
            "loader-essential files stay readable under the essential-dir carve"
        );
        assert!(
            !read_reachable(&d.grants, &etc.join("secret.txt")),
            "the explicitly-denied secret under the essential dir is carved out"
        );
    }

    #[test]
    fn essential_dir_without_user_deny_stays_wholesale() {
        // With no user deny reaching it, an essential dir stays on the wholesale
        // fast-path — the built-in `.env*` secret globs must NOT force a carve (they
        // never target system dirs, and forcing a carve would regress the common
        // generous-read path).
        let f = fixture();
        let etc = f.root.join("etc");
        fs::create_dir_all(&etc).unwrap();
        let policy = f.compile(serde_json::json!({ "fs": ["..."] }));
        assert!(
            !essential_dir_needs_carve(&policy, &etc),
            "the built-in .env carve must not force an essential-dir carve"
        );
    }

    #[test]
    fn env_prefixed_directory_allow_does_not_grant_its_secret_contents() {
        // THE SUB-DIRECTORY BYPASS on Landlock: a `.env*`-NAMED directory is a secret
        // container. Naming the directory (`{ "./.env.d": "r" }`) keeps the dir LISTABLE
        // (ReadDir) but must NOT read-grant its `.env*/**`-denied contents — the carve
        // descends and skips the denied child. The subtree deny (band 2c) is ordered after
        // the exact-file allows, so no re-emitted directory subtree twin can re-grant them.
        let f = fixture();
        let envd = f.proj.join(".env.d");
        fs::create_dir_all(&envd).unwrap();
        fs::write(envd.join("prod"), "DIRSECRET").unwrap();
        let allow = f.proj.join(".env.d").to_str().unwrap().to_string();
        let d = derive_read_grants(&f.compile(serde_json::json!({ "fs": { allow: "r" } })));
        assert!(
            !read_reachable(&d.grants, &envd.join("prod")),
            "a .env.d directory allow must not read-grant the secret inside it"
        );
        // The directory node itself stays listable (a ReadDir, not a subtree read).
        assert!(
            d.grants
                .iter()
                .any(|g| g.kind == GrantKind::ReadDir && g.path == envd),
            "the .env.d directory stays listable"
        );
    }

    #[test]
    fn literal_prefix_extraction() {
        assert!(matches!(literal_prefix("**"), Prefix::WholeFs));
        assert!(matches!(literal_prefix("/"), Prefix::WholeFs));
        assert!(matches!(literal_prefix("/a/b/**"), Prefix::Dir(p) if p == Path::new("/a/b")));
        assert!(matches!(literal_prefix("/a/b"), Prefix::Dir(p) if p == Path::new("/a/b")));
        assert!(matches!(literal_prefix("/a/*.pem"), Prefix::Dir(p) if p == Path::new("/a")));
        assert!(matches!(literal_prefix("**/.env"), Prefix::Relative));
    }
}
