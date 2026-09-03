use crate::{DepType, DirectDep, Error, LocalSource, LockedPackage, LockfileGraph};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;

#[derive(Debug, Serialize)]
struct WriteNpmLockfile<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<&'a str>,
    #[serde(rename = "lockfileVersion")]
    lockfile_version: u32,
    requires: bool,
    packages: BTreeMap<String, WriteNpmPackage<'a>>,
}

// The fields a `packages` entry can carry. The *declaration* order here
// is irrelevant: npm does NOT serialize these in any fixed sequence —
// its writer (`@npmcli/arborist` → `json-stringify-nice`) emits every
// object's keys with one comparator (`compare` in
// `json-stringify-nice/index.js`, with the `swKeyOrder` preferred list):
//
//   1. all NON-object keys, then all OBJECT (`{…}`) keys — JSON arrays
//      count as non-objects, so `os`/`cpu`/`libc` sort with the scalars;
//   2. within each of those two groups, the `swKeyOrder` preferred keys
//      (`name`, `version`, `resolved`, `integrity`, … `dependencies`)
//      come first in list order, and every remaining key falls back to
//      `localeCompare('en')` — i.e. plain alphabetical.
//
// A struct's serde field order can't express that two-pass type-then-
// alpha rule, so `Serialize` is hand-written below in npm's exact order
// instead of derived. Reproducing it (not a hand-curated sequence) is
// what keeps a write → `npm install` rewrite byte-identical: e.g. npm
// emits `…, cpu, license, optional, os, engines` for a platform-gated
// optional dep, with `engines` LAST because it's the only object key.
#[derive(Debug, Default)]
struct WriteNpmPackage<'a> {
    name: Option<&'a str>,
    version: Option<&'a str>,
    resolved: Option<String>,
    integrity: Option<&'a str>,
    license: Option<&'a str>,
    dependencies: BTreeMap<&'a str, &'a str>,
    dev_dependencies: BTreeMap<&'a str, &'a str>,
    optional_dependencies: BTreeMap<&'a str, &'a str>,
    peer_dependencies: BTreeMap<&'a str, &'a str>,
    /// Paired with `peer_dependencies` above. Required for round-trip
    /// parity: the `optional: true` bit gates
    /// `hoist_auto_installed_peers` and `detect_unmet_peers` — dropping
    /// it on write-back would silently re-flag every optional peer as
    /// required on the next install. Only the `optional` key is
    /// meaningful; other fields npm may add elsewhere aren't modeled.
    peer_dependencies_meta: BTreeMap<&'a str, WriteNpmPeerDepMeta>,
    bin: BTreeMap<&'a str, &'a str>,
    engines: BTreeMap<&'a str, &'a str>,
    os: Vec<String>,
    cpu: Vec<String>,
    libc: Vec<String>,
    /// Root importer only. npm mirrors the manifest's `workspaces` into
    /// `packages[""]` and copies it VERBATIM rather than normalizing it,
    /// so this borrows the parsed value instead of flattening to
    /// `patterns()`: an array stays an array and bun's object form stays
    /// an object, which is what npm writes back (measured, npm 11.19.0).
    /// The third form aube parses, a bare `"workspaces": "packages/*"`,
    /// has no npm spelling to match — npm rejects that manifest outright
    /// (`npm install` exits 1), so nothing it produces can diverge.
    workspaces: Option<&'a aube_manifest::Workspaces>,
    funding: Option<WriteNpmFunding<'a>>,
    link: bool,
    dev: bool,
    optional: bool,
    /// npm `bundleDependencies: ["name", …]` — a JSON array, so it
    /// sorts with the non-object scalars (at `b`, before `cpu`).
    /// Round-trip fidelity for packages declaring bundled deps.
    bundle_dependencies: Vec<&'a str>,
    /// npm `deprecated: "<message>"` — the registry deprecation message.
    deprecated: Option<&'a str>,
    /// npm `hasInstallScript: true` — present iff the package has an
    /// install/preinstall/postinstall script. npm only writes it when
    /// `true`, so the writer skips it when `false`.
    has_install_script: bool,
    /// npm `hasShrinkwrap: true` — present iff the package ships its own
    /// `npm-shrinkwrap.json`.
    has_shrinkwrap: bool,
    /// npm `inBundle: true` — present iff the package ships inside
    /// another package's tarball.
    in_bundle: bool,
    /// npm v3 collapses the "reachable via dev *and* via optional,
    /// but never via production" case into a single `devOptional`
    /// flag. Emitting both `dev: true` and `optional: true` instead
    /// would trip `npm install --omit=dev` into dropping a package
    /// that should have stayed because it's still reachable via
    /// the optional chain (or vice versa with `--omit=optional`).
    dev_optional: bool,
}

