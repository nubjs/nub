use super::dep_path::{
    dep_path_tail, parse_dep_path, peerless_alias_target, rewrite_peer_suffix,
    rewrite_snapshot_alias_deps,
    version_to_dep_path,
};
use super::raw::{
    RawBinSpec, RawDepSpec, RawRuntimeVariant, local_source_from_resolution, parse_raw_lockfile,
};
use crate::{
    CatalogEntry, DepType, DirectDep, Error, LocalSource, LockedPackage, LockfileGraph,
    PeerDepMeta, RuntimePin, RuntimeTarget, RuntimeVariant, git_commits_match,
};
use aube_util::path::normalize_lexical;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn rebase_importer_local(local: LocalSource, importer_path: &str) -> LocalSource {
    fn rebase(path: PathBuf, importer_path: &str) -> PathBuf {
        if importer_path == "." {
            path
        } else {
            normalize_lexical(&Path::new(importer_path).join(path))
        }
    }

    match local {
        LocalSource::Directory(path) => LocalSource::Directory(rebase(path, importer_path)),
        LocalSource::Tarball(path) => LocalSource::Tarball(rebase(path, importer_path)),
        LocalSource::Link(path) => LocalSource::Link(rebase(path, importer_path)),
        LocalSource::Portal(path) => LocalSource::Portal(rebase(path, importer_path)),
        LocalSource::Exec(path) => LocalSource::Exec(rebase(path, importer_path)),
        LocalSource::Git(_) | LocalSource::RemoteTarball(_) => local,
    }
}

