use tracing::{debug, trace, warn};

use crate::patches::apply_multi_file_patch;
use crate::sweep::{EntryState, classify_entry_state, mkdirp, try_remove_entry};
use crate::{Error, LinkStats, LinkStrategy, Linker, sys};
use aube_lockfile::{LockedPackage, LockfileGraph, resolve_dep_edge, shared_local_dep_path};
use aube_store::{PackageIndex, StoredFile};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

enum MaterializePlacement {
    Placed,
    LostRace,
}

fn materialize_tmp_name() -> String {
    static NEXT_TMP_ID: AtomicU64 = AtomicU64::new(0);
    let id = NEXT_TMP_ID.fetch_add(1, Ordering::Relaxed);
    format!(".tmp-{}-{id}", std::process::id())
}

fn place_materialized_entry(src: &Path, dst: &Path) -> io::Result<MaterializePlacement> {
    const MAX_ATTEMPTS: u32 = 5;
    let mut backoff_ms = 20u64;
    let mut attempt = 0;
    loop {
        match std::fs::rename(src, dst) {
            Ok(()) => return Ok(MaterializePlacement::Placed),
            Err(_) if dst.exists() => return Ok(MaterializePlacement::LostRace),
            Err(err) if is_transient_rename_error(&err) && attempt < MAX_ATTEMPTS - 1 => {
                thread::sleep(Duration::from_millis(backoff_ms));
                backoff_ms = backoff_ms.saturating_mul(2);
                attempt += 1;
            }
            Err(err) => return Err(err),
        }
    }
}

fn is_transient_rename_error(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::AlreadyExists
            | io::ErrorKind::PermissionDenied
            | io::ErrorKind::Interrupted
            | io::ErrorKind::WouldBlock
    )
}

/// Test-only switch that forces the reflink attempt in
/// [`Linker::link_file_fresh`] to be treated as failed, so the
/// clonefile-failure fallback path can be exercised deterministically on
/// any filesystem (CI runs on reflink-capable APFS/btrfs where a real
/// `clonefile` would otherwise succeed). Compiled out of release builds.
#[cfg(test)]
pub(crate) static FORCE_REFLINK_FAILURE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

impl Linker {
    /// Detect the best linking strategy for the filesystem at the given path.
    ///
    /// One-arg form. Probes within one dir. Fine when store and
    /// project node_modules share the same mount. Use the two-arg
    /// form for installs where the store lives on a different
    /// filesystem than the project (USB drives, bind mounts, Docker
    /// volumes, cross-drive Windows installs). Otherwise the probe
    /// reports hardlink based on project-FS self-test, then every
    /// real link call crosses an FS boundary and hits EXDEV. Runtime
    /// falls back to `fs::copy` per file silently, thousands of
    /// wasted syscalls, user thinks they got hardlinks.
    ///
    /// Returns the same-filesystem strategy `auto` resolves to when the
    /// probe succeeds, `Copy` otherwise. The same-FS strategy is
    /// OS-specific: on macOS `auto` resolves to `ReflinkAuto` (APFS
    /// clonefile benchmarks ~1.91x faster than hardlink), on Linux and
    /// other targets it resolves to `Hardlink` (btrfs/xfs hardlink
    /// benchmarks ~2.4-2.6x faster than FICLONE reflink).
    ///
    /// The macOS `auto` resolution is conservative on non-APFS volumes:
    /// `clonefile` is APFS-only, so on an HFS+ volume (external drives,
    /// Fusion/older disks) the same-FS hardlink probe succeeds and
    /// resolves `ReflinkAuto`, but the real `clonefile` then fails.
    /// `link_file_fresh` handles this by falling the reflink back to a
    /// hardlink (which HFS+ supports) before copy, so a non-APFS same-FS
    /// target still gets zero-cost links rather than a per-file copy.
    /// This probe never yields the plain `Reflink` strategy — that is
    /// reachable only through explicit `packageImportMethod = clone` /
    /// `clone-or-copy`, which keep a plain copy fallback.
    pub fn detect_strategy(path: &Path) -> LinkStrategy {
        Self::detect_strategy_cross(path, path)
    }

    /// Two-arg probe. src is the store shard (or any dir on the
    /// store FS), dst is the project modules dir (or any dir on the
    /// destination FS). Probe creates a real cross-mount src file
    /// and tries to hardlink into dst, which catches EXDEV up front.
    /// A successful hardlink proves src and dst share a mount, so it
    /// doubles as the same-FS probe for reflink too (APFS clonefile /
    /// btrfs FICLONE require the same FS). Returns the OS-specific
    /// same-FS strategy when the probe succeeds (`ReflinkAuto` on macOS,
    /// `Hardlink` elsewhere), `Copy` otherwise.
    pub fn detect_strategy_cross(src_dir: &Path, dst_dir: &Path) -> LinkStrategy {
        // Same-FS strategy `auto` resolves to once the probe succeeds.
        // macOS/APFS clonefile is measurably faster than hardlink there;
        // Linux btrfs/xfs hardlink is measurably faster than FICLONE.
        // `ReflinkAuto` (not the plain `Reflink` explicit selections use)
        // carries the non-APFS hardlink-before-copy fallback.
        #[cfg(target_os = "macos")]
        const SAME_FS_STRATEGY: LinkStrategy = LinkStrategy::ReflinkAuto;
        #[cfg(not(target_os = "macos"))]
        const SAME_FS_STRATEGY: LinkStrategy = LinkStrategy::Hardlink;

        // Memoize per (src_dir, dst_dir) for the process lifetime.
        // The probe writes a real test file and tries hardlink,
        // ~2 syscalls + 2 unlinks. Multiple Linker instances within
        // one install (prewarm + final + per-workspace) all repeat
        // the probe; cache the answer.
        type ProbeKey = (std::path::PathBuf, std::path::PathBuf);
        static CACHE: std::sync::OnceLock<
            std::sync::RwLock<std::collections::HashMap<ProbeKey, LinkStrategy>>,
        > = std::sync::OnceLock::new();
        let key = (src_dir.to_path_buf(), dst_dir.to_path_buf());
        let cache = CACHE.get_or_init(Default::default);
        if let Some(hit) = cache.read().expect("probe cache poisoned").get(&key) {
            return *hit;
        }

        let test_src = src_dir.join(".aube-link-test-src");
        let test_dst = dst_dir.join(".aube-link-test-dst");

        let strategy = if std::fs::write(&test_src, b"test").is_ok() {
            let result = if std::fs::hard_link(&test_src, &test_dst).is_ok() {
                SAME_FS_STRATEGY
            } else {
                LinkStrategy::Copy
            };
            let _ = std::fs::remove_file(&test_src);
            let _ = std::fs::remove_file(&test_dst);
            result
        } else {
            LinkStrategy::Copy
        };

        // First-write-wins via `entry().or_insert`. Two concurrent
        // linker probes (prewarm + final) sharing the same
        // (src_dir, dst_dir) can race on the test files: one observes
        // hardlink-ok, the other sees the first writer's leftover and
        // falls back to Copy. `.insert()` would let the wrong Copy
        // result clobber the correct Hardlink for the rest of the
        // process; `or_insert` keeps whichever value landed first. (The
        // value at stake is the same-FS strategy above, not literally
        // Hardlink — ReflinkAuto on macOS.)
        *cache
            .write()
            .expect("probe cache poisoned")
            .entry(key)
            .or_insert(strategy)
    }

