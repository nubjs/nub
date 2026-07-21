use super::*;
use aube_lockfile::dep_path_filename::{
    DEFAULT_VIRTUAL_STORE_DIR_MAX_LENGTH, dep_path_to_filename,
};
use aube_lockfile::{DepType, DirectDep, LockedPackage, LockfileGraph};
use aube_store::Store;

fn setup_store_with_files(dir: &Path) -> (Store, BTreeMap<String, aube_store::PackageIndex>) {
    let store = Store::at(dir.join("store/files"));

    let mut indices = BTreeMap::new();

    // foo@1.0.0 with index.js
    let foo_stored = store
        .import_bytes(b"module.exports = 'foo';", false)
        .unwrap();
    let mut foo_index = PackageIndex::default();
    foo_index.insert("index.js".to_string(), foo_stored);

    // foo also has package.json
    let foo_pkg = store
        .import_bytes(b"{\"name\":\"foo\",\"version\":\"1.0.0\"}", false)
        .unwrap();
    foo_index.insert("package.json".to_string(), foo_pkg);
    indices.insert("foo@1.0.0".to_string(), foo_index);

    // bar@2.0.0 with index.js
    let bar_stored = store
        .import_bytes(b"module.exports = 'bar';", false)
        .unwrap();
    let mut bar_index = PackageIndex::default();
    bar_index.insert("index.js".to_string(), bar_stored);
    indices.insert("bar@2.0.0".to_string(), bar_index);

    (store, indices)
}

fn package_index(store: &Store, package_json: &str, index_js: &str) -> PackageIndex {
    let mut index = PackageIndex::default();
    let package_json = store.import_bytes(package_json.as_bytes(), false).unwrap();
    index.insert("package.json".to_string(), package_json);
    let index_js = store.import_bytes(index_js.as_bytes(), false).unwrap();
    index.insert("index.js".to_string(), index_js);
    index
}

fn make_graph() -> LockfileGraph {
    let mut packages = BTreeMap::new();

    let mut foo_deps = BTreeMap::new();
    foo_deps.insert("bar".to_string(), "2.0.0".to_string());

    packages.insert(
        "foo@1.0.0".to_string(),
        LockedPackage {
            name: "foo".to_string(),
            version: "1.0.0".to_string(),
            integrity: None,
            dependencies: foo_deps,
            dep_path: "foo@1.0.0".to_string(),
            ..Default::default()
        },
    );
    packages.insert(
        "bar@2.0.0".to_string(),
        LockedPackage {
            name: "bar".to_string(),
            version: "2.0.0".to_string(),
            integrity: None,
            dependencies: BTreeMap::new(),
            dep_path: "bar@2.0.0".to_string(),
            ..Default::default()
        },
    );

    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "foo".to_string(),
            dep_path: "foo@1.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: None,
        }],
    );

    LockfileGraph {
        importers,
        packages,
        ..Default::default()
    }
}

#[test]
fn test_detect_strategy() {
    let dir = tempfile::tempdir().unwrap();
    let strategy = Linker::detect_strategy(dir.path());
    // The probe resolves a same-FS success to the OS-specific `auto`
    // strategy and a cross-FS failure to `Copy`. On macOS the same-FS
    // strategy is `ReflinkAuto` (APFS clonefile, with the auto-only
    // hardlink-before-copy fallback), elsewhere `Hardlink`. Whether the
    // tempdir's FS supports the same-mount link depends on the runner, so
    // accept the same-FS strategy or `Copy`, but reject the *other* OS's
    // same-FS strategy — that would mean the cfg gate resolved on the
    // wrong target. The probe must never yield the plain `Reflink`
    // explicit selections use.
    match strategy {
        LinkStrategy::Copy => {}
        LinkStrategy::Reflink => {
            panic!("`auto` probe must resolve same-FS to ReflinkAuto, never plain Reflink")
        }
        #[cfg(target_os = "macos")]
        LinkStrategy::ReflinkAuto => {}
        #[cfg(target_os = "macos")]
        LinkStrategy::Hardlink => panic!("macOS `auto` must resolve same-FS to ReflinkAuto"),
        #[cfg(not(target_os = "macos"))]
        LinkStrategy::Hardlink => {}
        #[cfg(not(target_os = "macos"))]
        LinkStrategy::ReflinkAuto => panic!("non-macOS `auto` must resolve same-FS to Hardlink"),
    }
}

// Representative dep_paths exercising every branch of
// `dep_path_to_filename`: plain, scoped (`/` → `+`), peer-context
// (parens flatten to `_`), uppercase (forces the BLAKE3 short-hash),
// and a long peer graph (overflows max_length → truncate + hash).
const ENCODE_FIXTURES: &[&str] = &[
    "foo@1.0.0",
    "@scope/bar@2.0.0",
    "baz@3.0.0(react@18.2.0)",
    "@ng/Core@17.0.0",
    "@fig/eslint-config-autocomplete@2.0.0(@typescript-eslint+eslint-plugin@7.18.0(@typescript-eslint+parser@7.18.0(eslint@8.57.1))(eslint@8.57.1))(@typescript-eslint+parser@7.18.0(eslint@8.57.1))(@withfig+eslint-plugin-fig-linter@1.4.1)(eslint@8.57.1)(eslint-plugin-compat@4.2.0(eslint@8.57.1))(typescript@5.9.3)",
];

// The link step's serial pre-pass now computes each package's local
// `.aube/<entry>` name and its virtual-store subdir once and threads
// them into the par_iter (and on into `ensure_in_virtual_store`),
// replacing what used to be 2-3 redundant `dep_path_to_filename`
// encodes per package. The hoist is only output-preserving if the
// value carried forward is byte-identical to what each eliminated
// recompute site would have produced.
//
// The eliminated sites all bottomed out at the SAME primitive
// composition: the local `.aube/` link name was
// `dep_path_to_filename(dep_path, max)`, and the shared-store target
// (the par_iter's `global_entry` and the now-removed `subdir` encode
// inside the public `ensure_in_virtual_store`) was
// `dep_path_to_filename(hashes.hashed_dep_path(dep_path), max)`. So the
// non-vacuous check is to recompute each carried value INDEPENDENTLY
// from `dep_path_to_filename` (and `GraphHashes::hashed_dep_path`)
// directly — NOT by re-calling the very wrapper under test — and assert
// the linker's `aube_dir_entry_name` / `virtual_store_subdir` outputs
// equal it. A `dep_path_to_filename(dep_path, max)` is what the linker
// builds at the eliminated `aube_dir.join(self.aube_dir_entry_name(..))`
// and `self.virtual_store.join(self.virtual_store_subdir(..))` sites, so
// if the hoist ever carried a value that diverged from that primitive,
// these fail.

fn linker_for_encode_test(dir: &Path, hashes: Option<GraphHashes>) -> Linker {
    let store = Store::at(dir.join("store/files"));
    let mut linker = Linker::new(&store, LinkStrategy::Copy);
    if let Some(h) = hashes {
        linker = linker.with_graph_hashes(h);
    }
    linker
}

#[test]
fn precomputed_entry_name_and_subdir_match_recompute_unhashed() {
    let dir = tempfile::tempdir().unwrap();
    let linker = linker_for_encode_test(dir.path(), None);

    for dep_path in ENCODE_FIXTURES {
        // The values the pre-pass carries forward into the par_iter and
        // on into `ensure_in_virtual_store_with_subdir`.
        let precomputed_entry = linker.aube_dir_entry_name(dep_path);
        let precomputed_subdir = linker.virtual_store_subdir(dep_path);

        // Independent recompute of what the eliminated recompute sites
        // produced: both the `.aube/` link name and (in the unhashed
        // mode, where `hashed_dep_path` is the identity) the shared-store
        // subdir bottom out at the same bare-dep_path encode. Built from
        // the primitive directly, NOT by re-calling the linker wrapper,
        // so this catches the hoist carrying any value that diverged from
        // what the original `aube_dir.join(aube_dir_entry_name(..))` /
        // `virtual_store.join(virtual_store_subdir(..))` sites encoded.
        let recompute = dep_path_to_filename(dep_path, DEFAULT_VIRTUAL_STORE_DIR_MAX_LENGTH);
        assert_eq!(
            precomputed_entry, recompute,
            "entry name diverged from the bare-dep_path encode for {dep_path}"
        );
        assert_eq!(
            precomputed_subdir, recompute,
            "unhashed subdir diverged from the bare-dep_path encode for {dep_path}"
        );

        // Cross-check: with no hash fold the two carried values must
        // coincide, so a hoist that swapped entry for subdir (or vice
        // versa) in the unhashed path would still produce correct output
        // here — the hashed test below is what pins their separation.
        assert_eq!(
            precomputed_entry, precomputed_subdir,
            "entry name and subdir must coincide in the unhashed mode for {dep_path}"
        );
    }
}

