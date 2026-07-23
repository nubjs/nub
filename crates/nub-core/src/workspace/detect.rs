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
/// and `pnpm-lock.yaml` at each directory the walk visited (both gate
/// `workspace_root`; see [`detect_project_uncached`]). The in-process PM engine
/// can rewrite `package.json` mid-command (`nub install`/`add`/`pm use`) and an
/// install can create or remove a `pnpm-lock.yaml`/`pnpm-workspace.yaml`; any
/// such change flips a stamp, so the next lookup misses and the walk re-runs.
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

    let (project, walked_dirs) = detect_project_walk(cwd)?;
    // Validate the cached value against the FULL input surface the walk read: the
    // project-root manifest, the workspace-root manifest when distinct, plus the
    // presence of the two pnpm-named files at every dir the walk visited. A change
    // to any of them invalidates the entry on the next lookup.
    let stamps = freshness_stamps(&project, &walked_dirs);
    let value = Arc::new(project);
    cache().insert(key, stamps, Arc::clone(&value));
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
/// pnpm-named files: a `pnpm-lock.yaml`/`pnpm-workspace.yaml` appearing or
/// disappearing at any dir the walk visited could move `workspace_root`, so the
/// memo must invalidate on it (see [`freshness_stamps`]). [`detect_project`]
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

        // Also check for pnpm-workspace.yaml — but ONLY when pnpm is the
        // incumbent PM here. The brand hard gate (AGENTS.md): when the project's
        // PM is not pnpm, nub must never read a pnpm-NAMED path. A committed
        // `pnpm-lock.yaml` beside it is the incumbent signal (file-presence
        // detection, not config-consumption). Without it, a stray
        // `pnpm-workspace.yaml` must not make this dir the workspace root.
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
    /// [`crate::pm::resolve`]'s manifest cache keys on). A rewrite bumps a stamp.
    manifests: Vec<(PathBuf, (SystemTime, u64))>,
    /// `(pnpm-named-file path, present)` for `pnpm-workspace.yaml` and
    /// `pnpm-lock.yaml` at every dir the walk visited. Both gate `workspace_root`
    /// (a `pnpm-lock.yaml` proves pnpm incumbent → its sibling
    /// `pnpm-workspace.yaml` makes the dir the workspace root). An install
    /// creating/removing either file flips a bool here and invalidates the memo,
    /// so a mid-command pnpm-file change can never serve a stale `workspace_root`.
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
    let manifests = manifest_paths
        .into_iter()
        .filter_map(|p| stamp_of(&p).map(|s| (p, s)))
        .collect();

    let mut pnpm_presence = Vec::with_capacity(walked_dirs.len() * 2);
    for dir in walked_dirs {
        for name in ["pnpm-workspace.yaml", "pnpm-lock.yaml"] {
            let path = dir.join(name);
            let present = path.is_file();
            pnpm_presence.push((path, present));
        }
    }

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
        let manifests_fresh = entry
            .stamp
            .manifests
            .iter()
            .all(|(path, stamp)| stamp_of(path).as_ref() == Some(stamp));
        let pnpm_fresh = entry
            .stamp
            .pnpm_presence
            .iter()
            .all(|(path, present)| path.is_file() == *present);
        (manifests_fresh && pnpm_fresh).then(|| Arc::clone(&entry.project))
    }

    fn insert(&self, key: PathBuf, stamp: FreshnessStamp, project: Arc<Project>) {
        self.map()
            .write()
            .expect("ProjectCache lock poisoned")
            .insert(key, Entry { stamp, project });
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
        dir
    }

    // pnpm-workspace.yaml brand hard gate (AGENTS.md): `detect_project` may treat
    // a dir as a workspace root via `pnpm-workspace.yaml` ONLY when pnpm is the
    // incumbent PM (a committed `pnpm-lock.yaml`). A root package.json with no
    // `workspaces` field isolates the pnpm-workspace.yaml signal.

    #[test]
    fn pnpm_workspace_yaml_does_not_set_root_when_pnpm_not_incumbent() {
        let dir = fixture("no-lock");
        let proj = detect_project(&dir).expect("root package.json detected");
        assert_eq!(
            proj.workspace_root, None,
            "a stray pnpm-workspace.yaml (no pnpm-lock.yaml) must not make this a workspace root"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pnpm_workspace_yaml_sets_root_when_pnpm_lock_present() {
        let dir = fixture("with-lock");
        std::fs::write(dir.join("pnpm-lock.yaml"), "lockfileVersion: '9.0'\n").unwrap();
        let proj = detect_project(&dir).expect("root package.json detected");
        assert_eq!(
            proj.workspace_root.as_deref(),
            Some(dir.as_path()),
            "pnpm-lock.yaml proves pnpm incumbent → pnpm-workspace.yaml sets the workspace root"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn mtime_of(path: &Path) -> SystemTime {
        std::fs::metadata(path).unwrap().modified().unwrap()
    }

    /// Rewrite `path` with `contents`, busy-rewriting until the reported mtime
    /// advances past `prev` so the test forces a real mtime change regardless of
    /// the filesystem's granularity, without a fixed sleep or a `filetime`
    /// dev-dep (matching `config_cache`'s test convention). Bounded so a stuck
    /// filesystem fails loudly rather than hanging.
    fn write_until_mtime_advances(path: &Path, contents: &str, prev: SystemTime) {
        for _ in 0..10_000 {
            std::fs::write(path, contents).unwrap();
            if mtime_of(path) > prev {
                return;
            }
        }
        panic!("filesystem mtime did not advance after repeated writes");
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
        let cached_mtime = mtime_of(&pkg);

        // The in-process PM engine rewriting package.json mid-command bumps the
        // mtime; the next lookup must miss and the walk must re-run with the new
        // content — the same protection ROOT_MANIFEST_CACHE gets.
        write_until_mtime_advances(&pkg, r#"{"name":"renamed"}"#, cached_mtime);
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

    // The pullfrog vector (detect.rs): `workspace_root` also depends on the
    // PRESENCE of `pnpm-lock.yaml`/`pnpm-workspace.yaml`, not just package.json
    // mtime/size. An install creating a `pnpm-lock.yaml` (with the
    // pnpm-workspace.yaml already there) flips the dir into the workspace root —
    // WITHOUT touching package.json. The memo must invalidate on that, or it
    // serves a stale `workspace_root: None`.
    #[test]
    fn pnpm_lock_appearing_invalidates_the_memo() {
        // fixture() writes both package.json AND pnpm-workspace.yaml, but no
        // pnpm-lock.yaml — so pnpm is not yet incumbent and workspace_root is None.
        let dir = fixture("pnpm-lock-appear");
        let cwd = std::fs::canonicalize(&dir).unwrap();
        let pkg = cwd.join("package.json");
        let cached_mtime = mtime_of(&pkg);

        let first = detect_project(&cwd).expect("root detected");
        assert_eq!(
            first.workspace_root, None,
            "no pnpm-lock.yaml yet → pnpm not incumbent → not a workspace root"
        );
        assert_eq!(walks_of(&cwd), 1, "first detect is a miss → one walk");

        // An install lands a pnpm-lock.yaml beside the existing pnpm-workspace.yaml.
        // package.json is UNTOUCHED — the manifest stamp alone would not catch this.
        std::fs::write(cwd.join("pnpm-lock.yaml"), "lockfileVersion: '9.0'\n").unwrap();
        assert_eq!(
            mtime_of(&pkg),
            cached_mtime,
            "package.json must be untouched — this isolates the pnpm-file vector"
        );

        let second = detect_project(&cwd).expect("root detected");
        assert_eq!(
            walks_of(&cwd),
            2,
            "a pnpm-lock.yaml appearing at a consulted dir must force a fresh walk (miss)"
        );
        assert_eq!(
            second.workspace_root.as_deref(),
            Some(cwd.as_path()),
            "the re-walk must see pnpm now incumbent → this dir is the workspace root"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // The reverse direction: removing the pnpm-workspace.yaml that made the dir a
    // workspace root must also invalidate (correct-by-construction covers both
    // appear AND disappear), again with package.json untouched.
    #[test]
    fn pnpm_workspace_yaml_removal_invalidates_the_memo() {
        let dir = fixture("pnpm-ws-remove");
        std::fs::write(dir.join("pnpm-lock.yaml"), "lockfileVersion: '9.0'\n").unwrap();
        let cwd = std::fs::canonicalize(&dir).unwrap();
        let pkg = cwd.join("package.json");
        let cached_mtime = mtime_of(&pkg);

        let first = detect_project(&cwd).expect("root detected");
        assert_eq!(
            first.workspace_root.as_deref(),
            Some(cwd.as_path()),
            "pnpm-lock.yaml + pnpm-workspace.yaml → this dir is the workspace root"
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
