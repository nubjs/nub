use crate::{DepType, DirectDep, Error, LocalSource, LockedPackage, LockfileGraph};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use super::raw::{InstallPathInfo, RawNpmLockfile};
/// Parse a package-lock.json or npm-shrinkwrap.json file into a LockfileGraph.
pub fn parse(path: &Path) -> Result<LockfileGraph, Error> {
    let content = crate::read_lockfile(path)?;
    let mut raw: RawNpmLockfile = crate::parse_json(path, content)?;

    if raw.lockfile_version < 2 {
        return Err(Error::parse(
            path,
            format!(
                "package-lock.json lockfileVersion {} is not supported (need v2 or v3)",
                raw.lockfile_version
            ),
        ));
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
    for target in &link_targets {
        if target.is_empty() {
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