#[test]
fn precomputed_subdir_matches_recompute_with_graph_hashes() {
    // With graph hashes installed, `virtual_store_subdir` folds the
    // per-dep_path hash into the leaf before encoding, so it diverges
    // from `aube_dir_entry_name` (which never applies the hash). The
    // hoist carries the *subdir* (hashed) into `ensure_in_virtual_store`
    // and the *entry name* (unhashed) into the local `.aube/` link —
    // mixing them up would be a behavior change. Pin that each carried
    // value matches its independent primitive recompute and that the two
    // are distinct under hashing.
    let dir = tempfile::tempdir().unwrap();
    let mut node_hash = std::collections::BTreeMap::new();
    for (i, dep_path) in ENCODE_FIXTURES.iter().enumerate() {
        // A deterministic non-empty hex hash per dep_path.
        node_hash.insert(
            (*dep_path).to_string(),
            format!("{:016x}{:016x}", 0x0123_4567_89ab_cdefu64, i as u64),
        );
    }
    let hashes = GraphHashes { node_hash };
    let linker = linker_for_encode_test(dir.path(), Some(hashes.clone()));

    for dep_path in ENCODE_FIXTURES {
        let entry = linker.aube_dir_entry_name(dep_path);
        let subdir = linker.virtual_store_subdir(dep_path);

        // Independent recompute of the eliminated sites: the `.aube/`
        // link name is the bare-dep_path encode, while the shared-store
        // subdir is the encode of the hash-folded dep_path. Both built
        // from the primitives directly (`dep_path_to_filename` and
        // `hashed_dep_path`), NOT by re-calling the wrappers under test.
        let entry_recompute = dep_path_to_filename(dep_path, DEFAULT_VIRTUAL_STORE_DIR_MAX_LENGTH);
        let subdir_recompute = dep_path_to_filename(
            &hashes.hashed_dep_path(dep_path),
            DEFAULT_VIRTUAL_STORE_DIR_MAX_LENGTH,
        );
        assert_eq!(
            entry, entry_recompute,
            "entry name diverged from the bare-dep_path encode for {dep_path}"
        );
        assert_eq!(
            subdir, subdir_recompute,
            "hashed subdir diverged from the hash-folded encode for {dep_path}"
        );

        // And the hash genuinely makes the subdir differ from the entry
        // name, so the two values are not interchangeable — proving the
        // hoist must carry them separately. For short names the subdir
        // gains a visible `-<hex>` leaf suffix the entry lacks; for the
        // long peer-graph fixture the suffix is truncated off, but the
        // entry and subdir still differ because the BLAKE3 short-hash is
        // computed over different inputs (the hash-folded path vs. the
        // bare one), so the trailing hash diverges either way.
        assert_ne!(
            entry, subdir,
            "graph hash should make subdir differ from entry name for {dep_path}"
        );
    }
}

#[test]
fn test_link_all_handles_self_referential_dep_at_different_version() {
    // `react_ujs@3.3.0` (and other publish-script artifacts)
    // declares its own name as a dep at a *different* version
    // (`react_ujs: ^2.7.1`). The transitive-symlink pass would
    // try to create a symlink at `node_modules/react_ujs`,
    // which is exactly where the package's own files live —
    // EEXIST. Skip self-name deps regardless of version so
    // these install cleanly. `require('<self>')` from inside
    // the package then resolves to its own files, matching how
    // npm / pnpm / yarn end up after their hoisting passes.
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();

    let store = Store::at(dir.path().join("store/files"));

    let mut indices = BTreeMap::new();
    let host_index_js = store.import_bytes(b"/* react_ujs 3.3.0 */", false).unwrap();
    let host_pkg_json = store
        .import_bytes(b"{\"name\":\"react_ujs\",\"version\":\"3.3.0\"}", false)
        .unwrap();
    let mut host_index = PackageIndex::default();
    host_index.insert("index.js".to_string(), host_index_js);
    host_index.insert("package.json".to_string(), host_pkg_json);
    indices.insert("react_ujs@3.3.0".to_string(), host_index);

    let mut host_deps = BTreeMap::new();
    // Self-reference at a different version, the shape that
    // triggered the EEXIST bug.
    host_deps.insert("react_ujs".to_string(), "^2.7.1".to_string());

    let mut packages = BTreeMap::new();
    packages.insert(
        "react_ujs@3.3.0".to_string(),
        LockedPackage {
            name: "react_ujs".to_string(),
            version: "3.3.0".to_string(),
            integrity: None,
            dependencies: host_deps,
            dep_path: "react_ujs@3.3.0".to_string(),
            ..Default::default()
        },
    );

    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "react_ujs".to_string(),
            dep_path: "react_ujs@3.3.0".to_string(),
            dep_type: DepType::Production,
            specifier: None,
        }],
    );

    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };

    let linker = Linker::new_with_gvs(&store, LinkStrategy::Copy, true);
    let stats = linker
        .link_all(&project_dir, &graph, &indices)
        .expect("install must succeed despite self-named dep");
    assert_eq!(stats.packages_linked, 1);
    let host_index =
        project_dir.join("node_modules/.aube/react_ujs@3.3.0/node_modules/react_ujs/index.js");
    assert!(host_index.exists(), "host package files must be present");
}

#[test]
fn test_ensure_in_aube_dir_handles_concurrent_same_dep_path() {
    const THREADS: usize = 16;

    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("project");
    let aube_dir = project_dir.join("node_modules/.aube");
    std::fs::create_dir_all(&aube_dir).unwrap();

    let store = Store::at(dir.path().join("store/files"));
    let mut index = PackageIndex::default();
    let pkg_json = store
        .import_bytes(b"{\"name\":\"debug\",\"version\":\"4.4.0\"}", false)
        .unwrap();
    index.insert("package.json".to_string(), pkg_json);
    for i in 0..128 {
        let stored = store
            .import_bytes(format!("module.exports = {i};").as_bytes(), false)
            .unwrap();
        index.insert(format!("files/{i}.js"), stored);
    }

    let pkg = LockedPackage {
        name: "debug".to_string(),
        version: "4.4.0".to_string(),
        integrity: None,
        dependencies: BTreeMap::new(),
        dep_path: "debug@4.4.0".to_string(),
        ..Default::default()
    };

    let linker = std::sync::Arc::new(Linker::new_with_gvs(&store, LinkStrategy::Copy, false));
    let index = std::sync::Arc::new(index);
    let pkg = std::sync::Arc::new(pkg);
    let aube_dir = std::sync::Arc::new(aube_dir);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(THREADS));

    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let linker = linker.clone();
            let index = index.clone();
            let pkg = pkg.clone();
            let aube_dir = aube_dir.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let mut stats = LinkStats::default();
                linker
                    .ensure_in_aube_dir(
                        &aube_dir,
                        "debug@4.4.0",
                        &aube_lockfile::LockfileGraph::default(),
                        &pkg,
                        &index,
                        &mut stats,
                        None,
                    )
                    .expect("duplicate materialization should be idempotent");
                stats
            })
        })
        .collect();

    let stats = handles.into_iter().map(|h| h.join().unwrap()).fold(
        LinkStats::default(),
        |mut total, stats| {
            total.packages_linked += stats.packages_linked;
            total.packages_cached += stats.packages_cached;
            total.files_linked += stats.files_linked;
            total
        },
    );

    assert_eq!(stats.packages_linked, 1);
    assert_eq!(stats.packages_cached, THREADS - 1);
    assert_eq!(stats.files_linked, index.len());
    assert!(
        aube_dir
            .join("debug@4.4.0/node_modules/debug/files/127.js")
            .exists(),
        "winning materialization must be usable"
    );
    assert!(
        std::fs::read_dir(aube_dir.as_ref())
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".tmp-")),
        "losing staging directories must be cleaned up"
    );
}

#[test]
fn test_link_all_creates_pnpm_virtual_store() {
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();

    let (store, indices) = setup_store_with_files(dir.path());
    let linker = Linker::new_with_gvs(&store, LinkStrategy::Copy, true).with_hoist(false);
    let graph = make_graph();

    let stats = linker.link_all(&project_dir, &graph, &indices).unwrap();

    // .aube virtual store should exist
    assert!(project_dir.join("node_modules/.aube").exists());

    // With hidden hoist disabled, .aube/foo@1.0.0 is a symlink to the global virtual store.
    let aube_foo = project_dir.join("node_modules/.aube/foo@1.0.0");
    assert!(aube_foo.symlink_metadata().unwrap().is_symlink());

    // foo@1.0.0 content should be accessible through the symlink
    let foo_in_pnpm = project_dir.join("node_modules/.aube/foo@1.0.0/node_modules/foo/index.js");
    assert!(foo_in_pnpm.exists());
    assert_eq!(
        std::fs::read_to_string(&foo_in_pnpm).unwrap(),
        "module.exports = 'foo';"
    );

    // bar@2.0.0 should also be accessible
    let bar_in_pnpm = project_dir.join("node_modules/.aube/bar@2.0.0/node_modules/bar/index.js");
    assert!(bar_in_pnpm.exists());

    assert_eq!(stats.packages_linked, 2);
    assert!(stats.files_linked >= 3); // foo has 2 files, bar has 1
}

#[test]
fn disk_materialize_makes_only_listed_package_a_real_dir_under_gvs() {
    // Under the global virtual store every `.aube/<dep>` is normally a symlink
    // into the shared store, so the package realpath escapes the project. A
    // package on the disk-materialize list must instead be a real
    // project-local directory (its realpath back inside the project) so Node's
    // upward walk from inside it reaches a consumer-installed, undeclared
    // backend at the project root — while every OTHER package stays a symlink.
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();

    let (store, indices) = setup_store_with_files(dir.path());
    let linker = Linker::new_with_gvs(&store, LinkStrategy::Copy, true)
        .with_hoist(false)
        .with_disk_materialize(&["foo".to_string()]);
    let graph = make_graph();

    linker.link_all(&project_dir, &graph, &indices).unwrap();

    // foo is disk-materialized: a real directory, not a shared-store symlink,
    // with its files materialized project-local.
    let aube_foo = project_dir.join("node_modules/.aube/foo@1.0.0");
    let foo_is_symlink = aube_foo
        .symlink_metadata()
        .unwrap()
        .file_type()
        .is_symlink();
    assert!(
        !foo_is_symlink,
        "disk-materialized foo must be a real dir, not a global-store symlink"
    );
    assert_eq!(
        std::fs::read_to_string(aube_foo.join("node_modules/foo/index.js")).unwrap(),
        "module.exports = 'foo';"
    );
    // foo's declared dep still resolves through the sibling symlink inside it.
    assert!(
        aube_foo
            .join("node_modules/bar")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink(),
        "foo's transitive dep must remain a sibling symlink"
    );

    // bar is unlisted: it stays a global-virtual-store symlink.
    assert!(
        project_dir
            .join("node_modules/.aube/bar@2.0.0")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink(),
        "an unlisted package must stay a global-virtual-store symlink"
    );

    // The top-level entry resolves foo's content regardless.
    assert_eq!(
        std::fs::read_to_string(project_dir.join("node_modules/foo/index.js")).unwrap(),
        "module.exports = 'foo';"
    );
}

