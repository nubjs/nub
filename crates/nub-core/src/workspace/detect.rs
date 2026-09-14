//! Workspace and project root detection.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::SystemTime;

/// A detected workspace or standalone project.
#[derive(Debug, Clone)]
pub struct Project {
    /// The project root (nearest package.json).
    pub root: PathBuf,
    /// The workspace root, if different from root.
    pub workspace_root: Option<PathBuf>,
    /// Parsed package.json at root.
    pub manifest: serde_json::Value,
}

/// Walk up from `cwd` to find the project root and workspace root.
///
/// The expensive part — a 32-ancestor directory walk, a `read_to_string` + a
/// serde parse of every `package.json` it passes, and two manifest clones — runs
/// 3–4× per command on the common `nub <file>` path (pin resolution via
/// [`crate::node::discovery`] calls it for `devEngines.runtime`, `engines.node`,
/// and the disagreement warning; the file runner calls it again for `.env`). It
/// is invoked on a CONSTANT cwd each time, so every call after the first repeats
/// identical work. This memoizes the result per process, keyed on the
/// canonicalized cwd.
///
/// Correctness mirrors [`crate::config_cache::MtimeCache`]: the cached `Project`
/// is served only when EVERY filesystem input the walk read to produce it still
/// reports the freshness stamp observed when it was cached. That input surface is
/// the full set the walk consults — the `package.json` mtime/size at the project
/// (and distinct workspace) root, PLUS the *presence* of `pnpm-workspace.yaml`
/// at each directory the walk visited and the `package.json` beside every one
/// found, whose declaration decides whether that yaml counts (see
/// [`detect_project_uncached`]). The in-process PM engine can rewrite
/// `package.json` mid-command (`nub install`/`add`/`pm use`) and create or
/// remove a `pnpm-workspace.yaml`; any such change flips a stamp, so the next
/// lookup misses and the walk re-runs.
/// A same-length rewrite landing inside the manifest's own mtime quantum flips no
/// stamp at all, which is what [`Entry::cached_at`] exists to catch.
///
/// The stamp captures `package.json` *content* only at the resolved project (and
/// workspace) root — not the *presence* of a `package.json` newly appearing at a
/// LOWER walked dir, which would relocate the project root. That residual is
/// unreachable in practice rather than stamped: a `None` walk is never cached
/// (so a no-project cwd always re-walks), and the only in-process creator of a
/// fresh `package.json` (`aube add`'s bootstrap) is gated on there being NO
/// ancestor manifest — so it can never materialize a manifest *below* an
/// already-resolved root. Canonicalizing the key keeps two cwds that resolve to
/// the same directory sharing one entry while never serving a `Project` computed
/// for a genuinely different directory.
pub fn detect_project(cwd: &Path) -> Option<Project> {
    // Canonicalize so equivalent spellings of one dir (`.`, a symlink, a
    // trailing slash) share a cache entry — and, conversely, distinct dirs never
    // collide on a key. A non-canonicalizable cwd (gone/inaccessible) can't have
    // a project above it anyway; fall through to the uncached walk, which returns
    // the same `None` without polluting the cache.
    let Ok(key) = fs::canonicalize(cwd) else {
        return detect_project_uncached(cwd);
    };

    if let Some(hit) = cache().lookup_fresh(&key) {
        return Some((*hit).clone());
    }

    // Sampled before the walk reads anything, so a manifest written during or
    // after it is never trusted on an unchanged stamp — see `Entry::cached_at`.
    let cached_at = SystemTime::now();
    let (project, walked_dirs) = detect_project_walk(cwd)?;
    // Validate the cached value against the FULL input surface the walk read (see
    // [`freshness_stamps`]). A change to any of it invalidates the entry on the
    // next lookup.
    let stamps = freshness_stamps(&project, &walked_dirs);
    let value = Arc::new(project);
    cache().insert(key, stamps, cached_at, Arc::clone(&value));
    Some((*value).clone())
}