    /// Materialize a package in the global virtual store if not already present.
    ///
    /// Materialize `dep_path` into the shared global virtual store.
    ///
    /// Uses atomic rename to avoid TOCTOU races: materializes into a
    /// PID-stamped temp directory, then renames into place. If another
    /// process wins the race, its result is kept and the temp dir is
    /// cleaned up.
    ///
    /// Exposed so the install driver can pipeline GVS population into
    /// the fetch phase: as each tarball finishes importing into the
    /// CAS, the driver calls this to reflink the package into its
    /// `~/.cache/aube/virtual-store/<subdir>` entry. Link step 1 then
    /// hits the `pkg_nm_dir.exists()` fast path and only creates the
    /// per-project `.aube/<dep_path>` symlink.
    pub fn ensure_in_virtual_store(
        &self,
        dep_path: &str,
        // Resolves each transitive dep's edge value to its canonical package
        // key across reader conventions during the sibling-symlink pass (only
        // `graph.packages` is read). See `materialize_into`.
        graph: &LockfileGraph,
        pkg: &LockedPackage,
        index: &PackageIndex,
        stats: &mut LinkStats,
        // `link:` transitives the resolver pinned (e.g. via root
        // `pnpm.overrides`) need their on-disk target so the parent's
        // sibling symlink doesn't dangle into a non-existent
        // `.aube/<name>@link+...`. `None` means "no nested links in
        // this graph" and the materialize hot path stays unchanged.
        nested_link_targets: Option<&BTreeMap<String, PathBuf>>,
    ) -> Result<(), Error> {
        // Global-store paths always run through the vstore_key map —
        // when hashes are installed this folds dep-graph + engine
        // state into the leaf name, so concurrent builds of the same
        // package against different toolchains don't collide.
        let subdir = self.virtual_store_subdir(dep_path);
        self.ensure_in_virtual_store_with_subdir(
            dep_path,
            &subdir,
            graph,
            pkg,
            index,
            stats,
            nested_link_targets,
        )
    }