#[test]
fn disk_materialize_keeps_store_copy_so_store_resident_dependents_dont_orphan() {
    // Orphan-safety regression (the shipped @storybook/builder-webpack5 ←
    // @storybook/react-webpack5 class). The graph is root → foo → bar; here bar
    // is disk-materialized while foo stays a store symlink. foo reaches bar
    // through its store entry's sibling symlink, which targets
    // `<store>/<subdir(bar)>/node_modules/bar`. If the disk-materialize branch
    // only wrote bar's project-local real dir and skipped bar's store copy, that
    // sibling would dangle — the exact orphan the builder-webpack5 case hit. The
    // fix keeps bar's store copy at its subdir IN ADDITION to the project-local
    // dir; assert BOTH the store copy and the dependent's resolved sibling.
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();

    let (store, indices) = setup_store_with_files(dir.path());
    let linker = Linker::new_with_gvs(&store, LinkStrategy::Copy, true)
        .with_hoist(false)
        .with_disk_materialize(&["bar".to_string()]);
    let graph = make_graph(); // root -> foo -> bar

    linker.link_all(&project_dir, &graph, &indices).unwrap();

    // bar is disk-materialized: a real project-local dir.
    let aube_bar = project_dir.join("node_modules/.aube/bar@2.0.0");
    assert!(
        !aube_bar
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink(),
        "disk-materialized bar must be a real project-local dir"
    );

    // The orphan-safety invariant: bar's store copy still exists at exactly the
    // subdir a store-resident dependent's sibling symlink points at.
    let bar_store_copy = store
        .virtual_store_dir()
        .join(linker.virtual_store_subdir("bar@2.0.0"))
        .join("node_modules/bar/index.js");
    assert_eq!(
        std::fs::read_to_string(&bar_store_copy).unwrap(),
        "module.exports = 'bar';",
        "disk-materialized bar's store copy must remain at its subdir so a \
         store-resident dependent's sibling symlink still resolves"
    );

    // End-to-end: foo (store-resident) resolves bar through its sibling symlink.
    // `.aube/foo@1.0.0` is a store symlink; its `node_modules/bar` sibling must
    // resolve all the way to bar's content — not dangle.
    let foo_sibling_bar =
        project_dir.join("node_modules/.aube/foo@1.0.0/node_modules/bar/index.js");
    assert_eq!(
        std::fs::read_to_string(&foo_sibling_bar).unwrap(),
        "module.exports = 'bar';",
        "store-resident foo's sibling to disk-materialized bar must resolve (no orphan)"
    );
}

/// Graph where the root declares `foo` and `bar` as siblings and `foo` declares
/// NOTHING — so `bar` is a pure phantom target of `foo` (foo imports it without
/// declaring it). Reuses the foo@1.0.0 / bar@2.0.0 store indices from
/// `setup_store_with_files`.
fn make_graph_phantom() -> LockfileGraph {
    let mut packages = BTreeMap::new();
    packages.insert(
        "foo@1.0.0".to_string(),
        LockedPackage {
            name: "foo".to_string(),
            version: "1.0.0".to_string(),
            dependencies: BTreeMap::new(),
            dep_path: "foo@1.0.0".to_string(),
            ..Default::default()
        },
    );
    packages.insert(
        "bar@2.0.0".to_string(),
        LockedPackage {
            name: "bar".to_string(),
            version: "2.0.0".to_string(),
            dependencies: BTreeMap::new(),
            dep_path: "bar@2.0.0".to_string(),
            ..Default::default()
        },
    );
    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![
            DirectDep {
                name: "foo".to_string(),
                dep_path: "foo@1.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: None,
            },
            DirectDep {
                name: "bar".to_string(),
                dep_path: "bar@2.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: None,
            },
        ],
    );
    LockfileGraph {
        importers,
        packages,
        ..Default::default()
    }
}

#[test]
fn collective_hidden_hoist_over_ejected_set_resolves_undeclared_phantom_under_gvs() {
    // The collective project-local hidden hoist tree. Under GVS an ejected
    // (disk-materialized) package is a real project-local dir, so Node's upward
    // node_modules walk from inside it passes through `.aube/node_modules/`.
    // Building that tree over the graph lets the ejected `foo` resolve its
    // UNDECLARED phantom `bar` (a sibling at the project root here) via the
    // walk-up — the detection-free, blanket successor to per-importer hoisting.
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();

    let (store, indices) = setup_store_with_files(dir.path());
    let linker = Linker::new_with_gvs(&store, LinkStrategy::Copy, true)
        .with_hoist(false)
        .with_disk_materialize(&["foo".to_string()]);

    linker
        .link_all(&project_dir, &make_graph_phantom(), &indices)
        .unwrap();

    // foo is ejected: a real project-local dir (its realpath is in the project,
    // so its walk-up reaches `.aube/node_modules/`).
    let aube_foo = project_dir.join("node_modules/.aube/foo@1.0.0");
    assert!(
        !aube_foo
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink(),
        "disk-materialized foo must be a real project-local dir"
    );

    // The collective hidden tree exists and carries the phantom target `bar`,
    // resolving all the way to bar's content — so foo's undeclared `require('bar')`
    // succeeds via the walk-up. foo lists itself in the blanket tree too.
    let hidden = project_dir.join("node_modules/.aube/node_modules");
    assert!(
        hidden.join("bar").symlink_metadata().unwrap().is_symlink(),
        "collective tree must carry the phantom target bar"
    );
    assert_eq!(
        std::fs::read_to_string(hidden.join("bar/index.js")).unwrap(),
        "module.exports = 'bar';",
        "the phantom target resolves to bar's real content through the tree"
    );
    assert!(
        hidden.join("foo").symlink_metadata().unwrap().is_symlink(),
        "the blanket collective tree lists every non-local package, incl. foo"
    );

    // #6 safety: the tree is project-local ONLY — the shared store carries no
    // hidden tree (its bare-name aliases would be cross-project-mutable state).
    assert!(
        store
            .virtual_store_dir()
            .join("node_modules")
            .symlink_metadata()
            .is_err(),
        "no hidden hoist tree may live in the shared global store (#6)"
    );

    // Isolation: bar stays a shared-store symlink (its realpath escapes the
    // project), so bar's own walk-up ascends the store and never reaches this
    // project-local tree — only the ejected foo consumes it.
    assert!(
        project_dir
            .join("node_modules/.aube/bar@2.0.0")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink(),
        "the symlinked majority (bar) stays a shared-store symlink, off this walk-up"
    );
}

#[test]
fn no_collective_hidden_hoist_when_ejected_set_empty_under_gvs() {
    // The collective tree is gated on a non-empty disk-materialize (ejected)
    // set. With none, plain GVS has no project-local-realpath consumer, so no
    // tree is built — the pass stays byte-for-byte the swept-empty behavior
    // (and standalone aube, whose disk-materialize is always empty, is
    // unaffected).
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();

    let (store, indices) = setup_store_with_files(dir.path());
    let linker = Linker::new_with_gvs(&store, LinkStrategy::Copy, true).with_hoist(false);

    linker
        .link_all(&project_dir, &make_graph(), &indices)
        .unwrap();

    assert!(
        project_dir
            .join("node_modules/.aube/node_modules")
            .symlink_metadata()
            .is_err(),
        "no collective tree without an ejected set"
    );
}

#[test]
fn test_link_file_fresh_reports_missing_cas_shard_and_invalidates_cache() {
    // Reproduces jdx/aube#393: a partially corrupt CAS leaves the
    // cached package index pointing at a missing shard. Materialize
    // must distinguish "source CAS file missing" from a generic ENOENT
    // and drop the stale index JSON so the next install re-imports
    // the tarball.
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();

    let (store, indices) = setup_store_with_files(dir.path());
    // Persist foo's index so invalidate_cached_index has something
    // to remove. Real installs save indices via the fetch path.
    let foo_index = indices.get("foo@1.0.0").unwrap();
    store.save_index("foo", "1.0.0", None, foo_index).unwrap();
    let cached_path = store.index_dir().join("foo@1.0.0.json");
    assert!(
        cached_path.exists(),
        "test setup: index cache must be written"
    );

    // Delete the CAS shard for foo's package.json (matches the
    // failure mode in #393 where one shard is missing while others
    // remain).
    let pkgjson_store_path = foo_index.get("package.json").unwrap().store_path.clone();
    std::fs::remove_file(&pkgjson_store_path).unwrap();

    let linker = Linker::new_with_gvs(&store, LinkStrategy::Copy, true);
    let graph = make_graph();
    let err = linker
        .link_all(&project_dir, &graph, &indices)
        .expect_err("link must fail when a referenced CAS shard is gone");
    assert!(
        matches!(&err, Error::MissingStoreFile { rel_path, .. } if rel_path == "package.json"),
        "expected MissingStoreFile {{ rel_path: \"package.json\" }}, got {err:?}"
    );

    // Side effect: cached index dropped, so the next install will
    // miss load_index and re-fetch instead of looping on the same
    // dead shard reference.
    assert!(
        !cached_path.exists(),
        "stale index cache must be invalidated on MissingStoreFile"
    );
}

