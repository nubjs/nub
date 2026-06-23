use super::dep_path::{
    dep_path_tail, parse_dep_path, peerless_dep_path, rewrite_peer_suffix, version_to_dep_path,
};
use super::format::reformat_for_pnpm_parity;
use crate::{DepType, Error, LocalSource, LockfileGraph};
use aube_manifest::PackageJson;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;

/// Serialized form of one `patchedDependencies:` entry. pnpm 9+ writes
/// a `{ hash, path }` mapping (`is-odd@3.0.1:\n  hash: dcac…\n  path:
/// patches/is-odd@3.0.1.patch`) and a frozen install compares the whole
/// object against the resolved manifest/workspace declaration — a bare
/// hash scalar or a hash-only object is rejected with
/// `ERR_PNPM_LOCKFILE_CONFIG_MISMATCH`. So we emit the full object
/// whenever we know the path. `HashOnly` is the degenerate fallback for
/// a graph parsed from a pnpm lockfile (which discards the path) and
/// re-emitted without re-resolving the patch from disk; a real install
/// always resolves the path first via `record_patches_on_graph`, so the
/// `{ hash, path }` form is what ships. `PathOnly` covers a hash-less
/// selector (a bun.lock conversion that never computed the sha256).
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum WritablePatchedDependency {
    HashAndPath { hash: String, path: String },
    HashOnly { hash: String },
    PathOnly(String),
}

/// Stamp pnpm's `(patch_hash=<hash>)` marker onto a dep-path tail or
/// snapshots key, in the position pnpm uses: immediately after the
/// version, before any peer-context suffix
/// (`6.1.0(patch_hash=…)(react@18.2.0)`). Idempotent — a tail parsed
/// from a lockfile that already carries the marker passes through
/// unchanged.
fn with_patch_hash(tail: &str, hash: &str) -> String {
    if tail.contains("(patch_hash=") {
        return tail.to_string();
    }
    let insert_at = tail.find('(').unwrap_or(tail.len());
    format!(
        "{}(patch_hash={hash}){}",
        &tail[..insert_at],
        &tail[insert_at..]
    )
}

