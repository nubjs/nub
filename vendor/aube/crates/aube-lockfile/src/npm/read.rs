use crate::{DepType, DirectDep, Error, LocalSource, LockedPackage, LockfileGraph};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use super::raw::{
    InstallPathInfo, RawNpmLegacyDep, RawNpmLegacyLockfile, RawNpmLockfile, RawNpmPackage,
};
/// Parse a package-lock.json or npm-shrinkwrap.json file into a LockfileGraph.
///
/// `manifest` is consulted only on the legacy (`lockfileVersion < 2` or
/// absent) path, where the lockfile carries no `""` root entry marking
/// which packages are direct deps — those come from package.json, the
/// same way the yarn-classic reader recovers importers. The v2/v3 path
/// ignores it.
pub fn parse(path: &Path, manifest: &aube_manifest::PackageJson) -> Result<LockfileGraph, Error> {
    let content = crate::read_lockfile(path)?;
    let mut raw: RawNpmLockfile = crate::parse_json(path, content)?;

    // Pre-npm-7 lockfiles — `package-lock.json` `lockfileVersion 1`
    // (npm 5/6) and pre-2017 `npm-shrinkwrap.json` (no `lockfileVersion`,
    // hence the `unwrap_or(1)` default) — use a nested `dependencies`
    // tree and have no flat `packages` map. Lift either into the same
    // flat install-path-keyed representation the v2/v3 reader below
    // consumes, then fall through unchanged. This is the whole legacy
    // story: one pre-pass, the battle-tested nested-resolution / hoist /
    // importer pipeline reused verbatim. Re-reading the file on this
    // rare path keeps the common v2/v3 path a single allocation-free
    // parse.
    if raw.lockfile_version.unwrap_or(1) < 2 {
        let content = crate::read_lockfile(path)?;
        let legacy: RawNpmLegacyLockfile = crate::parse_json(path, content)?;
        raw = lift_legacy_to_packages(&legacy, manifest);
    }

    // `npm install --prefix <proj>` (run from a different cwd than the
    // project) writes every `packages` key — and every `link.resolved`
    // target — as a path that *climbs out* of npm's cwd back to the
    // project: `../../../abs/path/to/proj/node_modules/debug` instead of
    // the canonical project-relative `node_modules/debug`. The whole
    // reader keys off the canonical form (`resolve_nested`,
    // `package_name_from_install_path`, the `node_modules/<name>` root
    // lookups), so the climb prefix made root direct deps resolve to
    // nothing: importers came out empty (every direct-dep specifier and
    // every hoist-tree root vanished), which then produced a pnpm-lock
    // with an empty importer `specifiers:` map and a bun.lock with
    // `"packages": {}`. Normalize each key/target down to its canonical
    // project-relative form (everything from the first `node_modules/`
    // segment) up front so the rest of the reader is climb-prefix-blind.
    normalize_install_path_prefixes(&mut raw);

    let mut graph = LockfileGraph {
        importers: BTreeMap::new(),
        packages: BTreeMap::new(),
        ..Default::default()
    };

    // npm does not tag remote-tarball package entries separately from
    // registry packages: both use an HTTP(S) `resolved` URL. The declared
    // dependency specifier is the discriminator. Collect every URL spec so
    // matching entries retain their non-registry identity instead of later
    // being sent through packument validation.
    let remote_tarball_specs: BTreeSet<&str> = raw
        .packages
        .values()
        .flat_map(|entry| {
            entry
                .dependencies
                .values()
                .chain(entry.dev_dependencies.values())
                .chain(entry.optional_dependencies.values())
        })
        .map(String::as_str)
        .filter(|spec| LocalSource::looks_like_remote_tarball_url(spec))
        .collect();

    // npm workspace links come in pairs:
    // - `node_modules/@scope/pkg: { resolved: "packages/pkg", link: true }`
    // - `packages/pkg: { name, version, dependencies, ... }`
    //
    // The `node_modules/` entry is the actual edge consumers resolve through;
    // the target path entry carries the package metadata. Skip the target-path
    // record during the main loop and let the link entry synthesize a local
    // package from it.
    let link_targets: BTreeSet<String> = raw
        .packages
        .values()
        .filter_map(|entry| entry.link.then(|| entry.resolved.clone()).flatten())
        .collect();

    // Map each install_path to the locked dep_path it resolves to. We need
    // this for the nested-resolution walk, including local/workspace links
    // whose dep_path isn't just `name@version`.
    let mut install_path_info: BTreeMap<String, InstallPathInfo> = BTreeMap::new();

    for (install_path, entry) in &raw.packages {
        if install_path.is_empty() {
            continue; // root project, handled separately
        }
        if link_targets.contains(install_path) {
            continue;
        }

        // The install-path segment is what every other package in the
        // tree refers to. For non-aliased deps that's the real package
        // name; for `"h3-v2": "npm:h3@..."` it's the alias `h3-v2`.
        // Keep it as the LockedPackage.name so the linker drops the
        // dep into `node_modules/<alias>/` and transitive symlinks
        // resolve by the string that appears in consumers'
        // `dependencies` maps.
        let install_name = crate::npm::layout::package_name_from_install_path(install_path)
            .or_else(|| entry.name.clone())
            .ok_or_else(|| {
                Error::parse(
                    path,
                    format!("could not determine package name for '{install_path}'"),
                )
            })?;
        // npm writes `name:` only for aliases. If present and different
        // from the install-path segment, this is `"<alias>": "npm:<real>@..."`
        // and the real name is what we hit the registry with. If absent
        // or equal, it's a regular dep.
        let alias_of = entry
            .name
            .as_ref()
            .filter(|real| real.as_str() != install_name.as_str())
            .cloned();
        let (package_entry, version, dep_path, local_source) = if entry.link {
            let target = entry.resolved.as_ref().ok_or_else(|| {
                Error::parse(
                    path,
                    format!("linked package '{install_name}' has no resolved target"),
                )
            })?;
            let target_entry = raw.packages.get(target).ok_or_else(|| {
                Error::parse(
                    path,
                    format!("linked package '{install_name}' points to missing target '{target}'"),
                )
            })?;
            let version = target_entry.version.clone().ok_or_else(|| {
                Error::parse(
                    path,
                    format!("linked package '{install_name}' target '{target}' has no version"),
                )
            })?;
            let local = LocalSource::Link(PathBuf::from(target));
            (
                target_entry,
                version,
                local.dep_path(&install_name),
                Some(local),
            )
        } else {
            let version = entry.version.clone().ok_or_else(|| {
                Error::parse(path, format!("package '{install_name}' has no version"))
            })?;
            let local_source = entry.resolved.as_deref().and_then(|r| {
                crate::npm::source::local_git_source_from_resolved(r)
                    .or_else(|| crate::npm::source::local_file_source_from_resolved(r))
                    .or_else(|| {
                        remote_tarball_specs.contains(r).then(|| {
                            LocalSource::RemoteTarball(crate::RemoteTarballSource {
                                url: r.to_string(),
                                integrity: entry.integrity.clone().unwrap_or_default(),
                                git_hosted: false,
                            })
                        })
                    })
            });
            let dep_path = local_source.as_ref().map_or_else(
                || format!("{install_name}@{version}"),
                |l| l.dep_path(&install_name),
            );
            (entry, version.clone(), dep_path, local_source)
        };
        install_path_info.insert(
            install_path.clone(),
            InstallPathInfo {
                name: install_name.clone(),
                dep_path: dep_path.clone(),
            },
        );

        // Same (name, version) may appear at multiple nest levels; keep the first occurrence.
        if graph.packages.contains_key(&dep_path) {
            continue;
        }

        let mut deps: BTreeMap<String, String> = BTreeMap::new();
        for dep_name in package_entry
            .dependencies
            .keys()
            .chain(package_entry.optional_dependencies.keys())
        {
            // Forward references — we'll resolve them in a second pass using
            // the node nested-resolution walk.
            deps.insert(dep_name.clone(), String::new());
        }
        // Preserve the declared ranges npm writes on each nested package
        // entry. Round-tripping these is what keeps
        // `aube install --no-frozen-lockfile` from rewriting every
        // `"^4.1.0"` to `"4.3.0"` on re-emit.
        let mut declared: BTreeMap<String, String> = BTreeMap::new();
        for (k, v) in package_entry
            .dependencies
            .iter()
            .chain(package_entry.optional_dependencies.iter())
        {
            declared.insert(k.clone(), v.clone());
        }

        // Keep the `resolved` URL on every registry package so the
        // npm writer can emit `resolved:` on every entry verbatim
        // (what npm itself writes), not just the aliased /
        // JSR-specific cases where the URL is strictly unrecoverable
        // from name+version. Dropping it was the single largest
        // source of churn against npm's own output.
        let tarball_url = package_entry
            .resolved
            .as_ref()
            .filter(|_| local_source.is_none())
            .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
            .cloned();

        // Peer fields are copied verbatim from the lockfile entry.
        // Downstream (`aube-resolver::apply_peer_contexts`) reads
        // these two maps to decide which packages need a peer-context
        // suffix and which sibling symlinks to create in the isolated
        // virtual store. An npm lockfile without these fields
        // populated here would silently produce a tree where
        // peer-dependent packages can't find their peers at runtime.
        let peer_dependencies = package_entry.peer_dependencies.clone();
        let peer_dependencies_meta: BTreeMap<String, crate::PeerDepMeta> = package_entry
            .peer_dependencies_meta
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    crate::PeerDepMeta {
                        optional: v.optional,
                    },
                )
            })
            .collect();

        graph.packages.insert(
            dep_path.clone(),
            LockedPackage {
                name: install_name,
                version,
                integrity: package_entry.integrity.clone(),
                dependencies: deps,
                peer_dependencies,
                peer_dependencies_meta,
                dep_path,
                local_source,
                os: package_entry.os.iter().cloned().collect(),
                cpu: package_entry.cpu.iter().cloned().collect(),
                libc: package_entry.libc.iter().cloned().collect(),
                alias_of,
                tarball_url,
                declared_dependencies: declared,
                engines: package_entry.engines.clone(),
                bin: package_entry.bin.clone(),
                license: package_entry.license.as_ref().and_then(|l| l.value.clone()),
                funding_url: package_entry.funding.as_ref().and_then(|f| f.url.clone()),
                has_install_script: package_entry.has_install_script,
                has_shrinkwrap: package_entry.has_shrinkwrap,
                in_bundle: package_entry.in_bundle,
                deprecated: package_entry.deprecated.clone(),
                bundled_dependencies: package_entry.bundle_dependencies.clone(),
                ..Default::default()
            },
        );
    }

    // Second pass: for each raw entry, resolve its transitive deps by walking
    // the npm nesting hierarchy. For an entry at `node_modules/foo`, a dep
    // `bar` resolves to whichever of `node_modules/foo/node_modules/bar` or
    // `node_modules/bar` exists — npm hoists shared versions to the root but
    // keeps conflicting versions nested.
    //
    // We then write the resolved (name → dep_path tail) back onto the
    // LockedPackage keyed by the *first* dep_path (name@version) we
    // stored. The map value is the substring that follows `<name>@` in
    // the target dep_path (just the version for simple packages), per
    // `LockedPackage.dependencies` doc — the linker recombines the
    // name and tail with an `@` separator when walking siblings.
    // Emitting the full dep_path here doubled the name and produced
    // broken sibling symlinks like `rolldown@rolldown@1.0.0` for every
    // transitive dep. This may lose fidelity if two entries share
    // (name, version) but have different resolved transitives —
    // npm.rs's data model doesn't express that, and in practice npm
    // dedupes only when the transitives match anyway.
    type ResolvedDepMap = BTreeMap<String, String>;
    let mut resolved_by_dep_path: BTreeMap<String, (ResolvedDepMap, ResolvedDepMap)> =
        BTreeMap::new();
    for (install_path, entry) in &raw.packages {
        if install_path.is_empty() {
            continue;
        }
        if link_targets.contains(install_path) {
            continue;
        }
        let Some(info) = install_path_info.get(install_path) else {
            continue;
        };
        let package_entry = if entry.link {
            let Some(target) = entry.resolved.as_ref() else {
                continue;
            };
            let Some(target_entry) = raw.packages.get(target) else {
                unreachable!("first pass validates that linked package target '{target}' exists");
            };
            target_entry
        } else {
            entry
        };
        let dep_path = info.dep_path.clone();
        let lookup_path = if entry.link {
            entry.resolved.as_deref().unwrap_or(install_path.as_str())
        } else {
            install_path.as_str()
        };

        // Skip if another occurrence already produced a resolution for this
        // dep_path (first wins, matching how we built `graph.packages`).
        if resolved_by_dep_path.contains_key(&dep_path) {
            continue;
        }

        let mut resolved: BTreeMap<String, String> = BTreeMap::new();
        let mut resolved_optional: BTreeMap<String, String> = BTreeMap::new();
        for (dep_name, is_optional) in package_entry
            .dependencies
            .keys()
            .map(|name| (name, false))
            .chain(
                package_entry
                    .optional_dependencies
                    .keys()
                    .map(|name| (name, true)),
            )
        {
            if let Some(target_install_path) =
                crate::npm::layout::resolve_nested(lookup_path, dep_name, &install_path_info)
                && let Some(target_info) = install_path_info.get(&target_install_path)
            {
                let tail =
                    crate::npm::dep_path_tail(&target_info.name, &target_info.dep_path).to_string();
                resolved.insert(dep_name.clone(), tail.clone());
                if is_optional {
                    resolved_optional.insert(dep_name.clone(), tail);
                }
            }
        }
        resolved_by_dep_path.insert(dep_path, (resolved, resolved_optional));
    }
    for (dep_path, (deps, optional_deps)) in resolved_by_dep_path {
        if let Some(pkg) = graph.packages.get_mut(&dep_path) {
            pkg.dependencies = deps;
            pkg.optional_dependencies = optional_deps;
        }
    }

    // Root importer: resolve direct deps from the "" entry. For root, the
    // only possible install path for `bar` is `node_modules/bar`.
    let root = raw.packages.get("").cloned().unwrap_or_default();

    let mut direct: Vec<DirectDep> = Vec::new();
    // Carry the declared range npm wrote on the root entry's
    // `dependencies`/`devDependencies`/`optionalDependencies` value
    // through to the importer's `specifier`. Without it the pnpm
    // writer emits an empty importer `specifiers:` map and pnpm's
    // frozen install rejects the lockfile with
    // `specifiers in the lockfile don't match package.json` — the same
    // way the non-root workspace importers below already thread it.
    let push_direct =
        |dep_name: &str, specifier: &str, dep_type: DepType, direct: &mut Vec<DirectDep>| {
            let root_path = format!("node_modules/{dep_name}");
            if let Some(info) = install_path_info.get(&root_path) {
                direct.push(DirectDep {
                    name: info.name.clone(),
                    dep_path: info.dep_path.clone(),
                    dep_type,
                    specifier: Some(specifier.to_string()),
                });
            }
        };
    for (dep_name, specifier) in &root.dependencies {
        push_direct(dep_name, specifier, DepType::Production, &mut direct);
    }
    for (dep_name, specifier) in &root.dev_dependencies {
        push_direct(dep_name, specifier, DepType::Dev, &mut direct);
    }
    for (dep_name, specifier) in &root.optional_dependencies {
        push_direct(dep_name, specifier, DepType::Optional, &mut direct);
    }

    // npm symlinks every workspace member (and any other top-level
    // `npm install ../local-pkg` link) into the root `node_modules/`
    // regardless of what the root manifest declares. Each one shows
    // up in the lockfile as `node_modules/<name>: { link: true,
    // resolved: "<rel>" }`. Surface those as direct deps of the
    // root importer so the linker recreates the same symlinks on
    // `aube install`. Without this, builds that resolve workspace
    // packages from the repo root (Angular CLI / Nx / many monorepo
    // build tools) silently break when migrating npm-managed
    // workspaces over to aube — the root `node_modules/<ws-pkg>`
    // entry simply isn't created. Sorted by name for deterministic
    // ordering.
    let already_added: BTreeSet<&str> = direct.iter().map(|d| d.name.as_str()).collect();
    let mut workspace_links: Vec<DirectDep> = Vec::new();
    for (install_path, raw_entry) in &raw.packages {
        if !raw_entry.link {
            continue;
        }
        let Some(rest) = install_path.strip_prefix("node_modules/") else {
            continue;
        };
        // Only consider top-level entries: `node_modules/<name>` or
        // `node_modules/@scope/<name>`. A nested `node_modules/`
        // segment means this is a non-hoisted nested link, not a
        // root symlink.
        if rest.contains("/node_modules/") {
            continue;
        }
        let segments = rest.split('/').count();
        let expected = if rest.starts_with('@') { 2 } else { 1 };
        if segments != expected {
            continue;
        }
        let Some(info) = install_path_info.get(install_path) else {
            continue;
        };
        if already_added.contains(info.name.as_str()) {
            continue;
        }
        workspace_links.push(DirectDep {
            name: info.name.clone(),
            dep_path: info.dep_path.clone(),
            dep_type: DepType::Production,
            specifier: None,
        });
    }
    workspace_links.sort_by(|a, b| a.name.cmp(&b.name));
    direct.extend(workspace_links);

    graph.importers.insert(".".to_string(), direct);

    // Workspace importers: npm records each workspace package twice:
    // `node_modules/<name>` is a link, while the target path (`web`,
    // `packages/app`, ...) carries that package's own dependency sections.
    // Preserve those target paths as graph importers so install/link and a
    // later package-lock rewrite keep each workspace's node_modules tree.
    //
    // But only for targets that are actually MEMBERS. npm writes the very
    // same pair for a local directory dependency — `vendor/local` gets a
    // bare `name`/`version` entry plus a `link: true` record — so taking
    // every link target registered phantom importers for paths outside the
    // workspace. `drift` then compared them against the real member list
    // and failed every `--frozen-lockfile` with `workspace importer
    // vendor/local is in the lockfile but not in the workspace`, including
    // against lockfiles npm itself had written.
    //
    // The shapes are identical in the lockfile — a root importer's own
    // `file:./dep` also produces a root link record — so the manifest's
    // `workspaces` patterns are the only thing that can decide. A
    // non-member still becomes a proper local-source package via the link
    // entry's own synthesis pass above; it just stops pretending to be an
    // importer.
    // The LOCKFILE's own copy comes first: npm mirrors `workspaces` into
    // `packages[""]`, so a lockfile is self-describing and a caller that
    // passes a bare manifest still gets the right answer. The manifest is
    // the fallback for a lockfile written before that field was emitted.
    //
    // When NEITHER carries patterns, keep every link target, exactly as
    // before. That is the conservative direction on purpose: dropping a
    // real member costs it its importer entry and breaks its install,
    // while keeping a stray one only reproduces the pre-existing
    // behaviour. It is also the case for a workspace whose members are
    // declared outside package.json (a `pnpm-workspace.yaml` project
    // converted to npm format), where absence of patterns says nothing
    // about whether members exist.
    let member_patterns: Vec<String> = raw
        .packages
        .get("")
        .and_then(|root| root.workspaces.as_ref())
        .or(manifest.workspaces.as_ref())
        .map(|w| w.patterns().to_vec())
        .unwrap_or_default();
    for target in &link_targets {
        if target.is_empty() {
            continue;
        }
        if !member_patterns.is_empty()
            && !aube_workspace::matches_member_patterns(target, &member_patterns)
        {
            continue;
        }
        let Some(package_entry) = raw.packages.get(target) else {
            continue;
        };
        let mut direct = Vec::new();
        for (dep_name, specifier, dep_type) in package_entry
            .dependencies
            .iter()
            .map(|(name, spec)| (name, spec, DepType::Production))
            .chain(
                package_entry
                    .dev_dependencies
                    .iter()
                    .map(|(name, spec)| (name, spec, DepType::Dev)),
            )
            .chain(
                package_entry
                    .optional_dependencies
                    .iter()
                    .map(|(name, spec)| (name, spec, DepType::Optional)),
            )
        {
            if let Some(target_install_path) =
                crate::npm::layout::resolve_nested(target, dep_name, &install_path_info)
                && let Some(info) = install_path_info.get(&target_install_path)
            {
                direct.push(DirectDep {
                    name: info.name.clone(),
                    dep_path: info.dep_path.clone(),
                    dep_type,
                    specifier: Some(specifier.clone()),
                });
            }
        }
        graph.importers.insert(target.clone(), direct);
    }
    Ok(graph)
}