#[test]
#[cfg(unix)]
fn test_link_file_fresh_hardlink_short_circuits_when_source_missing() {
    // Hardlink path used to silently fall through to `std::fs::copy`
    // on ENOENT and emit a misleading "hardlink failed, falling back
    // to copy" trace, even though the real cause was the source
    // shard going missing. Short-circuit returns MissingStoreFile
    // directly so traces stay accurate.
    let dir = tempfile::tempdir().unwrap();
    let store = Store::at(dir.path().join("store/files"));
    let stored = store.import_bytes(b"hello", false).unwrap();
    // Capture the path before we move `stored` into link_file_fresh.
    let store_path = stored.store_path.clone();
    std::fs::remove_file(&store_path).unwrap();

    let dst_dir = dir.path().join("dst");
    std::fs::create_dir_all(&dst_dir).unwrap();
    let dst = dst_dir.join("hello.txt");

    let linker = Linker::new_with_gvs(&store, LinkStrategy::Hardlink, true);
    let err = linker
        .link_file_fresh(&stored, "hello.txt", &dst)
        .expect_err("source missing must fail");
    assert!(
        matches!(
            &err,
            Error::MissingStoreFile { store_path: p, rel_path } if p == &store_path && rel_path == "hello.txt"
        ),
        "expected MissingStoreFile from Hardlink branch, got {err:?}"
    );
}

/// RAII guard that serializes `FORCE_REFLINK_FAILURE` across the parallel
/// test runner and always restores the flag — even if the body panics.
///
/// `FORCE_REFLINK_FAILURE` is a process-wide `AtomicBool`, so two reflink
/// fallback tests running concurrently could otherwise observe each other's
/// `store(false)` mid-`link_file_fresh` (a reflink-capable FS would then take
/// the real clonefile path and skew the inode assertion), and a bare manual
/// reset would leak the `true` state to the next test on a panic. Holding the
/// `Mutex` for the whole forced-failure window mutually excludes the tests; the
/// `Drop` impl makes the reset panic-safe.
#[cfg(unix)]
struct ForcedReflinkFailure {
    _guard: std::sync::MutexGuard<'static, ()>,
}

#[cfg(unix)]
impl ForcedReflinkFailure {
    fn engage() -> Self {
        use std::sync::atomic::Ordering;
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        // Recover from a poisoned lock: a prior test panicking inside the guard
        // is exactly the case this exists for, and the `Drop` reset still ran.
        let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::materialize::FORCE_REFLINK_FAILURE.store(true, Ordering::Relaxed);
        Self { _guard: guard }
    }
}

#[cfg(unix)]
impl Drop for ForcedReflinkFailure {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        crate::materialize::FORCE_REFLINK_FAILURE.store(false, Ordering::Relaxed);
    }
}

/// Materialize one file under `strategy` with the reflink attempt forced
/// to fail, then report whether the result is a hardlink (same inode as
/// the source) or a copy (distinct inode). Isolates the clonefile-failure
/// fallback so the split between `ReflinkAuto` and explicit `Reflink` is
/// observable on any filesystem, including reflink-capable APFS/btrfs CI.
#[cfg(unix)]
fn realized_inode_matches_source_on_reflink_failure(strategy: LinkStrategy) -> bool {
    use std::os::unix::fs::MetadataExt;

    let dir = tempfile::tempdir().unwrap();
    let store = Store::at(dir.path().join("store/files"));
    // >16 KiB so the macOS small-file copy shortcut does not pre-empt the
    // reflink path under test.
    let content = vec![b'x'; 32 * 1024];
    let stored = store.import_bytes(&content, false).unwrap();
    let store_path = stored.store_path.clone();

    let dst_dir = dir.path().join("dst");
    std::fs::create_dir_all(&dst_dir).unwrap();
    let dst = dst_dir.join("payload.bin");

    let linker = Linker::new_with_gvs(&store, strategy, true);
    let result = {
        // Hold the guard across the materialize so a sibling test can't flip
        // the global flag mid-call; the flag is restored on drop, panic-safe.
        let _forced = ForcedReflinkFailure::engage();
        linker.link_file_fresh(&stored, "payload.bin", &dst)
    };
    result.expect("a reflink strategy must still materialize the file via its fallback");

    // Data integrity holds whichever fallback fired.
    assert_eq!(std::fs::read(&dst).unwrap(), content);

    std::fs::metadata(&store_path).unwrap().ino() == std::fs::metadata(&dst).unwrap().ino()
}

#[test]
#[cfg(unix)]
fn test_reflink_auto_falls_back_to_hardlink_not_copy() {
    // `auto` on a same-FS macOS target resolves to `ReflinkAuto`; the
    // probe already proved the target shares a mount, so a clonefile
    // failure (non-APFS same-FS volume, e.g. HFS+) must degrade to a
    // zero-cost hardlink before a per-file copy.
    assert!(
        realized_inode_matches_source_on_reflink_failure(LinkStrategy::ReflinkAuto),
        "ReflinkAuto must fall back to a hardlink (same inode), not a copy, on reflink failure"
    );
}

#[test]
#[cfg(unix)]
fn test_explicit_reflink_falls_back_to_copy_not_hardlink() {
    // Explicit `clone` / `clone-or-copy` map to `Reflink`, whose
    // documented contract is reflink with a plain *copy* fallback. They
    // must NOT take the auto-only hardlink step on a clonefile failure —
    // the result is a distinct inode (copy), never the source's inode.
    assert!(
        !realized_inode_matches_source_on_reflink_failure(LinkStrategy::Reflink),
        "explicit Reflink must fall back to a copy (distinct inode), not a hardlink"
    );
}

#[test]
fn test_link_all_creates_top_level_entries() {
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();

    let (store, indices) = setup_store_with_files(dir.path());
    let linker = Linker::new(&store, LinkStrategy::Copy);
    let graph = make_graph();

    let stats = linker.link_all(&project_dir, &graph, &indices).unwrap();

    // Top-level foo/ should exist (it's a direct dep)
    let foo_top = project_dir.join("node_modules/foo/index.js");
    assert!(foo_top.exists());
    assert_eq!(
        std::fs::read_to_string(&foo_top).unwrap(),
        "module.exports = 'foo';"
    );

    // bar should NOT be top-level (it's only a transitive dep)
    let bar_top = project_dir.join("node_modules/bar/index.js");
    assert!(!bar_top.exists());

    assert_eq!(stats.top_level_linked, 1);
}

#[test]
fn test_link_all_transitive_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();

    let (store, indices) = setup_store_with_files(dir.path());
    let linker = Linker::new(&store, LinkStrategy::Copy);
    let graph = make_graph();

    linker.link_all(&project_dir, &graph, &indices).unwrap();

    // foo's node_modules/bar should be a symlink (inside the global virtual store)
    // The path resolves through the .aube symlink into the global store
    let bar_symlink = project_dir.join("node_modules/.aube/foo@1.0.0/node_modules/bar");
    assert!(bar_symlink.symlink_metadata().unwrap().is_symlink());
}

#[test]
fn test_link_all_cleans_existing_node_modules() {
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("project");
    let nm = project_dir.join("node_modules");
    std::fs::create_dir_all(&nm).unwrap();
    std::fs::write(nm.join("stale-file.txt"), "old").unwrap();

    let (store, indices) = setup_store_with_files(dir.path());
    let linker = Linker::new(&store, LinkStrategy::Copy);
    let graph = make_graph();

    linker.link_all(&project_dir, &graph, &indices).unwrap();

    // Old file should be gone
    assert!(!nm.join("stale-file.txt").exists());
    // New structure should exist
    assert!(nm.join(".aube").exists());
}

#[test]
fn test_link_all_nested_node_modules_for_direct_deps() {
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();

    let (store, indices) = setup_store_with_files(dir.path());
    let linker = Linker::new(&store, LinkStrategy::Copy);
    let graph = make_graph();

    linker.link_all(&project_dir, &graph, &indices).unwrap();

    // foo is a direct dep with bar as a transitive dep.
    // The top-level node_modules/foo is a symlink to .aube/foo@1.0.0/node_modules/foo,
    // and bar lives as a sibling at .aube/foo@1.0.0/node_modules/bar (also a symlink
    // pointing to .aube/bar@2.0.0/node_modules/bar). Node's directory walk from inside
    // foo finds bar this way without aube creating any nested node_modules.
    let foo_link = project_dir.join("node_modules/foo");
    assert!(foo_link.symlink_metadata().unwrap().is_symlink());
    let bar_sibling = project_dir.join("node_modules/.aube/foo@1.0.0/node_modules/bar");
    assert!(bar_sibling.symlink_metadata().unwrap().is_symlink());
}