/// Records the cwd of every uncached walk — the metric the memoization tests
/// assert on. A test counts how many times ITS OWN (unique temp) cwd was walked;
/// keying on the cwd keeps the count immune to sibling tests walking other dirs
/// concurrently (cargo runs tests in parallel). Test-only; zero cost in release.
#[cfg(test)]
static WALKED_CWDS: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

/// The walk itself — the pre-memo body of [`detect_project`]. A cache miss (and
/// any path where canonicalization fails) runs exactly this, so the memoized and
/// unmemoized results are byte-for-byte identical.
fn detect_project_uncached(cwd: &Path) -> Option<Project> {
    detect_project_walk(cwd).map(|(project, _walked)| project)
}

/// The walk, returning the resulting [`Project`] alongside the directories it
/// CONSULTED. The walked-dirs list is what the freshness stamp covers for the
/// pnpm-named file: a `pnpm-workspace.yaml` appearing or disappearing at any dir
/// the walk visited could move `workspace_root`, so the memo must invalidate on
/// it (see [`freshness_stamps`]). [`detect_project`]
/// keeps the walked dirs; [`detect_project_uncached`] discards them.
fn detect_project_walk(cwd: &Path) -> Option<(Project, Vec<PathBuf>)> {
    #[cfg(test)]
    WALKED_CWDS
        .lock()
        .expect("WALKED_CWDS lock poisoned")
        .push(cwd.to_path_buf());

    let mut dir = cwd.to_path_buf();
    let mut project_root = None;
    let mut workspace_root = None;
    // Every dir the walk visits is a dir whose pnpm-file presence influenced the
    // outcome (it decided whether the walk stopped here). Recording them lets the
    // stamp validate exactly the input surface read — not one dir more or less.
    let mut walked = Vec::new();

    for _ in 0..32 {
        walked.push(dir.clone());

        let pkg_path = dir.join("package.json");
        if pkg_path.is_file()
            && let Ok(content) = fs::read_to_string(&pkg_path)
            && let Ok(manifest) =
                serde_json::from_str::<serde_json::Value>(crate::strip_utf8_bom(&content))
        {
            if project_root.is_none() {
                project_root = Some((dir.clone(), manifest.clone()));
            }
            if manifest.get("workspaces").is_some() {
                workspace_root = Some(dir.clone());
                break;
            }
        }

        // Also check for pnpm-workspace.yaml — but ONLY in a project that is
        // pnpm's. The brand hard gate (AGENTS.md): a nub project never reads a
        // pnpm-NAMED path, so a yaml beside a declaration naming nub must not
        // make this dir the workspace root. What counts as pnpm's is the
        // install's own identity rule (`pnpm_is_incumbent`).
        let pnpm_ws = dir.join("pnpm-workspace.yaml");
        if pnpm_ws.is_file() && crate::workspace::filter::pnpm_is_incumbent(&dir) {
            workspace_root = Some(dir.clone());
            if project_root.is_none() {
                let pkg_path = dir.join("package.json");
                if let Ok(content) = fs::read_to_string(&pkg_path)
                    && let Ok(manifest) =
                        serde_json::from_str::<serde_json::Value>(crate::strip_utf8_bom(&content))
                {
                    project_root = Some((dir.clone(), manifest));
                }
            }
            break;
        }

        if !dir.pop() {
            break;
        }
    }

    let project = project_root.map(|(root, manifest)| Project {
        root,
        workspace_root,
        manifest,
    })?;
    Some((project, walked))
}