impl Serialize for WriteNpmPackage<'_> {
    // Emit keys in npm's order: NON-object keys first (preferred list,
    // then alphabetical), then OBJECT keys (preferred list, then
    // alphabetical). The two `match`-free sequences below are that
    // order spelled out for this struct's exact field set; each arm
    // applies the same emptiness skip the derived `skip_serializing_if`
    // used to. Keep the comments naming the bucket so the order stays
    // auditable against `json-stringify-nice`.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(None)?;

        // --- non-object keys ---
        // preferred (swKeyOrder): name, version, resolved, integrity
        if let Some(v) = self.name {
            map.serialize_entry("name", v)?;
        }
        if let Some(v) = self.version {
            map.serialize_entry("version", v)?;
        }
        if let Some(v) = &self.resolved {
            map.serialize_entry("resolved", v)?;
        }
        if let Some(v) = self.integrity {
            map.serialize_entry("integrity", v)?;
        }
        // remaining non-object keys, alphabetical:
        // bundleDependencies, cpu, deprecated, dev, devOptional,
        // hasInstallScript, hasShrinkwrap, inBundle, libc, license,
        // link, optional, os
        //
        // `bundleDependencies` is a JSON array, so `json-stringify-nice`
        // treats it as a non-object and sorts it with the scalars (at
        // `b`, ahead of `cpu`).
        if !self.bundle_dependencies.is_empty() {
            map.serialize_entry("bundleDependencies", &self.bundle_dependencies)?;
        }
        if !self.cpu.is_empty() {
            map.serialize_entry("cpu", &self.cpu)?;
        }
        if let Some(v) = self.deprecated {
            map.serialize_entry("deprecated", v)?;
        }
        if self.dev {
            map.serialize_entry("dev", &true)?;
        }
        if self.dev_optional {
            map.serialize_entry("devOptional", &true)?;
        }
        if self.has_install_script {
            map.serialize_entry("hasInstallScript", &true)?;
        }
        if self.has_shrinkwrap {
            map.serialize_entry("hasShrinkwrap", &true)?;
        }
        if self.in_bundle {
            map.serialize_entry("inBundle", &true)?;
        }
        if !self.libc.is_empty() {
            map.serialize_entry("libc", &self.libc)?;
        }
        if let Some(v) = self.license {
            map.serialize_entry("license", v)?;
        }
        if self.link {
            map.serialize_entry("link", &true)?;
        }
        if self.optional {
            map.serialize_entry("optional", &true)?;
        }
        if !self.os.is_empty() {
            map.serialize_entry("os", &self.os)?;
        }
        // `workspaces` is the one key whose BUCKET depends on its value,
        // because `json-stringify-nice` sorts on the runtime type and npm
        // copies this field through verbatim. The usual array form is a
        // non-object, so it sorts here, last among the scalars after `os`
        // — npm writes `name, version, license, workspaces, dependencies`.
        // Bun's object form sorts with the object keys instead, last
        // again: npm writes `… dependencies, engines, workspaces`. Both
        // measured, npm 11.19.0. A bare string has no npm spelling to
        // match (npm rejects that manifest), and is a non-object, so it
        // rides with the array.
        if let Some(
            v @ (aube_manifest::Workspaces::Array(_) | aube_manifest::Workspaces::String(_)),
        ) = self.workspaces
        {
            map.serialize_entry("workspaces", v)?;
        }

        // --- object keys ---
        // preferred (swKeyOrder): dependencies
        if !self.dependencies.is_empty() {
            map.serialize_entry("dependencies", &self.dependencies)?;
        }
        // remaining object keys, alphabetical: bin, devDependencies,
        // engines, funding, optionalDependencies, peerDependencies,
        // peerDependenciesMeta
        if !self.bin.is_empty() {
            map.serialize_entry("bin", &self.bin)?;
        }
        if !self.dev_dependencies.is_empty() {
            map.serialize_entry("devDependencies", &self.dev_dependencies)?;
        }
        if !self.engines.is_empty() {
            map.serialize_entry("engines", &self.engines)?;
        }
        if let Some(v) = &self.funding {
            map.serialize_entry("funding", v)?;
        }
        if !self.optional_dependencies.is_empty() {
            map.serialize_entry("optionalDependencies", &self.optional_dependencies)?;
        }
        if !self.peer_dependencies.is_empty() {
            map.serialize_entry("peerDependencies", &self.peer_dependencies)?;
        }
        if !self.peer_dependencies_meta.is_empty() {
            map.serialize_entry("peerDependenciesMeta", &self.peer_dependencies_meta)?;
        }
        // The object half of the split described above — `workspaces`
        // sorts alphabetically among the object keys, i.e. last.
        if let Some(v @ aube_manifest::Workspaces::Object { .. }) = self.workspaces {
            map.serialize_entry("workspaces", v)?;
        }

        map.end()
    }
}