#[test]
fn test_hoisted_duplicates_same_dep_path_at_multiple_sites() {
    // A version conflict at the root forces `shared@1.0.0` to nest under
    // BOTH `a` and `b` — two placements of the SAME dep_path at the SAME
    // depth. The hoisted materializer fills a depth level in parallel, so
    // this drives two concurrent materializations of one dep_path; on
    // macOS+APFS that means two concurrent `build_tree` calls for one
    // shared `trees/` entry. Asserts every copy lands complete (the
    // regression guard for the build_tree staging-dir collision: a shared
    // staging dir published a partial tree, dropping files here).
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();

    let store = Store::at(dir.path().join("store/files"));
    let mut indices = BTreeMap::new();

    // shared@1.0.0 carries several files so a partial tree is detectable.
    let mut shared_v1 = PackageIndex::default();
    for (path, body) in [
        (
            "package.json",
            "{\"name\":\"shared\",\"version\":\"1.0.0\"}",
        ),
        ("index.js", "module.exports = 'shared@1';"),
        ("lib/util.js", "module.exports = 42;"),
    ] {
        shared_v1.insert(
            path.to_string(),
            store.import_bytes(body.as_bytes(), false).unwrap(),
        );
    }
    indices.insert("shared@1.0.0".to_string(), shared_v1);
    indices.insert(
        "shared@2.0.0".to_string(),
        package_index(
            &store,
            "{\"name\":\"shared\",\"version\":\"2.0.0\"}",
            "module.exports = 'shared@2';",
        ),
    );
    indices.insert(
        "a@1.0.0".to_string(),
        package_index(
            &store,
            "{\"name\":\"a\",\"version\":\"1.0.0\"}",
            "module.exports = 'a';",
        ),
    );
    indices.insert(
        "b@1.0.0".to_string(),
        package_index(
            &store,
            "{\"name\":\"b\",\"version\":\"1.0.0\"}",
            "module.exports = 'b';",
        ),
    );

    let pkg = |name: &str, version: &str, deps: &[(&str, &str)]| LockedPackage {
        name: name.to_string(),
        version: version.to_string(),
        dep_path: format!("{name}@{version}"),
        dependencies: deps
            .iter()
            .map(|(n, v)| (n.to_string(), v.to_string()))
            .collect(),
        ..Default::default()
    };
    let mut packages = BTreeMap::new();
    packages.insert("shared@2.0.0".to_string(), pkg("shared", "2.0.0", &[]));
    packages.insert("shared@1.0.0".to_string(), pkg("shared", "1.0.0", &[]));
    packages.insert(
        "a@1.0.0".to_string(),
        pkg("a", "1.0.0", &[("shared", "1.0.0")]),
    );
    packages.insert(
        "b@1.0.0".to_string(),
        pkg("b", "1.0.0", &[("shared", "1.0.0")]),
    );

    let dep = |name: &str, dep_path: &str| DirectDep {
        name: name.to_string(),
        dep_path: dep_path.to_string(),
        dep_type: DepType::Production,
        specifier: None,
    };
    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![
            dep("shared", "shared@2.0.0"),
            dep("a", "a@1.0.0"),
            dep("b", "b@1.0.0"),
        ],
    );
    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };

    let linker = Linker::new(&store, LinkStrategy::Copy).with_node_linker(NodeLinker::Hoisted);
    linker.link_all(&project_dir, &graph, &indices).unwrap();

    let nm = project_dir.join("node_modules");
    // shared@2.0.0 at the root; shared@1.0.0 nested under both requesters.
    assert_eq!(
        std::fs::read_to_string(nm.join("shared/index.js")).unwrap(),
        "module.exports = 'shared@2';"
    );
    for parent in ["a", "b"] {
        let nested = nm.join(parent).join("node_modules/shared");
        assert_eq!(
            std::fs::read_to_string(nested.join("index.js")).unwrap(),
            "module.exports = 'shared@1';",
            "{parent}'s nested shared@1 index.js"
        );
        // The multi-file payload must be complete at every site — a
        // raced partial tree would drop lib/util.js.
        assert_eq!(
            std::fs::read_to_string(nested.join("lib/util.js")).unwrap(),
            "module.exports = 42;",
            "{parent}'s nested shared@1 lib/util.js"
        );
        assert!(
            nested.join("package.json").exists(),
            "{parent}'s nested shared@1 package.json"
        );
    }
}

#[test]
fn test_hoisted_workspace_shares_dep_at_root_and_nests_conflict() {
    // Issue #484: under `nodeLinker=hoisted` a workspace must plan ONE
    // shared tree, not a full closure per member. Two members share
    // `shared@1.0.0` (materializes ONCE at the workspace root, never under a
    // member) while `left` conflicts (root+a want 1.0.0 → root; b wants
    // 2.0.0 → nests under b). The pre-fix per-importer planner duplicated
    // the shared dep under every member — two module identities → the
    // reporter's invalid React hooks.
    let dir = tempfile::tempdir().unwrap();
    let root_dir = dir.path().join("ws");
    for member in ["packages/a", "packages/b"] {
        std::fs::create_dir_all(root_dir.join(member)).unwrap();
    }

    let store = Store::at(dir.path().join("store/files"));
    let mut indices = BTreeMap::new();
    for (dp, pj, body) in [
        (
            "shared@1.0.0",
            "{\"name\":\"shared\",\"version\":\"1.0.0\"}",
            "module.exports='shared@1';",
        ),
        (
            "left@1.0.0",
            "{\"name\":\"left\",\"version\":\"1.0.0\"}",
            "module.exports='left@1';",
        ),
        (
            "left@2.0.0",
            "{\"name\":\"left\",\"version\":\"2.0.0\"}",
            "module.exports='left@2';",
        ),
    ] {
        indices.insert(dp.to_string(), package_index(&store, pj, body));
    }

    let pkg = |name: &str, version: &str| LockedPackage {
        name: name.to_string(),
        version: version.to_string(),
        dep_path: format!("{name}@{version}"),
        ..Default::default()
    };
    let mut packages = BTreeMap::new();
    for dp in ["shared@1.0.0", "left@1.0.0", "left@2.0.0"] {
        let (name, version) = dp.split_once('@').unwrap();
        packages.insert(dp.to_string(), pkg(name, version));
    }

    let dep = |name: &str, dep_path: &str| DirectDep {
        name: name.to_string(),
        dep_path: dep_path.to_string(),
        dep_type: DepType::Production,
        specifier: None,
    };
    let mut importers = BTreeMap::new();
    importers.insert(".".to_string(), vec![dep("left", "left@1.0.0")]);
    importers.insert(
        "packages/a".to_string(),
        vec![dep("shared", "shared@1.0.0"), dep("left", "left@1.0.0")],
    );
    importers.insert(
        "packages/b".to_string(),
        vec![dep("shared", "shared@1.0.0"), dep("left", "left@2.0.0")],
    );
    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };

    let linker = Linker::new(&store, LinkStrategy::Copy).with_node_linker(NodeLinker::Hoisted);
    linker
        .link_workspace(&root_dir, &graph, &indices, &BTreeMap::new())
        .unwrap();

    let root_nm = root_dir.join("node_modules");
    let a_nm = root_dir.join("packages/a/node_modules");
    let b_nm = root_dir.join("packages/b/node_modules");
    // The shared single-version dep is hoisted once, to the workspace root.
    assert_eq!(
        std::fs::read_to_string(root_nm.join("shared/index.js")).unwrap(),
        "module.exports='shared@1';"
    );
    assert!(
        !a_nm.join("shared").exists() && !b_nm.join("shared").exists(),
        "the shared dep must NOT be duplicated under any member"
    );
    // The conflict: majority left@1 at root, minority left@2 nested under b.
    assert_eq!(
        std::fs::read_to_string(root_nm.join("left/index.js")).unwrap(),
        "module.exports='left@1';"
    );
    assert_eq!(
        std::fs::read_to_string(b_nm.join("left/index.js")).unwrap(),
        "module.exports='left@2';"
    );
    assert!(
        !a_nm.join("left").exists(),
        "member a's left hoisted to root — no member-local copy"
    );
}

#[test]
fn test_global_virtual_store_is_populated() {
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();

    let (store, indices) = setup_store_with_files(dir.path());
    let virtual_store = store.virtual_store_dir();
    let linker = Linker::new_with_gvs(&store, LinkStrategy::Copy, true).with_hoist(false);
    let graph = make_graph();

    linker.link_all(&project_dir, &graph, &indices).unwrap();

    // Global virtual store should contain materialized packages
    let foo_global = virtual_store.join("foo@1.0.0/node_modules/foo/index.js");
    assert!(foo_global.exists());
    assert_eq!(
        std::fs::read_to_string(&foo_global).unwrap(),
        "module.exports = 'foo';"
    );

    let bar_global = virtual_store.join("bar@2.0.0/node_modules/bar/index.js");
    assert!(bar_global.exists());
}

#[test]
fn test_hidden_hoist_prefers_root_direct_dep_over_transitive_version() {
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();

    let store = Store::at(dir.path().join("store/files"));
    let mut indices = BTreeMap::new();
    indices.insert(
        "@hookform/resolvers@5.2.2".to_string(),
        package_index(
            &store,
            r#"{"name":"@hookform/resolvers","version":"5.2.2"}"#,
            "module.exports = 'resolver';",
        ),
    );
    indices.insert(
        "transitive@1.0.0".to_string(),
        package_index(
            &store,
            r#"{"name":"transitive","version":"1.0.0"}"#,
            "module.exports = 'transitive';",
        ),
    );
    indices.insert(
        "zod@4.1.11".to_string(),
        package_index(
            &store,
            r#"{"name":"zod","version":"4.1.11"}"#,
            "module.exports = 'zod-4.1.11';",
        ),
    );
    indices.insert(
        "zod@4.3.5".to_string(),
        package_index(
            &store,
            r#"{"name":"zod","version":"4.3.5"}"#,
            "module.exports = 'zod-4.3.5';",
        ),
    );

    let mut packages = BTreeMap::new();
    packages.insert(
        "@hookform/resolvers@5.2.2".to_string(),
        LockedPackage {
            name: "@hookform/resolvers".to_string(),
            version: "5.2.2".to_string(),
            dep_path: "@hookform/resolvers@5.2.2".to_string(),
            ..Default::default()
        },
    );
    packages.insert(
        "transitive@1.0.0".to_string(),
        LockedPackage {
            name: "transitive".to_string(),
            version: "1.0.0".to_string(),
            dependencies: BTreeMap::from([("zod".to_string(), "4.1.11".to_string())]),
            dep_path: "transitive@1.0.0".to_string(),
            ..Default::default()
        },
    );
    packages.insert(
        "zod@4.1.11".to_string(),
        LockedPackage {
            name: "zod".to_string(),
            version: "4.1.11".to_string(),
            dep_path: "zod@4.1.11".to_string(),
            ..Default::default()
        },
    );
    packages.insert(
        "zod@4.3.5".to_string(),
        LockedPackage {
            name: "zod".to_string(),
            version: "4.3.5".to_string(),
            dep_path: "zod@4.3.5".to_string(),
            ..Default::default()
        },
    );

    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![
            DirectDep {
                name: "@hookform/resolvers".to_string(),
                dep_path: "@hookform/resolvers@5.2.2".to_string(),
                dep_type: DepType::Production,
                specifier: None,
            },
            DirectDep {
                name: "zod".to_string(),
                dep_path: "zod@4.3.5".to_string(),
                dep_type: DepType::Production,
                specifier: None,
            },
            DirectDep {
                name: "transitive".to_string(),
                dep_path: "transitive@1.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: None,
            },
        ],
    );
    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };

    let linker = Linker::new_with_gvs(&store, LinkStrategy::Copy, true);
    linker.link_all(&project_dir, &graph, &indices).unwrap();

    let resolver_real =
        std::fs::canonicalize(project_dir.join("node_modules/@hookform/resolvers/index.js"))
            .unwrap();
    assert!(
        resolver_real.starts_with(std::fs::canonicalize(&project_dir).unwrap()),
        "hidden-hoist fallback must keep package realpaths project-local"
    );

    let project_hidden = project_dir.join("node_modules/.aube/node_modules/zod");
    assert!(project_hidden.symlink_metadata().unwrap().is_symlink());
    assert_eq!(
        std::fs::read_to_string(project_hidden.join("index.js")).unwrap(),
        "module.exports = 'zod-4.3.5';"
    );
    assert!(
        store
            .virtual_store_dir()
            .join("node_modules/zod")
            .symlink_metadata()
            .is_err(),
        "global virtual store must not expose an unversioned zod alias"
    );
}