/// Write a LockfileGraph as pnpm-lock.yaml v9 format.
pub fn write(path: &Path, graph: &LockfileGraph, manifest: &PackageJson) -> Result<(), Error> {
    let native_pnpm_aliases = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "pnpm-lock.yaml");
    // pnpm never records workspace members in `packages:`/`snapshots:`;
    // the consuming importer carries `version: link:<dir relative to
    // that importer>` with the manifest's `workspace:` specifier
    // preserved. The resolver's workspace-link path records such deps
    // with a registry-style `name@version` dep_path and no package
    // entry, so recover each member's directory from the sibling
    // importers' manifests (read best-effort from disk, same pattern
    // as the bun writer). Key: `name@version` → importer path.
    let project_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut workspace_member_dirs: BTreeMap<String, String> = BTreeMap::new();
    for importer_path in graph.importers.keys() {
        if importer_path == "." {
            continue;
        }
        let pj_path = project_dir.join(importer_path).join("package.json");
        let pj = PackageJson::from_path(&pj_path).unwrap_or_default();
        if let (Some(name), Some(version)) = (pj.name, pj.version) {
            workspace_member_dirs.insert(format!("{name}@{version}"), importer_path.clone());
        }
    }
    // pnpm 10 stamps every patched package's dep path with a
    // `(patch_hash=<sha256-hex>)` suffix — on the importer's resolved
    // version, on the `snapshots:` key, and on dependents' dependency
    // tails — while the `packages:` key stays the clean `name@version`.
    // Resolve which dep paths are patched once, up front. Selector
    // matching is by display `name@version` (the same key the
    // `patchedDependencies` config uses); registry packages only —
    // pnpm's patch workflow targets registry versions, and git/file
    // sources don't carry a comparable version selector. Entries
    // without a recorded hash (bun.lock conversions, pnpm v8 lockfiles)
    // get no suffix, matching the hash-less bare-path block we emit
    // for them.
    let patched_by_dep_path: BTreeMap<&str, &str> = graph
        .packages
        .iter()
        .filter(|(_, pkg)| pkg.local_source.is_none())
        .filter_map(|(dep_path, pkg)| {
            let selector = format!("{}@{}", pkg.name, pkg.version);
            graph
                .patched_dependency_hashes
                .get(&selector)
                .filter(|_| graph.patched_dependencies.contains_key(&selector))
                .map(|hash| (dep_path.as_str(), hash.as_str()))
        })
        .collect();
    // Translate a *flat* peer reference from aube's internal FS-safe
    // hashed dep_path (`request@url+<hash>` / `request@git+<hash>`) to the
    // resolved spec pnpm writes inside a peer suffix
    // (`request@https://codeload.…/tar.gz/<sha>`). The reference is itself a
    // package key, so a direct lookup yields the target's source. Registry
    // peers aren't in the table under their suffix head (or carry no
    // `local_source`) and return `None`, leaving `react@18.2.0` untouched.
    // Restricted to git / remote-tarball so it stays the exact inverse of
    // the reader's `shared_local_dep_path` pass (which only re-derives those
    // two kinds); `file:` / `link:` peers never occur in practice and a
    // one-sided translation would break the round-trip.
    let peer_suffix_to_spec = |head: &str| -> Option<String> {
        let pkg = graph.packages.get(head)?;
        match pkg.local_source.as_ref()? {
            local @ (LocalSource::Git(_) | LocalSource::RemoteTarball(_)) => {
                Some(format!("{}@{}", pkg.name, local.specifier()))
            }
            _ => None,
        }
    };
    let mut importers = BTreeMap::new();
    let exclude_links = graph.settings.exclude_links_from_lockfile;
    for (importer_path, deps) in &graph.importers {
        let mut importer = WritableImporter::default();

        for dep in deps {
            // `excludeLinksFromLockfile: true` drops `link:` entries
            // from importer dep maps so a sibling-workspace symlink
            // change doesn't churn the lockfile. We check the package
            // table rather than `dep.specifier` because the importer's
            // DirectDep only carries the manifest-written range, not
            // the resolved source kind — the LocalSource lives on the
            // LockedPackage the dep_path points to.
            // Workspace-linked deps carry no package entry — match
            // them by `name@version` against the sibling importers.
            // Their importer line is `link:`-shaped like any other
            // link, so `excludeLinksFromLockfile: true` drops them
            // too (pnpm parity).
            let workspace_link_dir = (!graph.packages.contains_key(&dep.dep_path))
                .then(|| workspace_member_dirs.get(&dep.dep_path))
                .flatten();
            // npm symlinks every workspace member into the root
            // `node_modules/` and the npm reader surfaces those links as
            // direct deps of the `.` importer so the linker recreates the
            // symlinks. But pnpm never records a workspace member as a
            // `link:` dependency of the root unless the root manifest
            // actually declares it — members are importers, not deps of
            // the root. Emitting the phantom `link:` deps makes the root
            // importer's specifiers diverge from the root package.json,
            // so pnpm's frozen check rejects the converted lockfile with
            // ERR_PNPM_OUTDATED_LOCKFILE. A member shows up two ways: as
            // a `workspace_link_dir` match (no package entry) or, on the
            // npm-reader path, as a `LocalSource::Link` package whose
            // target directory is itself a sibling importer. Drop either
            // form on the root unless the root manifest declares it.
            let links_to_sibling_importer = graph
                .packages
                .get(&dep.dep_path)
                .and_then(|p| p.local_source.as_ref())
                .and_then(|src| match src {
                    LocalSource::Link(target) => target.to_str(),
                    _ => None,
                })
                .is_some_and(|target| graph.importers.contains_key(target));
            if importer_path == "."
                && (workspace_link_dir.is_some() || links_to_sibling_importer)
                && !manifest.dependencies.contains_key(&dep.name)
                && !manifest.dev_dependencies.contains_key(&dep.name)
                && !manifest.optional_dependencies.contains_key(&dep.name)
            {
                continue;
            }
            if exclude_links
                && (workspace_link_dir.is_some()
                    || matches!(
                        graph
                            .packages
                            .get(&dep.dep_path)
                            .and_then(|p| p.local_source.as_ref()),
                        Some(LocalSource::Link(_))
                    ))
            {
                continue;
            }
            // Specifier sources, in priority order:
            //   1. The specifier recorded on the DirectDep. For workspace
            //      importers this is the only manifest-local specifier the
            //      writer has, because `manifest` is the root package.json.
            //      Hoisted auto-installed peers also use this path.
            //   2. The root manifest entry for old hand-built graphs that
            //      omitted DirectDep.specifier.
            //   3. Fall back to `*` as a last resort.
            let root_manifest_specifier = (importer_path == ".")
                .then(|| match dep.dep_type {
                    DepType::Production => manifest.dependencies.get(&dep.name),
                    DepType::Dev => manifest.dev_dependencies.get(&dep.name),
                    DepType::Optional => manifest.optional_dependencies.get(&dep.name),
                })
                .flatten()
                .map(|s| s.as_str());
            let specifier = dep
                .specifier
                .as_deref()
                .or(root_manifest_specifier)
                .unwrap_or("*");

            // Local deps render with the canonical `file:<path>` /
            // `link:<path>` specifier, not the FS-safe encoded form
            // that lives in `dep_path`.
            let version = if let Some(local) = graph
                .packages
                .get(&dep.dep_path)
                .and_then(|p| p.local_source.as_ref())
            {
                // `link:` paths are stored project-root-relative on
                // the graph (the parser rebases them); pnpm renders
                // them relative to the consuming importer
                // (`link:../core` from `packages/app`). `file:` paths
                // stay root-relative — that's pnpm's own shape.
                match local {
                    LocalSource::Link(_) if importer_path != "." => {
                        format!(
                            "link:{}",
                            link_from_importer(importer_path, &local.path_posix())
                        )
                    }
                    _ => local.specifier(),
                }
            } else if let Some(member_dir) = workspace_link_dir {
                format!("link:{}", link_from_importer(importer_path, member_dir))
            } else if native_pnpm_aliases
                && let Some(pkg) = graph.packages.get(&dep.dep_path)
                && let Some(real_name) = pkg.alias_of.as_deref()
            {
                format!("{real_name}@{}", dep_path_tail(&dep.dep_path, &dep.name))
            } else {
                // Registry dep: the tail may carry a `(git/tarball@hash)`
                // peer suffix that must render as the resolved spec.
                rewrite_peer_suffix(
                    dep.dep_path
                        .strip_prefix(&format!("{}@", dep.name))
                        .unwrap_or(&dep.dep_path),
                    &peer_suffix_to_spec,
                )
            };
            let version = match patched_by_dep_path.get(dep.dep_path.as_str()) {
                Some(hash) => with_patch_hash(&version, hash),
                None => version,
            };

            let spec = WritableDepSpec {
                specifier: specifier.to_string(),
                version,
            };

            match dep.dep_type {
                DepType::Production => {
                    importer
                        .dependencies
                        .get_or_insert_with(BTreeMap::new)
                        .insert(dep.name.clone(), spec);
                }
                DepType::Dev => {
                    importer
                        .dev_dependencies
                        .get_or_insert_with(BTreeMap::new)
                        .insert(dep.name.clone(), spec);
                }
                DepType::Optional => {
                    importer
                        .optional_dependencies
                        .get_or_insert_with(BTreeMap::new)
                        .insert(dep.name.clone(), spec);
                }
            }
        }

        // Runtime pins render as synthetic deps on the root importer
        // (pnpm 10.14+ shape): `node: {specifier: runtime:^24.4.0,
        // version: runtime:24.4.1}`. Only the root carries them — the
        // pin comes from the root manifest's devEngines.
        if importer_path == "." {
            for (name, pin) in &graph.runtimes {
                let spec = WritableDepSpec {
                    specifier: format!("runtime:{}", pin.specifier),
                    version: format!("runtime:{}", pin.version),
                };
                let slot = if pin.dev {
                    importer.dev_dependencies.get_or_insert_with(BTreeMap::new)
                } else {
                    importer.dependencies.get_or_insert_with(BTreeMap::new)
                };
                slot.insert(name.clone(), spec);
            }
        }

        if let Some(skipped) = graph.skipped_optional_dependencies.get(importer_path)
            && !skipped.is_empty()
        {
            let mut map: BTreeMap<String, WritableDepSpec> = BTreeMap::new();
            for (name, specifier) in skipped {
                map.insert(
                    name.clone(),
                    WritableDepSpec {
                        specifier: specifier.clone(),
                        // No installed version on this platform — use a
                        // sentinel that's still parseable as a dep_path
                        // tail by `parse_dep_path` if older code happens
                        // to walk it.
                        version: "0.0.0".to_string(),
                    },
                );
            }
            importer.skipped_optional_dependencies = Some(map);
        }

        importers.insert(importer_path.clone(), importer);
    }

    // pnpm v9 splits the lockfile into two sections:
    //   `packages:` — keyed by the canonical `name@version` (no peer suffix),
    //                 holds the integrity hash and declared peer deps. The
    //                 same package-version with two different peer contexts
    //                 dedupes to a single entry here.
    //   `snapshots:` — keyed by the full contextualized dep_path including
    //                  any `(peer@ver)` suffix, holds the resolved
    //                  `dependencies:` map that the linker walks.
    //
    // We dedupe the packages map via BTreeMap::insert so repeated canonical
    // keys (one per peer context) collapse cleanly, and we take the last
    // writer's integrity/peer decls — they should all agree because they
    // come from the same canonical package.
    let mut packages = BTreeMap::new();
    for pkg in graph.packages.values() {
        // Local deps use the canonical specifier in their key (e.g.
        // `foo@file:./vendor/foo`) so pnpm can read the lockfile.
        // `link:` deps are omitted from the packages section entirely,
        // matching pnpm. `exec:` has no pnpm resolution analogue, so
        // keep it out too instead of writing a package key with no
        // resolution block.
        // Non-registry transitive entries (github overrides, remote
        // tarballs fetched by URL) keep the URL in their dep-path key
        // and carry the real semver on `pkg.version`. `tarball_url`
        // carries the URL through the graph — when the dep-path's
        // version segment is that same URL, the entry was parsed from
        // a URL-keyed pnpm snapshot and needs to round-trip under the
        // same URL key. Paired with the parser's `version_is_http_url
        // && tarball_url.is_some()` gate.
        let url_keyed = pkg
            .tarball_url
            .as_ref()
            .is_some_and(|url| parse_dep_path(&pkg.dep_path).is_some_and(|(_, v)| v == *url));
        let canonical = match pkg.local_source.as_ref() {
            Some(LocalSource::Link(_)) | Some(LocalSource::Exec(_)) => continue,
            Some(local) => format!("{}@{}", pkg.name, local.specifier()),
            None => {
                if native_pnpm_aliases && let Some(real_name) = pkg.alias_of.as_deref() {
                    version_to_dep_path(real_name, &pkg.version)
                } else if url_keyed {
                    // Strip any peer suffix; the packages section keys the
                    // canonical form (no peer contexts), the snapshots
                    // section keys the full dep_path.
                    let (name, version) = parse_dep_path(&pkg.dep_path)
                        .unwrap_or_else(|| (pkg.name.clone(), pkg.version.clone()));
                    format!("{name}@{version}")
                } else {
                    version_to_dep_path(&pkg.name, &pkg.version)
                }
            }
        };
        // pnpm records a `peerDependencies: { x: '*' }` entry for every
        // `peerDependenciesMeta` key a package declares without an explicit
        // range (the classic case is debug's optional `supports-color`,
        // shipped only under `peerDependenciesMeta`). `LockedPackage`'s
        // helper folds those `*` ranges in so the packages entry matches
        // pnpm byte-for-byte; doing it at write time keeps the optional
        // peer out of peer-context resolution (which would bind it to an
        // unrelated copy in the tree and grow spurious dep-path suffixes).
        let peer_deps = {
            let deps = pkg.peer_dependencies_with_meta_defaults();
            if deps.is_empty() { None } else { Some(deps) }
        };
        let peer_meta = if pkg.peer_dependencies_meta.is_empty() {
            None
        } else {
            Some(
                pkg.peer_dependencies_meta
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.clone(),
                            WritablePeerDepMeta {
                                optional: v.optional,
                            },
                        )
                    })
                    .collect(),
            )
        };
        // Always render the path through `path_posix()` so the
        // lockfile uses forward slashes regardless of the host OS —
        // a lockfile written on Windows must resolve identically on
        // Unix and vice versa. `Path::display()` honors the host
        // separator, so it would leak `\` into the YAML.
        let is_jsr_registry_pkg = pkg.registry_name().starts_with("@jsr/");
        let preserve_tarball_url = graph.settings.lockfile_include_tarball_url
            || is_jsr_registry_pkg
            || registry_tarball_url_is_not_derivable(
                pkg.registry_name(),
                &pkg.version,
                pkg.tarball_url.as_deref(),
            );
        debug_assert!(
            !is_jsr_registry_pkg || pkg.tarball_url.is_some(),
            "JSR packages must preserve dist.tarball for cold lockfile installs"
        );
        let resolution = match pkg.local_source.as_ref() {
            Some(local @ LocalSource::Directory(_)) => Some(WritableResolution {
                integrity: None,
                git_hosted: false,
                directory: Some(local.path_posix()),
                tarball: None,
                commit: None,
                repo: None,
                type_: Some("directory".to_string()),
                path: None,
                variants: None,
            }),
            Some(local @ LocalSource::Tarball(_)) => Some(WritableResolution {
                integrity: None,
                git_hosted: false,
                directory: None,
                tarball: Some(format!("file:{}", local.path_posix())),
                commit: None,
                repo: None,
                type_: None,
                path: None,
                variants: None,
            }),
            Some(LocalSource::Link(_)) | Some(LocalSource::Exec(_)) => None,
            Some(local @ LocalSource::Portal(_)) => Some(WritableResolution {
                integrity: None,
                git_hosted: false,
                directory: Some(local.path_posix()),
                tarball: None,
                commit: None,
                repo: None,
                type_: Some("directory".to_string()),
                path: None,
                variants: None,
            }),
            Some(LocalSource::Git(g)) => Some(WritableResolution {
                integrity: g.integrity.clone().or_else(|| pkg.integrity.clone()),
                git_hosted: crate::parse_hosted_git(&g.url).is_some(),
                directory: None,
                tarball: None,
                commit: Some(g.resolved.clone()),
                repo: Some(g.url.clone()),
                type_: Some("git".to_string()),
                // pnpm v9 emits `path: /<sub>` (with leading `/`) on
                // the resolution block when a git dep was installed
                // with a `&path:/<sub>` selector. Keep the same shape
                // so byte-identical round-trips survive.
                path: g.subpath.as_ref().map(|s| format!("/{s}")),
                variants: None,
            }),
            Some(LocalSource::RemoteTarball(t)) => Some(WritableResolution {
                integrity: if t.integrity.is_empty() {
                    None
                } else {
                    Some(t.integrity.clone())
                },
                git_hosted: t.git_hosted || super::tarball_url_is_hosted_git(&t.url),
                directory: None,
                tarball: Some(t.url.clone()),
                commit: None,
                repo: None,
                type_: None,
                path: None,
                variants: None,
            }),
            None if url_keyed => {
                // URL-keyed transitive entries (github overrides, etc.)
                // typically carry no integrity — just the tarball URL
                // in `resolution:`. Gating on `pkg.integrity` would
                // silently drop the tarball on round-trip, and a
                // re-parse would then have no way to fetch the package.
                Some(WritableResolution {
                    integrity: pkg.integrity.clone(),
                    git_hosted: pkg.registry_git_hosted
                        || pkg
                            .tarball_url
                            .as_deref()
                            .is_some_and(super::tarball_url_is_hosted_git),
                    directory: None,
                    tarball: pkg.tarball_url.clone(),
                    commit: None,
                    repo: None,
                    type_: None,
                    path: None,
                    variants: None,
                })
            }
            None if pkg.integrity.is_some() || preserve_tarball_url => Some(WritableResolution {
                integrity: pkg.integrity.clone(),
                git_hosted: pkg.registry_git_hosted
                    || pkg
                        .tarball_url
                        .as_deref()
                        .is_some_and(super::tarball_url_is_hosted_git),
                directory: None,
                // Emit the full registry tarball URL when the setting
                // opts in. JSR packages are the exception: npm.jsr.io
                // uses opaque `dist.tarball` paths that cannot be
                // reconstructed from package name + version, so the
                // URL must be preserved for cold installs from the
                // lockfile.
                tarball: if preserve_tarball_url {
                    pkg.tarball_url.clone()
                } else {
                    None
                },
                commit: None,
                repo: None,
                type_: None,
                path: None,
                variants: None,
            }),
            None => None,
        };
        // Mirror pnpm: emit `version:` alongside the resolution block
        // for URL-keyed transitive entries so tooling that matches
        // packages by (name, version) still has a handle on the real
        // semver. Ordinary registry entries skip this — the key already
        // carries the version, and adding a field would diverge from
        // byte-for-byte pnpm output.
        //
        // Freshly-resolved remote tarballs (codeload hosted-git deps,
        // pkg.pr.new, etc.) key their `packages:` entry by the URL via
        // `specifier()`, but their internal `dep_path` is the hashed
        // `url+<hash>` form, so `url_keyed` is false. pnpm still records
        // the real semver in a `version:` field next to the codeload
        // resolution (`node-expat@https://codeload…: { …, version: 2.4.3 }`),
        // so emit it for `RemoteTarball` too — otherwise a fresh resolve
        // drops the field and drifts from a re-read lockfile (and pnpm).
        let write_version = (url_keyed
            || matches!(pkg.local_source, Some(LocalSource::RemoteTarball(_))))
        .then(|| pkg.version.clone());
        packages.insert(
            canonical,
            WritablePackageInfo {
                resolution,
                version: write_version,
                // pnpm drops every engines entry whose value is exactly
                // `*` and omits the field when nothing survives
                // (updateLockfile.ts: `if (version === '*') continue`).
                // Mirror that so e.g. `engines: {node: '*'}` never lands
                // in the lockfile, while real constraints (including the
                // array-shaped `{'0': node >=0.6.0}` pnpm keeps verbatim)
                // are preserved.
                engines: {
                    let filtered: BTreeMap<String, String> = pkg
                        .engines
                        .iter()
                        .filter(|(_, v)| v.as_str() != "*")
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                    (!filtered.is_empty()).then_some(filtered)
                },
                cpu: pkg.cpu.to_vec(),
                os: pkg.os.to_vec(),
                libc: pkg.libc.to_vec(),
                deprecated: pkg
                    .extra_meta
                    .get("deprecated")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                has_bin: !pkg.bin.is_empty(),
                peer_dependencies: peer_deps,
                peer_dependencies_meta: peer_meta,
                alias_of: (!native_pnpm_aliases)
                    .then(|| pkg.alias_of.clone())
                    .flatten(),
            },
        );
    }

    // Runtime pin packages entries: `node@runtime:24.4.1` with a
    // `variations` resolution carrying one binary artifact per
    // platform (pnpm 10.14+ shape). The matching snapshot entry is
    // empty — runtimes have no dependencies.
    for (name, pin) in &graph.runtimes {
        let variants: Vec<WritableRuntimeVariant> = pin
            .variants
            .iter()
            .map(|v| WritableRuntimeVariant {
                resolution: WritableRuntimeBinaryResolution {
                    archive: v.archive.clone(),
                    bin: if v.bin_is_bare_string && v.bin.len() == 1 {
                        WritableRuntimeBin::Single(
                            v.bin
                                .values()
                                .next()
                                .expect("bin.len() == 1 checked above")
                                .clone(),
                        )
                    } else {
                        WritableRuntimeBin::Map(v.bin.clone())
                    },
                    integrity: if v.integrity.is_empty() {
                        None
                    } else {
                        Some(v.integrity.clone())
                    },
                    prefix: v.prefix.clone(),
                    type_: "binary",
                    url: v.url.clone(),
                },
                targets: v
                    .targets
                    .iter()
                    .map(|t| WritableRuntimeTarget {
                        cpu: t.cpu.clone(),
                        libc: t.libc.clone(),
                        os: t.os.clone(),
                    })
                    .collect(),
            })
            .collect();
        packages.insert(
            format!("{name}@runtime:{}", pin.version),
            WritablePackageInfo {
                resolution: Some(WritableResolution {
                    integrity: None,
                    git_hosted: false,
                    directory: None,
                    tarball: None,
                    commit: None,
                    repo: None,
                    type_: Some("variations".to_string()),
                    path: None,
                    variants: Some(variants),
                }),
                version: Some(pin.version.clone()),
                engines: None,
                cpu: Vec::new(),
                os: Vec::new(),
                libc: Vec::new(),
                deprecated: None,
                has_bin: pin.has_bin,
                peer_dependencies: None,
                peer_dependencies_meta: None,
                alias_of: None,
            },
        );
    }

    // Translate internal dep_path tails (`git+<hash>`, `url+<hash>`,
    // `file+<hash>`) to the specifier form pnpm expects in snapshot
    // dependency maps (`<url>#<sha>` for git, raw URL for tarball,
    // `file:<path>` for local). Registry deps keep their plain semver
    // values. The target package's `local_source` is authoritative:
    // the tail alone doesn't encode the URL.
    let rewrite_local_deps = |deps: BTreeMap<String, String>| -> BTreeMap<String, String> {
        deps.into_iter()
            .map(|(name, value)| {
                let dp = version_to_dep_path(&name, &value);
                let target = graph
                    .packages
                    .get(&dp)
                    .or_else(|| graph.packages.get(&peerless_dep_path(&name, &value)));
                if let Some(target) = target
                    && let Some(ref local) = target.local_source
                    && !matches!(local, LocalSource::Link(_))
                {
                    (name, local.specifier())
                } else if native_pnpm_aliases
                    && let Some(target) = target
                    && let Some(real_name) = target.alias_of.as_deref()
                {
                    (name, format!("{real_name}@{value}"))
                } else if let Some(hash) = patched_by_dep_path.get(dp.as_str()) {
                    // A dependent of a patched package references the
                    // suffixed dep path, same as the snapshots key.
                    (name, with_patch_hash(&value, hash))
                } else {
                    // Registry dep whose value may carry a
                    // `(git/tarball@hash)` peer suffix — render the suffix
                    // as the resolved spec (`1.1.4(request@https://…)`).
                    (name, rewrite_peer_suffix(&value, &peer_suffix_to_spec))
                }
            })
            .collect()
    };
    let mut snapshots = BTreeMap::new();
    for (dep_path, pkg) in &graph.packages {
        // `link:` deps are omitted from snapshots (pnpm parity). `exec:`
        // is omitted for the same reason it is omitted from packages:
        // pnpm has no resolution shape for generated packages.
        // Other local deps use the canonical specifier key so pnpm's
        // parser lines them up with the packages entry above.
        let key = match pkg.local_source.as_ref() {
            Some(LocalSource::Link(_)) | Some(LocalSource::Exec(_)) => continue,
            Some(local) => format!("{}@{}", pkg.name, local.specifier()),
            None => {
                if native_pnpm_aliases && let Some(real_name) = pkg.alias_of.as_deref() {
                    format!("{real_name}@{}", dep_path_tail(dep_path, &pkg.name))
                } else {
                    // Registry snapshot key whose `(git/tarball@hash)` peer
                    // suffix must render as the resolved spec to match pnpm
                    // (`request-promise-core@1.1.4(request@https://…)`).
                    rewrite_peer_suffix(dep_path, &peer_suffix_to_spec)
                }
            }
        };
        let key = match patched_by_dep_path.get(dep_path.as_str()) {
            Some(hash) => with_patch_hash(&key, hash),
            None => key,
        };
        let pkg_deps = rewrite_local_deps(pkg.dependencies.clone());
        let pkg_opt_deps = rewrite_local_deps(pkg.optional_dependencies.clone());
        snapshots.insert(
            key,
            WritableSnapshot {
                dependencies: {
                    let mut deps = pkg_deps;
                    for name in pkg_opt_deps.keys() {
                        deps.remove(name);
                    }
                    if deps.is_empty() { None } else { Some(deps) }
                },
                optional_dependencies: if pkg_opt_deps.is_empty() {
                    None
                } else {
                    Some(pkg_opt_deps)
                },
                transitive_peer_dependencies: if pkg.transitive_peer_dependencies.is_empty() {
                    None
                } else {
                    Some(pkg.transitive_peer_dependencies.clone())
                },
                optional: if pkg.optional { Some(true) } else { None },
                bundled_dependencies: if pkg.bundled_dependencies.is_empty() {
                    None
                } else {
                    Some(pkg.bundled_dependencies.clone())
                },
            },
        );
    }

    // Empty snapshot entries for runtime pins (`node@runtime:24.4.1: {}`),
    // matching pnpm's writer.
    for (name, pin) in &graph.runtimes {
        snapshots.insert(
            format!("{name}@runtime:{}", pin.version),
            WritableSnapshot {
                dependencies: None,
                optional_dependencies: None,
                transitive_peer_dependencies: None,
                optional: None,
                bundled_dependencies: None,
            },
        );
    }

    let time = pruned_time_entries(graph, native_pnpm_aliases);

    let catalogs = if graph.catalogs.is_empty() {
        None
    } else {
        Some(
            graph
                .catalogs
                .iter()
                .map(|(name, entries)| {
                    let inner: BTreeMap<String, WritableCatalogEntry> = entries
                        .iter()
                        .map(|(pkg, e)| {
                            (
                                pkg.clone(),
                                WritableCatalogEntry {
                                    specifier: e.specifier.clone(),
                                    version: e.version.clone(),
                                },
                            )
                        })
                        .collect();
                    (name.clone(), inner)
                })
                .collect(),
        )
    };

    let lockfile = WritablePnpmLockfile {
        lockfile_version: "9.0".to_string(),
        settings: WritableSettings {
            auto_install_peers: graph.settings.auto_install_peers,
            exclude_links_from_lockfile: graph.settings.exclude_links_from_lockfile,
            lockfile_include_tarball_url: graph.settings.lockfile_include_tarball_url,
        },
        catalogs,
        // Skipped at serialization time when empty so the YAML stays
        // byte-identical to a no-overrides install.
        overrides: if graph.overrides.is_empty() {
            None
        } else {
            Some(graph.overrides.clone())
        },
        // Already `sha256-`-prefixed (or `None`) on the graph; emitted
        // verbatim. pnpm omits these when absent, and `skip_serializing_if`
        // mirrors that.
        package_extensions_checksum: graph.package_extensions_checksum.clone(),
        pnpmfile_checksum: graph.pnpmfile_checksum.clone(),
        ignored_optional_dependencies: if graph.ignored_optional_dependencies.is_empty() {
            None
        } else {
            Some(
                graph
                    .ignored_optional_dependencies
                    .iter()
                    .cloned()
                    .collect(),
            )
        },
        // pnpm 9+ records each patch as a `{ hash, path }` object, and a
        // frozen install rejects any other shape (a bare hash scalar or a
        // hash-only object) with `ERR_PNPM_LOCKFILE_CONFIG_MISMATCH`. We
        // emit the full object whenever both are known — a real install
        // resolves the path via `record_patches_on_graph` before writing,
        // so this is the form that ships. A hash-only entry (a graph
        // parsed from a pnpm lockfile and re-emitted without re-resolving
        // the patch) keeps just the hash, and a hash-less selector
        // (bun.lock conversion) falls back to the path string. The
        // selector set is the union of the path and hash maps so neither
        // a path-only nor a hash-only graph drops its entries. Skipped
        // when empty to keep parity with no-patch installs.
        patched_dependencies: {
            let selectors: std::collections::BTreeSet<&String> = graph
                .patched_dependencies
                .keys()
                .chain(graph.patched_dependency_hashes.keys())
                .collect();
            if selectors.is_empty() {
                None
            } else {
                Some(
                    selectors
                        .into_iter()
                        .map(|selector| {
                            let hash = graph.patched_dependency_hashes.get(selector);
                            let path = graph.patched_dependencies.get(selector);
                            let entry = match (hash, path) {
                                (Some(hash), Some(path)) => {
                                    WritablePatchedDependency::HashAndPath {
                                        hash: hash.clone(),
                                        path: path.clone(),
                                    }
                                }
                                (Some(hash), None) => WritablePatchedDependency::HashOnly {
                                    hash: hash.clone(),
                                },
                                (None, Some(path)) => {
                                    WritablePatchedDependency::PathOnly(path.clone())
                                }
                                // Selector came from the union of both maps;
                                // it must appear in at least one.
                                (None, None) => unreachable!(
                                    "patched-dependency selector {selector:?} in neither map"
                                ),
                            };
                            (selector.clone(), entry)
                        })
                        .collect(),
                )
            }
        },
        time,
        importers,
        packages,
        snapshots,
    };

    let yaml = yaml_serde::to_string(&lockfile).map_err(|e| Error::parse(path, e.to_string()))?;
    let yaml = reformat_for_pnpm_parity(&yaml);
    // Atomic via tempfile + persist. Crash, Ctrl+C, or AV
    // quarantine during the write used to leave the user with a
    // truncated pnpm-lock.yaml on disk, next install failed to
    // parse and the user thought their lockfile was gone. See
    // atomic_write_lockfile for full rationale.
    crate::atomic_write_lockfile(path, yaml.as_bytes())?;
    Ok(())
}