/// The root project's `license` and `bin`, normalized the way npm normalizes
/// them into the root importer entry (verified against npm 11.17): an object
/// `license` collapses to its `type`, a string `bin` expands to
/// `{ <name-without-scope>: <path> }`, and every bin path sheds its leading
/// `./` segments — npm writes `"./a.js"` and `"././d.js"` alike as `d.js`,
/// so leaving them on churns the lockfile against npm's own output.
///
/// Both are read out of [`aube_manifest::PackageJson::extra`], the flattened
/// catch-all, rather than promoted to typed fields — nothing else in aube
/// consumes them, and a typed field would change how a manifest re-serializes.
/// `engines` is already typed, so the caller reads it directly.
///
/// Only the ROOT importer gets this. A workspace member's entry is built from
/// whichever of the link package or an on-disk manifest is available, and the
/// manifest is absent on exactly the path that matters here (a graph read back
/// from an npm lockfile), so there is nothing to mirror.
fn root_manifest_metadata(
    manifest: &aube_manifest::PackageJson,
) -> (Option<&str>, BTreeMap<&str, &str>) {
    let license = manifest.extra.get("license").and_then(|v| match v {
        serde_json::Value::String(s) => Some(s.as_str()),
        serde_json::Value::Object(o) => o.get("type").and_then(|t| t.as_str()),
        _ => None,
    });

    // Repeated rather than single: npm collapses `././d.js` all the way to
    // `d.js`. Prefix-stripping keeps the result a borrow of the manifest.
    fn strip_dot_slash(mut path: &str) -> &str {
        while let Some(rest) = path.strip_prefix("./") {
            path = rest;
        }
        path
    }

    let mut bin: BTreeMap<&str, &str> = BTreeMap::new();
    match manifest.extra.get("bin") {
        Some(serde_json::Value::Object(entries)) => {
            for (target, path) in entries {
                if let Some(path) = path.as_str() {
                    bin.insert(target.as_str(), strip_dot_slash(path));
                }
            }
        }
        Some(serde_json::Value::String(path)) => {
            if let Some(name) = manifest.name.as_deref() {
                bin.insert(
                    name.rsplit('/').next().unwrap_or(name),
                    strip_dot_slash(path.as_str()),
                );
            }
        }
        _ => {}
    }

    (license, bin)
}

/// npm emits `funding: {"url": "…"}` verbatim, one key, on every
/// package entry that declared funding. We only carry the URL on
/// `LockedPackage`, so this wrapper slots it back into the expected
/// shape on write.
#[derive(Debug, Serialize, Default)]
struct WriteNpmFunding<'a> {
    url: &'a str,
}

/// Serialized form of a `peerDependenciesMeta` entry. Mirrors the
/// reader's `RawNpmPeerDepMeta` so writer → reader → writer round
/// trips byte-identically for every meta variant we model today.
#[derive(Debug, Serialize, Default)]
struct WriteNpmPeerDepMeta {
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    optional: bool,
}

