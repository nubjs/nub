//! Process-wide directory lookups.
//!
//! `cwd()` returns the logical command working directory. It starts as
//! `std::env::current_dir()`, but in-process command fanout can retarget
//! it with [`set_cwd`] instead of spawning a fresh `aube` process just to
//! get clean global state.

use miette::{IntoDiagnostic, miette};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

static CWD: RwLock<Option<PathBuf>> = RwLock::new(None);

/// Return the process's current working directory, resolving it via
/// `std::env::current_dir()` on first call and caching the result.
/// Returns an owned `PathBuf` as a drop-in for the previous inline
/// `std::env::current_dir().into_diagnostic()?` pattern.
pub fn cwd() -> miette::Result<PathBuf> {
    if let Some(p) = CWD.read().expect("cwd lock poisoned").as_ref() {
        return Ok(p.clone());
    }

    let mut cwd = CWD.write().expect("cwd lock poisoned");
    if let Some(p) = cwd.as_ref() {
        return Ok(p.clone());
    }
    let p = std::env::current_dir().into_diagnostic()?;
    Ok(cwd.insert(p).clone())
}

/// Walk upward from `start` looking for the nearest directory that
/// contains a `package.json`. Returns the directory path, or `None` if
/// no ancestor has one. Used by `install` and `run` so subdirectories
/// of a project (e.g. `repo/docs`) resolve to the project root,
/// matching pnpm's behavior of walking up when run outside a project
/// directory.
pub fn find_project_root(start: &Path) -> Option<PathBuf> {
    // Memoized per-process. Run-class commands (`aube run`, `aube
    // exec`, `aube dlx`) hit this 4-8 times per invocation from
    // different call sites; without the cache each call repeats the
    // ancestor stat walk.
    //
    // ONLY caches positive results. `aube add` and `aube create` run
    // before any package.json exists; caching the initial `None`
    // would shadow the file these commands then create. A miss
    // re-runs the walk on the next call, which is the same cost as
    // pre-cache behavior.
    static CACHE: aube_util::cache::ProcessCache<PathBuf, PathBuf> =
        aube_util::cache::ProcessCache::new();
    let key = start.to_path_buf();
    if let Some(hit) = CACHE.get(&key) {
        return Some((*hit).clone());
    }
    let result = find_project_root_uncached(start)?;
    Some((*CACHE.get_or_compute(key, || result)).clone())
}

fn find_project_root_uncached(start: &Path) -> Option<PathBuf> {
    let home = home_stop_boundary();
    find_project_root_with_home(start, home.as_deref())
}

fn find_project_root_with_home(start: &Path, home: Option<&Path>) -> Option<PathBuf> {
    find_ancestor_before_home(start, home, |dir| {
        dir.join("package.json")
            .is_file()
            .then(|| dir.to_path_buf())
    })
}

/// Resolve home dir for the find_project_root walk boundary. On Unix reads
/// HOME. On Windows falls back to USERPROFILE since HOME is typically unset.
/// The ancestor walker canonicalizes HOME and its start together so physical
/// path spellings remain comparable. Returns None if neither is set, which
/// preserves the previous unbounded fallback rather than panicking.
fn home_stop_boundary() -> Option<PathBuf> {
    aube_util::env::home_dir()
}

/// Normalize both operands of a HOME-bounded ancestor walk through the same
/// native (non-verbatim) canonicalization. This keeps physical Unix paths and
/// normal Windows spellings comparable to their HOME boundary.
fn normalized_home_walk_paths(start: &Path, home: Option<&Path>) -> (PathBuf, Option<PathBuf>) {
    (
        canonicalize(start).unwrap_or_else(|_| start.to_path_buf()),
        home.map(|home| canonicalize(home).unwrap_or_else(|_| home.to_path_buf())),
    )
}

/// Walk ancestors without selecting `$HOME` when the invocation started below
/// it. Starting at `$HOME` remains valid, but the walk never escapes above it.
fn find_ancestor_before_home<T>(
    start: &Path,
    home: Option<&Path>,
    mut find: impl FnMut(&Path) -> Option<T>,
) -> Option<T> {
    let (start, home) = normalized_home_walk_paths(start, home);
    for dir in start.ancestors() {
        let at_home = home.as_deref().is_some_and(|home| home == dir);
        if at_home && dir != start.as_path() {
            return None;
        }
        if let Some(found) = find(dir) {
            return Some(found);
        }
        if at_home {
            return None;
        }
    }
    None
}