#[test]
fn test_global_virtual_store_removes_stale_hidden_hoist_tree() {
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();

    let (store, indices) = setup_store_with_files(dir.path());
    let virtual_store = store.virtual_store_dir();
    let hidden = virtual_store.join("node_modules");
    std::fs::create_dir_all(&hidden).unwrap();
    std::fs::write(hidden.join(".sentinel"), "old").unwrap();
    let stale_target = virtual_store.join("zod@4.1.11/node_modules/zod");
    std::fs::create_dir_all(&stale_target).unwrap();
    sys::create_dir_link(
        &pathdiff::diff_paths(&stale_target, &hidden).unwrap(),
        &hidden.join("zod"),
    )
    .unwrap();

    let linker = Linker::new_with_gvs(&store, LinkStrategy::Copy, true);
    linker
        .link_all(&project_dir, &make_graph(), &indices)
        .unwrap();

    assert!(
        hidden.symlink_metadata().is_err(),
        "shared hidden-hoist aliases must be removed, not pruned in place"
    );
    assert!(
        project_dir
            .join("node_modules/.aube/node_modules/bar")
            .symlink_metadata()
            .unwrap()
            .is_symlink(),
        "project-local hidden hoist remains populated"
    );
}

#[test]
fn test_second_install_reuses_global_store() {
    let dir = tempfile::tempdir().unwrap();

    let (store, indices) = setup_store_with_files(dir.path());
    let linker = Linker::new_with_gvs(&store, LinkStrategy::Copy, true).with_hoist(false);
    let graph = make_graph();

    // First install
    let project1 = dir.path().join("project1");
    std::fs::create_dir_all(&project1).unwrap();
    let stats1 = linker.link_all(&project1, &graph, &indices).unwrap();
    assert_eq!(stats1.packages_linked, 2);
    assert_eq!(stats1.packages_cached, 0);

    // Second install with same deps — should reuse global virtual store
    let project2 = dir.path().join("project2");
    std::fs::create_dir_all(&project2).unwrap();
    let stats2 = linker.link_all(&project2, &graph, &indices).unwrap();
    assert_eq!(stats2.packages_linked, 0);
    assert_eq!(stats2.packages_cached, 2);
    assert_eq!(stats2.files_linked, 0); // no CAS linking needed

    // Both projects should work
    let foo1 = project1.join("node_modules/foo/index.js");
    let foo2 = project2.join("node_modules/foo/index.js");
    assert!(foo1.exists());
    assert!(foo2.exists());
    assert_eq!(
        std::fs::read_to_string(&foo1).unwrap(),
        std::fs::read_to_string(&foo2).unwrap()
    );
}

#[test]
fn gvs_shareable_source_dep_without_index_errors_loudly() {
    // A git / remote-tarball dep is keyed in the GVS under a
    // content-addressed path whose hash folds in the dep's content
    // fingerprint. That fingerprint comes from its package index, so
    // under the global virtual store the dep MUST have an index — a
    // missing one would otherwise leave the dep unmaterialized and
    // dangle every dependent's sibling symlink. The loop used to
    // silently `continue`; assert it now fails loudly with a
    // `MissingPackageIndex` diagnostic instead (the registry pass
    // already does, but git/tarball deps have no `load_index` fallback
    // because their indices aren't persisted by coordinate).
    use aube_lockfile::{GitSource, LocalSource};

    let dir = tempfile::tempdir().unwrap();
    let store = Store::at(dir.path().join("store/files"));

    let git = LocalSource::Git(GitSource {
        url: "https://github.com/request/request.git".to_string(),
        committish: None,
        resolved: "0123456789abcdef0123456789abcdef01234567".to_string(),
        integrity: None,
        subpath: None,
    });
    let dep_path = git.dep_path("request");

    let mut packages = BTreeMap::new();
    packages.insert(
        dep_path.clone(),
        LockedPackage {
            name: "request".to_string(),
            version: "2.88.0".to_string(),
            integrity: None,
            dependencies: BTreeMap::new(),
            dep_path: dep_path.clone(),
            local_source: Some(git),
            ..Default::default()
        },
    );
    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "request".to_string(),
            dep_path: dep_path.clone(),
            dep_type: DepType::Production,
            specifier: None,
        }],
    );
    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };

    // Deliberately omit `dep_path` from the indices map — the contract
    // violation the fetch driver normally prevents.
    let indices: BTreeMap<String, aube_store::PackageIndex> = BTreeMap::new();

    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();
    let linker = Linker::new_with_gvs(&store, LinkStrategy::Copy, true).with_hoist(false);
    let err = linker
        .link_all(&project_dir, &graph, &indices)
        .expect_err("a shareable source dep with no index must error, not dangle");
    assert!(
        matches!(err, Error::MissingPackageIndex(ref dp) if dp == &dep_path),
        "expected MissingPackageIndex({dep_path}), got: {err:?}"
    );
}

/// Regression (#341): with the global virtual store OFF, the source-dep
/// pass keyed its "already materialized?" check on the raw `dep_path`
/// (`@scope/name@url+<hash>`, still containing `/`) instead of the
/// flattened `.aube/` entry name (`@scope+name@url+<hash>`). For a
/// SCOPED git/url source the raw path resolves to a nested dir that
/// never exists, so a relink re-ran `materialize_into` over the already
/// populated entry and its transitive-dep sibling symlink collided with
/// EEXIST. The second `link_all` must be a clean cache hit.
#[test]
fn gvs_off_relinks_scoped_git_source_without_eexist() {
    use aube_lockfile::{GitSource, LocalSource};

    let dir = tempfile::tempdir().unwrap();
    let store = Store::at(dir.path().join("store/files"));

    let git = LocalSource::Git(GitSource {
        url: "https://github.com/lishid/cm-language".to_string(),
        committish: None,
        resolved: "0123456789abcdef0123456789abcdef01234567".to_string(),
        integrity: None,
        subpath: None,
    });
    let lang_dep_path = git.dep_path("@codemirror/language");
    assert!(
        lang_dep_path.starts_with("@codemirror/language@git+"),
        "fixture must exercise a scoped git source: {lang_dep_path}"
    );

    // The scoped git source declares one transitive dep, so
    // `materialize_into` creates a sibling symlink under the entry's
    // `node_modules/@codemirror/` — exactly the collision site.
    let mut lang_deps = BTreeMap::new();
    lang_deps.insert("@codemirror/state".to_string(), "6.0.0".to_string());

    let mut packages = BTreeMap::new();
    packages.insert(
        lang_dep_path.clone(),
        LockedPackage {
            name: "@codemirror/language".to_string(),
            version: "0.0.0".to_string(),
            dependencies: lang_deps,
            dep_path: lang_dep_path.clone(),
            local_source: Some(git),
            ..Default::default()
        },
    );
    packages.insert(
        "@codemirror/state@6.0.0".to_string(),
        LockedPackage {
            name: "@codemirror/state".to_string(),
            version: "6.0.0".to_string(),
            dep_path: "@codemirror/state@6.0.0".to_string(),
            ..Default::default()
        },
    );

    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "@codemirror/language".to_string(),
            dep_path: lang_dep_path.clone(),
            dep_type: DepType::Production,
            specifier: None,
        }],
    );
    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };

    let mut indices = BTreeMap::new();
    indices.insert(
        lang_dep_path.clone(),
        package_index(
            &store,
            "{\"name\":\"@codemirror/language\",\"version\":\"0.0.0\"}",
            "module.exports = 'language';",
        ),
    );
    indices.insert(
        "@codemirror/state@6.0.0".to_string(),
        package_index(
            &store,
            "{\"name\":\"@codemirror/state\",\"version\":\"6.0.0\"}",
            "module.exports = 'state';",
        ),
    );

    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();
    // GVS off — the branch the bug lives in.
    let linker = Linker::new_with_gvs(&store, LinkStrategy::Copy, false).with_hoist(false);

    linker
        .link_all(&project_dir, &graph, &indices)
        .expect("fresh install of a scoped git source must succeed");

    let entry_name = dep_path_to_filename(&lang_dep_path, DEFAULT_VIRTUAL_STORE_DIR_MAX_LENGTH);
    let sibling = project_dir
        .join("node_modules/.aube")
        .join(&entry_name)
        .join("node_modules/@codemirror/state");
    assert!(
        sibling.symlink_metadata().unwrap().is_symlink(),
        "the transitive-dep sibling symlink must exist after the fresh install"
    );

    // Relink over the populated entry: pre-fix this hit EEXIST creating
    // the sibling symlink because the entry-existence check missed.
    linker
        .link_all(&project_dir, &graph, &indices)
        .expect("relinking an existing scoped git source entry must not fail with EEXIST");
}