/// Parse a pnpm-lock.yaml file into a LockfileGraph.
pub fn parse(path: &Path) -> Result<LockfileGraph, Error> {
    let content = crate::read_lockfile(path)?;
    let raw = parse_raw_lockfile(&content)
        .map_err(|e| Error::parse_yaml_err(path, content.clone(), &e))?;

    // Parse importers (direct deps of each workspace package).
    // We track synthesized LockedPackages for local (`file:` / `link:`)
    // deps here so the main packages loop below doesn't try to process
    // them off the canonical lockfile key.
    let mut importers = BTreeMap::new();
    let mut local_packages: BTreeMap<String, LockedPackage> = BTreeMap::new();
    let mut local_importers: BTreeMap<String, String> = BTreeMap::new();
    let mut local_snapshot_keys: BTreeMap<String, String> = BTreeMap::new();
    let mut all_local_snapshot_keys: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let mut skipped_optional_dependencies: BTreeMap<String, BTreeMap<String, String>> =
        BTreeMap::new();
    // pnpm v9 encodes npm-aliases implicitly: the importer key is
    // the alias (`express-fork`), `specifier:` carries `npm:<real>@<range>`,
    // and `version:` is `<real>@<resolved>`. There is no `aliasOf:`
    // field — that's an aube-specific writer extension. We record
    // each alias here and synthesize an alias-keyed LockedPackage
    // after the canonical packages loop, mirroring the shape the
    // resolver-fresh path emits so the linker stays single-shape.
    // Tuple: (alias_dep_path, real_dep_path, alias_name, real_name).
    let mut alias_remaps: Vec<(String, String, String, String)> = Vec::new();

    // pnpm 10.14+ records `devEngines.runtime` pins as synthetic
    // importer deps whose specifier/version carry a `runtime:` prefix
    // (`node: {specifier: runtime:^24.4.0, version: runtime:24.4.1}`).
    // Those are not packages — route them into `graph.runtimes`
    // instead of `DirectDep`s so the resolver/linker never tries to
    // fetch `node` from the npm registry. Map: runtime name →
    // (specifier, exact version, came from devDependencies). First
    // importer to declare a runtime wins (pnpm only writes it on the
    // importer whose manifest declares devEngines — the root).
    let mut runtime_imports: BTreeMap<String, (String, String, bool)> = BTreeMap::new();
    let mut record_runtime = |name: &str, info: &RawDepSpec, dep_type: DepType| -> bool {
        let Some(version) = info.version.strip_prefix("runtime:") else {
            return false;
        };
        let specifier = info
            .specifier
            .strip_prefix("runtime:")
            .unwrap_or(&info.specifier);
        runtime_imports.entry(name.to_string()).or_insert_with(|| {
            (
                specifier.to_string(),
                version.to_string(),
                matches!(dep_type, DepType::Dev),
            )
        });
        true
    };

    let mut push_direct = |deps: &mut Vec<DirectDep>,
                           alias_remaps: &mut Vec<(String, String, String, String)>,
                           importer_path: &str,
                           name: &str,
                           info: &RawDepSpec,
                           dep_type: DepType| {
        // pnpm appends a `(peer@ver)` suffix to the importer
        // `version:` of URL- and git-based direct deps when the
        // resolved snapshot carries peer context, the same way it
        // does for semver versions. `LocalSource::parse` treats the
        // whole string as the URL, so a RemoteTarballSource built
        // from the raw value fetches `…/tar.gz/SHA(peer@ver)` and
        // 404s. Strip it here so the URL that reaches the fetcher
        // and the dep_path hash are both peer-context-free —
        // consistent with what `parse_dep_path` does for snapshot
        // keys downstream.
        let classify_version = info.version.split('(').next().unwrap_or(&info.version);
        if let Some(local) = LocalSource::parse(classify_version, Path::new("")) {
            // `Path::new("")` means tarball-vs-dir classification is
            // skipped; we default to Directory and rely on the
            // resolver's on-disk re-read for the authoritative source
            // type during a subsequent `aube install` (lockfile-only
            // path never materializes local deps anyway before the
            // fetch step re-classifies).
            //
            // Re-classify Directory → Tarball if the path looks
            // like a tarball filename, so `.tgz`/`.tar.gz`
            // targets round-trip correctly even when the file
            // isn't present at parse time. The filename
            // heuristic lives on `LocalSource` so this stays in
            // lockstep with `LocalSource::parse`.
            let local = match local {
                LocalSource::Directory(p) if LocalSource::path_looks_like_tarball(&p) => {
                    LocalSource::Tarball(p)
                }
                // Importer `version:` for git deps is the canonical
                // `<url>#<commit>` form pnpm writes. The parser
                // puts the `<commit>` into `committish`; since
                // this is a lockfile round-trip (not a raw user
                // spec), treat it as the pinned commit.
                LocalSource::Git(mut g) if g.resolved.is_empty() => {
                    if let Some(c) = g.committish.take() {
                        g.resolved = c;
                    }
                    LocalSource::Git(g)
                }
                other => other,
            };
            let snapshot_key = format!("{name}@{}", local.specifier());
            // pnpm writes a non-root importer's local `version:` path relative to
            // that importer, so it must be rebased to root. This covers a literal
            // `link:`/`file:` spec (`specifier == version`), the `workspace:`
            // protocol, AND a plain semver range pnpm resolved to a workspace
            // sibling — that last case carries a NON-`workspace:` specifier (e.g.
            // `^0.0.18`) but still an importer-relative `version: link:../sibling`
            // (real-world: dub's `@dub/embed-react` -> `@dub/embed-core`). The
            // common thread is an importer-relative path, which pnpm always spells
            // with a leading `..`; aube's own format is already root-relative and
            // never escapes upward, so this never double-rebases (the
            // `not_rebased_twice` invariant).
            let importer_relative = local.path().is_some_and(|p| p.starts_with(".."));
            let should_rebase = importer_path != "."
                && (info.specifier == classify_version
                    || info.specifier.starts_with("workspace:")
                    || importer_relative);
            let local = if should_rebase {
                rebase_importer_local(local, importer_path)
            } else {
                local
            };
            let dep_path = local.dep_path(name);
            deps.push(DirectDep {
                name: name.to_string(),
                dep_path: dep_path.clone(),
                dep_type,
                specifier: Some(info.specifier.clone()),
            });
            local_packages
                .entry(dep_path.clone())
                .or_insert_with(|| LockedPackage {
                    name: name.to_string(),
                    version: "0.0.0".to_string(),
                    integrity: None,
                    dependencies: BTreeMap::new(),
                    peer_dependencies: BTreeMap::new(),
                    peer_dependencies_meta: BTreeMap::new(),
                    dep_path: dep_path.clone(),
                    local_source: Some(local),
                    ..Default::default()
                });
            if should_rebase {
                local_importers
                    .entry(dep_path.clone())
                    .or_insert_with(|| importer_path.to_string());
            }
            local_snapshot_keys
                .entry(dep_path)
                .or_insert_with(|| snapshot_key.clone());
            all_local_snapshot_keys.insert(snapshot_key);
        } else {
            // Detect npm-aliased deps purely from the shape of
            // `version:`. pnpm encodes aliases as
            // `<real_name>@<resolved>(peers…)` regardless of how the
            // alias was declared:
            //   - direct:  `specifier: npm:beamcoder-prebuild@0.7.1`
            //   - catalog: `specifier: 'catalog:'` (the alias lives
            //              in `pnpm-workspace.yaml#catalog`)
            // The earlier `specifier.starts_with("npm:")` gate missed
            // the catalog flavor and silently dropped those deps.
            // Strip any peer suffix before parsing so `version:
            // 18.2.0(react@18.2.0)` (a regular dep with peers) does
            // not parse as `name="18.2.0(react"`.
            let bare_version = info
                .version
                .split('(')
                .next()
                .unwrap_or(info.version.as_str());
            let dep_path = if let Some((real_name, resolved)) = parse_dep_path(bare_version)
                && real_name != name
            {
                let peer_suffix = info
                    .version
                    .find('(')
                    .map(|i| &info.version[i..])
                    .unwrap_or("");
                let alias_dep_path = format!("{name}@{resolved}{peer_suffix}");
                let real_dep_path = info.version.clone();
                alias_remaps.push((
                    alias_dep_path.clone(),
                    real_dep_path,
                    name.to_string(),
                    real_name,
                ));
                alias_dep_path
            } else {
                version_to_dep_path(name, &info.version)
            };
            deps.push(DirectDep {
                name: name.to_string(),
                dep_path,
                dep_type,
                specifier: Some(info.specifier.clone()),
            });
        }
    };

    for (importer_path, importer) in &raw.importers {
        // pnpm writes the workspace root as either `'.'` (most
        // common / current) or `''` (seen on v9 lockfiles in the
        // wild, e.g. npmx.dev). Both mean "the repo root" — we key
        // the graph on `.` everywhere downstream (linker, filters,
        // stats), so normalize at parse time and keep the rest of
        // the pipeline single-shape.
        let importer_path = if importer_path.is_empty() {
            "."
        } else {
            importer_path.as_str()
        };

        // Guard against a malformed lockfile that writes both `''`
        // and `'.'` for root — `BTreeMap` iteration visits `''`
        // first, so the real `'.'` entry would otherwise silently
        // overwrite the normalized empty-key entry. pnpm never
        // emits this, but skipping the second visit is cheap and
        // makes the intent explicit.
        if importers.contains_key(importer_path) {
            continue;
        }

        let mut deps = Vec::new();

        if let Some(ref d) = importer.dependencies {
            for (name, info) in d {
                if record_runtime(name, info, DepType::Production) {
                    continue;
                }
                push_direct(
                    &mut deps,
                    &mut alias_remaps,
                    importer_path,
                    name,
                    info,
                    DepType::Production,
                );
            }
        }
        if let Some(ref d) = importer.dev_dependencies {
            for (name, info) in d {
                if record_runtime(name, info, DepType::Dev) {
                    continue;
                }
                push_direct(
                    &mut deps,
                    &mut alias_remaps,
                    importer_path,
                    name,
                    info,
                    DepType::Dev,
                );
            }
        }
        if let Some(ref d) = importer.optional_dependencies {
            for (name, info) in d {
                if record_runtime(name, info, DepType::Optional) {
                    continue;
                }
                push_direct(
                    &mut deps,
                    &mut alias_remaps,
                    importer_path,
                    name,
                    info,
                    DepType::Optional,
                );
            }
        }

        if let Some(ref d) = importer.skipped_optional_dependencies
            && !d.is_empty()
        {
            let mut map = BTreeMap::new();
            for (name, info) in d {
                map.insert(name.clone(), info.specifier.clone());
            }
            skipped_optional_dependencies.insert(importer_path.to_string(), map);
        }

        importers.insert(importer_path.to_string(), deps);
    }

    // pnpm v9 splits packages (canonical, keyed by `name@version`) from
    // snapshots (contextualized, keyed by the full dep_path with any
    // `(peer@ver)` suffix). The LockfileGraph needs one entry per snapshot
    // — the same canonical package can produce multiple snapshots when
    // different parts of the tree resolve its peers differently.
    //
    // If `snapshots:` is missing (older aube lockfiles where we wrote
    // everything into packages), fall back to iterating packages directly.
    let mut packages = BTreeMap::new();

    // Harvest snapshot dependencies for any local (`file:`) package
    // that showed up in the importers loop. The canonical snapshot
    // key for a local dep is `<name>@<specifier>` — e.g.
    // `foo@file:./vendor/foo` — so we construct it from each
    // synthesized entry and pull its `dependencies` block out of the
    // raw snapshots map.
    for local_pkg in local_packages.values_mut() {
        if let Some(ref local) = local_pkg.local_source {
            let canonical = format!("{}@{}", local_pkg.name, local.specifier());
            let snapshot_key = local_snapshot_keys
                .get(&local_pkg.dep_path)
                .map(String::as_str)
                .unwrap_or(canonical.as_str());
            // URL-based direct deps have their peer-context suffix
            // stripped (see `push_direct`), but the matching snapshot
            // entry pnpm wrote still carries the suffix. Fall back to
            // any snapshot whose peer-stripped canonical matches so
            // transitive dependency metadata still flows through.
            let snap = raw
                .snapshots
                .get(snapshot_key)
                .or_else(|| {
                    if snapshot_key == canonical {
                        None
                    } else {
                        raw.snapshots.get(&canonical)
                    }
                })
                .or_else(|| {
                    raw.snapshots.iter().find_map(|(k, v)| {
                        parse_dep_path(k)
                            .filter(|(n, ver)| format!("{n}@{ver}") == canonical)
                            .map(|_| v)
                    })
                });
            if let Some(snap) = snap
                && let Some(mut deps) = snap.dependencies.clone()
            {
                rewrite_snapshot_alias_deps(&mut deps, &mut alias_remaps);
                local_pkg.dependencies = deps;
            }
            if let Some(snap) = snap
                && let Some(mut opt_deps) = snap.optional_dependencies.clone()
            {
                rewrite_snapshot_alias_deps(&mut opt_deps, &mut alias_remaps);
                local_pkg.dependencies.extend(opt_deps.clone());
                local_pkg.optional_dependencies = opt_deps;
            }
            // Prefer the authoritative LocalSource classification
            // from the `resolution:` block over the guess the
            // importers loop made from the bare specifier. For git
            // deps, preserve any `path:` selector already captured
            // from the importer's `version:` URL — pnpm v9 encodes
            // the subpath in the snapshot key and doesn't always
            // echo it on the resolution block.
            let pkg_info = raw.packages.get(&canonical).or_else(|| match local {
                LocalSource::Git(git) => raw.packages.iter().find_map(|(key, pkg_info)| {
                    parse_dep_path(key)
                        .filter(|(name, _)| name == &local_pkg.name)
                        .and(pkg_info.resolution.as_ref())
                        .and_then(local_source_from_resolution)
                        .and_then(|candidate| match candidate {
                            LocalSource::Git(candidate)
                                if git_commits_match(&candidate.resolved, &git.resolved)
                                    && candidate.subpath == git.subpath =>
                            {
                                Some(pkg_info)
                            }
                            _ => None,
                        })
                }),
                LocalSource::RemoteTarball(tarball) => {
                    raw.packages.iter().find_map(|(key, pkg_info)| {
                        parse_dep_path(key)
                            .filter(|(name, _)| name == &local_pkg.name)
                            .and(pkg_info.resolution.as_ref())
                            .and_then(local_source_from_resolution)
                            .and_then(|candidate| match candidate {
                                LocalSource::RemoteTarball(candidate)
                                    if candidate.url == tarball.url =>
                                {
                                    Some(pkg_info)
                                }
                                _ => None,
                            })
                    })
                }
                _ => None,
            });
            // pnpm records the real semver of a URL-keyed package (remote
            // tarball, hosted-git codeload archive) on the `packages:`
            // entry's `version:` field, since the dep-path key is a URL
            // rather than a `name@semver` pin. The importer loop seeds these
            // with a `0.0.0` placeholder; lift the recorded semver here so a
            // conversion writer (yarn's `version "x.y.z"`, bun's `name@x.y.z`
            // for non-git tarballs) emits the real version instead of
            // `0.0.0` — which yarn frozen-rejects on a git dep.
            if let Some(pkg_info) = pkg_info
                && let Some(ver) = pkg_info.version.as_ref()
                && local_pkg.version == "0.0.0"
            {
                local_pkg.version = ver.clone();
            }
            if let Some(pkg_info) = pkg_info
                && let Some(ref res) = pkg_info.resolution
                && let Some(mut ls) = local_source_from_resolution(res)
            {
                if let Some(importer_path) = local_importers.get(&local_pkg.dep_path) {
                    ls = rebase_importer_local(ls, importer_path);
                }
                if matches!(ls, LocalSource::Git(_) | LocalSource::RemoteTarball(_)) {
                    local_pkg.integrity = res.integrity.clone();
                }
                if let LocalSource::Git(ref mut g) = ls
                    && let Some(LocalSource::Git(prior)) = &local_pkg.local_source
                {
                    if git_commits_match(&g.resolved, &prior.resolved) {
                        g.resolved = prior.resolved.clone();
                    }
                    if g.subpath.is_none() {
                        g.subpath = prior.subpath.clone();
                    }
                }
                local_pkg.local_source = Some(ls);
            }
        }
    }
    // Rebuild keys in case the local_source rewrite above changed
    // the classification — kind alone doesn't affect the encoded
    // dep_path (the hash is over the path string only), but the
    // `resolution:` block can also hand us a *different path* than
    // the importer's specifier, which does. Recompute both the map
    // key and the struct field from the final `local_source` so
    // `graph.packages.get(&dep.dep_path)` stays consistent with how
    // DirectDeps were keyed up in the importer loop above. Resolution
    // metadata can refine the importer `version:` form (for example,
    // preserving a `git+ssh://` repo URL), so update DirectDeps for
    // any key that shifts during reclassification.
    let mut rekeyed: BTreeMap<String, LockedPackage> = BTreeMap::new();
    let mut local_rekeys: BTreeMap<String, String> = BTreeMap::new();
    for (old_key, mut pkg) in local_packages {
        let new_key = pkg.local_source.as_ref().unwrap().dep_path(&pkg.name);
        pkg.dep_path = new_key.clone();
        if old_key != new_key {
            local_rekeys.insert(old_key, new_key.clone());
        }
        rekeyed.insert(new_key, pkg);
    }
    let local_packages = rekeyed;
    if !local_rekeys.is_empty() {
        for deps in importers.values_mut() {
            for dep in deps {
                if let Some(new_key) = local_rekeys.get(&dep.dep_path) {
                    dep.dep_path.clone_from(new_key);
                }
            }
        }
    }
    // Canonical keys the main loop should ignore — those are the
    // snapshot keys we already absorbed above.
    let mut local_canonical_keys: std::collections::HashSet<String> = local_packages
        .values()
        .filter_map(|p| {
            p.local_source
                .as_ref()
                .map(|l| format!("{}@{}", p.name, l.specifier()))
        })
        .collect();
    local_canonical_keys.extend(all_local_snapshot_keys);

    let snapshot_keys: Vec<String> = if raw.snapshots.is_empty() {
        raw.packages.keys().cloned().collect()
    } else {
        raw.snapshots.keys().cloned().collect()
    };

    for dep_path in snapshot_keys {
        if local_canonical_keys.contains(&dep_path) {
            continue;
        }
        let (name, version) = parse_dep_path(&dep_path)
            .ok_or_else(|| Error::parse(path, format!("invalid dep path: {dep_path}")))?;
        // Runtime pin entries (`node@runtime:24.4.1`) are not packages
        // — they're absorbed into `graph.runtimes` below. Skipping them
        // here keeps them out of the package table so the fetch/link
        // pipeline never sees them.
        if version.starts_with("runtime:") {
            continue;
        }
        // URL-based direct deps are absorbed into `local_packages`
        // under the peer-stripped URL form (see `push_direct`), but the
        // snapshot key still carries any `(peer@ver)` suffix pnpm
        // appended. Check the peer-stripped canonical too so we don't
        // create a duplicate entry that round-trips as a stray
        // `packages:` block.
        if local_canonical_keys.contains(&format!("{name}@{version}")) {
            continue;
        }

        // Look up the canonical package entry by stripping any peer suffix.
        let canonical_key = version_to_dep_path(&name, &version);
        let pkg_info = raw
            .packages
            .get(&canonical_key)
            .or_else(|| raw.packages.get(&dep_path));

        let integrity = pkg_info
            .and_then(|p| p.resolution.as_ref())
            .and_then(|r| r.integrity.clone());

        // Registry packages record a `tarball:` URL only when
        // `lockfileIncludeTarballUrl=true` was active at write time.
        // Preserve it on read so the round-trip writes the same URL
        // back without having to reconsult the registry client.
        //
        // pnpm also writes a `tarball:` entry for non-registry transitive
        // deps whose key is a URL (remote tarball from a github override,
        // pkg.pr.new, etc.) — capture those on the same field so the
        // install path can fetch them verbatim instead of deriving a
        // registry URL that would 404.
        let tarball_url = pkg_info
            .and_then(|p| p.resolution.as_ref())
            .and_then(|r| r.tarball.as_ref())
            .filter(|t| t.starts_with("http://") || t.starts_with("https://"))
            .cloned();
        let registry_git_hosted = pkg_info
            .and_then(|p| p.resolution.as_ref())
            .is_some_and(|r| r.git_hosted);

        // pnpm writes `version: <semver>` alongside non-registry entries
        // whose dep-path key is a URL. Prefer that over the URL itself
        // when the dep-path version isn't a real semver — the install
        // path uses `pkg.version` for the store-content cross-check,
        // and comparing a URL to the tarball's declared `2.4.1` would
        // fail every github override'd package.
        //
        // Gated on `tarball_url.is_some()` so the swap only applies to
        // the remote-tarball case (where the URL is recoverable from
        // `resolution.tarball` at write time). `git+`/`git://` /
        // `.git#sha` transitive entries resolve through
        // `resolution: {type: git, commit, repo}` and keep the URL as
        // their version so install can fetch the git source.
        let version_is_http_url = version.starts_with("http://") || version.starts_with("https://");
        let dep_path_git_commit = git_commit_from_dep_path_version(&version).map(str::to_string);
        let version = if version_is_http_url && tarball_url.is_some() {
            pkg_info.and_then(|p| p.version.clone()).unwrap_or(version)
        } else {
            version
        };

        let snapshot = raw.snapshots.get(&dep_path);
        let mut optional_dependencies = snapshot
            .and_then(|s| s.optional_dependencies.clone())
            .unwrap_or_default();
        let mut dependencies = snapshot
            .and_then(|s| s.dependencies.clone())
            .unwrap_or_default();
        rewrite_snapshot_alias_deps(&mut dependencies, &mut alias_remaps);
        rewrite_snapshot_alias_deps(&mut optional_dependencies, &mut alias_remaps);
        dependencies.extend(optional_dependencies.clone());
        let bundled_dependencies = snapshot
            .and_then(|s| s.bundled_dependencies.clone())
            .unwrap_or_default();
        let optional = snapshot.and_then(|s| s.optional).unwrap_or(false);
        let transitive_peer_dependencies = snapshot
            .and_then(|s| s.transitive_peer_dependencies.clone())
            .unwrap_or_default();

        let peer_dependencies = pkg_info
            .and_then(|p| p.peer_dependencies.clone())
            .unwrap_or_default();
        let peer_dependencies_meta = pkg_info
            .and_then(|p| p.peer_dependencies_meta.clone())
            .unwrap_or_default()
            .into_iter()
            .map(|(k, v)| {
                (
                    k,
                    PeerDepMeta {
                        optional: v.optional,
                    },
                )
            })
            .collect();
        let os = pkg_info.map(|p| p.os.clone()).unwrap_or_default();
        let cpu = pkg_info.map(|p| p.cpu.clone()).unwrap_or_default();
        let libc = pkg_info.map(|p| p.libc.clone()).unwrap_or_default();
        let engines = pkg_info.map(|p| p.engines.clone()).unwrap_or_default();
        // pnpm records a registry `deprecated:` reason on package
        // entries; stash it on the generic meta map so the writer can
        // re-emit it (matching how bun round-trips the same field).
        let extra_meta = pkg_info
            .and_then(|p| p.deprecated.clone())
            .map(|msg| BTreeMap::from([("deprecated".to_string(), serde_json::Value::String(msg))]))
            .unwrap_or_default();
        // pnpm's lockfile only stores `hasBin: true/false` (no paths);
        // reconstruct an opaque single-entry map on parse so
        // `!bin.is_empty()` stays equivalent to `hasBin`, then let
        // downstream writers fill in real paths when they have them.
        // The map key + value are placeholders — writers that care
        // about bin names (bun) read from richer sources.
        let bin = if pkg_info.map(|p| p.has_bin).unwrap_or(false) {
            let mut m = BTreeMap::new();
            m.insert(String::new(), String::new());
            m
        } else {
            BTreeMap::new()
        };
        // Aube-specific extension (see `WritablePackageInfo::alias_of`)
        // — ordinary pnpm lockfiles never carry it, so this stays
        // `None` on pnpm-authored input and round-trips the resolver-
        // emitted value for aliased packages.
        let alias_of = pkg_info.and_then(|p| p.alias_of.clone());

        // Reclassify transitive URL-keyed entries — github forks,
        // pkg.pr.new, `file:` targets — so they round-trip with the
        // right `local_source`. Without this, the install path sees
        // `local_source: None` + a URL-form version and tries to
        // fetch the dep from the npm registry (404).
        let local_source = pkg_info
            .and_then(|p| p.resolution.as_ref())
            .and_then(local_source_from_resolution);
        let local_source = local_source.map(|mut source| {
            if let LocalSource::Git(git) = &mut source
                && let Some(commit) = dep_path_git_commit.as_deref()
                && git_commits_match(&git.resolved, commit)
            {
                git.resolved = commit.to_string();
            }
            source
        });
        // `lockfileIncludeTarballUrl` puts registry tarball URLs on
        // ordinary `name@version` entries; only URL-keyed entries are
        // true remote-tarball deps.
        let local_source = match local_source {
            Some(LocalSource::RemoteTarball(_)) if !version_is_http_url => None,
            other => other,
        };
        if integrity.is_none()
            && resolution_requires_integrity(
                pkg_info.and_then(|p| p.resolution.as_ref()),
                &local_source,
            )
        {
            return Err(Error::parse(
                path,
                format!(
                    "lockfile entry {dep_path:?} has a remote tarball resolution without integrity"
                ),
            ));
        }

        packages.insert(
            dep_path.clone(),
            LockedPackage {
                name,
                version,
                integrity,
                dependencies,
                optional_dependencies,
                peer_dependencies,
                peer_dependencies_meta,
                dep_path,
                local_source,
                os: os.into(),
                cpu: cpu.into(),
                libc: libc.into(),
                bundled_dependencies,
                optional,
                transitive_peer_dependencies,
                tarball_url,
                registry_git_hosted,
                alias_of,
                yarn_checksum: None,
                engines,
                bin,
                // pnpm's `snapshots:` only records resolved pins, so
                // the parser has no declared ranges to restore. Left
                // empty; npm / yarn / bun writers fall back to pins
                // when re-emitting a pnpm-sourced graph into one of
                // their formats.
                declared_dependencies: BTreeMap::new(),
                // pnpm's format doesn't carry per-package license or
                // funding metadata, so a pnpm → npm conversion
                // degrades to empty rather than re-fetching each
                // packument. npm writers skip these fields when
                // `None`.
                license: None,
                funding_url: None,
                extra_meta,
                // npm-specific verbatim flags — a pnpm lockfile doesn't
                // record them on the package entry (pnpm keeps
                // `deprecated` in `extra_meta` and derives the rest), so
                // they stay at their defaults on a pnpm parse.
                has_install_script: false,
                has_shrinkwrap: false,
                in_bundle: false,
                deprecated: None,
            },
        );
    }

    // Bridge from the lockfile-canonical spelling of each
    // importer-declared local package (`<name>@<specifier>`, e.g.
    // `pkg@file:vendor/pkg` — the exact string an npm-alias
    // `version:` or a rewritten transitive alias value references)
    // to the final hashed key the package is stored under. Needed
    // by the alias synthesis below: aliases whose target is a
    // `file:` package reference it by the canonical spelling, but
    // `local_packages` is keyed by `LocalSource::dep_path` hashes.
    let local_by_canonical: BTreeMap<String, String> = {
        let mut m = BTreeMap::new();
        for (final_key, pkg) in &local_packages {
            if let Some(l) = &pkg.local_source {
                m.insert(format!("{}@{}", pkg.name, l.specifier()), final_key.clone());
            }
        }
        // The snapshot keys recorded at importer-parse time cover
        // spellings that a later resolution-block refinement or
        // importer rebase no longer reproduces from the final
        // `local_source`.
        for (dep_path, snapshot_key) in &local_snapshot_keys {
            let final_key = local_rekeys.get(dep_path).unwrap_or(dep_path);
            m.entry(snapshot_key.clone())
                .or_insert_with(|| final_key.clone());
        }
        m
    };

    // Merge the synthesized local (`file:`/`link:`) packages in
    // *before* alias synthesis so alias targets can resolve against
    // them. No same-key collision is possible: the main loop above
    // skips every key in `local_canonical_keys`.
    for (k, v) in local_packages {
        packages.insert(k, v);
    }

    // Synthesize alias-keyed LockedPackages for npm-aliased importer
    // deps. pnpm v9 only writes the canonical (real-name-keyed) entry
    // in `packages:`; we clone it under the alias dep_path with
    // `name=alias` and `alias_of=Some(real)` so the linker — which
    // already supports this shape via the resolver-fresh path — can
    // create `node_modules/<alias>` symlinks correctly.
    //
    // Aliases targeting an importer-declared *local* (`file:`)
    // package — pnpm writes `version: <real>@file:<path>` when a
    // local dep is consumed under a different in-tree name, e.g.
    // vite's playground/ssr-deps fixtures — get the same treatment,
    // except the clone is keyed like every other local package
    // (`LocalSource::dep_path(alias)`, the hashed form that is safe
    // as a filesystem name) and every reference to the raw alias
    // spelling is remapped afterwards.
    let mut alias_local_renames: BTreeMap<String, String> = BTreeMap::new();
    for (alias_dep_path, real_dep_path, alias_name, real_name) in alias_remaps {
        // Skip if the alias entry already exists (aube-written
        // lockfile that emitted both `aliasOf:` and an alias-keyed
        // packages entry), or if an earlier remap for the same
        // alias spelling already synthesized the local clone.
        if packages.contains_key(&alias_dep_path)
            || alias_local_renames.contains_key(&alias_dep_path)
        {
            continue;
        }
        let bare_real = real_dep_path
            .split('(')
            .next()
            .unwrap_or(&real_dep_path)
            .to_string();
        if let Some(local_key) = local_by_canonical.get(&bare_real)
            && let Some(real_pkg) = packages.get(local_key)
            && let Some(local) = real_pkg.local_source.clone()
        {
            let final_alias_key = local.dep_path(&alias_name);
            let mut aliased = real_pkg.clone();
            aliased.name = alias_name;
            aliased.dep_path = final_alias_key.clone();
            aliased.alias_of = Some(real_name);
            alias_local_renames.insert(alias_dep_path, final_alias_key.clone());
            packages.insert(final_alias_key, aliased);
            continue;
        }
        let Some(real_pkg) = packages
            .get(&real_dep_path)
            .or_else(|| peerless_alias_target(&packages, &real_dep_path))
        else {
            return Err(Error::parse(
                path,
                format!(
                    "npm-alias references missing package {real_dep_path} (alias dep_path: {alias_dep_path})"
                ),
            ));
        };
        let mut aliased = real_pkg.clone();
        aliased.name = alias_name;
        aliased.dep_path = alias_dep_path.clone();
        aliased.alias_of = Some(real_name);
        packages.insert(alias_dep_path, aliased);
    }

    // Point every reference at the rekeyed local-alias entries: the
    // importer DirectDeps and the snapshot dep values were written
    // with the raw `<alias>@file:<path>` spelling before the hashed
    // key existed.
    if !alias_local_renames.is_empty() {
        for deps in importers.values_mut() {
            for dep in deps {
                if let Some(new_key) = alias_local_renames.get(&dep.dep_path) {
                    dep.dep_path.clone_from(new_key);
                }
            }
        }
        for pkg in packages.values_mut() {
            for map in [&mut pkg.dependencies, &mut pkg.optional_dependencies] {
                for (dep_name, value) in map.iter_mut() {
                    let referenced = format!("{dep_name}@{value}");
                    if let Some(new_key) = alias_local_renames.get(&referenced) {
                        *value = dep_path_tail(new_key, dep_name).to_string();
                    }
                }
            }
        }
    }

    // Normalize git / remote-tarball references so a lockfile round-trip
    // produces the same graph a fresh resolve does. pnpm (and aube's own
    // writer) record these deps by their resolved URL, but aube's linker
    // and graph hasher look them up under the FS-safe hashed form
    // (`request@url+<hash>` / `request@git+<hash>`) — the same form
    // `push_direct` already uses for *direct* git/tarball deps. Two
    // independent rewrites are needed, and both run whenever a git/tarball
    // dep is present (not only when a peer suffix exists):
    //
    //   1. Package keys. A *transitive* git/tarball package keyed by URL
    //      (`<pkg>@https://codeload.…/<sha>`) has its own virtual-store
    //      dir materialized at the escaped `https+++…` name, while every
    //      parent's sibling symlink (and the hasher's child lookup, via
    //      `shared_local_dep_path`) targets the `url+<hash>` name. The
    //      symlink then dangles — Node throws `Cannot find module '<dep>'`
    //      — and the child's content/engine taint never reaches the
    //      parent's GVS hash. Re-key the head to the canonical hashed form.
    //   2. Peer suffixes. `request-promise-core@1.1.4(request@https://…/<sha>)`
    //      → `…(request@url+<hash>)`, the inverse of the writer's
    //      hashed→spec pass. Without it a registry package that peers with
    //      a git/tarball dep would re-key on the next install, busting the
    //      warm path (and emitting a churned lockfile).
    let spec_peer_to_hashed = |head: &str| -> Option<String> {
        let (name, value) = parse_dep_path(head)?;
        crate::shared_local_dep_path(&name, &value)
    };
    // Canonicalize a git/remote-tarball package's own `name@<url>` head to
    // the hashed form, preserving any peer suffix verbatim (URLs aube keys
    // never contain `(`, so the first `(` always starts the suffix).
    let canonical_local_head = |key: &str, pkg: &LockedPackage| -> Option<String> {
        let local @ (LocalSource::Git(_) | LocalSource::RemoteTarball(_)) =
            pkg.local_source.as_ref()?
        else {
            return None;
        };
        let suffix = key.find('(').map_or("", |i| &key[i..]);
        let new_key = format!("{}{suffix}", local.dep_path(&pkg.name));
        (new_key != key).then_some(new_key)
    };
    let has_local_source = packages.values().any(|p| {
        matches!(
            p.local_source,
            Some(LocalSource::Git(_) | LocalSource::RemoteTarball(_))
        )
    });
    if has_local_source {
        let rekeyed: BTreeMap<String, LockedPackage> = std::mem::take(&mut packages)
            .into_iter()
            .map(|(key, mut pkg)| {
                let head = canonical_local_head(&key, &pkg).unwrap_or(key);
                let new_key = rewrite_peer_suffix(&head, &spec_peer_to_hashed);
                pkg.dep_path = new_key.clone();
                pkg.dependencies = pkg
                    .dependencies
                    .into_iter()
                    .map(|(n, v)| (n, rewrite_peer_suffix(&v, &spec_peer_to_hashed)))
                    .collect();
                pkg.optional_dependencies = pkg
                    .optional_dependencies
                    .into_iter()
                    .map(|(n, v)| (n, rewrite_peer_suffix(&v, &spec_peer_to_hashed)))
                    .collect();
                (new_key, pkg)
            })
            .collect();
        packages = rekeyed;
        for deps in importers.values_mut() {
            for dep in deps {
                dep.dep_path = rewrite_peer_suffix(&dep.dep_path, &spec_peer_to_hashed);
            }
        }
    }

    let settings = raw
        .settings
        .map(|s| crate::LockfileSettings {
            auto_install_peers: s.auto_install_peers.unwrap_or(true),
            exclude_links_from_lockfile: s.exclude_links_from_lockfile.unwrap_or(false),
            lockfile_include_tarball_url: s.lockfile_include_tarball_url.unwrap_or(false),
        })
        .unwrap_or_default();

    let times = raw.time.unwrap_or_default();

    let catalogs = raw
        .catalogs
        .unwrap_or_default()
        .into_iter()
        .map(|(name, entries)| {
            let inner = entries
                .into_iter()
                .map(|(pkg, e)| {
                    (
                        pkg,
                        CatalogEntry {
                            specifier: e.specifier,
                            version: e.version,
                        },
                    )
                })
                .collect();
            (name, inner)
        })
        .collect();

    // pnpm models the `patchedDependencies` value as a per-file *hash*,
    // not a path (the current CLI writes a bare hash; older 9.x wrote a
    // `{ hash, path }` object that pnpm migrates to the bare hash on
    // read). So the hash lands in `patched_dependency_hashes`, and the
    // path map stays empty — the install path derives the patch *path*
    // from the manifest/workspace `patchedDependencies` declaration, and
    // drift compares hash-against-hash. (Bun keeps storing a real path
    // in `patched_dependencies`; that reader is unchanged.)
    // The path map stays empty for pnpm regardless of which on-disk
    // form we read — pnpm itself discards the legacy object's `path` on
    // migration (`migratePatchedDependencies` keeps only `.hash`), and
    // the install path derives the patch path from the manifest.
    let patched_dependencies: BTreeMap<String, String> = BTreeMap::new();
    let mut patched_dependency_hashes: BTreeMap<String, String> = BTreeMap::new();
    for (k, v) in raw.patched_dependencies.unwrap_or_default() {
        let (_path, hash) = v.into_path_and_hash();
        patched_dependency_hashes.insert(k, hash);
    }

    // Lift the synthetic runtime importer deps recorded above into
    // typed pins, pulling the per-platform artifact list out of the
    // matching `<name>@runtime:<version>` packages entry. A missing or
    // variant-less packages entry still yields a pin (version intent
    // survives); installs on platforms not covered by the variant list
    // re-resolve against live SHASUMS.
    let mut runtimes = BTreeMap::new();
    for (name, (specifier, version, dev)) in runtime_imports {
        let pkg_info = raw.packages.get(&format!("{name}@runtime:{version}"));
        let variants = pkg_info
            .and_then(|p| p.resolution.as_ref())
            .and_then(|r| r.variants.as_ref())
            .map(|vs| {
                vs.iter()
                    .map(|v| convert_runtime_variant(&name, v))
                    .collect()
            })
            .unwrap_or_default();
        let has_bin = pkg_info.map(|p| p.has_bin).unwrap_or(true);
        runtimes.insert(
            name,
            RuntimePin {
                specifier,
                version,
                dev,
                has_bin,
                variants,
            },
        );
    }

    Ok(LockfileGraph {
        importers,
        packages,
        settings,
        overrides: raw.overrides.unwrap_or_default(),
        package_extensions_checksum: raw.package_extensions_checksum,
        pnpmfile_checksum: raw.pnpmfile_checksum,
        ignored_optional_dependencies: raw
            .ignored_optional_dependencies
            .unwrap_or_default()
            .into_iter()
            .collect(),
        times,
        skipped_optional_dependencies,
        catalogs,
        bun_config_version: None,
        patched_dependencies,
        patched_dependency_hashes,
        trusted_dependencies: Vec::new(),
        runtimes,
        extra_fields: BTreeMap::new(),
        workspace_extra_fields: BTreeMap::new(),
    })
}