/// Shape-only check for a `package.json`'s `workspaces` field. Parses
/// just enough JSON to know if the field is present and non-null,
/// skipping the full `PackageJson` parse (which allocates IndexMaps,
/// deps maps, scripts, the whole thing) for every ancestor in the
/// walk. Callers up the chain run this 5-20 times per `aube run` /
/// `aube exec` / `aube dlx`.
fn package_json_has_workspaces(path: &Path) -> bool {
    #[derive(serde::Deserialize)]
    struct ShapeOnly {
        #[serde(default)]
        workspaces: Option<serde::de::IgnoredAny>,
    }
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    serde_json::from_slice::<ShapeOnly>(&bytes)
        .map(|s| s.workspaces.is_some())
        .unwrap_or(false)
}

/// Walk upward from `start` looking for the nearest workspace root.
///
/// A workspace root is any ancestor that either:
/// - contains `aube-workspace.yaml` or `pnpm-workspace.yaml`, or
/// - has a `package.json` with a `workspaces` field (yarn / npm / bun).
///
/// The aube-owned yaml name wins at read time elsewhere, but discovery
/// only needs to know whether any of those markers fixes the root.
pub fn find_workspace_root(start: &Path) -> Option<PathBuf> {
    // Same positive-only caching as `find_project_root`: bootstrap
    // commands like `aube add` may create the workspace boundary
    // mid-execution, so a cached `None` would shadow it.
    static CACHE: aube_util::cache::ProcessCache<PathBuf, PathBuf> =
        aube_util::cache::ProcessCache::new();
    let key = start.to_path_buf();
    if let Some(hit) = CACHE.get(&key) {
        return Some((*hit).clone());
    }
    let result = find_workspace_root_uncached(start)?;
    Some((*CACHE.get_or_compute(key, || result)).clone())
}

fn find_workspace_root_uncached(start: &Path) -> Option<PathBuf> {
    let home = home_stop_boundary();
    find_workspace_root_with_home(start, home.as_deref())
}

fn find_workspace_root_with_home(start: &Path, home: Option<&Path>) -> Option<PathBuf> {
    // Any `pnpm-workspace.yaml` is a hard workspace boundary, matching
    // pnpm: a file with no `packages:` list configures a single-package
    // workspace (just the root package) — it does not mean "ignore this
    // file and keep walking to an enclosing workspace". So `cd member &&
    // aube install` anchors on the member's own yaml rather than the
    // outer root; per-member lockfile freshness (tracked in install
    // state) is what keeps repeat installs warm under
    // `sharedWorkspaceLockfile=false`.
    find_ancestor_before_home(start, home, |dir| {
        if aube_manifest::workspace::workspace_yaml_existing(dir).is_some() {
            return Some(dir.to_path_buf());
        }
        let pkg = dir.join("package.json");
        (pkg.is_file() && package_json_has_workspaces(&pkg)).then(|| dir.to_path_buf())
    })
}

/// Walk upward from `start` looking for the nearest ancestor that
/// contains `aube-workspace.yaml` or `pnpm-workspace.yaml`. Unlike
/// [`find_workspace_root`], this ignores `package.json#workspaces`
/// because it feeds callers that specifically need the yaml file path
/// (catalog loader, settings loader).
pub fn find_workspace_yaml_root(start: &Path) -> Option<PathBuf> {
    let home = home_stop_boundary();
    find_workspace_yaml_root_with_home(start, home.as_deref())
}

fn find_workspace_yaml_root_with_home(start: &Path, home: Option<&Path>) -> Option<PathBuf> {
    find_ancestor_before_home(start, home, |dir| {
        aube_manifest::workspace::workspace_yaml_existing(dir).map(|_| dir.to_path_buf())
    })
}

/// Return the nearest project root at or above the cached cwd.
///
/// Commands that operate on the current project should use this
/// instead of [`cwd`] so running from a subdirectory targets the same
/// package root as `install` and `run`.
pub fn project_root() -> miette::Result<PathBuf> {
    let initial_cwd = cwd()?;
    find_project_root(&initial_cwd).ok_or_else(|| {
        miette!(
            "no package.json found in {} or any parent directory",
            initial_cwd.display()
        )
    })
}

/// Return the nearest project root, falling back to the cached cwd when
/// no ancestor contains `package.json`.
///
/// This is for commands that can also operate outside a package tree
/// but should still inherit project config when launched from a
/// subdirectory, such as `fetch` and registry/config helpers.
pub fn project_root_or_cwd() -> miette::Result<PathBuf> {
    Ok(project_root_or_cwd_from(&cwd()?))
}

