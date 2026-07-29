//! Drop `com.apple.quarantine` from the native binaries of a freshly
//! materialized package (macOS only; a no-op stub elsewhere).
//!
//! ## Why the materialized target, and not the store entry
//!
//! Quarantine reaches a materialized package by two independent routes,
//! and only stripping the *target, after creation* closes both:
//!
//! 1. **Source propagation.** `clonefile(2)` and `copyfile(3)` carry the
//!    source's xattrs across, so one quarantined CAS blob poisons every
//!    package materialized from it — and `rm -rf node_modules` never
//!    clears it, because the store copy is the source.
//! 2. **Creation-time stamping.** A process that inherited quarantine
//!    flags — a terminal embedded in a Gatekeeper-enabled app, e.g. a
//!    GUI Git client with `LSFileQuarantineEnabled` — has the kernel
//!    stamp *every inode it creates*, whatever the source was. Measured
//!    against a verifiably clean store blob: the `clonefile` and
//!    `copyfile` targets still come out quarantined. Only a hardlink
//!    escapes, because it creates no new inode.
//!
//! (2) is the reason this is not a store-side fix. Stripping the CAS
//! entry would only help the hardlink strategy, and macOS defaults to
//! `ReflinkAuto` (`clonefile`). Opting the process out of quarantine
//! instead is not available either: the flags are monotonic, so
//! `qtn_proc_apply_to_self` with them cleared reports success and
//! changes nothing, and the path-exclusion SPI returns `EPERM`.
//!
//! Dropping the attribute is safe here for the same reason Homebrew
//! drops it after checksum verification: the tarball was verified
//! against the lockfile's integrity hash before any of its files reached
//! the CAS, so Gatekeeper's unverified-download provenance adds nothing
//! on top. Only `com.apple.quarantine` is touched; every other xattr on
//! the file is left alone.
//!
//! ## Why the warm path strips too
//!
//! The strip runs on cache hits as well as fresh materializations. The
//! global virtual store is on by default and lives outside the project,
//! so an entry written by a pre-fix build survives `rm -rf node_modules`
//! and would be re-linked, still quarantined, forever — which is exactly
//! the state #575 reports. Stripping the cached entry is what makes the
//! recovery that issue describes — remove `node_modules`, reinstall —
//! actually heal a poisoned store. That is deliberately preferred over a
//! one-shot migration marker, which would have to choose between
//! sweeping the whole store and marking it swept while most entries were
//! never visited.
//!
//! ## Known boundaries
//!
//! - Reach is "every package this run links", not "every package on
//!   disk". Besides the frozen fast path, which short-circuits before
//!   the linker entirely, the per-package warm checks in `link.rs`
//!   (`EntryState::Fresh` and the `aube_entry.exists()` checks the
//!   install fast path relies on) return before the index is loaded —
//!   they lazy-load it only after the guard, so stripping there would
//!   cost a `load_index` per package on the warm path, far more than the
//!   strip it enables. The practical consequence: a repeat install with
//!   `node_modules` intact strips nothing new, and healing a poisoned
//!   store goes through the remove-and-reinstall path above.
//! - The index is the tarball's file list, so files written into a
//!   package *after* linking are invisible here: `node-gyp` output
//!   (`build/Release/*.node`) and side-effects-cache restores, which
//!   hardlink or `fcopyfile` a cached build tree into an already-linked
//!   package. Both are real and both are out of this module's reach by
//!   construction — do not assume this seam covers build output.

use aube_store::StoredFile;
use std::path::Path;