    /// `ensure_in_virtual_store` with the virtual-store subdir already
    /// computed by the caller. The link step's per-package par_iter
    /// derives `virtual_store_subdir(dep_path)` once to build the
    /// shared-store entry path, so passing it in here avoids recomputing
    /// the same `dep_path_to_filename` encode (a `String` alloc plus the
    /// escape/uppercase scans, and a second alloc + BLAKE3 short-hash on
    /// long/scoped/peer-context names). `subdir` MUST equal
    /// `self.virtual_store_subdir(dep_path)`; the public wrapper above
    /// guarantees that for callers that don't already have it.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn ensure_in_virtual_store_with_subdir(
        &self,
        dep_path: &str,
        subdir: &str,
        graph: &LockfileGraph,
        pkg: &LockedPackage,
        index: &PackageIndex,
        stats: &mut LinkStats,
        nested_link_targets: Option<&BTreeMap<String, PathBuf>>,
    ) -> Result<(), Error> {
        let _diag =
            aube_util::diag::Span::new(aube_util::diag::Category::Linker, "ensure_in_vstore")
                .with_meta_fn(|| {
                    format!(
                        r#"{{"name":{},"files":{}}}"#,
                        aube_util::diag::jstr(&pkg.name),
                        index.len()
                    )
                });
        let pkg_nm_dir = self
            .virtual_store
            .join(subdir)
            .join("node_modules")
            .join(&pkg.name);

        if pkg_nm_dir.exists() {
            trace!("virtual store hit: {dep_path}");
            stats.packages_cached += 1;
            return Ok(());
        }

        // Materialize into a temp directory, then atomically rename into place
        // to avoid TOCTOU races between concurrent `aube install` processes.
        let tmp_name = materialize_tmp_name();
        let tmp_base = self.virtual_store.join(&tmp_name);

        let result = self.materialize_into(
            &tmp_base,
            &self.virtual_store,
            dep_path,
            graph,
            pkg,
            index,
            stats,
            true,
            nested_link_targets,
        );

        if result.is_err() {
            let _ = std::fs::remove_dir_all(&tmp_base);
            return result;
        }

        // Atomically move the dep_path entry from the temp dir to the final location.
        let tmp_entry = tmp_base.join(subdir);
        let final_entry = self.virtual_store.join(subdir);

        // Ensure the parent of the final entry exists (e.g. for scoped packages).
        if let Some(parent) = final_entry.parent()
            && let Err(e) = mkdirp(parent)
        {
            let _ = std::fs::remove_dir_all(&tmp_base);
            return Err(e);
        }

        match place_materialized_entry(&tmp_entry, &final_entry) {
            Ok(MaterializePlacement::Placed) => {
                trace!("atomically placed {subdir} in virtual store");
            }
            Ok(MaterializePlacement::LostRace) => {
                // Another process won the race — that's fine, use theirs.
                trace!("lost rename race for {dep_path}, using existing");
                // Undo the stats from our materialization since we're discarding it
                stats.packages_linked = stats.packages_linked.saturating_sub(1);
                stats.files_linked = stats.files_linked.saturating_sub(index.len());
                stats.packages_cached += 1;
                // Lost-race path: our `subdir` is still inside
                // `tmp_base`, so a full recursive delete is needed.
                let _ = std::fs::remove_dir_all(&tmp_base);
                return Ok(());
            }
            Err(e) => {
                let _ = std::fs::remove_dir_all(&tmp_base);
                return Err(Error::Io(final_entry, e));
            }
        }

        // Successful rename: `tmp_base` is now an empty wrapper directory
        // (its single child was the subdir we just renamed out). Use
        // `remove_dir` instead of `remove_dir_all` — the latter still
        // does the full `opendir`/`fdopendir`(fcntl)/`readdir`/`close`
        // walk even on an empty dir, which dtrace shows as ~6 extra
        // syscalls per package. At 227 packages that's ~1.4k wasted
        // syscalls on every cold install.
        //
        // `remove_dir` fails with `ENOTEMPTY` if a future change to
        // `materialize_into` starts dropping extra files into
        // `tmp_base`. Log at debug so the leak is observable without
        // being fatal; the worst-case outcome is a stray tmp dir, and
        // concurrent-writer races already use the full
        // `remove_dir_all` branch above.
        if let Err(e) = std::fs::remove_dir(&tmp_base) {
            debug!(
                "remove_dir({}) failed, leaving tmp in place: {e}",
                tmp_base.display()
            );
        }

        Ok(())
    }

    /// Materialize a globally-reproducible local source (a `git`
    /// dependency or a remote `.tgz`) into the shared virtual store and
    /// point the per-project `.aube/<dep_path>` entry at it — the exact
    /// arrangement Step 1 produces for a registry package.
    ///
    /// Used by the isolated linker in global-virtual-store mode. Plain
    /// `file:` / `link:` / `portal:` / `exec:` sources resolve against
    /// a path inside the project and are materialized per-project
    /// instead (see `materialize_into` with `apply_hashes = false`),
    /// but git and remote-tarball sources are content-pinned and shared
    /// like registry packages. They MUST live in the shared store when
    /// it is enabled: a registry dependent in the shared store links
    /// its dependency siblings to the hashed global path
    /// (`virtual_store_subdir(dep_path)`), so a git/tarball dep that
    /// only existed in the per-project `.aube/` would leave that
    /// sibling symlink dangling — and Node would resolve whatever
    /// unrelated `<name>` it found walking up the tree.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn ensure_shared_local_in_global_store(
        &self,
        aube_dir: &Path,
        dep_path: &str,
        graph: &LockfileGraph,
        pkg: &LockedPackage,
        index: &PackageIndex,
        stats: &mut LinkStats,
        nested_link_targets: Option<&BTreeMap<String, PathBuf>>,
    ) -> Result<(), Error> {
        let local_aube_entry = aube_dir.join(self.aube_dir_entry_name(dep_path));
        let global_entry = self.virtual_store.join(self.virtual_store_subdir(dep_path));
        let state = classify_entry_state(&local_aube_entry, &global_entry);
        if matches!(state, EntryState::Fresh) {
            stats.packages_cached += 1;
            return Ok(());
        }
        self.ensure_in_virtual_store(dep_path, graph, pkg, index, stats, nested_link_targets)?;
        if matches!(state, EntryState::Stale) {
            // A prior install — or an older aube that always
            // materialized git/remote sources per-project — may have
            // left a real directory or a stale symlink here. Clear
            // either shape before pointing the entry at the shared
            // store (`try_remove_entry` handles dir, symlink, and
            // dangling-link cases).
            try_remove_entry(&local_aube_entry);
        }
        if let Some(parent) = local_aube_entry.parent() {
            mkdirp(parent)?;
        }
        sys::create_dir_link(&global_entry, &local_aube_entry)
            .map_err(|e| Error::Io(local_aube_entry.clone(), e))?;
        Ok(())
    }

    /// Materialize a single package directly into the per-project
    /// virtual store at `aube_dir/<dep_path>/node_modules/<name>/`.
    ///
    /// Idempotent: if the entry already exists, counts as cached and
    /// returns. Otherwise materializes into a unique temp directory
    /// and atomically renames that entry into place so duplicate
    /// in-process fetch events for the same dep-path cannot race while
    /// writing `node_modules/.aube/<dep_path>/`. Used by the
    /// install-time materializer to pipeline the link work into the
    /// fetch phase under non-GVS mode, so the dedicated link phase only
    /// has to create top-level `node_modules/<name>` symlinks.
    #[allow(clippy::too_many_arguments)]
    pub fn ensure_in_aube_dir(
        &self,
        aube_dir: &Path,
        dep_path: &str,
        graph: &LockfileGraph,
        pkg: &LockedPackage,
        index: &PackageIndex,
        stats: &mut LinkStats,
        nested_link_targets: Option<&BTreeMap<String, PathBuf>>,
    ) -> Result<(), Error> {
        let subdir = self.aube_dir_entry_name(dep_path);
        let final_entry = aube_dir.join(&subdir);
        if final_entry.exists() {
            stats.packages_cached += 1;
            return Ok(());
        }

        let tmp_name = materialize_tmp_name();
        let tmp_base = aube_dir.join(&tmp_name);
        let result = self.materialize_into(
            &tmp_base,
            aube_dir,
            dep_path,
            graph,
            pkg,
            index,
            stats,
            false,
            nested_link_targets,
        );

        if result.is_err() {
            let _ = std::fs::remove_dir_all(&tmp_base);
            return result;
        }

        let tmp_entry = tmp_base.join(&subdir);
        if let Some(parent) = final_entry.parent()
            && let Err(e) = mkdirp(parent)
        {
            let _ = std::fs::remove_dir_all(&tmp_base);
            return Err(e);
        }

        match place_materialized_entry(&tmp_entry, &final_entry) {
            Ok(MaterializePlacement::Placed) => {}
            Ok(MaterializePlacement::LostRace) => {
                stats.packages_linked = stats.packages_linked.saturating_sub(1);
                stats.files_linked = stats.files_linked.saturating_sub(index.len());
                stats.packages_cached += 1;
                let _ = std::fs::remove_dir_all(&tmp_base);
                return Ok(());
            }
            Err(e) => {
                let _ = std::fs::remove_dir_all(&tmp_base);
                return Err(Error::Io(final_entry, e));
            }
        }

        if let Err(e) = std::fs::remove_dir(&tmp_base) {
            debug!(
                "remove_dir({}) failed, leaving tmp in place: {e}",
                tmp_base.display()
            );
        }

        Ok(())
    }

    /// Materialize a package's files and transitive dep symlinks into a base directory.
    ///
    /// `base_dir` is where files are written during materialization.
    /// `final_base_dir` is where those files will live after any
    /// wrapper rename. These differ for `.tmp-*` staging dirs; Windows
    /// junctions need the final root because they persist absolute
    /// targets at creation time.
    ///
    /// `apply_hashes` controls whether per-dep subdir names are run
    /// through `vstore_key` (the content-addressed name) or used as
    /// raw `dep_path` strings. Global-store callers pass `true` so
    /// the shared `~/.cache/aube/virtual-store/` can hold isolated
    /// copies for each `(deps_hash, engine)` combination;
    /// per-project `.aube/` callers pass `false` because node's
    /// runtime module walk resolves by dep_path only.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn materialize_into(
        &self,
        base_dir: &Path,
        final_base_dir: &Path,
        dep_path: &str,
        // The whole graph, so a transitive dep's VALUE can be resolved to its
        // canonical package key across every reader convention (see the sibling
        // symlink loop below). Only `graph.packages` is consulted.
        graph: &LockfileGraph,
        pkg: &LockedPackage,
        index: &PackageIndex,
        stats: &mut LinkStats,
        apply_hashes: bool,
        // dep_path → absolute on-disk target for any `link:` packages
        // referenced as transitive deps. When the parent itself is a
        // `file:` Directory or `link:` Link (workspace-style locals),
        // its `package.json` may declare `link:./libs/foo` deps that
        // point inside the parent's source tree. We sidestep the
        // virtual store for those — there is no `.aube/<dep>@link+...`
        // entry — and symlink straight to the on-disk path the
        // resolver pinned. `None` means "no nested link transitives in
        // this graph", which is the common case.
        nested_link_targets: Option<&BTreeMap<String, PathBuf>>,
    ) -> Result<(), Error> {
        #[cfg(not(windows))]
        let _ = final_base_dir;

        validate_package_link_name(&pkg.name)?;
        for dep_name in pkg.dependencies.keys() {
            validate_package_link_name(dep_name)?;
        }
        let subdir = if apply_hashes {
            self.virtual_store_subdir(dep_path)
        } else {
            self.aube_dir_entry_name(dep_path)
        };
        let pkg_nm_dir = base_dir.join(&subdir).join("node_modules").join(&pkg.name);
        let pkg_nm_parent = base_dir.join(&subdir).join("node_modules");

        // Whole-dir `clonefile(2)` fast path (macOS+APFS, same volume).
        // When the store's extracted-tree tier holds this package, the
        // kernel clones the entire package directory in ONE syscall
        // instead of the per-file reflink loop below — measured ~12x on
        // the link pass. The clone replaces ONLY the file-fill; the +x
        // pass is unneeded (clonefile preserves mode bits) and the
        // patch + transitive-symlink passes run identically afterward.
        //
        // Gate is conservative and additive: any miss (tier not built,
        // non-macOS, non-APFS dst, cross-volume, or the clone itself
        // erroring) falls through to the unchanged per-file path, so
        // default behavior is byte-for-byte today's. `tree_key` is the
        // global-store subdir name regardless of `apply_hashes` — the
        // tree tier is a shared global resource keyed the same way the
        // GVS is, so per-project `.aube/` materializations can clone
        // from the same trees the GVS built.
        let tree_key = self.virtual_store_subdir(dep_path);
        let tree_src = self.store.tree_path(&tree_key);
        let used_clonedir = self.try_clonedir_fill(
            &pkg_nm_dir,
            &pkg_nm_parent,
            &tree_src,
            dep_path,
            pkg,
            index,
            stats,
        )?;

        // Pre-compute the set of unique parent directories across
        // every file in the index AND every scoped transitive-dep
        // symlink we're about to create, then mkdir them in a single
        // pass. Previously each file looped through `mkdirp(parent)`
        // which always did an `exists()` check (= statx syscall) even
        // though the same parents were shared by dozens of siblings —
        // `materialize_into` for a typical 32-file npm package
        // resulted in ~25 redundant statx calls. Collecting the unique
        // parents first, sorting by length (so ancestors precede
        // descendants), and calling `create_dir_all` once each cuts
        // out the redundant stats entirely. `BTreeSet` sorts
        // lexicographically, which is good enough because every
        // ancestor of a directory is a prefix of it.
        // Collect into Vec + sort + dedup instead of BTreeSet. For a
        // package with thousands of files (typescript, next), the
        // BTreeSet's per-insert log-N PathBuf comparison (~50-byte
        // memcmps) was a measurable cost on top of the redundant
        // create_dir_all that the set was deduplicating in the first
        // place.
        let mut parents: Vec<PathBuf> = Vec::with_capacity(index.len() / 4 + 4);
        // The whole-dir clone already created `pkg_nm_dir` and every
        // per-file subdir under it, and `clonefile(2)` REQUIRES its
        // destination not pre-exist — so in the CloneDir case we must
        // NOT push `pkg_nm_dir` or the per-file parents. We still
        // validate every index key (the path-traversal guard is not
        // optional) and still create the scoped-`@scope` parents the
        // transitive-symlink pass needs (those live under
        // `pkg_nm_parent`, a sibling of the cloned tree, not inside it).
        if !used_clonedir {
            parents.push(pkg_nm_dir.clone());
        }
        // Validate every key once here. The file-linking loop below
        // walks the same immutable index, so skipping the check
        // there is safe.
        for rel_path in index.keys() {
            validate_index_key(rel_path)?;
            if !used_clonedir {
                let target = pkg_nm_dir.join(rel_path);
                if let Some(parent) = target.parent() {
                    parents.push(parent.to_path_buf());
                }
            }
        }
        // Scoped transitive deps need `pkg_nm_parent/@scope/` to exist
        // before the symlink call; include those parents in the batch.
        for dep_name in pkg.dependencies.keys() {
            if let Some(slash) = dep_name.find('/')
                && dep_name.starts_with('@')
            {
                parents.push(pkg_nm_parent.join(&dep_name[..slash]));
            }
        }
        parents.sort_unstable();
        parents.dedup();
        for parent in &parents {
            std::fs::create_dir_all(parent).map_err(|e| Error::Io(parent.clone(), e))?;
        }

        // `materialize_into` always writes into a fresh location
        // (either a `.tmp-<pid>-...` staging dir for the global virtual
        // store or a per-project `.aube/<dep_path>` just created by
        // the caller), so we can skip the `remove_file(dst)` that
        // `link_file` does defensively. Pass `fresh = true` to suppress
        // the unlink syscall on every file. For a 1.4k-package install
        // that's ~45k wasted `unlink` calls on the hot path.
        //
        // Skipped entirely when the whole-dir clone already filled the
        // package directory — files, subdirs, symlinks, and +x bits all
        // came across in the single `clonefile(2)`.
        for (rel_path, stored) in index {
            if used_clonedir {
                break;
            }
            // Key already validated in the parent-collection loop
            // above. The index is immutable between the two loops.
            let target = pkg_nm_dir.join(rel_path);

            if let Err(e) = self.link_file_fresh(stored, rel_path, &target) {
                if let Error::MissingStoreFile { .. } = &e {
                    invalidate_stale_index_for_package(&self.store, pkg, self.index_read_key(pkg));
                }
                return Err(e);
            }
            stats.files_linked += 1;
            self.note_files_linked(1);

            if stored.executable {
                // `create_cas_file` writes every CAS entry as 0o644
                // unconditionally; the only place a CAS entry's
                // shared inode gets the +x bit is the very first
                // `make_executable` call against a hardlinked or
                // reflinked target — that `chmod` upgrades the
                // shared inode for every later linker that points
                // at it. Skipping the call (an earlier optimization)
                // produced 0o644 binaries on cold installs and
                // broke every CLI shipped via npm.
                #[cfg(unix)]
                xx::file::make_executable(&target).map_err(|e| Error::Xx(e.to_string()))?;
            }
        }

        // Apply any user-supplied patch for this `(name, version)`.
        // Patches are applied *after* the files have been linked into
        // the virtual store but *before* transitive symlinks, so the
        // patched bytes live alongside the unpatched ones at a
        // distinct subdir (the graph hash callback is responsible for
        // making sure that's true).
        let patch_key = pkg.spec_key();
        if let Some(patch_text) = self.patches.get(&patch_key) {
            apply_multi_file_patch(&pkg_nm_dir, patch_text)
                .map_err(|msg| Error::Patch(patch_key.clone(), msg))?;
        }

        // Create symlinks for transitive dependencies. Parents for
        // scoped packages were added to the `parents` batch above, so
        // we no longer need a per-symlink mkdirp. We also skip the
        // `symlink_metadata().is_ok()` existence check: callers
        // guarantee the target directory is freshly created (either a
        // `.tmp-<pid>-...` staging dir for the global virtual store or
        // a per-project `.aube/<dep_path>` that the caller just
        // ensured is empty), so nothing can be in the way.
        for (dep_name, dep_version) in &pkg.dependencies {
            // Resolve the edge VALUE to the canonical package key the dep was
            // materialized under. Readers disagree on what the value holds — the
            // yarn readers store the full dep_path (`is-plain-obj@4.1.0`),
            // npm/pnpm the tail, git/remote-tarball the resolved URL — so the
            // former inline `name@tail` guess doubled the name for the yarn
            // convention (`is-plain-obj@is-plain-obj@4.1.0`), pointing the
            // sibling symlink at a nonexistent `.aube` entry (a dangling link
            // that breaks resolution). `resolve_dep_edge` tries all three
            // conventions against the real graph keys; the fallback preserves
            // today's behavior for an edge that isn't a graph node (a `link:`
            // target keyed only in `nested_link_targets`, or a pruned optional).
            let dep_dep_path =
                resolve_dep_edge(dep_name, dep_version, |k| graph.packages.contains_key(k))
                    .unwrap_or_else(|| {
                        shared_local_dep_path(dep_name, dep_version)
                            .unwrap_or_else(|| format!("{dep_name}@{dep_version}"))
                    });
            // Skip any dep whose name matches the package being
            // materialized, regardless of version. The symlink would
            // land at `pkg_nm_parent.join(dep_name)` which is exactly
            // `pkg_nm_dir` — the directory we just populated with the
            // package's own files — and `create_dir_link` would fail
            // EEXIST. The skip used to require version-equality too,
            // but published packages occasionally declare a *different*
            // version of themselves as a dep (e.g. `react_ujs@3.3.0`
            // pins `react_ujs@^2.7.1`, an artifact of how its build
            // script generates its package.json). Treat that as a
            // self-reference: `require('<self>')` from inside the
            // package resolves to its own files, matching what npm /
            // pnpm / yarn end up with after their hoisting passes.
            if dep_name == &pkg.name {
                continue;
            }
            let symlink_path = pkg_nm_parent.join(dep_name);
            // `link:` transitive: the resolver pinned an absolute
            // on-disk target. Skip the virtual-store sibling lookup
            // (there is no `.aube/<dep>@link+...` entry for these) and
            // symlink straight at the source directory.
            //
            // Store the absolute target verbatim. A relative path
            // would have to thread two pitfalls at once: the GVS
            // tmp→final rename (link's own depth changes by one) AND
            // macOS `/tmp`→`/private/tmp` symlink expansion (the dir
            // the OS resolves the link from is one level deeper than
            // `self.virtual_store` lexically suggests). Either alone
            // is fixable; together every `pathdiff` variant lands one
            // component off and the link dangles. Sibling symlinks
            // get away with relative paths because both endpoints
            // live inside `base_dir` and move together; nested-link
            // targets are *external* (under `project_dir`) so the
            // tricks that work for siblings don't apply. Windows
            // already uses absolute targets for the same reason (see
            // the `#[cfg(windows)]` block below).
            if let Some(map) = nested_link_targets
                && let Some(abs_target) = map.get(&dep_dep_path)
            {
                sys::create_dir_link(abs_target, &symlink_path)
                    .map_err(|e| Error::Io(symlink_path.clone(), e))?;
                continue;
            }
            // Match the parent's convention: global-store materialization
            // walks sibling subdirs under their hashed names, while the
            // per-project `.aube/` layout uses raw dep_paths.
            let sibling_subdir = if apply_hashes {
                self.virtual_store_subdir(&dep_dep_path)
            } else {
                self.aube_dir_entry_name(&dep_dep_path)
            };
            // Compute the relative path from the symlink's parent to
            // the sibling dep directory. The symlink's parent is
            // `pkg_nm_parent/` for a bare name but
            // `pkg_nm_parent/@scope/` for a scoped one, so we can't
            // hard-code `../..` — doing so would undercount by one
            // level for every scoped transitive dep and produce a
            // dangling link. `pathdiff::diff_paths` walks the
            // difference for us, yielding `../..` for `foo` and
            // `../../..` for `@vue/shared`, both relative to whatever
            // parent `symlink_path` ends up with.
            // `pkg_nm_parent` is `<base_dir>/<subdir>/node_modules/`, so
            // two parents deep brings us to `<base_dir>/` where all
            // sibling subdirs live side-by-side.
            #[cfg(not(windows))]
            let target = {
                let virtual_root = pkg_nm_parent
                    .parent()
                    .and_then(Path::parent)
                    .unwrap_or(&pkg_nm_parent);
                let sibling_abs = virtual_root
                    .join(&sibling_subdir)
                    .join("node_modules")
                    .join(dep_name);
                let link_parent = symlink_path.parent().unwrap_or(&pkg_nm_parent);
                pathdiff::diff_paths(&sibling_abs, link_parent)
                    .unwrap_or_else(|| sibling_abs.clone())
            };

            // Staged materialization writes into `.tmp-<pid>-<id>/`,
            // then atomic-renames into `final_base_dir/<subdir>/`.
            // POSIX symlinks store the relative offset verbatim.
            // Offset stays invariant under the wrapper rename, so the
            // link resolves correctly after the move. Windows junctions
            // resolve the target against `link.parent()` at create time
            // and persist an absolute path, which binds the junction to
            // the tmp wrapper. Point Windows at the final root up front
            // so the stored absolute path survives the rename.
            #[cfg(windows)]
            let target = final_base_dir
                .join(&sibling_subdir)
                .join("node_modules")
                .join(dep_name);

            sys::create_dir_link(&target, &symlink_path)
                .map_err(|e| Error::Io(symlink_path.clone(), e))?;
        }

        stats.packages_linked += 1;
        trace!("materialized {dep_path} ({} files)", index.len());
        Ok(())
    }

    /// Hardlink-or-copy a file into a freshly-created destination.
    /// Assumes `dst` does not exist — callers (`materialize_into`)
    /// always write into a `.tmp-<pid>-...` staging dir or a
    /// just-wiped per-project `.aube/<dep_path>`, so the defensive
    /// `remove_file(dst)` an idempotent variant would need is skipped.
    /// Eliminates one syscall per linked file (~45k on the medium
    /// benchmark fixture).
    pub(crate) fn link_file_fresh(
        &self,
        stored: &StoredFile,
        rel_path: &str,
        dst: &Path,
    ) -> Result<(), Error> {
        #[cfg(target_os = "macos")]
        const SMALL_FILE_COPY_MAX: u64 = 16 * 1024;
        let map_io = |e: std::io::Error| classify_link_error(stored, rel_path, dst, e);
        let missing_source = || Error::MissingStoreFile {
            store_path: stored.store_path.clone(),
            rel_path: rel_path.to_string(),
        };
        // Track the realized strategy (may differ from `self.strategy` when
        // a reflink or hardlink falls back to copy) for diagnostic
        // attribution. Diag emits a `linker.link_<strategy>` event with
        // the per-file duration so the analyzer can break down link cost
        // by realized path: reflink (zero-copy CoW), hardlink (zero-cost
        // metadata link), or copy (full byte transfer).
        let diag_t0 = aube_util::diag::enabled().then(std::time::Instant::now);
        let realized: &'static str;
        match self.strategy {
            // Two reflink strategies share the clonefile attempt and the
            // macOS small-file copy shortcut, but differ in fallback:
            //   * `Reflink` (explicit `clone` / `clone-or-copy`) — the
            //     documented contract is reflink with a plain copy
            //     fallback, so a clonefile failure degrades straight to
            //     copy.
            //   * `ReflinkAuto` (`auto` on a same-FS macOS target) — the
            //     probe already proved the target shares a mount, so on a
            //     non-APFS same-FS volume (HFS+, where `clonefile` is
            //     unsupported but hardlinks are not) it tries a zero-cost
            //     hardlink before copy.
            LinkStrategy::Reflink | LinkStrategy::ReflinkAuto => {
                let auto = matches!(self.strategy, LinkStrategy::ReflinkAuto);
                #[cfg(target_os = "macos")]
                if matches!(stored.size, Some(size) if size <= SMALL_FILE_COPY_MAX) {
                    std::fs::copy(&stored.store_path, dst).map_err(map_io)?;
                    if let Some(t0) = diag_t0 {
                        aube_util::diag::event(
                            aube_util::diag::Category::Linker,
                            "link_macos_small_copy",
                            t0.elapsed(),
                            None,
                        );
                    }
                    return Ok(());
                }
                let reflink_result = {
                    #[cfg(test)]
                    {
                        if FORCE_REFLINK_FAILURE.load(std::sync::atomic::Ordering::Relaxed) {
                            Err(std::io::Error::new(
                                std::io::ErrorKind::Unsupported,
                                "forced reflink failure (test)",
                            ))
                        } else {
                            reflink_copy::reflink(&stored.store_path, dst)
                        }
                    }
                    #[cfg(not(test))]
                    {
                        reflink_copy::reflink(&stored.store_path, dst)
                    }
                };
                if let Err(e) = reflink_result {
                    // Source-missing short-circuit avoids the misleading
                    // "fell back to copy" trace and the redundant copy
                    // attempt that would just ENOENT for the same reason.
                    if !stored.store_path.exists() {
                        return Err(missing_source());
                    }
                    // `auto` (ReflinkAuto) is the resolved strategy only
                    // when the same-FS probe succeeded, so the target is
                    // known same-filesystem. `reflink_copy::reflink` uses
                    // `clonefile`, which is APFS-only: on an HFS+ volume it
                    // fails even though the volume is same-FS and supports
                    // hardlinks. There, try a hardlink before degrading to
                    // a full per-file copy — a hardlink is zero-cost and
                    // preserves the dedupe the probe promised, where copy
                    // silently regresses to a byte transfer per file.
                    // Explicit `clone` / `clone-or-copy` (Reflink) keep
                    // their documented copy fallback and skip this step.
                    //
                    // Surface the hardlink attempt's OWN error in the copy
                    // trace: if the auto hardlink itself fails (a cross-mount
                    // edge the probe missed, a transient permission error), it
                    // — not the original reflink error — is the proximate
                    // cause of the copy, so reporting only `e` would point at
                    // the wrong failure.
                    let hardlinked = if auto {
                        match std::fs::hard_link(&stored.store_path, dst) {
                            Ok(()) => {
                                trace!("reflink failed, fell back to hardlink: {e}");
                                true
                            }
                            Err(he) => {
                                trace!(
                                    "reflink failed ({e}); hardlink fallback also failed ({he}); falling back to copy"
                                );
                                false
                            }
                        }
                    } else {
                        false
                    };
                    if hardlinked {
                        realized = "reflink_fallback_hardlink";
                    } else {
                        if !auto {
                            trace!("reflink failed, falling back to copy: {e}");
                        }
                        std::fs::copy(&stored.store_path, dst).map_err(map_io)?;
                        realized = "reflink_fallback_copy";
                    }
                } else {
                    realized = "reflink";
                }
            }
            LinkStrategy::Hardlink => {
                if let Err(e) = std::fs::hard_link(&stored.store_path, dst) {
                    if !stored.store_path.exists() {
                        return Err(missing_source());
                    }
                    // Fall back to copy on cross-filesystem errors (EXDEV)
                    trace!("hardlink failed, falling back to copy: {e}");
                    std::fs::copy(&stored.store_path, dst).map_err(map_io)?;
                    realized = "hardlink_fallback_copy";
                } else {
                    realized = "hardlink";
                }
            }
            LinkStrategy::Copy => {
                std::fs::copy(&stored.store_path, dst).map_err(map_io)?;
                realized = "copy";
            }
        }

        if let Some(t0) = diag_t0 {
            // `realized` is one of seven static strings; matching is
            // O(1) and the static `&str` keeps the JSONL category compact.
            let name = match realized {
                "reflink" => "link_reflink",
                "reflink_fallback_hardlink" => "link_reflink_fallback_hardlink",
                "reflink_fallback_copy" => "link_reflink_fallback",
                "hardlink" => "link_hardlink",
                "hardlink_fallback_copy" => "link_hardlink_fallback",
                "copy" => "link_copy",
                _ => "link_unknown",
            };
            aube_util::diag::event(aube_util::diag::Category::Linker, name, t0.elapsed(), None);
        }
        Ok(())
    }

    /// Attempt the whole-dir `clonefile(2)` fill of `pkg_nm_dir` from
    /// the store's extracted-tree tier. Returns `Ok(true)` when the
    /// clone happened (caller skips the per-file loop) and `Ok(false)`
    /// when any gate condition failed (caller runs the unchanged
    /// per-file path). Never returns a hard error for a *clone* failure
    /// — those degrade to the per-file path — only for the bookkeeping
    /// that would also fail the per-file path.
    ///
    /// Steps, all gated so a miss is byte-for-byte today's behavior:
    /// 1. macOS+APFS+same-volume probe (`clonedir::can_clonedir`),
    ///    against `pkg_nm_parent` (the dir the clone lands inside).
    /// 2. Ensure the tree source exists (lazily build it once from the
    ///    CAS, reflinking each file — the per-package amortized cost).
    /// 3. One `clonefile(2)` of the whole tree into `pkg_nm_dir`.
    ///
    /// `pub(crate)` so the hoisted linker reuses the identical whole-dir
    /// clone — its per-placement fill is otherwise bound by APFS
    /// serializing file creation, which the one-syscall clone sidesteps.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn try_clonedir_fill(
        &self,
        pkg_nm_dir: &Path,
        pkg_nm_parent: &Path,
        tree_src: &Path,
        dep_path: &str,
        pkg: &LockedPackage,
        index: &PackageIndex,
        stats: &mut LinkStats,
    ) -> Result<bool, Error> {
        #[cfg(not(target_os = "macos"))]
        {
            // No recursive-dir clone primitive off macOS — keep the
            // per-file path. Suppress unused-variable warnings.
            let _ = (
                pkg_nm_dir,
                pkg_nm_parent,
                tree_src,
                dep_path,
                pkg,
                index,
                stats,
            );
            Ok(false)
        }
        #[cfg(target_os = "macos")]
        {
            // Kill-switch for the whole-dir clone fast path. Set
            // `AUBE_DISABLE_CLONEDIR=1` to force the per-file path on
            // macOS — used to A/B the mechanism and as a regression
            // escape hatch. Read once per process.
            {
                static DISABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
                if *DISABLED
                    .get_or_init(|| aube_util::env::embedder_env("DISABLE_CLONEDIR").is_some())
                {
                    return Ok(false);
                }
            }
            // Empty packages (no files) gain nothing and the tree-build
            // would create an empty source dir; let the (no-op) per-file
            // path handle them.
            if index.is_empty() {
                return Ok(false);
            }
            // Create the clone destination's IMMEDIATE parent, not just
            // `pkg_nm_parent`. `clonefile(2)` ENOENTs unless the dst's
            // direct parent exists; for a SCOPED package the dst is
            // `<node_modules>/@scope/<name>`, so that parent is the
            // `@scope` dir — `pkg_nm_parent` (`<node_modules>` itself) is
            // one level too shallow. Creating only `pkg_nm_parent` left
            // `@scope` absent, so every scoped package's clone ENOENT'd and
            // silently fell to the per-file path — the whole-dir clone fast
            // path never fired for `@babel/*`/`@types/*`/`@firebase/*`/…,
            // the bulk of a modern tree. `pkg_nm_dir.parent()` is
            // `pkg_nm_parent` for unscoped (unchanged) and its `@scope`
            // child for scoped; `create_dir_all` still makes `pkg_nm_parent`
            // (the same-volume probe target below) as an ancestor.
            // Idempotent with the caller's later parent batch.
            let clone_parent = pkg_nm_dir.parent().unwrap_or(pkg_nm_parent);
            std::fs::create_dir_all(clone_parent)
                .map_err(|e| Error::Io(clone_parent.to_path_buf(), e))?;

            // Volume/fs probe against `trees/` (created lazily if this
            // is the first clonedir attempt of the install) vs the
            // destination parent. A `false` here keeps the per-file
            // path: non-APFS dst, cross-volume, or `trees/` uncreatable.
            if !self.ensure_trees_dir_then_probe(pkg_nm_parent) {
                return Ok(false);
            }

            // Ensure the clone source exists. Build it once if missing.
            if !tree_src.exists() && self.build_tree(tree_src, dep_path, pkg, index).is_err() {
                // Tree build failed (e.g. a CAS shard went missing) —
                // fall back to the per-file path, which surfaces the
                // same error with full attribution + index invalidation.
                return Ok(false);
            }

            // The destination must not pre-exist for clonefile. The
            // caller guarantees `pkg_nm_dir` is in a fresh staging tree,
            // so it does not — but guard anyway: a stray dir would make
            // the clone EEXIST, and silently falling back is safer than
            // erroring.
            if pkg_nm_dir.exists() {
                return Ok(false);
            }

            match crate::clonedir::clonefile_dir(tree_src, pkg_nm_dir) {
                Ok(()) => {
                    // Keep stats identical to the per-file path: every
                    // index entry is "linked", just in one syscall.
                    stats.files_linked += index.len();
                    self.note_files_linked(index.len());
                    if let Some(t0) = aube_util::diag::enabled().then(std::time::Instant::now) {
                        aube_util::diag::event(
                            aube_util::diag::Category::Linker,
                            "link_clonedir",
                            t0.elapsed(),
                            None,
                        );
                    }
                    trace!("clonedir-materialized {dep_path} ({} files)", index.len());
                    Ok(true)
                }
                Err(e) => {
                    // A failed clone can leave a partial dst. Remove it
                    // so the per-file fallback writes into a clean dir.
                    let _ = std::fs::remove_dir_all(pkg_nm_dir);
                    trace!("clonedir failed for {dep_path}, falling back to per-file: {e}");
                    Ok(false)
                }
            }
        }
    }

    /// Lazily create the `trees/` root then re-run the volume probe.
    /// `trees/` may not exist on the very first clonedir attempt of an
    /// install; `can_clonedir` needs it to stat the source volume. We
    /// create it, then probe `trees/` against `pkg_nm_parent`. Returns
    /// the probe result. macOS-only caller.
    ///
    /// Both halves are run-invariant, so both are memoized. `create_dir_all`
    /// of the store `trees/` root only needs to succeed once per process;
    /// `can_clonedir`'s answer for a `(trees_dir, dst_parent)` pair is fixed
    /// by the two volumes' `st_dev` + the destination filesystem type, none
    /// of which change during a run. Uncached, the hoisted link pass calls
    /// this once per package (hundreds of times), each repeating a
    /// `create_dir_all` walk + two `metadata` + one `statfs` — pure
    /// redundant syscalls after the first probe of a given parent. The
    /// per-parent memo collapses them: a mostly-flat hoisted tree shares one
    /// `node_modules` parent across most packages, so the probe runs a
    /// handful of times instead of hundreds. Mirrors `detect_strategy_cross`'s
    /// process-lifetime cache; the return value is byte-identical either way.
    #[cfg(target_os = "macos")]
    fn ensure_trees_dir_then_probe(&self, pkg_nm_parent: &Path) -> bool {
        let trees_dir = self.store.trees_dir();

        // `trees/` creation: attempt once per process, memoize success.
        static TREES_READY: std::sync::OnceLock<
            std::sync::Mutex<std::collections::HashSet<PathBuf>>,
        > = std::sync::OnceLock::new();
        {
            let ready = TREES_READY.get_or_init(Default::default);
            let already = ready
                .lock()
                .expect("trees-ready cache poisoned")
                .contains(&trees_dir);
            if !already {
                if std::fs::create_dir_all(&trees_dir).is_err() {
                    return false;
                }
                ready
                    .lock()
                    .expect("trees-ready cache poisoned")
                    .insert(trees_dir.clone());
            }
        }

        // Probe answer keyed on (trees_dir, dst_parent) for the process
        // lifetime. First-write-wins so a concurrent linker can't clobber.
        type ProbeKey = (PathBuf, PathBuf);
        static PROBE_CACHE: std::sync::OnceLock<
            std::sync::RwLock<std::collections::HashMap<ProbeKey, bool>>,
        > = std::sync::OnceLock::new();
        let key = (trees_dir.clone(), pkg_nm_parent.to_path_buf());
        let cache = PROBE_CACHE.get_or_init(Default::default);
        if let Some(hit) = cache.read().expect("probe cache poisoned").get(&key) {
            return *hit;
        }
        let result = crate::clonedir::can_clonedir(trees_dir.as_path(), pkg_nm_parent);
        *cache
            .write()
            .expect("probe cache poisoned")
            .entry(key)
            .or_insert(result)
    }

    /// Build the extracted-tree clone source for a package at
    /// `tree_src`, reflinking each CAS file into it exactly as the
    /// per-file materialize loop would. Written into a PID-stamped temp
    /// dir then atomically renamed into place so concurrent installers
    /// either see the complete tree or none of it. A lost rename race
    /// (another process built it first) is success — its tree is
    /// byte-identical (same CAS content) and we discard ours.
    ///
    /// The tree root IS the package root: files land at their index
    /// `rel_path` directly under `tree_src`, +x bits applied, matching
    /// what a `clonefile(2)` of it into `<entry>/node_modules/<name>/`
    /// must reproduce. Transitive-dep symlinks are deliberately NOT
    /// written here — those are per-materialization (they point at
    /// sibling entries whose paths differ per project/GVS), so the
    /// clone fills only the package's own files and the symlink pass
    /// runs after every clone. macOS-only caller.
    #[cfg(target_os = "macos")]
    fn build_tree(
        &self,
        tree_src: &Path,
        dep_path: &str,
        pkg: &LockedPackage,
        index: &PackageIndex,
    ) -> Result<(), Error> {
        let trees_dir = self.store.trees_dir();
        std::fs::create_dir_all(&trees_dir).map_err(|e| Error::Io(trees_dir.clone(), e))?;

        let leaf = tree_src
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| dep_path.to_string());
        // The staging dir name MUST be unique per call, not just per
        // (pid, leaf): the hoisted linker materializes the same
        // `dep_path` at multiple placement sites, and two same-depth
        // duplicates run concurrently in the per-level par_iter. Both
        // would build the SAME `tree_src` (identical leaf) on a cold
        // cache — sharing one tmp dir means one build's
        // `remove_dir_all`/`link_file_fresh` corrupts the other's
        // in-flight tree before its atomic publish, landing a partial
        // tree in the global store. A per-call counter keeps each build
        // isolated; the atomic `rename` below still makes the redundant
        // second build a harmless lost race. (The isolated linker never
        // hit this — it par_iters a dep_path-keyed map, so its leaves
        // are always distinct.)
        let uniq = {
            static NEXT_TREE_TMP_ID: AtomicU64 = AtomicU64::new(0);
            NEXT_TREE_TMP_ID.fetch_add(1, Ordering::Relaxed)
        };
        let tmp = trees_dir.join(format!(".tmp-tree-{}-{uniq}-{leaf}", std::process::id()));
        // Clear any leftover from a crashed predecessor.
        let _ = std::fs::remove_dir_all(&tmp);

        // Reuse the per-file fill. Collect unique parents first (same
        // single-pass mkdir discipline as `materialize_into`).
        let mut parents: Vec<PathBuf> = Vec::with_capacity(index.len() / 4 + 4);
        parents.push(tmp.clone());
        for rel_path in index.keys() {
            validate_index_key(rel_path)?;
            if let Some(parent) = tmp.join(rel_path).parent() {
                parents.push(parent.to_path_buf());
            }
        }
        parents.sort_unstable();
        parents.dedup();
        for parent in &parents {
            std::fs::create_dir_all(parent).map_err(|e| Error::Io(parent.clone(), e))?;
        }

        for (rel_path, stored) in index {
            let target = tmp.join(rel_path);
            if let Err(e) = self.link_file_fresh(stored, rel_path, &target) {
                let _ = std::fs::remove_dir_all(&tmp);
                if let Error::MissingStoreFile { .. } = &e {
                    invalidate_stale_index_for_package(&self.store, pkg, self.index_read_key(pkg));
                }
                return Err(e);
            }
            #[cfg(unix)]
            if stored.executable {
                xx::file::make_executable(&target).map_err(|e| Error::Xx(e.to_string()))?;
            }
        }

        // Atomic publish. A lost race means another writer's
        // byte-identical tree already landed — keep theirs.
        match aube_util::fs_atomic::rename_with_retry(&tmp, tree_src) {
            Ok(()) => Ok(()),
            Err(_) if tree_src.exists() => {
                let _ = std::fs::remove_dir_all(&tmp);
                Ok(())
            }
            Err(e) => {
                let _ = std::fs::remove_dir_all(&tmp);
                Err(Error::Io(tree_src.to_path_buf(), e))
            }
        }
    }
}