/// Render a project-root-relative directory as a `link:` target
/// relative to the consuming importer, the way pnpm writes importer
/// `version:` values (`packages/app` → `packages/core` renders as
/// `../core`; the root importer keeps `packages/core` as-is). Pure
/// lexical computation over `/`-separated components — both inputs are
/// normalized root-relative paths, so no filesystem access is needed.
fn link_from_importer(importer_path: &str, target_posix: &str) -> String {
    if importer_path == "." {
        return target_posix.to_string();
    }
    let from: Vec<&str> = importer_path
        .split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .collect();
    let to: Vec<&str> = target_posix
        .split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .collect();
    let common = from
        .iter()
        .zip(to.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let mut parts: Vec<&str> = vec![".."; from.len() - common];
    parts.extend(&to[common..]);
    if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    }
}

fn registry_tarball_url_is_not_derivable(
    name: &str,
    version: &str,
    tarball_url: Option<&str>,
) -> bool {
    let Some(url) = tarball_url else {
        return false;
    };
    let basename = name.rsplit('/').next().unwrap_or(name);
    let expected_suffix = format!("/-/{basename}-{version}.tgz");
    let path_only = url.split_once('?').map_or(url, |(path, _)| path);
    let path_only = path_only
        .split_once('#')
        .map_or(path_only, |(path, _)| path);
    !path_only.ends_with(&expected_suffix)
}