/// Gatekeeper only inspects Mach-O images, and neither obvious signal
/// finds all of them alone. Loadable modules (`.node`, `.dylib`, `.so`)
/// ship mode 0644, so the executable bit misses them; native CLI
/// binaries (`bin/esbuild`, `biome`, `opencode`) ship 0755 with no
/// extension, so an extension list misses them — and those are the worse
/// case, since a quarantined executable is killed on exec rather than
/// merely failing to load. Hence the union of the two. (Both halves are
/// load-bearing in practice: seeding a store and reinstalling leaves
/// `bin/esbuild` and `lightningcss.darwin-arm64.node` quarantined, one
/// caught by each half.)
///
/// Reading the Mach-O magic would be more precise, and is not worth it:
/// an `open`+`read`+`close` measured 2.4x the cost of the `removexattr`
/// it would save. Across a full store this predicate selects ~1.3% of
/// files (~1 per package), so the no-op calls on JS `bin` shims are
/// cheaper than any test that would exclude them.
///
/// Lives outside the macOS-only module, and is the one piece of this
/// file the CI gate can reach: aube's suite runs `cargo test --workspace`
/// on ubuntu, where everything below compiles away.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn is_gatekeeper_guarded(rel_path: &str, stored: &StoredFile) -> bool {
    stored.executable
        || matches!(
            Path::new(rel_path).extension().and_then(|ext| ext.to_str()),
            Some("node" | "dylib" | "so")
        )
}

#[cfg(target_os = "macos")]
mod imp {
    use super::is_gatekeeper_guarded;
    use aube_store::PackageIndex;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tracing::warn;

    /// Best-effort: a failure here can only leave a binary Gatekeeper
    /// might refuse, never a broken install, so nothing propagates.
    fn remove_quarantine(path: &Path) {
        let Ok(c_path) = CString::new(path.as_os_str().as_bytes()) else {
            return;
        };
        // SAFETY: both pointers are NUL-terminated C strings that
        // outlive the call. `XATTR_NOFOLLOW` stops a symlinked index
        // entry from redirecting the removal onto whatever it points at,
        // which could be outside the package directory entirely.
        let rc = unsafe {
            libc::removexattr(
                c_path.as_ptr(),
                c"com.apple.quarantine".as_ptr(),
                libc::XATTR_NOFOLLOW,
            )
        };
        if rc == 0 {
            return;
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            // ENOATTR — nothing to remove, which is the overwhelmingly
            // common case. ENOENT — an entry the importer dropped or
            // renamed after the index was written.
            Some(libc::ENOATTR | libc::ENOENT) => {}
            _ => warn_once(path, &err),
        }
    }

    /// At most one warning per process. A systemic failure (a volume
    /// with no xattr support, say) fails identically for every package,
    /// and the linker runs this across all of them in parallel — so
    /// without the latch a single root cause would bury the install
    /// output under one line per package.
    fn warn_once(path: &Path, err: &std::io::Error) {
        static WARNED: AtomicBool = AtomicBool::new(false);
        if !WARNED.swap(true, Ordering::Relaxed) {
            warn!(
                code = aube_codes::warnings::WARN_AUBE_QUARANTINE_STRIP_FAILED,
                "could not remove com.apple.quarantine from {}: {err}; \
                 macOS may refuse to load or run native binaries from this install",
                path.display()
            );
        }
    }

    /// Strip quarantine from every Gatekeeper-relevant file the package
    /// index names under `pkg_dir`. Call once per package, after the
    /// last step that can create files there (the patch pass) — a patch
    /// rewrites through a fresh inode, which route (2) would re-stamp.
    ///
    /// Driven off the index rather than a directory walk so it costs no
    /// traversal syscalls and stays correct on the whole-dir
    /// `clonefile` path, which fills the package without ever visiting
    /// the files individually.
    pub(crate) fn strip_from_native_binaries(pkg_dir: &Path, index: &PackageIndex) {
        for (rel_path, stored) in index {
            if is_gatekeeper_guarded(rel_path, stored) {
                remove_quarantine(&pkg_dir.join(rel_path));
            }
        }
    }