/// Translate a copy failure into the most informative linker error.
/// ENOENT can mean either side of the operation is missing — stat the
/// source CAS shard to attribute it. A missing shard means the cached
/// package index is out of sync with the on-disk store, which the
/// caller can recover from by invalidating the cached index and
/// re-importing the tarball.
fn classify_link_error(
    stored: &StoredFile,
    rel_path: &str,
    dst: &Path,
    err: std::io::Error,
) -> Error {
    if err.kind() == std::io::ErrorKind::NotFound && !stored.store_path.exists() {
        return Error::MissingStoreFile {
            store_path: stored.store_path.clone(),
            rel_path: rel_path.to_string(),
        };
    }
    Error::Io(dst.to_path_buf(), err)
}

/// Best-effort drop the cached package index when materialize discovers
/// its referenced CAS shard is gone. Callers always surface the original
/// `MissingStoreFile` error first; this side effect just makes sure the
/// next install miss `load_index` instead of looping on the same dead
/// reference. If the cache write fails (e.g. permission error), warn
/// loudly so the user knows the auto-recovery didn't take and they need
/// to wipe the index dir by hand (run `aube store path` to find it).
pub(crate) fn invalidate_stale_index_for_package(
    store: &aube_store::Store,
    pkg: &LockedPackage,
    // The same key the warm read uses (`Linker::index_read_key`): the
    // lockfile SRI, or the per-project computed-sha512 binding for a
    // no-integrity package. Invalidating by `pkg.integrity` (`None`)
    // would clear a key the read never uses and leave the stale hex
    // index in place — the exact dead-reference loop this guards against.
    read_key: Option<&str>,
) {
    match store.invalidate_cached_index(pkg.registry_name(), &pkg.version, read_key) {
        Ok(true) => debug!("invalidated stale index for {}", pkg.spec_key()),
        Ok(false) => {}
        Err(e) => warn!(
            "failed to invalidate stale index for {}: {e}; manual recovery: rm -rf \"$(aube store path)/index\"",
            pkg.spec_key()
        ),
    }
}