fn pruned_time_entries(
    graph: &LockfileGraph,
    native_pnpm_aliases: bool,
) -> Option<BTreeMap<String, String>> {
    if graph.times.is_empty() {
        return None;
    }

    let mut time = BTreeMap::new();
    for deps in graph.importers.values() {
        for dep in deps {
            let Some(pkg) = graph.packages.get(&dep.dep_path) else {
                tracing::debug!(
                    dep_path = %dep.dep_path,
                    "direct importer dep missing from package table while pruning pnpm time entries"
                );
                continue;
            };
            if pkg.local_source.is_some() {
                continue;
            }
            let name = if native_pnpm_aliases {
                pkg.alias_of.as_deref().unwrap_or(dep.name.as_str())
            } else {
                dep.name.as_str()
            };
            let tail = dep_path_tail(&dep.dep_path, &dep.name);
            let version = tail.split('(').next().unwrap_or(tail);
            let key = version_to_dep_path(name, version);
            let internal_key = version_to_dep_path(&dep.name, version);
            let value = graph
                .times
                .get(&key)
                .or_else(|| graph.times.get(&internal_key))
                .or_else(|| {
                    (!native_pnpm_aliases)
                        .then_some(pkg.alias_of.as_deref())
                        .flatten()
                        .and_then(|real_name| {
                            graph.times.get(&version_to_dep_path(real_name, version))
                        })
                });
            if let Some(value) = value {
                time.insert(key, value.clone());
            }
        }
    }

    (!time.is_empty()).then_some(time)
}