/// Serialize a [`LockfileGraph`] as a `package-lock.json` v3 file.
///
/// The graph is flat (one entry per `name@version`, peer contexts
/// collapsed to a single `(name, version)` identity) and npm wants a
/// hoist + nest layout, so we rebuild it here. Algorithm:
///
/// 1. Place each root direct dep at `node_modules/<name>` — these are
///    the "hoisted" versions.
/// 2. BFS from each placed node: for every child dep, walk up the
///    ancestor chain looking for a matching entry. If an ancestor
///    already carries the right version, the child resolves through
///    nested-resolution and needs no entry of its own. Otherwise,
///    hoist to root if the root slot is free or already matches; if
///    the root is occupied by a different version, nest directly
///    under the current node.
/// 3. Continue until the queue drains. Cycles terminate because each
///    install_path is placed at most once.
///
/// Lossy areas (documented so callers know what to expect):
///  - Peer-contextualized variants of the same `name@version` collapse
///    to one entry. npm's layout can't represent per-context peers.
///  - Registry `resolved` tarball URLs are emitted when they were
///    present in the parsed graph. Graphs synthesized without
///    `tarball_url` fall back to npm's tolerated no-`resolved` form.
///  - Non-git local source entries (`file:`, URL tarballs) aren't
///    emitted yet. Git sources emit their pinned `resolved:` URL.
///    Workspace `link:` packages are emitted as importer entries plus
///    a root `node_modules/<name>` link record.
pub fn write(
    path: &Path,
    graph: &LockfileGraph,
    manifest: &aube_manifest::PackageJson,
) -> Result<(), Error> {
    // Key packages by `name@version` (ignore peer-context suffix) so
    // lookups from parent deps resolve to one canonical entry even if
    // the graph has several contextualized variants.
    let mut canonical = crate::build_canonical_map(graph);
    for pkg in graph
        .packages
        .values()
        .filter(|pkg| super::source::is_git_local_source(pkg.local_source.as_ref()))
    {
        canonical
            .entry(super::canonical_key_from_dep_path(&pkg.dep_path))
            .or_insert(pkg);
    }

    // Compute reachability for dev/optional flags, matching npm's
    // path-based semantics: a package is `dev: true` iff *every* path
    // from the root crosses a dev edge, `optional: true` iff every
    // path crosses an optional edge (a root `optionalDependencies`
    // entry or any package's `optionalDependencies` edge). Both are
    // answered by complement: a package escapes the flag iff it stays
    // reachable when the BFS refuses to cross edges of that type.
    let roots = graph.importers.get(".").cloned().unwrap_or_default();
    let all_roots: Vec<DirectDep> = graph
        .importers
        .values()
        .flat_map(|deps| deps.iter().cloned())
        .collect();
    let any_reach = reachable_without(&canonical, &all_roots, &[]);
    let non_dev_reach = reachable_without(&canonical, &all_roots, &[DepType::Dev]);
    let non_opt_reach = reachable_without(&canonical, &all_roots, &[DepType::Optional]);
    let prod_reach = reachable_without(&canonical, &all_roots, &[DepType::Dev, DepType::Optional]);

    // Build a hoist/nest tree keyed by a sequence of "node_modules"
    // path segments — e.g. `["foo"]` for `node_modules/foo`,
    // `["foo", "bar"]` for `node_modules/foo/node_modules/bar`. Shared
    // with bun (which renders the same segment list as `foo/bar`).
    let root_tree_roots = non_link_roots(graph, &roots);
    let tree = super::build_hoist_tree(&canonical, &root_tree_roots);
    // For the npm writer, re-key the tree by install_path strings.
    let mut placed: BTreeMap<String, String> = tree
        .into_iter()
        .map(|(segs, key)| (super::segments_to_install_path(&segs), key))
        .collect();

    // Build the JSON structure.
    let root_key = ""; // npm's root importer install path.

    let mut packages: BTreeMap<String, WriteNpmPackage> = BTreeMap::new();

    // Root importer entry — mirrors the manifest's dep fields, plus the
    // `license`/`bin`/`engines` npm copies across from the project's own
    // `package.json`. Omitting the latter three silently dropped them from a
    // hand-written `package-lock.json` on the first rewrite.
    let (root_license, root_bin) = root_manifest_metadata(manifest);
    packages.insert(
        root_key.to_string(),
        WriteNpmPackage {
            name: manifest.name.as_deref(),
            version: manifest.version.as_deref(),
            license: root_license,
            dependencies: borrow_map(&manifest.dependencies),
            dev_dependencies: borrow_map(&manifest.dev_dependencies),
            optional_dependencies: borrow_map(&manifest.optional_dependencies),
            peer_dependencies: borrow_map(&manifest.peer_dependencies),
            bin: root_bin,
            engines: manifest
                .engines
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect(),
            workspaces: manifest.workspaces.as_ref(),
            ..Default::default()
        },
    );

    // `file:` local directory/tarball deps (`npm install file:../foo`)
    // surface in npm's lockfile as a pair, exactly like workspace links:
    // a `<path>: { name, version }` package entry keyed by the on-disk
    // path, plus a `node_modules/<name>: { resolved: "<path>", link: true }`
    // record. The hoist-tree pass below never places these — its roots key
    // off `name@version` while a local dep's dep_path is `name@file+<hash>`,
    // so the canonical-map lookup misses and the package would otherwise be
    // dropped entirely. `npm ci` then rejects the lockfile with
    // `Missing: <name>@<version> from lock file`. Emit the pair here.
    //
    // The root importer's own local deps always take the root slot, so
    // they need no reservation set; the members' do, and theirs is built
    // below once the member identities are known.
    emit_file_dep_links(graph, &roots, ".", &Default::default(), &mut packages);

    // Resolve each workspace member's identity (name/version/peers).
    // A `LocalSource::Link` package exists only when the graph was
    // *read* from an npm lockfile (the reader synthesizes it from the
    // `node_modules/<name>: {link:true}` pair). On a fresh resolve from
    // package.json there is no such package, so recover the member's
    // name/version/peers from its own `package.json` on disk — the same
    // best-effort disk read the pnpm and bun writers use. Without this
    // fallback every member importer + its child deps were dropped and
    // `npm ci` rejected the lockfile with `Missing: <member> from lock
    // file`.
    // Pre-read every member's `package.json` once, up front, so the
    // borrowed name/version/peer strings live as long as `packages`
    // (the `WriteNpmPackage` arena borrows them). Only read for importers
    // without a `LocalSource::Link` package — i.e. a fresh resolve.
    let project_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let member_manifests: BTreeMap<&str, aube_manifest::PackageJson> = graph
        .importers
        .keys()
        .filter(|p| p.as_str() != ".")
        .filter(|p| workspace_package_for_importer(graph, p).is_none())
        .map(|p| {
            let m =
                aube_manifest::PackageJson::from_path(&project_dir.join(p).join("package.json"))
                    .unwrap_or_default();
            (p.as_str(), m)
        })
        .collect();

    // Every name that will end up owning a root `node_modules/<name>`
    // entry, gathered before the member loop writes anything: each
    // top-level hoist placement, and each workspace member, which the
    // loop links at the root. A member's local link consults this to
    // decide root-vs-nested, because both of those passes serialize
    // AFTER it and would otherwise overwrite the link — leaving the dep
    // with orphaned metadata and no link node anywhere.
    //
    // Read off `placed` rather than recomputed from the root importer's
    // direct deps: `placed` IS the hoist tree's own answer to what lands
    // at the top level, so a TRANSITIVE that hoists there is covered by
    // construction. Enumerating direct deps missed exactly those — root
    // depending on `outer`, which pulls `shared@2`, alongside a member's
    // `shared = file:../local`, silently dropped the member's link. The
    // loop below grows `placed`, but only with `<importer>/node_modules/…`
    // keys, so reading it here already sees the whole top level.
    //
    // Member names come from the same two sources the loop itself uses: a
    // `LocalSource::Link` package when the graph was READ from a
    // lockfile, and the on-disk manifest when it was freshly resolved.
    // Consulting only the former missed every member on a fresh resolve,
    // which is the common case.
    let root_claimed: BTreeSet<String> = placed
        .keys()
        .filter_map(|install_path| install_path.strip_prefix("node_modules/"))
        .filter(|name| !name.contains("/node_modules/"))
        .map(str::to_string)
        .chain(
            graph
                .importers
                .keys()
                .filter(|p| p.as_str() != ".")
                .filter_map(|p| {
                    workspace_package_for_importer(graph, p)
                        .map(|pkg| pkg.name.as_str())
                        .or_else(|| {
                            member_manifests
                                .get(p.as_str())
                                .and_then(|m| m.name.as_deref())
                        })
                })
                .map(str::to_string),
        )
        .collect();

    for (importer_path, importer_roots) in graph.importers.iter().filter(|(path, _)| *path != ".") {
        let linked = workspace_package_for_importer(graph, importer_path);
        let disk_manifest = member_manifests.get(importer_path.as_str());
        let member_name = linked
            .map(|p| p.name.as_str())
            .or(disk_manifest.and_then(|m| m.name.as_deref()));
        let Some(member_name) = member_name else {
            // No identity anywhere (no link package, no/anonymous
            // manifest on disk) — can't emit a coherent member entry.
            continue;
        };
        let member_version = linked
            .map(|p| p.version.as_str())
            .or(disk_manifest.and_then(|m| m.version.as_deref()));
        let peer_dependencies: BTreeMap<&str, &str> = match (linked, disk_manifest) {
            (Some(p), _) => p
                .peer_dependencies
                .iter()
                .map(|(n, v)| (n.as_str(), v.as_str()))
                .collect(),
            (None, Some(m)) => m
                .peer_dependencies
                .iter()
                .map(|(n, v)| (n.as_str(), v.as_str()))
                .collect(),
            (None, None) => BTreeMap::new(),
        };

        let (dependencies, dev_dependencies, optional_dependencies) =
            dep_sections_from_direct_deps(importer_roots);
        packages.insert(
            importer_path.clone(),
            WriteNpmPackage {
                name: Some(member_name),
                version: member_version,
                dependencies,
                dev_dependencies,
                optional_dependencies,
                peer_dependencies,
                ..Default::default()
            },
        );
        packages.insert(
            format!("node_modules/{member_name}"),
            WriteNpmPackage {
                resolved: Some(importer_path.clone()),
                link: true,
                ..Default::default()
            },
        );

        emit_file_dep_links(
            graph,
            importer_roots,
            importer_path,
            &root_claimed,
            &mut packages,
        );

        let workspace_tree_roots = non_link_roots(graph, importer_roots);
        let workspace_tree = super::build_hoist_tree(&canonical, &workspace_tree_roots);
        // Skip subtrees whose top-level segment is already hoisted to
        // `node_modules/<name>` at the same canonical version: Node's
        // upward `node_modules` walk from `<importer>/...` resolves to
        // the root copy, so the workspace-nested entries are dead
        // weight. npm's writer omits them, and emitting them produces
        // round-trip diffs vs npm-generated lockfiles.
        let redundant_tops: BTreeSet<String> = workspace_tree
            .iter()
            .filter(|(segs, key)| {
                segs.len() == 1
                    && placed
                        .get(&format!("node_modules/{}", segs[0]))
                        .is_some_and(|root_key| root_key == *key)
            })
            .map(|(segs, _)| segs[0].clone())
            .collect();
        for (segs, canonical_key) in workspace_tree {
            if redundant_tops.contains(&segs[0]) {
                continue;
            }
            let install_path =
                format!("{importer_path}/{}", super::segments_to_install_path(&segs));
            placed.entry(install_path).or_insert(canonical_key);
        }
    }

    for (install_path, canonical_key) in &placed {
        let Some(pkg) = canonical.get(canonical_key).copied() else {
            continue;
        };
        // Re-serialize pkg.dependencies as `name → version` (strip
        // peer suffixes so npm's parser sees plain version ranges).
        // npm's format wants semver ranges here in theory, but since
        // we only have exact resolved versions, emit those — real
        // npm does the same thing for nested packages.
        //
        // Filter out deps whose canonical key isn't in the map.
        // These are typically platform-filtered optional deps or
        // ignoredOptionalDependencies — the resolver has already
        // dropped them from `canonical`, so emitting them here
        // would produce a `dependencies` entry referencing a
        // package with no matching `packages` record. `npm ci`
        // treats that as a corrupt lockfile, and `npm install`
        // would refetch the dropped package. Matches the bun and
        // yarn writers, which filter the same way.
        let optional_deps: BTreeMap<&str, &str> = pkg
            .optional_dependencies
            .iter()
            .filter(|(n, value)| canonical.contains_key(&super::child_canonical_key(n, value)))
            .map(|(n, value)| {
                // Prefer the declared range from the package's own
                // manifest (what npm itself writes) over the resolved
                // pin. Falls back to the pin for entries where the
                // source lockfile didn't carry declared ranges (e.g.
                // pnpm → npm conversion).
                let rendered = pkg
                    .declared_dependencies
                    .get(n)
                    .map(String::as_str)
                    .unwrap_or_else(|| super::dep_value_as_version(n, value));
                (n.as_str(), rendered)
            })
            .collect();
        let deps: BTreeMap<&str, &str> = pkg
            .dependencies
            .iter()
            .filter(|(n, value)| {
                !pkg.optional_dependencies.contains_key(*n)
                    && canonical.contains_key(&super::child_canonical_key(n, value))
            })
            .map(|(n, value)| {
                let rendered = pkg
                    .declared_dependencies
                    .get(n)
                    .map(String::as_str)
                    .unwrap_or_else(|| super::dep_value_as_version(n, value));
                (n.as_str(), rendered)
            })
            .collect();

        // npm v3 flag semantics:
        //   prod-reachable     → neither flag
        //   dev only           → `dev: true`
        //   optional only      → `optional: true`
        //   dev + optional     → `devOptional: true` (single flag)
        // Emitting both `dev` and `optional` for the both-reachable
        // case is *wrong*: `npm install --omit=dev` drops anything
        // with `dev: true` and `--omit=optional` drops anything with
        // `optional: true`, so a package reachable through both
        // chains would get removed under either omit even though the
        // other chain still needs it.
        // Unreachable entries (canonical-key mismatches, hand-built
        // graphs) stay unflagged rather than collapsing into
        // `devOptional` vacuously.
        let is_reachable = any_reach.contains(canonical_key);
        let is_dev = is_reachable && !non_dev_reach.contains(canonical_key);
        let is_opt = is_reachable && !non_opt_reach.contains(canonical_key);
        // Third bit, npm's `devOptional`: no pure-production path
        // exists, but neither "every path is dev" nor "every path is
        // optional" holds (e.g. reachable via a dev chain *and* via an
        // optional chain). The all-paths-dev-and-optional case
        // collapses into the same flag.
        let is_dev_opt = is_reachable && !prod_reach.contains(canonical_key);
        let dev_optional = (is_dev && is_opt) || (is_dev_opt && !is_dev && !is_opt);
        let dev = is_dev && !is_opt;
        let optional = is_opt && !is_dev;

        // Aliased deps (`"h3-v2": "npm:h3@..."` in package.json)
        // round-trip as `node_modules/h3-v2` with an explicit
        // `name: "h3"`, and every registry package gets a
        // `resolved:` line — what npm itself writes. JSR packages
        // are just the degenerate case where the URL can't be
        // reconstructed from name+version alone. The URL is
        // populated on the LockedPackage by the resolver (from the
        // packument's `dist.tarball`) or carried through from a
        // prior parse of the same npm lockfile.
        let alias_name = pkg.alias_of.as_deref();
        let resolved = super::source::npm_resolved_field(pkg);

        // Round-trip `peerDependencies` so a subsequent read of the
        // rewritten lockfile still feeds the peer-context pass. Values
        // are the declared peer ranges; they never carry the peer
        // suffix the snapshot side uses, so no re-encoding is needed.
        let peer_deps: BTreeMap<&str, &str> = pkg
            .peer_dependencies
            .iter()
            .map(|(n, v)| (n.as_str(), v.as_str()))
            .collect();
        // Paired `peerDependenciesMeta` round-trip. The `optional: true`
        // bit is what `hoist_auto_installed_peers` and
        // `detect_unmet_peers` key off to distinguish "user opted
        // out" from "peer missing and required" — dropping this
        // on write-back silently re-flags every optional peer as
        // required on the next install.
        let peer_deps_meta: BTreeMap<&str, WriteNpmPeerDepMeta> = pkg
            .peer_dependencies_meta
            .iter()
            .map(|(n, m)| {
                (
                    n.as_str(),
                    WriteNpmPeerDepMeta {
                        optional: m.optional,
                    },
                )
            })
            .collect();

        packages.insert(
            install_path.clone(),
            WriteNpmPackage {
                name: alias_name,
                version: Some(pkg.version.as_str()),
                resolved,
                integrity: pkg.integrity.as_deref(),
                license: pkg.license.as_deref(),
                dependencies: deps,
                optional_dependencies: optional_deps,
                peer_dependencies: peer_deps,
                peer_dependencies_meta: peer_deps_meta,
                bin: pkg
                    .bin
                    .iter()
                    .filter(|(k, _)| !k.is_empty())
                    .map(|(k, v)| (k.as_str(), v.as_str()))
                    .collect(),
                engines: pkg
                    .engines
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.as_str()))
                    .collect(),
                os: pkg.os.to_vec(),
                cpu: pkg.cpu.to_vec(),
                libc: pkg.libc.to_vec(),
                funding: pkg
                    .funding_url
                    .as_deref()
                    .map(|url| WriteNpmFunding { url }),
                dev,
                optional,
                dev_optional,
                bundle_dependencies: pkg
                    .bundled_dependencies
                    .iter()
                    .map(String::as_str)
                    .collect(),
                deprecated: pkg.deprecated.as_deref(),
                has_install_script: pkg.has_install_script,
                has_shrinkwrap: pkg.has_shrinkwrap,
                in_bundle: pkg.in_bundle,
                ..Default::default()
            },
        );
    }

    let doc = WriteNpmLockfile {
        name: manifest.name.as_deref(),
        version: manifest.version.as_deref(),
        lockfile_version: 3,
        requires: true,
        packages,
    };

    let mut body =
        serde_json::to_string_pretty(&doc).map_err(|e| Error::parse(path, e.to_string()))?;
    // npm writes a trailing newline; match it so diffs stay clean.
    body.push('\n');
    crate::atomic_write_lockfile(path, body.as_bytes())?;
    Ok(())
}