/// Convert a raw `variations` variant into the typed graph shape.
/// pnpm's bare-string `bin:` form names a single executable after the
/// runtime itself (`bin: bin/node` on the `node` entry).
fn convert_runtime_variant(runtime_name: &str, raw: &RawRuntimeVariant) -> RuntimeVariant {
    let (bin, bin_is_bare_string) = match &raw.resolution.bin {
        Some(RawBinSpec::Single(path)) => {
            let mut m = BTreeMap::new();
            m.insert(runtime_name.to_string(), path.clone());
            (m, true)
        }
        Some(RawBinSpec::Map(m)) => (m.clone(), false),
        None => (BTreeMap::new(), false),
    };
    RuntimeVariant {
        targets: raw
            .targets
            .iter()
            .map(|t| RuntimeTarget {
                os: t.os.clone(),
                cpu: t.cpu.clone(),
                libc: t.libc.clone(),
            })
            .collect(),
        archive: raw
            .resolution
            .archive
            .clone()
            .unwrap_or_else(|| "tarball".to_string()),
        url: raw.resolution.url.clone(),
        integrity: raw.resolution.integrity.clone().unwrap_or_default(),
        bin,
        bin_is_bare_string,
        prefix: raw.resolution.prefix.clone(),
    }
}

fn resolution_requires_integrity(
    resolution: Option<&super::raw::Resolution>,
    local_source: &Option<LocalSource>,
) -> bool {
    let Some(resolution) = resolution else {
        return false;
    };
    if resolution.integrity.is_some() || resolution.git_hosted {
        return false;
    }
    match local_source {
        Some(LocalSource::Tarball(_))
        | Some(LocalSource::Directory(_))
        | Some(LocalSource::Link(_))
        | Some(LocalSource::Portal(_))
        | Some(LocalSource::Git(_))
        | Some(LocalSource::Exec(_)) => false,
        Some(LocalSource::RemoteTarball(t)) => {
            !t.git_hosted && !super::tarball_url_is_hosted_git(&t.url)
        }
        None => resolution
            .tarball
            .as_deref()
            .is_some_and(|t| is_http_url(t) && !super::tarball_url_is_hosted_git(t)),
    }
}

fn is_http_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

fn git_commit_from_dep_path_version(version: &str) -> Option<&str> {
    let (_, fragment) = version.rsplit_once('#')?;
    let commit = fragment.split('&').next().unwrap_or(fragment);
    if commit.len() == 40 && commit.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(commit)
    } else {
        None
    }
}