/// Lift a pre-npm-7 nested-`dependencies` tree into the flat,
/// install-path-keyed `RawNpmLockfile.packages` representation the
/// v2/v3 reader consumes. npm's nesting encodes the install path
/// exactly as the v2/v3 `packages` keys do — a top-level
/// `dependencies.foo` lives at `node_modules/foo`; a nested
/// `dependencies.foo.dependencies.bar` (npm nests only the conflicting
/// version, hoisting the shared one to the top) lives at
/// `node_modules/foo/node_modules/bar` — so the synthesized paths feed
/// `resolve_nested` and the hoist walk with no special-casing.
///
/// v1 lockfiles list every resolved package (direct + hoisted
/// transitive) flat at the top level and carry no `""` root entry
/// marking which are direct, so the root importer's deps are taken from
/// the manifest (the yarn-classic precedent). Reading the manifest is
/// the *correct* source, not a shortcut: mislabeling a hoisted
/// transitive as a root direct dep would over-hoist it into aube's
/// isolated root `node_modules/` and break the phantom-dep guarantee.
pub(super) fn lift_legacy_to_packages(
    legacy: &RawNpmLegacyLockfile,
    manifest: &aube_manifest::PackageJson,
) -> RawNpmLockfile {
    let mut packages: BTreeMap<String, RawNpmPackage> = BTreeMap::new();

    // The `""` root entry: the v2/v3 reader reads the project's direct
    // deps (and their declared ranges, used as importer specifiers) from
    // its `dependencies`/`devDependencies`/`optionalDependencies`
    // sections. Synthesize them from the manifest. name/version are
    // unused by the reader for the root entry, so they stay defaulted.
    packages.insert(
        String::new(),
        RawNpmPackage {
            dependencies: manifest.dependencies.clone(),
            dev_dependencies: manifest.dev_dependencies.clone(),
            optional_dependencies: manifest.optional_dependencies.clone(),
            ..Default::default()
        },
    );

    lift_legacy_tree(&legacy.dependencies, "node_modules", &mut packages);

    // Honesty diagnostic for the pre-npm-5 flat-shrinkwrap limitation
    // (see `legacy_unreferenced_top_level`): a fully-hoisted npm 3/4
    // shrinkwrap omits `requires`, so a top-level transitive whose incoming
    // edge is recorded nowhere is unreachable and the writer's reachability
    // walk drops it — it will not install. Surface it rather than silently
    // shipping a broken tree. Legacy-path-only: never runs for v2/v3.
    let orphans = legacy_unreferenced_top_level(legacy, &packages, manifest);
    if !orphans.is_empty() {
        let list = orphans.join(", ");
        tracing::warn!(
            code = aube_codes::warnings::WARN_AUBE_LOCKFILE_LEGACY_INCOMPLETE_GRAPH,
            "legacy lockfile omits dependency edges for {} hoisted package(s) ({list}); they are unreachable from the project's dependencies and will not be installed. Re-lock with a modern npm for a complete graph.",
            orphans.len(),
        );
    }

    RawNpmLockfile {
        lockfile_version: Some(1),
        packages,
    }
}