// -- Writable serde types for pnpm-lock.yaml v9 --

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WritablePnpmLockfile {
    lockfile_version: String,
    settings: WritableSettings,
    /// pnpm v9 emits a top-level `catalogs:` map immediately after
    /// `settings:` and before `overrides:` — see pnpm's
    /// `sortLockfileKeys` ROOT_KEYS order (lockfileVersion, settings,
    /// catalogs, overrides, packageExtensionsChecksum, pnpmfileChecksum,
    /// patchedDependencies, importers, packages). Field order matters
    /// because we serialize through yaml_serde and want byte-for-byte
    /// parity with pnpm. Skipped when empty so a no-catalogs install
    /// stays byte-identical to pnpm output.
    #[serde(skip_serializing_if = "Option::is_none")]
    catalogs: Option<BTreeMap<String, BTreeMap<String, WritableCatalogEntry>>>,
    // pnpm v9 places `overrides:` after `catalogs:` and before
    // `packageExtensionsChecksum:`. Field order matters because we
    // serialize through yaml_serde and want byte-for-byte parity with
    // pnpm output (the field is skipped when empty).
    #[serde(skip_serializing_if = "Option::is_none")]
    overrides: Option<BTreeMap<String, String>>,
    /// pnpm v9's top-level `packageExtensionsChecksum:` — emitted right
    /// after `overrides:` and before `pnpmfileChecksum:` when the
    /// effective config declares any `packageExtensions`. Already
    /// carries pnpm's `sha256-` prefix. Skipped when absent so a
    /// no-extensions install stays byte-identical to pnpm.
    #[serde(skip_serializing_if = "Option::is_none")]
    package_extensions_checksum: Option<String>,
    /// pnpm v9's top-level `pnpmfileChecksum:` — emitted immediately
    /// after `packageExtensionsChecksum:` and before
    /// `patchedDependencies:` when a local pnpmfile participates.
    /// Skipped when absent for byte-identical output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pnpmfile_checksum: Option<String>,
    /// pnpm v9+ top-level `patchedDependencies:` — preserved so a
    /// bun→aube-lock conversion keeps the user's patches and a
    /// re-emit doesn't strip the block. pnpm emits this block right
    /// after `pnpmfileChecksum:` and before `importers:`, so the field
    /// order here follows the same sequence for byte-identical output.
    #[serde(skip_serializing_if = "Option::is_none")]
    patched_dependencies: Option<BTreeMap<String, WritablePatchedDependency>>,
    /// pnpm v9 emits a top-level `time:` map when `resolution-mode=time-based`
    /// is active. Keyed by canonical `name@version`; values are ISO-8601
    /// publish timestamps pulled from the registry packument. Placed
    /// after `overrides:` and before `importers:` to match pnpm's
    /// field order.
    #[serde(skip_serializing_if = "Option::is_none")]
    time: Option<BTreeMap<String, String>>,
    importers: BTreeMap<String, WritableImporter>,
    packages: BTreeMap<String, WritablePackageInfo>,
    /// pnpm v9 emits a top-level `ignoredOptionalDependencies:` array
    /// after `packages:` and before `snapshots:` when the root
    /// manifest's `pnpm.ignoredOptionalDependencies` is non-empty.
    /// Skipped when empty so a no-ignored install stays byte-for-byte
    /// identical to pnpm's output.
    #[serde(skip_serializing_if = "Option::is_none")]
    ignored_optional_dependencies: Option<Vec<String>>,
    snapshots: BTreeMap<String, WritableSnapshot>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WritableSettings {
    auto_install_peers: bool,
    exclude_links_from_lockfile: bool,
    /// Skipped at serialization time when false so pnpm-parity
    /// projects that don't opt into the tarball-URL recording keep
    /// byte-identical lockfiles.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    lockfile_include_tarball_url: bool,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct WritableImporter {
    #[serde(skip_serializing_if = "Option::is_none")]
    dependencies: Option<BTreeMap<String, WritableDepSpec>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dev_dependencies: Option<BTreeMap<String, WritableDepSpec>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    optional_dependencies: Option<BTreeMap<String, WritableDepSpec>>,
    /// Optionals the resolver intentionally skipped on this importer's
    /// platform — round-tripped so drift detection can distinguish
    /// "previously skipped" from "newly added". Aube-specific extension
    /// to pnpm v9's importer schema; the field is omitted when empty so
    /// no-skip projects stay byte-identical to pnpm output.
    #[serde(skip_serializing_if = "Option::is_none")]
    skipped_optional_dependencies: Option<BTreeMap<String, WritableDepSpec>>,
}

#[derive(Debug, Serialize)]
struct WritableDepSpec {
    specifier: String,
    version: String,
}

#[derive(Debug, Serialize)]
struct WritableCatalogEntry {
    specifier: String,
    version: String,
}

// Field order is alphabetical by *serialized* key name to match pnpm's
// sorted-key lockfile emitter (it runs every `resolution:` map through
// `sortKeys`). The cases this spans:
//   registry  → {integrity}  /  {integrity, tarball}
//   directory → {directory, type: directory}
//   git       → {commit, integrity?, path?, repo, type: git}
//   codeload  → {gitHosted, integrity, tarball}   (hosted-git tarball)
//   runtime   → {type: variations, variants}
// Serde serializes in declaration order regardless of `rename`, so the
// fields are declared in the order of their renamed names (`gitHosted`,
// `type`) — not the Rust identifiers.
#[derive(Debug, Serialize)]
struct WritableResolution {
    // Git resolution fields (pnpm v9 `{type: git, repo, commit}` form).
    #[serde(skip_serializing_if = "Option::is_none")]
    commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    directory: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not", rename = "gitHosted")]
    git_hosted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    integrity: Option<String>,
    /// pnpm `&path:/<sub>` selector — emitted with leading `/` to
    /// match pnpm's own writer.
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tarball: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "type")]
    type_: Option<String>,
    /// `type: variations` artifact list for runtime pins. `None` for
    /// every ordinary package resolution.
    #[serde(skip_serializing_if = "Option::is_none")]
    variants: Option<Vec<WritableRuntimeVariant>>,
}