/// Resolve the nearest project root for `start`, falling back to `start` when
/// no ancestor contains `package.json`.
///
/// Use this when planning per-project work without changing the process cwd.
/// It is the parameterized form of [`project_root_or_cwd`], so config
/// preflights and their later project-scoped actions share one root rule.
pub fn project_root_or_cwd_from(start: &Path) -> PathBuf {
    find_project_root(start).unwrap_or_else(|| start.to_path_buf())
}

/// Return the workspace root if one exists above the cwd, falling back
/// to the nearest project root.
///
/// Used by `install` and `patch` so `cd packages/app && aube install`
/// writes the lockfile + `.aube/` virtual store at the workspace root
/// (matching pnpm), and `aube patch` from a member finds the shared
/// store. Also used by workspace-scoped read commands (`list`, `query`,
/// `why`) so they read the workspace lockfile when invoked from inside
/// a subpackage instead of failing with a "no lockfile" error against
/// the subpackage's own directory. Falls back to the project root for
/// non-workspace trees (no workspace yaml and no `package.json#workspaces`).
pub fn workspace_or_project_root() -> miette::Result<PathBuf> {
    workspace_or_project_root_from(&cwd()?)
}

/// Resolve the workspace root for `start`, falling back to its nearest project
/// root. This is the root an install started from `start` will use.
///
/// Hosts that preflight before delegating to `install` must use this instead of
/// inferring a root from lockfile placement or the process cwd.
pub fn workspace_or_project_root_from(start: &Path) -> miette::Result<PathBuf> {
    if let Some(root) = find_workspace_root(start) {
        return Ok(root);
    }
    if let Some(root) = find_project_root(start) {
        return Ok(root);
    }
    Err(no_root_error(start))
}

fn no_root_error(initial_cwd: &Path) -> miette::Report {
    miette!(
        "no package.json or workspace yaml \
         (pnpm-workspace.yaml / aube-workspace.yaml) found in {} \
         or any parent directory",
        initial_cwd.display()
    )
}

/// Retarget the logical cwd to an explicit path.
pub fn set_cwd(path: &Path) -> miette::Result<()> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().into_diagnostic()?.join(path)
    };
    *CWD.write().expect("cwd lock poisoned") = Some(path);
    Ok(())
}