/// Regression: a version bump keeps the same top-level name
/// (`foo`) but must repoint `node_modules/foo` at the new
/// `.aube/foo@<new>` entry. The old `.aube/foo@<old>/` is left
/// on disk (no one sweeps the virtual store by name), so a
/// plain `path.exists()` check would see a still-resolving
/// stale symlink and keep it. The target-aware
/// `reconcile_top_level_link` compares the expected target
/// string and rewrites the link.
#[test]
fn test_link_all_repoints_symlink_after_version_bump() {
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();
    let store = Store::at(dir.path().join("store/files"));

    // Install 1: foo@1.0.0 as the root's direct dep.
    let mut indices_v1 = BTreeMap::new();
    let foo_v1 = store
        .import_bytes(b"module.exports = 'foo@1';", false)
        .unwrap();
    let mut foo_v1_index = PackageIndex::default();
    foo_v1_index.insert("index.js".to_string(), foo_v1);
    indices_v1.insert("foo@1.0.0".to_string(), foo_v1_index);

    let mut graph_v1 = LockfileGraph::default();
    graph_v1.packages.insert(
        "foo@1.0.0".to_string(),
        LockedPackage {
            name: "foo".to_string(),
            version: "1.0.0".to_string(),
            dep_path: "foo@1.0.0".to_string(),
            ..Default::default()
        },
    );
    graph_v1.importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "foo".to_string(),
            dep_path: "foo@1.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: None,
        }],
    );

    let linker = Linker::new(&store, LinkStrategy::Copy);
    linker
        .link_all(&project_dir, &graph_v1, &indices_v1)
        .unwrap();
    let foo_link = project_dir.join("node_modules/foo");
    assert!(foo_link.symlink_metadata().unwrap().is_symlink());
    assert_eq!(
        std::fs::read_to_string(foo_link.join("index.js")).unwrap(),
        "module.exports = 'foo@1';"
    );

    // Install 2: foo upgraded to 2.0.0. The `.aube/foo@1.0.0/`
    // tree stays on disk (nothing prunes the virtual store by
    // name), so the old `node_modules/foo` symlink still
    // resolves — a naive "does the target exist?" check would
    // keep it.
    let mut indices_v2 = BTreeMap::new();
    let foo_v2 = store
        .import_bytes(b"module.exports = 'foo@2';", false)
        .unwrap();
    let mut foo_v2_index = PackageIndex::default();
    foo_v2_index.insert("index.js".to_string(), foo_v2);
    indices_v2.insert("foo@2.0.0".to_string(), foo_v2_index);

    let mut graph_v2 = LockfileGraph::default();
    graph_v2.packages.insert(
        "foo@2.0.0".to_string(),
        LockedPackage {
            name: "foo".to_string(),
            version: "2.0.0".to_string(),
            dep_path: "foo@2.0.0".to_string(),
            ..Default::default()
        },
    );
    graph_v2.importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "foo".to_string(),
            dep_path: "foo@2.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: None,
        }],
    );
    linker
        .link_all(&project_dir, &graph_v2, &indices_v2)
        .unwrap();

    // The top-level symlink must now resolve to foo@2.0.0's
    // bytes, not foo@1.0.0's.
    assert_eq!(
        std::fs::read_to_string(project_dir.join("node_modules/foo/index.js")).unwrap(),
        "module.exports = 'foo@2';"
    );
}

/// Regression: `shamefully_hoist` hoists transitive deps to the
/// top-level `node_modules/<name>`. When the hoisted version
/// changes between installs (transitive bump), the previous
/// implementation kept the stale symlink because
/// `keep_or_reclaim_broken_symlink` only checked "does target
/// resolve?" and the old `.aube/<old-dep-path>/` was still on
/// disk. `reconcile_top_level_link` + the explicit
/// direct-dep/claimed tracking in `hoist_remaining_into` together
/// fix this.
#[test]
fn test_shamefully_hoist_repoints_after_transitive_version_bump() {
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();
    let store = Store::at(dir.path().join("store/files"));

    // Install 1: root → bar@1.0.0 → foo@1.0.0 (transitive).
    let foo_v1 = store
        .import_bytes(b"module.exports = 'foo@1';", false)
        .unwrap();
    let mut foo_v1_idx = PackageIndex::default();
    foo_v1_idx.insert("index.js".to_string(), foo_v1);
    let bar_v1 = store
        .import_bytes(b"module.exports = 'bar@1';", false)
        .unwrap();
    let mut bar_v1_idx = PackageIndex::default();
    bar_v1_idx.insert("index.js".to_string(), bar_v1);
    let mut indices_v1 = BTreeMap::new();
    indices_v1.insert("foo@1.0.0".to_string(), foo_v1_idx);
    indices_v1.insert("bar@1.0.0".to_string(), bar_v1_idx);

    let mut graph_v1 = LockfileGraph::default();
    let mut bar_deps_v1 = BTreeMap::new();
    bar_deps_v1.insert("foo".to_string(), "1.0.0".to_string());
    graph_v1.packages.insert(
        "bar@1.0.0".to_string(),
        LockedPackage {
            name: "bar".to_string(),
            version: "1.0.0".to_string(),
            dep_path: "bar@1.0.0".to_string(),
            dependencies: bar_deps_v1,
            ..Default::default()
        },
    );
    graph_v1.packages.insert(
        "foo@1.0.0".to_string(),
        LockedPackage {
            name: "foo".to_string(),
            version: "1.0.0".to_string(),
            dep_path: "foo@1.0.0".to_string(),
            ..Default::default()
        },
    );
    graph_v1.importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "bar".to_string(),
            dep_path: "bar@1.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: None,
        }],
    );

    let linker = Linker::new(&store, LinkStrategy::Copy).with_shamefully_hoist(true);
    linker
        .link_all(&project_dir, &graph_v1, &indices_v1)
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(project_dir.join("node_modules/foo/index.js")).unwrap(),
        "module.exports = 'foo@1';",
        "install 1 should hoist foo@1.0.0"
    );

    // Install 2: bar@1.0.0 → foo@2.0.0 (transitive bump). The
    // stale `.aube/foo@1.0.0/` tree is still on disk (nothing
    // sweeps the virtual store by name), so the old hoisted
    // symlink would still resolve — the old `exists?` check
    // would silently keep it.
    let foo_v2 = store
        .import_bytes(b"module.exports = 'foo@2';", false)
        .unwrap();
    let mut foo_v2_idx = PackageIndex::default();
    foo_v2_idx.insert("index.js".to_string(), foo_v2);
    let mut indices_v2 = BTreeMap::new();
    // Reuse bar's materialized index from v1.
    let bar_v1_for_v2 = store
        .import_bytes(b"module.exports = 'bar@1';", false)
        .unwrap();
    let mut bar_v1_idx_v2 = PackageIndex::default();
    bar_v1_idx_v2.insert("index.js".to_string(), bar_v1_for_v2);
    indices_v2.insert("bar@1.0.0".to_string(), bar_v1_idx_v2);
    indices_v2.insert("foo@2.0.0".to_string(), foo_v2_idx);

    let mut graph_v2 = LockfileGraph::default();
    let mut bar_deps_v2 = BTreeMap::new();
    bar_deps_v2.insert("foo".to_string(), "2.0.0".to_string());
    graph_v2.packages.insert(
        "bar@1.0.0".to_string(),
        LockedPackage {
            name: "bar".to_string(),
            version: "1.0.0".to_string(),
            dep_path: "bar@1.0.0".to_string(),
            dependencies: bar_deps_v2,
            ..Default::default()
        },
    );
    graph_v2.packages.insert(
        "foo@2.0.0".to_string(),
        LockedPackage {
            name: "foo".to_string(),
            version: "2.0.0".to_string(),
            dep_path: "foo@2.0.0".to_string(),
            ..Default::default()
        },
    );
    graph_v2.importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "bar".to_string(),
            dep_path: "bar@1.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: None,
        }],
    );

    linker
        .link_all(&project_dir, &graph_v2, &indices_v2)
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(project_dir.join("node_modules/foo/index.js")).unwrap(),
        "module.exports = 'foo@2';",
        "install 2 should repoint the hoisted symlink to foo@2.0.0"
    );
}

// ---------------------------------------------------------------
// `validate_index_key` rejects every shape of index key that
// would make `base.join(key)` escape `base`. Primary defence is
// in `aube-store::import_tarball`; this is the last-chance guard
// before the linker actually writes to disk.
// ---------------------------------------------------------------

#[test]
fn validate_index_key_accepts_normal_keys() {
    validate_index_key("index.js").unwrap();
    validate_index_key("lib/sub/a.js").unwrap();
    validate_index_key("package.json").unwrap();
    validate_index_key("a/b/c/d/e/f.js").unwrap();
}

#[cfg(not(windows))]
#[test]
fn validate_index_key_accepts_posix_colon_filename() {
    validate_index_key("dist/__mocks__/package-json:version.d.ts").unwrap();
}

#[test]
fn validate_index_key_rejects_empty() {
    assert!(matches!(
        validate_index_key(""),
        Err(Error::UnsafeIndexKey(_))
    ));
}

#[test]
fn validate_index_key_rejects_leading_slash() {
    assert!(matches!(
        validate_index_key("/etc/passwd"),
        Err(Error::UnsafeIndexKey(_))
    ));
    assert!(matches!(
        validate_index_key("\\evil"),
        Err(Error::UnsafeIndexKey(_))
    ));
}

#[test]
fn validate_index_key_rejects_parent_dir() {
    assert!(matches!(
        validate_index_key("../../etc/passwd"),
        Err(Error::UnsafeIndexKey(_))
    ));
    assert!(matches!(
        validate_index_key("lib/../../../etc"),
        Err(Error::UnsafeIndexKey(_))
    ));
}