/// Top-level legacy packages with NO incoming edge — referenced by neither
/// the project manifest nor any lifted package's recorded dependencies
/// (`requires` plus nesting-derived edges). Such a package is unreachable
/// from the project's dependencies, so the reachability-pruning writer drops
/// it and it never installs. The cause is structural: a fully-hoisted npm
/// 3/4 `npm-shrinkwrap.json` records transitives at the top level but omits
/// the `requires` links that say which package needs them, so the edge
/// exists nowhere in the file and is unrecoverable from the lockfile alone
/// (npm 3 rebuilt it by reading each fetched package.json — a thing a
/// lockfile reader cannot do). Returning the list lets the caller warn.
pub(super) fn legacy_unreferenced_top_level(
    legacy: &RawNpmLegacyLockfile,
    packages: &BTreeMap<String, RawNpmPackage>,
    manifest: &aube_manifest::PackageJson,
) -> Vec<String> {
    let mut referenced: BTreeSet<&str> = BTreeSet::new();
    for k in manifest
        .dependencies
        .keys()
        .chain(manifest.dev_dependencies.keys())
        .chain(manifest.optional_dependencies.keys())
    {
        referenced.insert(k.as_str());
    }
    for (install_path, pkg) in packages {
        if install_path.is_empty() {
            continue;
        }
        for dep_name in pkg.dependencies.keys() {
            referenced.insert(dep_name.as_str());
        }
    }
    legacy
        .dependencies
        .keys()
        .filter(|name| !referenced.contains(name.as_str()))
        .cloned()
        .collect()
}