/// One `variants:` entry of a runtime pin's `variations` resolution.
/// Field order is alphabetical (`resolution` before `targets`),
/// matching pnpm's sorted-key lockfile emitter.
#[derive(Debug, Serialize)]
struct WritableRuntimeVariant {
    resolution: WritableRuntimeBinaryResolution,
    targets: Vec<WritableRuntimeTarget>,
}

/// pnpm `BinaryResolution` — alphabetical field order to match pnpm's
/// sorted-key emitter.
#[derive(Debug, Serialize)]
struct WritableRuntimeBinaryResolution {
    archive: String,
    bin: WritableRuntimeBin,
    #[serde(skip_serializing_if = "Option::is_none")]
    integrity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prefix: Option<String>,
    #[serde(rename = "type")]
    type_: &'static str,
    url: String,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum WritableRuntimeBin {
    /// Bare-string form (`bin: bin/node`) — a single executable named
    /// after the runtime itself.
    Single(String),
    Map(BTreeMap<String, String>),
}

#[derive(Debug, Serialize)]
struct WritableRuntimeTarget {
    cpu: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    libc: Option<String>,
    os: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WritablePeerDepMeta {
    // pnpm v9 omits `optional: false` entirely; only the truthy form
    // shows up in real-world lockfiles. Skip the default so we stay
    // byte-identical for the rare case where a packument explicitly
    // marks a peer as non-optional.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    optional: bool,
}

// Field order matches pnpm v9's `packages:` entries: resolution, then
// engines, then os/cpu/libc, then hasBin, then peerDependencies /
// peerDependenciesMeta. Don't reorder.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WritablePackageInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    resolution: Option<WritableResolution>,
    /// Real semver for non-registry entries (remote tarball / git),
    /// where the dep-path key is a URL rather than a version. pnpm
    /// emits this field so tooling that reads lockfile entries by
    /// `(name, version)` still finds the right semver. Omitted for
    /// ordinary registry entries — the version lives in the key.
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    /// pnpm writes `engines: {node: '>=8'}` in flow form immediately
    /// after `resolution:` when the package declared any engines —
    /// minus entries whose value is exactly `*`, which pnpm drops (so a
    /// manifest's `engines: {node: '*'}` yields no `engines:` line).
    /// Emitted as a block map here — `reformat_for_pnpm_parity` flips it
    /// to flow form to match pnpm byte-for-byte.
    #[serde(skip_serializing_if = "Option::is_none")]
    engines: Option<BTreeMap<String, String>>,
    // pnpm v9 emits `cpu`, then `os`, then `libc` after `engines` and
    // before `hasBin` (see pnpm's `sortLockfileKeys` ORDERED_KEYS:
    // cpu=6, os=7, libc=8). Keep this order to stay byte-identical with
    // pnpm-written lockfiles for native packages. `reformat_for_pnpm_parity`
    // flips each of these block sequences to flow form (`cpu: [arm64]`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    cpu: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    os: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    libc: Vec<String>,
    /// Registry deprecation reason. pnpm emits `deprecated: <reason>`
    /// right after `cpu`/`os`/`libc` and before `hasBin` (verified
    /// against pnpm v11 output for `request` / `coffee-script` /
    /// `fsevents`). Skipped when absent so non-deprecated packages stay
    /// byte-identical to pnpm.
    #[serde(skip_serializing_if = "Option::is_none")]
    deprecated: Option<String>,
    /// pnpm emits `hasBin: true` only when the package has executables;
    /// `hasBin: false` is never written. Skip the default to match.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    has_bin: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    peer_dependencies: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    peer_dependencies_meta: Option<BTreeMap<String, WritablePeerDepMeta>>,
    /// Real registry name for npm-alias deps. Aube-specific extension
    /// (pnpm encodes aliases in the snapshot key itself — e.g.
    /// `odd-alias@npm:is-odd@3.0.1` — but aube keys by `alias@version`
    /// for linker simplicity, so the real name has to round-trip
    /// out-of-band via this field). Omitted for non-aliased packages
    /// so non-alias lockfiles stay byte-identical to pnpm's output.
    #[serde(skip_serializing_if = "Option::is_none")]
    alias_of: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WritableSnapshot {
    // Order mirrors pnpm's `LockfilePackageSnapshot` emit order
    // (dependencies → optionalDependencies → transitivePeerDependencies
    // → optional) so a parse-then-write round-trip stays diff-clean
    // against pnpm's own output. `bundledDependencies` is not in pnpm's
    // snapshot schema (lives on `LockfilePackageInfo`, pre-existing
    // aube quirk) — placed last so it does not split the pnpm-
    // canonical block.
    #[serde(skip_serializing_if = "Option::is_none")]
    dependencies: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    optional_dependencies: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    transitive_peer_dependencies: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    optional: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bundled_dependencies: Option<Vec<String>>,
}