/// A file's freshness stamp `(mtime, size)`, or `None` when it can't be stat'd
/// or the platform reports no mtime — the same signal
/// [`crate::config_cache::MtimeCache`] validates on. A `None` here means "can't
/// validate", which is treated as a miss so the value is never wrongly served.
fn stamp_of(path: &Path) -> Option<(SystemTime, u64)> {
    let meta = fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// The complete freshness stamp a cached [`Project`] is validated against — the
/// full filesystem input surface the walk read. A lookup serves the cached value
/// only when every component still matches; any change misses (fresh walk).
struct FreshnessStamp {
    /// `(package.json path, (mtime, size))` for the project root, plus the
    /// workspace root when distinct (the same two manifests
    /// [`crate::pm::resolve`]'s manifest cache keys on), plus the manifest beside
    /// every `pnpm-workspace.yaml` the walk passed. That last one can sit above
    /// the project root without being the workspace root — a yaml beside a
    /// declaration naming nub does not count — and dropping the declaration
    /// turns the yaml on. A rewrite bumps a stamp.
    manifests: Vec<(PathBuf, (SystemTime, u64))>,
    /// `(path, present)` for `pnpm-workspace.yaml` at every dir the walk
    /// visited. An install creating or removing one flips a bool here and
    /// invalidates the memo, so a mid-command change can never serve a stale
    /// `workspace_root`.
    pnpm_presence: Vec<(PathBuf, bool)>,
}

/// Build the [`FreshnessStamp`] from the walk's result + the dirs it consulted.
fn freshness_stamps(project: &Project, walked_dirs: &[PathBuf]) -> FreshnessStamp {
    let mut manifest_paths = vec![project.root.join("package.json")];
    if let Some(ws) = &project.workspace_root
        && *ws != project.root
    {
        manifest_paths.push(ws.join("package.json"));
    }

    let mut pnpm_presence = Vec::with_capacity(walked_dirs.len());
    for dir in walked_dirs {
        let path = dir.join("pnpm-workspace.yaml");
        let present = path.is_file();
        if present {
            let beside = dir.join("package.json");
            if !manifest_paths.contains(&beside) {
                manifest_paths.push(beside);
            }
        }
        pnpm_presence.push((path, present));
    }

    let manifests = manifest_paths
        .into_iter()
        .filter_map(|p| stamp_of(&p).map(|s| (p, s)))
        .collect();

    FreshnessStamp {
        manifests,
        pnpm_presence,
    }
}

struct Entry {
    /// The freshness stamp observed when this `Project` was cached. A lookup
    /// serves the value only if it still matches in full; any change (a manifest
    /// rewrite, or a pnpm-named file appearing/disappearing at a consulted dir)
    /// misses.
    stamp: FreshnessStamp,
    /// The instant sampled before the walk ran, closing the same racy window the
    /// `config_cache` module doc explains: a manifest rewritten within one mtime
    /// quantum of the walk reports an unchanged `(mtime, size)`, so an entry is
    /// trusted only once every stamped manifest clears the granularity slop (see
    /// [`crate::config_cache::mtime_quantum_closed`]). The `pnpm_presence` half
    /// needs no such guard — presence is exact and carries no timestamp.
    ///
    /// This memo caches the parsed manifest the walk read, so without the guard
    /// a stale `Project` would flow into `pm::resolve::root_manifest` even when
    /// the manifest cache itself missed.
    cached_at: SystemTime,
    project: Arc<Project>,
}

/// Per-process memo for [`detect_project`], keyed on the canonicalized cwd.
/// Mirrors [`crate::config_cache::MtimeCache`]'s structure (a lazily-initialized
/// `RwLock<HashMap>` behind a `OnceLock`) so the memo is thread-safe — workspace
/// member runs `thread::spawn` and may resolve concurrently.
struct ProjectCache {
    inner: OnceLock<RwLock<std::collections::HashMap<PathBuf, Entry>>>,
}

impl ProjectCache {
    const fn new() -> Self {
        Self {
            inner: OnceLock::new(),
        }
    }

    fn map(&self) -> &RwLock<std::collections::HashMap<PathBuf, Entry>> {
        self.inner
            .get_or_init(|| RwLock::new(std::collections::HashMap::new()))
    }

    /// The cached `Project` for `key` when every input it was derived from still
    /// matches its cached stamp; otherwise `None` (miss). Re-stat'ing on every
    /// lookup — manifest mtime/size AND pnpm-file presence at the consulted dirs
    /// — is what makes a mid-command manifest rewrite or pnpm-file change safe.
    fn lookup_fresh(&self, key: &Path) -> Option<Arc<Project>> {
        let guard = self.map().read().expect("ProjectCache lock poisoned");
        let entry = guard.get(key)?;
        let manifests_fresh = entry.stamp.manifests.iter().all(|(path, stamp)| {
            let (mtime, _size) = stamp;
            stamp_of(path).as_ref() == Some(stamp)
                && crate::config_cache::mtime_quantum_closed(*mtime, entry.cached_at)
        });
        let pnpm_fresh = entry
            .stamp
            .pnpm_presence
            .iter()
            .all(|(path, present)| path.is_file() == *present);
        (manifests_fresh && pnpm_fresh).then(|| Arc::clone(&entry.project))
    }

    fn insert(
        &self,
        key: PathBuf,
        stamp: FreshnessStamp,
        cached_at: SystemTime,
        project: Arc<Project>,
    ) {
        self.map()
            .write()
            .expect("ProjectCache lock poisoned")
            .insert(
                key,
                Entry {
                    stamp,
                    cached_at,
                    project,
                },
            );
    }
}

fn cache() -> &'static ProjectCache {
    static CACHE: ProjectCache = ProjectCache::new();
    &CACHE
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn fixture(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "nub-detect-gate-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("package.json"), r#"{"name":"root"}"#).unwrap();
        std::fs::write(dir.join("pnpm-workspace.yaml"), "packages:\n  - 'pkgs/*'\n").unwrap();
        // A manifest is only cacheable once its mtime quantum has closed (see
        // `Entry::cached_at`), which every real on-disk manifest already satisfies.
        // The memo tests below count walks, so the fixture has to start aged or
        // every lookup would be a racy-distrusted miss.
        age(&dir.join("package.json"), 60);
        dir
    }

    fn set_mtime(path: &Path, mtime: SystemTime) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
    }

    /// Backdate `path` by `secs`, clear of the granularity slop, so it is
    /// cacheable. Callers aging one path twice pass different offsets so the two
    /// mtimes differ by construction.
    fn age(path: &Path, secs: u64) {
        set_mtime(
            path,
            SystemTime::now() - std::time::Duration::from_secs(secs),
        );
    }

    // pnpm-workspace.yaml brand hard gate (AGENTS.md): `detect_project` treats a
    // dir as a workspace root via `pnpm-workspace.yaml` exactly when the project
    // is pnpm's by the install's own identity rule — the yaml alone is enough,
    // and a declaration naming nub beside it keeps it unread. A root
    // package.json with no `workspaces` field isolates the yaml signal. Each
    // test is the other's control.

    #[test]
    fn pnpm_workspace_yaml_sets_the_root_without_a_lockfile() {
        let dir = fixture("no-lock");
        let proj = detect_project(&dir).expect("root package.json detected");
        assert_eq!(
            proj.workspace_root.as_deref(),
            Some(dir.as_path()),
            "a pnpm workspace that has not been installed yet is still a workspace"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pnpm_workspace_yaml_does_not_set_root_beside_a_nub_declaration() {
        let dir = fixture("declares-nub");
        std::fs::write(
            dir.join("package.json"),
            r#"{"name":"root","packageManager":"nub@0.9.0"}"#,
        )
        .unwrap();
        let proj = detect_project(&dir).expect("root package.json detected");
        assert_eq!(
            proj.workspace_root, None,
            "a nub project must leave its pnpm-workspace.yaml unread"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn mtime_of(path: &Path) -> SystemTime {
        std::fs::metadata(path).unwrap().modified().unwrap()
    }

    /// How many uncached walks have run for exactly `cwd` so far. Scoping the
    /// count to one (unique temp) cwd makes it immune to sibling tests, which
    /// walk other dirs in parallel.
    fn walks_of(cwd: &Path) -> usize {
        WALKED_CWDS
            .lock()
            .expect("WALKED_CWDS lock poisoned")
            .iter()
            .filter(|p| p.as_path() == cwd)
            .count()
    }

    // DISC-1: the expensive walk (`detect_project_uncached` — the 32-ancestor
    // climb + per-`package.json` read + serde parse) runs 3–4× per command on
    // the common `nub <file>` path against a CONSTANT cwd. The memo must collapse
    // those to ONE walk while returning the identical `Project`.

    #[test]
    fn repeated_detect_on_constant_cwd_walks_once() {
        let dir = fixture("memo-once");
        std::fs::remove_file(dir.join("pnpm-workspace.yaml")).unwrap();
        let cwd = std::fs::canonicalize(&dir).unwrap();

        // The 3–4 calls a single `nub <file>` command makes (pin chain, engines,
        // disagreement warning, .env) on the same cwd.
        let a = detect_project(&cwd).expect("root detected");
        let b = detect_project(&cwd).expect("root detected");
        let c = detect_project(&cwd).expect("root detected");
        let d = detect_project(&cwd).expect("root detected");

        assert_eq!(
            walks_of(&cwd),
            1,
            "the expensive walk must run exactly once per command on a constant cwd, not per call"
        );
        // The memo is transparent: every call returns the identical Project.
        assert_eq!(a.root, cwd);
        assert_eq!(b.root, c.root);
        assert_eq!(c.root, d.root);
        assert_eq!(a.workspace_root, d.workspace_root);
        assert_eq!(a.manifest, d.manifest);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The memo caches the PARSED manifest, so a stale `Project` reaches
    /// `pm::resolve::root_manifest` even when the manifest cache itself misses —
    /// which is how a `pnpm@10.0.0` → `pnpm@11.0.0` rewrite (32 bytes either
    /// way) served the pre-write pin and flaked `pnpm_v11_surface` on Windows.
    /// A manifest still inside its own mtime quantum must therefore never be
    /// cached. Both versions are pinned to `now()` — where a real coarse-clock
    /// write lands — which reproduces the collision on every platform; see
    /// `config_cache`'s counterpart test for why racing it does not.
    #[test]
    fn manifest_rewritten_inside_its_mtime_quantum_is_never_served_stale() {
        let dir = fixture("memo-racy");
        std::fs::remove_file(dir.join("pnpm-workspace.yaml")).unwrap();
        let cwd = std::fs::canonicalize(&dir).unwrap();
        let pkg = cwd.join("package.json");
        let collided = SystemTime::now();

        std::fs::write(&pkg, r#"{"packageManager":"pnpm@10.0.0"}"#).unwrap();
        set_mtime(&pkg, collided);
        let first = detect_project(&cwd).expect("root detected");
        assert_eq!(first.manifest.get("packageManager").unwrap(), "pnpm@10.0.0");

        // Same byte length, and the stamp is pinned identical — neither half of
        // `(mtime, size)` can tell the two versions apart.
        std::fs::write(&pkg, r#"{"packageManager":"pnpm@11.0.0"}"#).unwrap();
        set_mtime(&pkg, collided);
        assert_eq!(
            mtime_of(&pkg),
            collided,
            "the two versions must be indistinguishable by stamp for this to test anything"
        );

        let second = detect_project(&cwd).expect("root detected");
        assert_eq!(
            second.manifest.get("packageManager").unwrap(),
            "pnpm@11.0.0",
            "a manifest rewrite sharing the cached mtime must not serve the stale pin"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn manifest_rewrite_invalidates_the_memo() {
        let dir = fixture("memo-inval");
        std::fs::remove_file(dir.join("pnpm-workspace.yaml")).unwrap();
        let cwd = std::fs::canonicalize(&dir).unwrap();
        let pkg = cwd.join("package.json");

        let first = detect_project(&cwd).expect("root detected");
        assert_eq!(first.manifest.get("name").unwrap(), "root");
        assert_eq!(
            walks_of(&cwd),
            1,
            "the first detect is a cache miss → one walk"
        );
        // The in-process PM engine rewriting package.json mid-command bumps the
        // mtime; the next lookup must miss and the walk must re-run with the new
        // content — the same protection ROOT_MANIFEST_CACHE gets. Aged to a
        // different offset than the fixture's, so the miss is attributable to the
        // changed mtime rather than to the granularity slop.
        std::fs::write(&pkg, r#"{"name":"renamed"}"#).unwrap();
        age(&pkg, 30);
        let second = detect_project(&cwd).expect("root detected");

        assert_eq!(
            walks_of(&cwd),
            2,
            "a manifest rewrite must force a fresh walk (miss), not serve the stale cache"
        );
        assert_eq!(
            second.manifest.get("name").unwrap(),
            "renamed",
            "the re-walk must reflect the rewritten manifest, never the pre-write value"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Whether a `pnpm-workspace.yaml` counts is the declaration in ITS OWN
    // directory's manifest, which can sit above the project root while being
    // neither root, so neither root's stamp covers it. Dropping a `nub`
    // declaration there turns the yaml on without touching the project's
    // manifest or any pnpm-named file; the memo must invalidate on that, or it
    // serves a stale `workspace_root: None`.
    #[test]
    fn a_declaration_beside_a_workspace_yaml_invalidates_the_memo() {
        let dir = fixture("yaml-declaration");
        let root = std::fs::canonicalize(&dir).unwrap();
        let root_pkg = root.join("package.json");
        std::fs::write(&root_pkg, r#"{"name":"root","packageManager":"nub@0.9.0"}"#).unwrap();
        age(&root_pkg, 60);
        let member = root.join("pkgs").join("a");
        std::fs::create_dir_all(&member).unwrap();
        std::fs::write(member.join("package.json"), r#"{"name":"a"}"#).unwrap();
        // Aged too: an unaged project manifest is never cached, which would make
        // the second walk below happen whether or not the stamp covers the root.
        age(&member.join("package.json"), 60);

        let first = detect_project(&member).expect("member detected");
        assert_eq!(
            first.workspace_root, None,
            "a yaml beside a nub declaration is not a workspace root"
        );
        assert_eq!(walks_of(&member), 1, "first detect is a miss → one walk");

        std::fs::write(&root_pkg, r#"{"name":"root"}"#).unwrap();
        age(&root_pkg, 30);

        let second = detect_project(&member).expect("member detected");
        assert_eq!(
            walks_of(&member),
            2,
            "a rewrite of the manifest beside a consulted yaml must force a fresh walk (miss)"
        );
        assert_eq!(
            second.workspace_root.as_deref(),
            Some(root.as_path()),
            "the re-walk must see the yaml count now → the parent is the workspace root"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // The reverse direction: removing the pnpm-workspace.yaml that made the dir a
    // workspace root must also invalidate (correct-by-construction covers both
    // appear AND disappear), again with package.json untouched.
    #[test]
    fn pnpm_workspace_yaml_removal_invalidates_the_memo() {
        let dir = fixture("pnpm-ws-remove");
        let cwd = std::fs::canonicalize(&dir).unwrap();
        let pkg = cwd.join("package.json");
        let cached_mtime = mtime_of(&pkg);

        let first = detect_project(&cwd).expect("root detected");
        assert_eq!(
            first.workspace_root.as_deref(),
            Some(cwd.as_path()),
            "pnpm-workspace.yaml → this dir is the workspace root"
        );
        assert_eq!(walks_of(&cwd), 1, "first detect is a miss → one walk");

        std::fs::remove_file(cwd.join("pnpm-workspace.yaml")).unwrap();
        assert_eq!(
            mtime_of(&pkg),
            cached_mtime,
            "package.json must be untouched — this isolates the pnpm-file vector"
        );

        let second = detect_project(&cwd).expect("root detected");
        assert_eq!(
            walks_of(&cwd),
            2,
            "removing the pnpm-workspace.yaml must force a fresh walk (miss)"
        );
        assert_eq!(
            second.workspace_root, None,
            "the re-walk must see no pnpm-workspace.yaml → no longer a workspace root"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