/// Defence in depth for the tarball path-traversal class. The
/// primary guard lives in `aube_store::import_tarball`, which
/// refuses malformed entries before they enter the `PackageIndex`.
/// This helper is the last check before `base.join(key)` is
/// written through the linker, so an index loaded from a cache
/// file that predates the store-side validation (or a bug that
/// lets a traversing key slip past it) still cannot produce a
/// file outside the package root.
pub(crate) fn validate_index_key(key: &str) -> Result<(), Error> {
    if key.is_empty()
        || key.starts_with('/')
        || key.starts_with('\\')
        || key.contains('\0')
        || key.contains('\\')
    {
        return Err(Error::UnsafeIndexKey(key.to_string()));
    }
    // Reject any `..` component or Windows drive prefix like `C:`
    // that would make `Path::join` escape the base.
    for component in std::path::Path::new(key).components() {
        match component {
            std::path::Component::ParentDir
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => {
                return Err(Error::UnsafeIndexKey(key.to_string()));
            }
            std::path::Component::Normal(os) => {
                #[cfg(windows)]
                {
                    if let Some(s) = os.to_str()
                        && s.contains(':')
                    {
                        return Err(Error::UnsafeIndexKey(key.to_string()));
                    }
                }
                #[cfg(not(windows))]
                {
                    let _ = os;
                }
            }
            std::path::Component::CurDir => {}
        }
    }
    Ok(())
}