/// Recursively flatten one level of the legacy `dependencies` tree at
/// install-path `prefix` (`node_modules` for the top level), pushing one
/// `RawNpmPackage` per entry and recursing into nested `dependencies`
/// one `node_modules` level deeper.
fn lift_legacy_tree(
    deps: &BTreeMap<String, RawNpmLegacyDep>,
    prefix: &str,
    out: &mut BTreeMap<String, RawNpmPackage>,
) {
    for (name, dep) in deps {
        let install_path = format!("{prefix}/{name}");
        // Edges come from `requires` (declared name → range), which the
        // reader uses to seed forward-refs and preserve declared ranges;
        // the second pass resolves the actual target via the nested-tree
        // walk. Pre-npm-5 shrinkwraps omit `requires` entirely and
        // express edges ONLY through nesting, so a nested child IS a
        // declared dependency of its parent — fold those names in (a `*`
        // range stands in for the unrecorded specifier; the resolved
        // target still comes from the nested-tree walk). A hoisted-to-root
        // transitive of a no-`requires` shrinkwrap has no edge anywhere in
        // the file and stays unrecoverable — a flat-layout limitation
        // documented in the PR.
        let mut dependencies = dep.requires.clone();
        for child_name in dep.dependencies.keys() {
            dependencies
                .entry(child_name.clone())
                .or_insert_with(|| "*".to_string());
        }
        // v1 records bundled deps only as a per-entry `bundled: true`
        // flag on each nested child; it has no parent-level
        // `bundleDependencies` array the way v3 does. The install path
        // keys off the parent's `bundled_dependencies` to avoid fetching
        // the bundled closure (it ships inside the parent's tarball), so
        // reconstruct that array here from the direct `bundled` children
        // — otherwise a v1 restore promotes the whole bundled subtree to
        // registry-fetch nodes and breaks `--offline`.
        let bundle_dependencies: Vec<String> = dep
            .dependencies
            .iter()
            .filter(|(_, child)| child.bundled)
            .map(|(child_name, _)| child_name.clone())
            .collect();
        out.insert(
            install_path.clone(),
            RawNpmPackage {
                version: dep.version.clone(),
                resolved: dep.resolved.clone(),
                integrity: dep.integrity.clone(),
                dependencies,
                in_bundle: dep.bundled,
                bundle_dependencies,
                ..Default::default()
            },
        );
        if !dep.dependencies.is_empty() {
            let child_prefix = format!("{install_path}/node_modules");
            lift_legacy_tree(&dep.dependencies, &child_prefix, out);
        }
    }
}