    /// Warm-path counterpart, for a virtual-store entry that already
    /// existed so nothing was materialized and the cold-path strip never
    /// ran. `entry_dir` holds the package at `node_modules/<name>` — the
    /// layout `materialize_into` writes.
    pub(crate) fn strip_cached_entry(entry_dir: &Path, pkg_name: &str, index: &PackageIndex) {
        strip_from_native_binaries(&entry_dir.join("node_modules").join(pkg_name), index);
    }

    /// Behavioural coverage for the strip itself. macOS-only by
    /// construction, so it does NOT run on aube's ubuntu CI leg — the
    /// portable half of the contract is `super::filter_tests`.
    #[cfg(test)]
    mod tests {
        use super::*;
        use aube_store::StoredFile;
        use std::os::unix::fs::symlink;

        const QUARANTINE: &str = "com.apple.quarantine";

        fn stored(executable: bool) -> StoredFile {
            StoredFile {
                hex_hash: String::new(),
                store_path: std::path::PathBuf::new(),
                executable,
                size: None,
            }
        }

        fn set_quarantine(path: &Path) {
            let c = CString::new(path.as_os_str().as_bytes()).unwrap();
            let value = b"0083;68000000;TestApp;";
            // SAFETY: NUL-terminated paths/names outliving the call.
            let rc = unsafe {
                libc::setxattr(
                    c.as_ptr(),
                    c"com.apple.quarantine".as_ptr(),
                    value.as_ptr().cast(),
                    value.len(),
                    0,
                    libc::XATTR_NOFOLLOW,
                )
            };
            assert_eq!(rc, 0, "seeding {QUARANTINE} failed");
        }

        fn is_quarantined(path: &Path) -> bool {
            xattrs(path).iter().any(|a| a == QUARANTINE)
        }