/// Validate a package/dependency alias before it becomes a path below
/// `node_modules`. npm names allow either `name` or `@scope/name`; every
/// other slash shape is a filesystem path, not a package slot.
pub(crate) fn validate_package_link_name(name: &str) -> Result<(), Error> {
    if name.is_empty() || name.contains('\0') || name.contains('\\') || name.starts_with('/') {
        return Err(Error::UnsafePackageName(name.to_string()));
    }
    let parts: Vec<&str> = name.split('/').collect();
    let ok = match parts.as_slice() {
        [bare] => is_safe_package_component(bare),
        [scope, bare] => {
            scope.starts_with('@')
                && scope.len() > 1
                && is_safe_package_component(scope)
                && is_safe_package_component(bare)
        }
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(Error::UnsafePackageName(name.to_string()))
    }
}

fn is_safe_package_component(component: &str) -> bool {
    if component.is_empty() || matches!(component, "." | "..") {
        return false;
    }
    if component.len() >= 2 && component.as_bytes()[1] == b':' {
        return false;
    }
    !std::path::Path::new(component).components().any(|c| {
        matches!(
            c,
            std::path::Component::ParentDir
                | std::path::Component::RootDir
                | std::path::Component::Prefix(_)
        )
    })
}

#[cfg(test)]
mod package_name_tests {
    use super::*;

    #[test]
    fn validate_package_link_name_accepts_npm_slots() {
        validate_package_link_name("react").unwrap();
        validate_package_link_name("@scope/pkg").unwrap();
    }

    #[test]
    fn validate_package_link_name_rejects_path_shapes() {
        for name in [
            "",
            ".",
            "..",
            "../evil",
            "@scope/../evil",
            "@scope/pkg/extra",
            "/abs",
            "C:evil",
            "pkg\\evil",
            "pkg\0evil",
        ] {
            assert!(
                matches!(
                    validate_package_link_name(name),
                    Err(Error::UnsafePackageName(_))
                ),
                "{name:?} should be rejected"
            );
        }
    }
}