/// Canonical project-relative form of an npm `packages` install path
/// (or a `link.resolved` target). `--prefix` installs prepend a climb
/// out of npm's cwd back to the project dir
/// (`../../../abs/proj/node_modules/foo`) instead of the canonical
/// project-relative spelling (`node_modules/foo`). Only those outside-root
/// paths are stripped. Workspace-member install paths such as
/// `packages/cli/node_modules/commander` are already project-relative and
/// must be preserved so member-local dependencies do not collapse onto the
/// root hoist slot.
fn canonical_install_path(install_path: &str) -> &str {
    if install_path.starts_with("node_modules/") || !looks_outside_project_prefix(install_path) {
        return install_path;
    }
    match install_path.find("node_modules/") {
        Some(idx) => &install_path[idx..],
        None => install_path,
    }
}

fn looks_outside_project_prefix(path: &str) -> bool {
    path.starts_with("../")
        || path.starts_with('/')
        || path.starts_with("\\\\")
        || path.as_bytes().get(1) == Some(&b':')
}

/// Rewrite every `packages` key and every `link.resolved` target to its
/// canonical project-relative form (see [`canonical_install_path`]) so
/// `--prefix`-written lockfiles read identically to in-directory ones.
/// A no-op for the common case where npm wrote project-relative paths.
fn normalize_install_path_prefixes(raw: &mut super::raw::RawNpmLockfile) {
    let needs_rewrite = raw
        .packages
        .keys()
        .any(|k| canonical_install_path(k) != k.as_str())
        || raw.packages.values().any(|p| {
            p.resolved
                .as_deref()
                .is_some_and(|r| canonical_install_path(r) != r)
        });
    if !needs_rewrite {
        return;
    }

    let old = std::mem::take(&mut raw.packages);
    for (key, mut pkg) in old {
        // Only `link.resolved` is an install-path target that must be
        // canonicalized to match the rewritten keys. A non-link
        // `resolved` is a tarball URL and must be left verbatim.
        if pkg.link
            && let Some(resolved) = pkg.resolved.as_deref()
        {
            let canonical = canonical_install_path(resolved);
            if canonical != resolved {
                pkg.resolved = Some(canonical.to_string());
            }
        }
        let canonical_key = canonical_install_path(&key).to_string();
        // First write wins, mirroring the rest of the reader's
        // dedupe-by-canonical-path behavior.
        raw.packages.entry(canonical_key).or_insert(pkg);
    }
}