#[test]
fn validate_index_key_rejects_nul_and_backslash() {
    assert!(matches!(
        validate_index_key("lib\0evil"),
        Err(Error::UnsafeIndexKey(_))
    ));
    assert!(matches!(
        validate_index_key("lib\\..\\etc"),
        Err(Error::UnsafeIndexKey(_))
    ));
}

#[cfg(windows)]
#[test]
fn validate_index_key_rejects_windows_drive() {
    assert!(matches!(
        validate_index_key("C:Windows"),
        Err(Error::UnsafeIndexKey(_))
    ));
}

// --- Whole-dir clonefile(2) materialization (macOS+APFS) ---------------
//
// These cover the load-bearing invariant: a package filled by the
// whole-dir clone fast path must be byte-identical to one filled by
// the per-file loop — files, nested directories, symlinks, and the +x
// mode bits. A wrong tree is worse than a slow one.

/// Recursively compare two trees for byte-identical content, identical
/// symlink targets, identical +x bits, and identical directory shape.
/// Returns a human-readable mismatch description, or `None` on match.
#[cfg(all(unix, test))]
fn diff_trees(a: &Path, b: &Path) -> Option<String> {
    use std::collections::BTreeSet;
    use std::os::unix::fs::PermissionsExt;

    let mut names_a = BTreeSet::new();
    for e in std::fs::read_dir(a)
        .map_err(|e| format!("read_dir {a:?}: {e}"))
        .ok()?
    {
        names_a.insert(e.ok()?.file_name());
    }
    let mut names_b = BTreeSet::new();
    for e in std::fs::read_dir(b)
        .map_err(|e| format!("read_dir {b:?}: {e}"))
        .ok()?
    {
        names_b.insert(e.ok()?.file_name());
    }
    if names_a != names_b {
        return Some(format!(
            "entry sets differ at {a:?} vs {b:?}: {names_a:?} != {names_b:?}"
        ));
    }
    for name in names_a {
        let pa = a.join(&name);
        let pb = b.join(&name);
        let ma = std::fs::symlink_metadata(&pa).ok()?;
        let mb = std::fs::symlink_metadata(&pb).ok()?;
        let ta = ma.file_type();
        let tb = mb.file_type();
        if ta.is_symlink() != tb.is_symlink() {
            return Some(format!("symlink-ness differs at {name:?}"));
        }
        if ta.is_symlink() {
            let la = std::fs::read_link(&pa).ok()?;
            let lb = std::fs::read_link(&pb).ok()?;
            if la != lb {
                return Some(format!(
                    "symlink target differs at {name:?}: {la:?} != {lb:?}"
                ));
            }
            continue;
        }
        if ta.is_dir() != tb.is_dir() {
            return Some(format!("dir-ness differs at {name:?}"));
        }
        if ta.is_dir() {
            if let Some(d) = diff_trees(&pa, &pb) {
                return Some(d);
            }
            continue;
        }
        // Regular file: compare content + the +x bit.
        let ca = std::fs::read(&pa).ok()?;
        let cb = std::fs::read(&pb).ok()?;
        if ca != cb {
            return Some(format!("content differs at {name:?}"));
        }
        let xa = ma.permissions().mode() & 0o111;
        let xb = mb.permissions().mode() & 0o111;
        if xa != xb {
            return Some(format!(
                "exec bits differ at {name:?}: {:o} != {:o}",
                ma.permissions().mode() & 0o777,
                mb.permissions().mode() & 0o777
            ));
        }
    }
    None
}

/// Direct test of the clone primitive: a hand-built package directory
/// with every tricky shape — a regular file, an executable bin, a
/// nested subdir, an in-package symlink, and a nested `node_modules`
/// — must come across a raw `clonefile(2)` byte-identical.
#[cfg(target_os = "macos")]
#[test]
fn clonefile_dir_preserves_files_symlinks_exec_and_nested_node_modules() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(src.join("lib/deep")).unwrap();
    std::fs::create_dir_all(src.join("bin")).unwrap();
    std::fs::create_dir_all(src.join("node_modules/inner")).unwrap();

    std::fs::write(
        src.join("package.json"),
        br#"{"name":"x","version":"1.0.0"}"#,
    )
    .unwrap();
    std::fs::write(src.join("lib/deep/mod.js"), b"module.exports = 42;\n").unwrap();
    let bin = src.join("bin/cli.js");
    std::fs::write(&bin, b"#!/usr/bin/env node\nconsole.log(1);\n").unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("deep/mod.js", src.join("lib/alias.js")).unwrap();
    std::fs::write(src.join("node_modules/inner/index.js"), b"1").unwrap();

    let dst = dir.path().join("dst");
    crate::clonedir::clonefile_dir(&src, &dst).expect("clonefile of a directory tree");

    assert!(
        diff_trees(&src, &dst).is_none(),
        "clone diverged: {:?}",
        diff_trees(&src, &dst)
    );
    // Spell out the load-bearing specifics so a failure names the cause.
    assert_eq!(
        std::fs::metadata(dst.join("bin/cli.js"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0o111,
        "+x bit must survive the clone"
    );
    assert!(
        std::fs::symlink_metadata(dst.join("lib/alias.js"))
            .unwrap()
            .file_type()
            .is_symlink(),
        "in-package symlink must clone as a symlink"
    );
    assert_eq!(
        std::fs::read_link(dst.join("lib/alias.js")).unwrap(),
        Path::new("deep/mod.js")
    );
    assert!(dst.join("node_modules/inner/index.js").exists());

    // CoW independence: mutating the clone must not write through to src.
    std::fs::write(dst.join("package.json"), b"MUTATED").unwrap();
    assert_eq!(
        std::fs::read(src.join("package.json")).unwrap(),
        br#"{"name":"x","version":"1.0.0"}"#,
        "clone must be an independent CoW copy"
    );
}

/// End-to-end: materialize the same package via the CloneDir fast path
/// (real `ensure_in_aube_dir` on APFS, which builds the tree then
/// clones) and via the per-file loop (a hand-rolled baseline using the
/// same `link_file_fresh` the loop uses), and assert the two package
/// directories are byte-identical including +x bits. This is the
/// "materialize both ways and diff" guard the design hinges on.
#[cfg(target_os = "macos")]
#[test]
fn clonedir_materialize_matches_per_file_byte_for_byte() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let store = Store::at(dir.path().join("store/files"));

    // A package with a plain file, a package.json, a nested subdir
    // file, and an executable bin — the shapes the +x and nested-dir
    // handling must get right.
    let idx_js = store
        .import_bytes(b"module.exports = 'pkg';\n", false)
        .unwrap();
    let pkg_json = store
        .import_bytes(
            br#"{"name":"pkg","version":"1.2.3","bin":{"pkg":"bin/cli.js"}}"#,
            false,
        )
        .unwrap();
    let deep = store.import_bytes(b"export const x = 1;\n", false).unwrap();
    let cli = store
        .import_bytes(b"#!/usr/bin/env node\nconsole.log('cli');\n", true)
        .unwrap();
    let mut index = PackageIndex::default();
    index.insert("index.js".to_string(), idx_js);
    index.insert("package.json".to_string(), pkg_json);
    index.insert("lib/deep/mod.js".to_string(), deep);
    index.insert("bin/cli.js".to_string(), cli);

    let pkg = LockedPackage {
        name: "pkg".to_string(),
        version: "1.2.3".to_string(),
        integrity: None,
        dependencies: BTreeMap::new(),
        dep_path: "pkg@1.2.3".to_string(),
        ..Default::default()
    };

    // --- Path A: the real CloneDir materialize via ensure_in_aube_dir.
    // On the APFS dev box / CI this exercises build_tree + clonefile_dir.
    let aube_dir = dir.path().join("projectA/node_modules/.aube");
    std::fs::create_dir_all(&aube_dir).unwrap();
    let linker = Linker::new_with_gvs(&store, LinkStrategy::Reflink, false);
    let mut stats = LinkStats::default();
    linker
        .ensure_in_aube_dir(
            &aube_dir,
            "pkg@1.2.3",
            &aube_lockfile::LockfileGraph::default(),
            &pkg,
            &index,
            &mut stats,
            None,
        )
        .expect("clonedir materialize");
    let entry_name = linker.aube_dir_entry_name("pkg@1.2.3");
    let clonedir_pkg = aube_dir.join(&entry_name).join("node_modules").join("pkg");

    // Confirm the tree tier was actually built and the clone path taken
    // (otherwise this test would silently degrade to comparing per-file
    // against per-file and prove nothing).
    let tree = store.tree_path(&linker.virtual_store_subdir("pkg@1.2.3"));
    assert!(
        tree.exists(),
        "tree tier must have been built (clonedir path not exercised?)"
    );

    // --- Path B: per-file baseline. Reflink each file straight into a
    // fresh dir exactly as the per-file loop does, then chmod +x.
    let baseline = dir.path().join("baseline/node_modules/pkg");
    for rel in ["index.js", "package.json", "lib/deep/mod.js", "bin/cli.js"] {
        let stored = &index[rel];
        let target = baseline.join(rel);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        linker.link_file_fresh(stored, rel, &target).unwrap();
        if stored.executable {
            xx::file::make_executable(&target).unwrap();
        }
    }

    // The two materializations must be byte-identical.
    assert!(
        diff_trees(&baseline, &clonedir_pkg).is_none(),
        "clonedir vs per-file diverged: {:?}",
        diff_trees(&baseline, &clonedir_pkg)
    );
    // And the bin must be executable in the cloned result specifically.
    assert_eq!(
        std::fs::metadata(clonedir_pkg.join("bin/cli.js"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0o111,
        "cloned bin must carry +x"
    );
    // Stats parity: the clone counts every index entry as linked.
    assert_eq!(stats.files_linked, index.len());
}