fn workspace_package_for_importer<'a>(
    graph: &'a LockfileGraph,
    importer_path: &str,
) -> Option<&'a LockedPackage> {
    graph.packages.values().find(|pkg| {
        matches!(
            &pkg.local_source,
            Some(LocalSource::Link(path)) if path == Path::new(importer_path)
        )
    })
}

fn non_link_roots(graph: &LockfileGraph, roots: &[DirectDep]) -> Vec<DirectDep> {
    roots
        .iter()
        .filter(|dep| {
            // All three are emitted out of band by `emit_file_dep_links`
            // as npm's `link: true` pair, and `Link` additionally has no
            // virtual-store node at all — so none of them belongs in the
            // hoisted `name@version` tree.
            !graph.packages.get(&dep.dep_path).is_some_and(|pkg| {
                matches!(
                    pkg.local_source,
                    Some(
                        LocalSource::Link(_) | LocalSource::Directory(_) | LocalSource::Tarball(_)
                    )
                )
            })
        })
        .cloned()
        .collect()
}

/// npm emits each `file:` local directory/tarball dependency as a pair
/// of `packages` entries:
///
/// ```json
/// "local-pkg":              { "name": "local-utils", "version": "1.0.0" },
/// "node_modules/local-utils": { "resolved": "local-pkg", "link": true }
/// ```
///
/// The first is keyed by the dep's on-disk path (npm strips the `file:`
/// prefix and a leading `./`, but keeps `../` parent climbs), carries
/// only `name`/`version`, and is what `npm ci` validates the root
/// `dependencies` entry against. The second is the `node_modules/<name>`
/// symlink record pointing back at that path.
///
/// `LocalSource::Link` gets the SAME pair, with one exception. npm has no
/// separate encoding for `link:` — a `file:` directory dependency is
/// already written as a link record — so the two only differ inside aube,
/// where `Link` skips the virtual store. The exception is a workspace
/// MEMBER, which is also a `Link` but whose pair the importer/workspace
/// pass below owns. A member is exactly a link whose TARGET is an
/// importer, so ask `graph.importers` directly.
///
/// That guard is belt-and-braces, not load-bearing, and the comment says
/// so rather than implying a correctness dependency that does not exist:
/// this function runs BEFORE the workspace pass, which rewrites the same
/// two keys and wins on ordering alone — deleting the guard leaves the
/// whole crate's tests green (measured). It is here to state which pass
/// owns a member instead of resting that on pass order.
///
/// Not `workspace_package_for_importer`: that asks "is there a package
/// linking AT this path", which a plain `link:` dep answers about
/// ITSELF — it is the package linking at its own target — so using it
/// here silently skips every dep this function exists to emit.
///
/// This function used to take `Directory`/`Tarball` only, on the stated
/// grounds that "the importer/workspace machinery already emits their
/// pair" for every `Link`. That is true of a workspace member and false
/// of a `link:` to a plain directory, which has no importer entry at all
/// — so nothing emitted it, `non_link_roots` also kept it out of the
/// hoist tree, and it vanished from the lockfile. nub's own freshness
/// check then read the importer as having zero direct deps and failed
/// every later `--frozen-lockfile` with `manifest adds <name>@link:…`.
fn emit_file_dep_links<'a>(
    graph: &'a LockfileGraph,
    roots: &[DirectDep],
    importer_path: &str,
    root_claimed: &BTreeSet<String>,
    packages: &mut BTreeMap<String, WriteNpmPackage<'a>>,
) {
    for dep in roots {
        let Some(pkg) = graph.packages.get(&dep.dep_path) else {
            continue;
        };
        let resolved = match &pkg.local_source {
            Some(LocalSource::Link(path))
                if graph.importers.contains_key(&*path.to_string_lossy()) =>
            {
                continue;
            }
            Some(
                local
                @ (LocalSource::Directory(_) | LocalSource::Tarball(_) | LocalSource::Link(_)),
            ) => npm_file_dep_path(&local.path_posix()),
            _ => continue,
        };
        packages.insert(
            resolved.clone(),
            WriteNpmPackage {
                name: Some(pkg.name.as_str()),
                version: Some(pkg.version.as_str()),
                ..Default::default()
            },
        );

        // npm hoists the first target to the root and NESTS a conflicting
        // one under the importer that wants it, which is the only way the
        // format can express two members depending on the same NAME at
        // different paths. Measured, npm 11.19.0, members `a`→`vendor/one` and
        // `b`→`vendor/two`:
        //
        //     "node_modules/shared":            { resolved: "vendor/one" }
        //     "packages/b/node_modules/shared": { resolved: "vendor/two" }
        //
        // Keying every link at the root unconditionally made the last
        // writer win, so `a` silently resolved to `b`'s package — no error,
        // a wrong module. Take the root slot only if it is free or already
        // points at this exact target; otherwise nest.
        //
        // `packages` alone cannot answer that, because it is still being
        // built: the hoist tree and the workspace pass both serialize AFTER
        // this one and can claim the same root alias. A root registry dep
        // aliased `shared` plus a member's `shared = file:…` hit exactly
        // that — the link went to root while the slot looked free, the
        // later hoist pass overwrote it, and the member's link vanished
        // from the lockfile entirely rather than merely moving. Hence
        // `root_claimed`, read off the hoist tree's own top level plus the
        // member roster before either pass runs. Measured, npm 11.19.0:
        //
        //     "node_modules/shared":                 <the registry package>
        //     "packages/app/node_modules/shared":    { resolved: "vendor/local" }
        let is_root_importer = importer_path == "." || importer_path.is_empty();
        let root_key = format!("node_modules/{}", dep.name);
        let taken_by_other = match packages.get(&root_key) {
            Some(existing) => existing.resolved.as_deref() != Some(resolved.as_str()),
            None => !is_root_importer && root_claimed.contains(dep.name.as_str()),
        };
        let key = if taken_by_other && !is_root_importer {
            format!("{importer_path}/node_modules/{}", dep.name)
        } else {
            root_key
        };
        packages.insert(
            key,
            WriteNpmPackage {
                resolved: Some(resolved),
                link: true,
                ..Default::default()
            },
        );
    }
}