/// Canonicalize a path to its on-disk form using a "native" (non-verbatim)
/// Windows path.
///
/// On Windows, `std::fs::canonicalize` returns the UNC / extended-length
/// form (`\\?\C:\foo\bar`). That prefix breaks every downstream step that
/// concatenates the result with another path, which is exactly what the
/// global-install bin-shim path builder does — `%~dp0\{rel}` where `{rel}`
/// starts with `\\?\C:\...` produces a path that neither `cmd.exe` nor
/// Node.js can dereference, and the installed bin silently fails with
/// `Cannot find module '<bin_dir>\?\<target>'`.
///
/// This helper gives the same behavior as `dunce::canonicalize` without
/// adding the dep: canonicalize, then strip the `\\?\` prefix when it
/// didn't turn into a genuine UNC share path. `CreateDirectoryW` also
/// returns `ERROR_INVALID_NAME` (os 123) on verbatim-prefixed paths that
/// contain a `.`-relative leaf, so downstream `create_dir_all` calls on
/// the result likewise stay clean.
///
/// No-op on non-Windows.
pub fn canonicalize(path: &Path) -> std::io::Result<PathBuf> {
    let canon = std::fs::canonicalize(path)?;
    Ok(aube_util::path::strip_verbatim_prefix(&canon))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn home_markers_are_ignored_from_descendants_but_not_home_itself() {
        let home = tempfile::tempdir().unwrap();
        let child = home.path().join("scratch/nested");
        write(&child.join(".keep"), "");
        write(
            &home.path().join("package.json"),
            r#"{"name":"home","workspaces":["packages/*"]}"#,
        );
        write(
            &home.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'packages/*'\n",
        );

        assert_eq!(find_project_root_with_home(&child, Some(home.path())), None);
        assert_eq!(
            find_workspace_root_with_home(&child, Some(home.path())),
            None
        );
        assert_eq!(
            find_workspace_yaml_root_with_home(&child, Some(home.path())),
            None
        );

        assert_eq!(
            find_project_root_with_home(home.path(), Some(home.path())),
            Some(canonicalize(home.path()).unwrap())
        );
        assert_eq!(
            find_workspace_root_with_home(home.path(), Some(home.path())),
            Some(canonicalize(home.path()).unwrap())
        );
        assert_eq!(
            find_workspace_yaml_root_with_home(home.path(), Some(home.path())),
            Some(canonicalize(home.path()).unwrap())
        );
    }

    #[test]
    fn find_workspace_root_finds_pnpm_workspace_yaml() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'packages/*'\n",
        );
        write(&dir.path().join("packages/a/package.json"), "{}");

        let child = dir.path().join("packages/a");
        assert_eq!(
            find_workspace_root(&child).unwrap(),
            canonicalize(dir.path()).unwrap()
        );
    }

    #[test]
    fn find_workspace_root_finds_package_json_workspaces_array() {
        // yarn / npm / bun: no yaml, just a `workspaces` field in the
        // root package.json. Running aube from a subpackage must still
        // resolve to the monorepo root.
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("package.json"),
            r#"{"name":"root","workspaces":["packages/*"]}"#,
        );
        write(
            &dir.path().join("packages/a/package.json"),
            r#"{"name":"a"}"#,
        );

        let child = dir.path().join("packages/a");
        assert_eq!(
            find_workspace_root(&child).unwrap(),
            canonicalize(dir.path()).unwrap()
        );
    }

    #[test]
    fn find_workspace_root_finds_package_json_workspaces_object() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("package.json"),
            r#"{"name":"root","workspaces":{"packages":["apps/*"]}}"#,
        );
        write(&dir.path().join("apps/a/package.json"), r#"{"name":"a"}"#);

        let child = dir.path().join("apps/a");
        assert_eq!(
            find_workspace_root(&child).unwrap(),
            canonicalize(dir.path()).unwrap()
        );
    }

    #[test]
    fn find_workspace_root_ignores_package_json_without_workspaces() {
        // A child package.json with no `workspaces` field must not
        // short-circuit the walk — otherwise nested single packages
        // inside a monorepo would each be treated as a workspace root.
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("package.json"),
            r#"{"name":"root","workspaces":["packages/*"]}"#,
        );
        write(
            &dir.path().join("packages/a/package.json"),
            r#"{"name":"a"}"#,
        );

        let child = dir.path().join("packages/a");
        let root = find_workspace_root(&child).unwrap();
        assert_eq!(root, canonicalize(dir.path()).unwrap());
        assert_ne!(root, child);
    }

    #[test]
    fn find_workspace_yaml_root_ignores_package_json_workspaces() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("package.json"),
            r#"{"name":"root","workspaces":["packages/*"]}"#,
        );
        write(
            &dir.path().join("packages/a/package.json"),
            r#"{"name":"a"}"#,
        );

        let child = dir.path().join("packages/a");
        assert!(find_workspace_yaml_root(&child).is_none());
    }

    #[test]
    fn find_workspace_root_returns_none_without_markers() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("package.json"), r#"{"name":"solo"}"#);
        assert!(find_workspace_root(dir.path()).is_none());
    }

    #[test]
    fn find_workspace_root_stops_at_member_settings_only_yaml() {
        // A `pnpm-workspace.yaml` is a hard boundary even when it declares
        // no `packages:` list. pnpm treats a memberless yaml as a
        // single-package workspace (just the root package), not as "ignore
        // this file and keep walking to the enclosing workspace". So a
        // member that drops its own settings-only yaml resolves to
        // *itself*, not the outer `packages:`-declaring root.
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'services/*'\n",
        );
        let member = dir.path().join("services/svc-a");
        write(&member.join("package.json"), r#"{"name":"@t/svc-a"}"#);
        write(
            &member.join("pnpm-workspace.yaml"),
            "# per-service settings, no packages:\nenableGlobalVirtualStore: true\n",
        );

        assert_eq!(
            find_workspace_root(&member).unwrap(),
            canonicalize(&member).unwrap()
        );
    }

    #[test]
    fn find_workspace_root_keeps_standalone_settings_only_yaml() {
        // With no members-declaring ancestor, a settings-only
        // `pnpm-workspace.yaml` is a standalone single-package root (the
        // pnpm v9+ "keep config in pnpm-workspace.yaml" shape). It must
        // still resolve to itself, not fall through to None.
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("package.json"), r#"{"name":"solo"}"#);
        write(
            &dir.path().join("pnpm-workspace.yaml"),
            "enableGlobalVirtualStore: true\n",
        );

        assert_eq!(
            find_workspace_root(dir.path()).unwrap(),
            canonicalize(dir.path()).unwrap()
        );
    }

    #[test]
    fn find_workspace_root_stops_at_member_with_broken_yaml() {
        // A `pnpm-workspace.yaml` is a hard boundary regardless of whether
        // it parses: its mere presence anchors discovery on this directory
        // rather than the enclosing members-declaring workspace, and the
        // parse error surfaces later when the config is loaded for real.
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'services/*'\n",
        );
        let member = dir.path().join("services/svc-a");
        write(&member.join("package.json"), r#"{"name":"@t/svc-a"}"#);
        // Tab as the first indent char is a spec-level YAML syntax error.
        write(
            &member.join("pnpm-workspace.yaml"),
            "packages:\n\t- broken\n",
        );

        assert_eq!(
            find_workspace_root(&member).unwrap(),
            canonicalize(&member).unwrap()
        );
    }

    #[test]
    fn normalized_home_walk_paths_share_one_representation() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let start = home.join("project/member");
        std::fs::create_dir_all(&start).unwrap();
        let canonical_home = canonicalize(&home).unwrap();
        let canonical_start = canonicalize(&start).unwrap();

        for (start, home) in [
            (start.as_path(), home.as_path()),
            (start.as_path(), canonical_home.as_path()),
            (canonical_start.as_path(), home.as_path()),
            (canonical_start.as_path(), canonical_home.as_path()),
        ] {
            let (normalized_start, normalized_home) = normalized_home_walk_paths(start, Some(home));
            assert_eq!(normalized_start, canonical_start);
            assert_eq!(normalized_home.as_deref(), Some(canonical_home.as_path()));
        }
    }

    #[cfg(windows)]
    #[test]
    fn normalized_home_walk_paths_handles_verbatim_spellings_both_directions() {
        let dir = tempfile::tempdir().unwrap();
        let plain_home = dir.path().join("home");
        let plain_start = plain_home.join("project/member");
        std::fs::create_dir_all(&plain_start).unwrap();
        let verbatim_home = std::fs::canonicalize(&plain_home).unwrap();
        let verbatim_start = std::fs::canonicalize(&plain_start).unwrap();
        assert!(verbatim_home.to_string_lossy().starts_with(r"\\?\"));
        assert!(verbatim_start.to_string_lossy().starts_with(r"\\?\"));

        for (start, home) in [
            (plain_start.as_path(), verbatim_home.as_path()),
            (verbatim_start.as_path(), plain_home.as_path()),
        ] {
            let (normalized_start, normalized_home) = normalized_home_walk_paths(start, Some(home));
            assert_eq!(normalized_start, canonicalize(&plain_start).unwrap());
            assert_eq!(normalized_home, Some(canonicalize(&plain_home).unwrap()));
            assert!(!normalized_start.to_string_lossy().starts_with(r"\\?\"));
            assert!(
                !normalized_home
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(r"\\?\")
            );
        }
    }

    #[test]
    fn canonicalize_round_trips_an_existing_path() {
        // Smoke test on every platform: the helper should resolve an
        // existing path the same way `std::fs::canonicalize` does on
        // POSIX, and additionally strip the `\\?\` verbatim prefix on
        // Windows. The latter is exercised in `canonicalize_strips_…`
        // below.
        let dir = tempfile::tempdir().unwrap();
        let canon = canonicalize(dir.path()).unwrap();
        assert!(canon.is_absolute());
        assert!(canon.exists());
    }

    #[cfg(windows)]
    #[test]
    fn canonicalize_strips_verbatim_drive_prefix() {
        // `std::fs::canonicalize` on Windows always returns
        // `\\?\C:\…`. The helper must hand callers the plain drive
        // form, otherwise downstream `%~dp0\{rel}` shim concatenation
        // produces the `<bin>\?\C:\…` path that `cmd.exe` and Node
        // both fail to dereference.
        let dir = tempfile::tempdir().unwrap();
        let canon = canonicalize(dir.path()).unwrap();
        let s = canon.to_string_lossy();
        assert!(
            !s.starts_with(r"\\?\"),
            "expected non-verbatim path, got {s}"
        );
        assert!(
            s.chars().nth(1) == Some(':'),
            "expected drive form, got {s}"
        );
    }
}