        fn xattrs(path: &Path) -> Vec<String> {
            let c = CString::new(path.as_os_str().as_bytes()).unwrap();
            let mut buf = vec![0u8; 4096];
            // SAFETY: `buf` is written up to the length we pass; a
            // negative return means nothing was written.
            let n = unsafe {
                libc::listxattr(
                    c.as_ptr(),
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                    libc::XATTR_NOFOLLOW,
                )
            };
            if n <= 0 {
                return Vec::new();
            }
            buf.truncate(n as usize);
            buf.split(|&b| b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect()
        }

        #[test]
        fn strips_quarantine_from_selected_files_and_leaves_others() {
            let dir = tempfile::tempdir().unwrap();
            let pkg = dir.path();
            std::fs::create_dir_all(pkg.join("lib")).unwrap();
            for rel in ["lib/binding.node", "bin/shim", "index.js"] {
                let p = pkg.join(rel);
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(&p, b"x").unwrap();
                set_quarantine(&p);
            }

            let mut index = PackageIndex::default();
            index.insert("lib/binding.node".into(), stored(false));
            index.insert("bin/shim".into(), stored(true));
            index.insert("index.js".into(), stored(false));
            strip_from_native_binaries(pkg, &index);

            assert!(
                !is_quarantined(&pkg.join("lib/binding.node")),
                "loadable module"
            );
            assert!(!is_quarantined(&pkg.join("bin/shim")), "executable");
            // Not Gatekeeper-relevant, so never touched.
            assert!(
                is_quarantined(&pkg.join("index.js")),
                "plain .js left alone"
            );
        }

        #[test]
        fn preserves_other_xattrs_on_a_stripped_file() {
            let dir = tempfile::tempdir().unwrap();
            let target = dir.path().join("binding.node");
            std::fs::write(&target, b"x").unwrap();
            set_quarantine(&target);
            let c = CString::new(target.as_os_str().as_bytes()).unwrap();
            // SAFETY: NUL-terminated path/name outliving the call.
            let rc = unsafe {
                libc::setxattr(
                    c.as_ptr(),
                    c"user.keepme".as_ptr(),
                    b"v".as_ptr().cast(),
                    1,
                    0,
                    libc::XATTR_NOFOLLOW,
                )
            };
            assert_eq!(rc, 0, "seeding the sibling xattr failed");

            let mut index = PackageIndex::default();
            index.insert("binding.node".into(), stored(false));
            strip_from_native_binaries(dir.path(), &index);

            let attrs = xattrs(&target);
            assert!(!attrs.iter().any(|a| a == QUARANTINE));
            assert!(attrs.iter().any(|a| a == "user.keepme"), "got {attrs:?}");
        }

        /// The warm seam end to end: materialize a package with a native
        /// addon for real, poison the materialized file, then re-enter
        /// `ensure_in_aube_dir` so it takes the `final_entry.exists()`
        /// branch, and assert the addon came back clean.
        ///
        /// This is what actually couples `strip_cached_entry`'s
        /// `<entry>/node_modules/<name>` reconstruction to the layout
        /// `materialize_into` writes. The unit test below builds that
        /// path itself, so it would stay green if the materializer
        /// moved — leaving the warm strip a silent no-op, since the
        /// strip is best-effort and a missing path is not an error.
        #[test]
        fn cached_entry_hit_strips_a_materialized_native_addon() {
            use crate::{LinkStats, LinkStrategy, Linker};
            use aube_lockfile::{LockedPackage, LockfileGraph};
            use aube_store::Store;
            use std::collections::BTreeMap;

            let dir = tempfile::tempdir().unwrap();
            let store = Store::at(dir.path().join("store/files"));
            let addon = store
                .import_bytes(b"\xcf\xfa\xed\xfe not really\n", false)
                .unwrap();
            let manifest = store
                .import_bytes(br#"{"name":"pkg","version":"1.2.3"}"#, false)
                .unwrap();
            let mut index = PackageIndex::default();
            index.insert("build/Release/native.node".to_string(), addon);
            index.insert("package.json".to_string(), manifest);

            let pkg = LockedPackage {
                name: "pkg".to_string(),
                version: "1.2.3".to_string(),
                integrity: None,
                dependencies: BTreeMap::new(),
                dep_path: "pkg@1.2.3".to_string(),
                ..Default::default()
            };
            let aube_dir = dir.path().join("project/node_modules/.aube");
            std::fs::create_dir_all(&aube_dir).unwrap();
            let linker = Linker::new_with_gvs(&store, LinkStrategy::Copy, false);

            let mut cold = LinkStats::default();
            linker
                .ensure_in_aube_dir(
                    &aube_dir,
                    "pkg@1.2.3",
                    &LockfileGraph::default(),
                    &pkg,
                    &index,
                    &mut cold,
                    None,
                )
                .expect("cold materialize");

            let addon_path = aube_dir
                .join(linker.aube_dir_entry_name("pkg@1.2.3"))
                .join("node_modules")
                .join("pkg")
                .join("build/Release/native.node");
            assert!(
                addon_path.is_file(),
                "materialize did not put the addon where the warm strip looks for it"
            );
            set_quarantine(&addon_path);

            let mut warm = LinkStats::default();
            linker
                .ensure_in_aube_dir(
                    &aube_dir,
                    "pkg@1.2.3",
                    &LockfileGraph::default(),
                    &pkg,
                    &index,
                    &mut warm,
                    None,
                )
                .expect("warm re-entry");
            assert_eq!(
                warm.packages_cached, 1,
                "expected the cache-hit branch, not a re-materialize"
            );
            assert!(
                !is_quarantined(&addon_path),
                "the cache-hit seam left the addon quarantined"
            );
        }

        /// Scoped names nest a level deeper than plain ones; this covers
        /// that composition directly. The end-to-end test above is what
        /// guards against `materialize_into` drifting.
        #[test]
        fn strip_cached_entry_resolves_the_virtual_store_entry_layout() {
            let dir = tempfile::tempdir().unwrap();
            let entry = dir.path().join("lightningcss@1.30.2-abc123");
            // Scoped names nest one level deeper; cover that too.
            for name in ["lightningcss-darwin-arm64", "@scope/pkg"] {
                let pkg_dir = entry.join("node_modules").join(name);
                std::fs::create_dir_all(&pkg_dir).unwrap();
                let target = pkg_dir.join("native.node");
                std::fs::write(&target, b"x").unwrap();
                set_quarantine(&target);

                let mut index = PackageIndex::default();
                index.insert("native.node".into(), stored(false));
                strip_cached_entry(&entry, name, &index);

                assert!(
                    !is_quarantined(&target),
                    "{name}: strip_cached_entry did not resolve to the package dir"
                );
            }
        }

        /// A missing entry (dropped or renamed after indexing) is a
        /// normal outcome, not an error — and must not warn.
        #[test]
        fn tolerates_missing_files() {
            let dir = tempfile::tempdir().unwrap();
            let mut index = PackageIndex::default();
            index.insert("absent.node".into(), stored(false));
            strip_from_native_binaries(dir.path(), &index);
        }

        /// `XATTR_NOFOLLOW`: a symlinked index entry must not let the
        /// removal reach through to its target.
        #[test]
        fn does_not_follow_symlinks_out_of_the_package() {
            let dir = tempfile::tempdir().unwrap();
            let outside = dir.path().join("outside.node");
            std::fs::write(&outside, b"x").unwrap();
            set_quarantine(&outside);

            let pkg = dir.path().join("pkg");
            std::fs::create_dir_all(&pkg).unwrap();
            symlink(&outside, pkg.join("link.node")).unwrap();

            let mut index = PackageIndex::default();
            index.insert("link.node".into(), stored(false));
            strip_from_native_binaries(&pkg, &index);

            assert!(
                xattrs(&outside).iter().any(|a| a == QUARANTINE),
                "the symlink target outside the package must be untouched"
            );
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) use imp::{strip_cached_entry, strip_from_native_binaries};

/// Quarantine is a macOS concept; every other platform links without it.
#[cfg(not(target_os = "macos"))]
pub(crate) fn strip_from_native_binaries(
    _pkg_dir: &std::path::Path,
    _index: &aube_store::PackageIndex,
) {
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn strip_cached_entry(
    _entry_dir: &std::path::Path,
    _pkg_name: &str,
    _index: &aube_store::PackageIndex,
) {
}

/// The portable half of the contract, and the only part aube's CI can
/// reach — its suite runs on ubuntu, where everything in `imp` (and its
/// behavioural tests) compiles away. Kept last in the file so no item
/// follows a test module under any `cfg` combination.
#[cfg(test)]
mod filter_tests {
    use super::*;

    fn stored(executable: bool) -> StoredFile {
        StoredFile {
            hex_hash: String::new(),
            store_path: std::path::PathBuf::new(),
            executable,
            size: None,
        }
    }

    /// The union is the whole point: npm ships loadable modules
    /// non-executable and native CLI binaries extension-less, so either
    /// signal alone misses real Mach-O that Gatekeeper will block.
    #[test]
    fn selects_loadable_modules_and_executables_but_not_plain_files() {
        for rel in ["lib/binding.node", "lib/libvips.dylib", "lib/native.so"] {
            assert!(
                is_gatekeeper_guarded(rel, &stored(false)),
                "{rel} is a loadable module and npm ships it mode 0644"
            );
        }
        assert!(
            is_gatekeeper_guarded("bin/esbuild", &stored(true)),
            "extension-less 0755 binaries are the exec-kill case"
        );
        assert!(!is_gatekeeper_guarded("index.js", &stored(false)));
        assert!(!is_gatekeeper_guarded("README.md", &stored(false)));
        // Only the final extension counts — `.node` as a directory
        // component must not match.
        assert!(!is_gatekeeper_guarded("node/index.js", &stored(false)));
    }
}