/// Render the lockfile path key for a local dep's package entry the way
/// npm does: drop a leading `./` (npm normalizes `file:./local-pkg` to
/// `local-pkg`) but preserve `../` climbs verbatim (`file:../sib` →
/// `../sib`).
///
/// The path arrives PROJECT-ROOT-relative whichever importer declared
/// it — `aube_resolver::local_source::rebase_local` rewrites every
/// variant that way precisely so downstream code can resolve it with one
/// `project_root.join(rel)` — and npm's keys are project-root-relative
/// too, so the two frames already agree and nothing needs re-anchoring.
///
/// This used to take an `importer_path` and prepend it, on the stated
/// belief that "for a non-root importer the stored path is
/// importer-relative". That belief contradicted `rebase_local`, and the
/// result was a doubled prefix: a member `packages/app` declaring
/// `file:../../vendor/local-pkg` was keyed `packages/app/vendor/local-pkg`,
/// a path matching nothing on disk, where npm writes `vendor/local-pkg`
/// (measured, npm 11.19.0). Root importers were unaffected, which is why it
/// went unnoticed.
fn npm_file_dep_path(path_posix: &str) -> String {
    path_posix
        .strip_prefix("./")
        .unwrap_or(path_posix)
        .to_string()
}

type DepSections<'a> = (
    BTreeMap<&'a str, &'a str>,
    BTreeMap<&'a str, &'a str>,
    BTreeMap<&'a str, &'a str>,
);

fn dep_sections_from_direct_deps(deps: &[DirectDep]) -> DepSections<'_> {
    let mut dependencies = BTreeMap::new();
    let mut dev_dependencies = BTreeMap::new();
    let mut optional_dependencies = BTreeMap::new();

    for dep in deps {
        let rendered = dep.specifier.as_deref().unwrap_or_else(|| {
            super::dep_value_as_version(&dep.name, super::dep_path_tail(&dep.name, &dep.dep_path))
        });
        match dep.dep_type {
            DepType::Production => {
                dependencies.insert(dep.name.as_str(), rendered);
            }
            DepType::Dev => {
                dev_dependencies.insert(dep.name.as_str(), rendered);
            }
            DepType::Optional => {
                optional_dependencies.insert(dep.name.as_str(), rendered);
            }
        }
    }

    (dependencies, dev_dependencies, optional_dependencies)
}
/// Compute the set of canonical keys (`name@version`) reachable from
/// the root importer's direct deps of a given type. Traversal follows
/// `LockedPackage.dependencies`, dropping peer suffixes so the visited
/// keys match the canonical map built at the top of [`write`].
/// BFS over the locked graph refusing to cross edges of the
/// `excluded` types (empty = plain reachability). npm's `dev` /
/// `optional` flags mean "every install path crosses a dev /
/// optional edge", so a package earns a flag iff it drops out of
/// the set when those edges are off-limits (and `devOptional` iff it
/// drops out when both are). Root edges carry their importer dep
/// type; below the root the only typed edges are
/// `optionalDependencies` (a dependency's `devDependencies` are
/// never installed), so the `Dev` exclusion filters seeds only,
/// while the `Optional` exclusion also filters child edges.
fn reachable_without(
    canonical: &BTreeMap<String, &LockedPackage>,
    roots: &[DirectDep],
    excluded: &[DepType],
) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    for dep in roots {
        if excluded.contains(&dep.dep_type) {
            continue;
        }
        let key = super::canonical_key_from_dep_path(&dep.dep_path);
        if canonical.contains_key(&key) && out.insert(key.clone()) {
            queue.push_back(key);
        }
    }
    while let Some(key) = queue.pop_front() {
        let Some(pkg) = canonical.get(&key).copied() else {
            continue;
        };
        for (child_name, child_value) in &pkg.dependencies {
            if excluded.contains(&DepType::Optional)
                && pkg.optional_dependencies.contains_key(child_name)
            {
                continue;
            }
            let child_key = super::child_canonical_key(child_name, child_value);
            if canonical.contains_key(&child_key) && out.insert(child_key.clone()) {
                queue.push_back(child_key);
            }
        }
    }
    out
}
fn borrow_map(m: &BTreeMap<String, String>) -> BTreeMap<&str, &str> {
    m.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect()
}
