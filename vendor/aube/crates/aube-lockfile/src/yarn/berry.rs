use crate::{
    DepType, DirectDep, Error, GitSource, LocalSource, LockedPackage, LockfileGraph,
    RemoteTarballSource,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Parse a yarn berry (v2+) lockfile's pre-read contents.
///
/// Public entry point is [`parse`]; this function is split out so we
/// don't re-read the file just to peek for the `__metadata:` marker.
pub(super) fn parse_berry_str(
    path: &Path,
    content: &str,
    manifest: &aube_manifest::PackageJson,
) -> Result<LockfileGraph, Error> {
    let doc: yaml_serde::Value = yaml_serde::from_str(content)
        .map_err(|e| Error::parse_yaml_err(path, content.to_string(), &e))?;
    let map = doc
        .as_mapping()
        .ok_or_else(|| Error::parse(path, "yarn berry lockfile root must be a mapping"))?;

    // Validate `__metadata.version` — berry has been at major
    // version 3 since yarn 2, 6 from yarn 3.x, and 8 from yarn 4.x.
    // We accept any value >= 3; the shape we care about (block
    // headers, `resolution:` / `checksum:`) hasn't changed across
    // those versions.
    let meta_version = map
        .get("__metadata")
        .and_then(|m| m.as_mapping())
        .and_then(|m| m.get("version"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    if meta_version < 3 {
        return Err(Error::parse(
            path,
            format!(
                "yarn berry lockfile has unexpected __metadata.version: {meta_version} (expected >= 3)"
            ),
        ));
    }

    // First pass: walk every top-level block, turning each into a
    // `LockedPackage` keyed by canonical `name@version` and recording
    // every header-spec → dep_path mapping for the second pass.
    let mut spec_to_dep_path: BTreeMap<String, String> = BTreeMap::new();
    let mut packages: BTreeMap<String, LockedPackage> = BTreeMap::new();
    let mut patched_dependencies: BTreeMap<String, String> = BTreeMap::new();
    // Transitive dep values written into each `LockedPackage` in this
    // pass are the raw header specs (e.g. `"foo@npm:^2.0.0"`); the
    // second pass collapses them down to dep_paths.

    for (key, value) in map {
        let Some(key_str) = key.as_str() else {
            continue;
        };
        if key_str.starts_with("__") {
            continue;
        }
        let block = value.as_mapping().ok_or_else(|| {
            Error::parse(
                path,
                format!("yarn berry block '{key_str}' is not a mapping"),
            )
        })?;

        let specs = split_berry_header(key_str);
        if specs.is_empty() {
            continue;
        }

        // Berry writes versions unquoted (`version: 1.0.0`). Depending
        // on the YAML resolver, scalar-looking versions may parse as
        // non-string values, so coerce them back to strings rather than
        // reporting "has no version" against a spec that obviously does.
        let version = block
            .get("version")
            .and_then(yaml_scalar_as_string)
            .ok_or_else(|| {
                Error::parse(path, format!("yarn berry block '{key_str}' has no version"))
            })?;

        let resolution = block
            .get("resolution")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                Error::parse(
                    path,
                    format!("yarn berry block '{key_str}' has no resolution"),
                )
            })?;
        let (res_name, res_protocol, res_body) = parse_berry_spec(resolution).ok_or_else(|| {
            Error::parse(
                path,
                format!("yarn berry block '{key_str}' has malformed resolution '{resolution}'"),
            )
        })?;

        // Map the resolution protocol onto aube's data model. `npm:` is
        // the default and the only thing with a conventional
        // `name@version` dep_path; everything else is a non-registry
        // source the linker needs to materialize differently.
        let local_source = match res_protocol {
            "npm" => None,
            "workspace" => {
                // The root workspace block lives here (`name@workspace:.`)
                // and is driven by `package.json`, not the lockfile. We
                // rely on the manifest pass below to populate
                // `importers["."]`, and skip emitting workspace blocks
                // as `LockedPackage` entries.
                for spec in &specs {
                    spec_to_dep_path.insert(spec.clone(), format!("{res_name}@{version}"));
                }
                continue;
            }
            "patch" => {
                let Some(patch_path) = patch_protocol_path(res_body) else {
                    tracing::warn!(
                        code = aube_codes::warnings::WARN_AUBE_YARN_BERRY_UNSUPPORTED,
                        "yarn berry patch protocol in block '{}' is not supported — entry skipped",
                        key_str,
                    );
                    continue;
                };
                patched_dependencies.insert(format!("{res_name}@{version}"), patch_path);
                None
            }
            "portal" => Some(LocalSource::Portal(PathBuf::from(strip_hash_fragment(
                res_body,
            )))),
            "exec" => Some(LocalSource::Exec(PathBuf::from(strip_hash_fragment(
                res_body,
            )))),
            "file" => Some(file_protocol_source(res_body)),
            "link" => Some(LocalSource::Link(PathBuf::from(strip_hash_fragment(
                res_body,
            )))),
            // Plain HTTP(S) tarball or git-over-HTTPS: berry records
            // the full URL in the spec, which `parse_berry_spec` split
            // into `res_protocol = "https"` and `res_body =
            // "//host/path..."`. Glue them back together with
            // `<protocol>:<body>` to get the original URL, then let
            // `classify_remote` pick tarball vs git based on `.git`
            // in the URL.
            "http" | "https" => {
                let url = format!("{res_protocol}:{res_body}");
                classify_remote(&url, block)
            }
            // Git via a non-HTTP transport. Covers `git:`, `ssh:`, and
            // the compound `git+ssh:` / `git+https:` / `git+file:`
            // variants berry emits for git deps whose commit is pinned
            // after `#`. Rejoin `<protocol>:<body>` into the full URL
            // the linker will hand to `git clone`.
            p if p == "git" || p == "ssh" || p.starts_with("git+") || p.starts_with("ssh+") => {
                let url = format!("{res_protocol}:{res_body}");
                Some(LocalSource::Git(GitSource {
                    url: strip_commit_hash(&url),
                    committish: None,
                    resolved: extract_commit_hash(&url).unwrap_or_default(),
                    integrity: None,
                    subpath: None,
                }))
            }
            _ => {
                tracing::warn!(
                    code = aube_codes::warnings::WARN_AUBE_YARN_BERRY_UNSUPPORTED,
                    "yarn berry unrecognized protocol '{}' in block '{}' — entry skipped",
                    res_protocol,
                    key_str,
                );
                continue;
            }
        };

        // Canonical dep_path: `name@version` for registry packages,
        // whatever `LocalSource::dep_path` returns for non-registry ones.
        // Berry always pairs registry and local deps by `name@version`
        // at the graph layer, so duplicate names with the same version
        // but different protocols collapse — same as the classic writer.
        let dep_path = match &local_source {
            Some(src) => src.dep_path(res_name),
            None => format!("{res_name}@{version}"),
        };

        for spec in &specs {
            spec_to_dep_path.insert(spec.clone(), dep_path.clone());
        }

        // Transitive deps: `name: "protocol:range"`. We store the raw
        // header-style spec (`name@protocol:range`) and rewrite to a
        // dep_path in pass two.
        let raw_deps = collect_dep_map(block, "dependencies");
        let peer_deps = collect_dep_map(block, "peerDependencies");
        let peer_deps_meta = collect_peer_meta(block);
        let optional_deps = collect_dep_map(block, "optionalDependencies");

        // Declared ranges — same source as `raw_deps` / `optional_deps`
        // but kept as the bare range string (no `name@` prefix) so
        // writers can slot them straight back into the output.
        let mut declared: BTreeMap<String, String> = BTreeMap::new();
        for (n, v) in raw_deps.iter().chain(optional_deps.iter()) {
            declared.insert(n.clone(), v.clone());
        }

        let raw_deps_specs: BTreeMap<String, String> = raw_deps
            .into_iter()
            .map(|(n, v)| (n.clone(), format!("{n}@{v}")))
            .collect();
        let optional_deps_specs: BTreeMap<String, String> = optional_deps
            .into_iter()
            .map(|(n, v)| (n.clone(), format!("{n}@{v}")))
            .collect();

        let checksum = block
            .get("checksum")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        if !packages.contains_key(&dep_path) {
            packages.insert(
                dep_path.clone(),
                LockedPackage {
                    name: res_name.to_string(),
                    version: version.clone(),
                    integrity: None,
                    yarn_checksum: checksum,
                    dependencies: raw_deps_specs,
                    optional_dependencies: optional_deps_specs,
                    peer_dependencies: peer_deps,
                    peer_dependencies_meta: peer_deps_meta,
                    dep_path: dep_path.clone(),
                    local_source,
                    declared_dependencies: declared,
                    ..Default::default()
                },
            );
        }
    }

    // Second pass: resolve raw header specs on each package's
    // `dependencies` / `optional_dependencies` map to canonical dep_paths.
    let mut resolved_deps: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut resolved_opts: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for (dep_path, pkg) in &packages {
        let resolve = |raw: &BTreeMap<String, String>| {
            let mut out = BTreeMap::new();
            for (name, raw_spec) in raw {
                if let Some(target) = spec_to_dep_path.get(raw_spec) {
                    out.insert(name.clone(), target.clone());
                }
            }
            out
        };
        resolved_deps.insert(dep_path.clone(), resolve(&pkg.dependencies));
        resolved_opts.insert(dep_path.clone(), resolve(&pkg.optional_dependencies));
    }
    for (dep_path, deps) in resolved_deps {
        if let Some(pkg) = packages.get_mut(&dep_path) {
            pkg.dependencies = deps;
        }
    }
    for (dep_path, deps) in resolved_opts {
        if let Some(pkg) = packages.get_mut(&dep_path) {
            pkg.optional_dependencies = deps;
        }
    }

    // A root `resolutions` entry rewrites the descriptor yarn writes
    // to the lockfile, so the block is keyed by the *resolved* range,
    // not the manifest range. With `resolutions: {"@types/node":
    // "18.x"}` and a manifest range of `^18.14`, yarn.lock holds only
    // `@types/node@npm:18.x`; matching the raw range misses and the dep
    // is silently dropped from the importer. Apply the resolution to
    // each direct dep before building its lockfile-spec candidates so
    // the resolved descriptor is tried too. `overrides_map` collects
    // yarn `resolutions` (alongside npm/pnpm overrides, harmless here);
    // `override_match` handles both bare-name (`@types/node`) and
    // descriptor-keyed (`lru-cache@^10.0.1`) resolution keys.
    let resolution_rules = crate::override_match::compile(&manifest.overrides_map());

    // Build direct deps from the manifest, using the yarn berry
    // convention that a range `"^1.0.0"` corresponds to the spec
    // `"name@npm:^1.0.0"`. If the manifest range already carries a
    // protocol prefix (`workspace:*`, `file:./pkgs/foo`, ...), it's
    // already a valid spec suffix and we try it verbatim first.
    let mut direct: Vec<DirectDep> = Vec::new();
    let push_direct = |name: &str, range: &str, dep_type: DepType, direct: &mut Vec<DirectDep>| {
        let resolved = crate::override_match::apply(&resolution_rules, name, range);
        let candidates = berry_spec_candidates(name, range, resolved);
        for candidate in candidates {
            if let Some(dep_path) = spec_to_dep_path.get(&candidate) {
                direct.push(DirectDep {
                    name: name.to_string(),
                    dep_path: dep_path.clone(),
                    dep_type,
                    specifier: None,
                });
                return;
            }
        }
    };
    for (name, range) in &manifest.dependencies {
        push_direct(name, range, DepType::Production, &mut direct);
    }
    for (name, range) in &manifest.dev_dependencies {
        push_direct(name, range, DepType::Dev, &mut direct);
    }
    for (name, range) in &manifest.optional_dependencies {
        push_direct(name, range, DepType::Optional, &mut direct);
    }

    let mut importers = BTreeMap::new();
    importers.insert(".".to_string(), direct);

    Ok(LockfileGraph {
        importers,
        packages,
        patched_dependencies,
        ..Default::default()
    })
}

/// Split a berry block header like `"foo@npm:^1.0.0, foo@npm:^2.0.0"`
/// into individual specs. The YAML layer already unquoted the key;
/// berry separates multiple specs inside the single key string with
/// `", "` (space required). Leading/trailing whitespace tolerated.
pub(super) fn split_berry_header(header: &str) -> Vec<String> {
    header
        .split(", ")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Parse a berry spec `<name>@<protocol>:<rest>` into its three parts.
/// Scoped names (`@scope/pkg`) are handled by skipping the leading `@`.
/// Returns `None` if the spec is malformed.
pub(super) fn parse_berry_spec(spec: &str) -> Option<(&str, &str, &str)> {
    let (name, after_at) = if let Some(rest) = spec.strip_prefix('@') {
        // Scoped: `@scope/pkg@<protocol>:<rest>` — find the second `@`.
        let slash = rest.find('/')?;
        let after_slash = &rest[slash + 1..];
        let at = after_slash.find('@')?;
        let full_name_len = 1 + slash + 1 + at;
        (&spec[..full_name_len], &spec[full_name_len + 1..])
    } else {
        let at = spec.find('@')?;
        (&spec[..at], &spec[at + 1..])
    };
    let colon = after_at.find(':')?;
    let protocol = &after_at[..colon];
    let body = &after_at[colon + 1..];
    Some((name, protocol, body))
}

/// Build the ordered list of berry spec strings to try when matching
/// a manifest entry against the lockfile's spec-to-dep-path index.
/// First we try the raw `name@range` (covers cases where the manifest
/// already carries a protocol prefix like `workspace:*`); failing
/// that, fall back to `name@npm:range` which is the default berry
/// adds when the user writes an un-prefixed semver range.
///
/// When a root `resolution` rewrites this dep's descriptor, yarn keys
/// the block by the *resolved* range — so we try the resolved
/// descriptor first, before the manifest range. The resolution value
/// may already carry a protocol (`patch:...`, `npm:...`); the same
/// raw-then-`npm:` candidate pair covers both spellings.
fn berry_spec_candidates(name: &str, range: &str, resolved: Option<&str>) -> Vec<String> {
    let mut out = Vec::with_capacity(4);
    let mut push_for = |r: &str| {
        out.push(format!("{name}@{r}"));
        if !range_has_protocol(r) {
            out.push(format!("{name}@npm:{r}"));
        }
    };
    if let Some(resolved) = resolved.filter(|r| *r != range) {
        push_for(resolved);
    }
    push_for(range);
    out
}

/// True when a manifest range carries a berry protocol prefix like
/// `npm:...`, `workspace:...`, `file:...`, `link:...`, `portal:...`,
/// `patch:...`, `exec:...`, `git:...`, `git+ssh:...`, `http:`,
/// `https:`. Used to decide whether to prepend `npm:` when building
/// the spec candidate.
pub(super) fn range_has_protocol(range: &str) -> bool {
    let Some(colon) = range.find(':') else {
        return false;
    };
    let head = &range[..colon];
    // Yarn berry's protocol heads are alphabetic with optional `+`
    // separators for compound transports (`git+ssh`, `git+https`,
    // `git+file`). Digits and other punctuation are not valid
    // protocol chars, which also rules out Windows drive letters
    // (single-letter heads are technically still valid protocols,
    // but yarn itself doesn't emit any and the `file:` spelling
    // handles those deps).
    !head.is_empty() && head.chars().all(|c| c.is_ascii_alphabetic() || c == '+')
}

/// Render a scalar YAML value as its source-text-equivalent string.
///
/// Berry emits scalar fields unquoted in several places. YAML parsers
/// may resolve integer-, float-, or boolean-looking tokens to typed
/// values; returning those as strings preserves the graph edge instead
/// of silently dropping an otherwise valid lockfile entry.
fn yaml_scalar_as_string(v: &yaml_serde::Value) -> Option<String> {
    match v {
        yaml_serde::Value::String(s) => Some(s.clone()),
        yaml_serde::Value::Number(n) => Some(n.to_string()),
        yaml_serde::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Extract a `BTreeMap<name, value>` from a sub-mapping like
/// `dependencies:` or `peerDependencies:`. Missing section or
/// non-mapping values return empty. Values go through
/// `yaml_scalar_as_string` for the same reason `version` does — a
/// bare `dep: 5` would otherwise silently drop the edge instead of
/// recording `"5"` as the range.
fn collect_dep_map(block: &yaml_serde::Mapping, key: &str) -> BTreeMap<String, String> {
    block
        .get(key)
        .and_then(|v| v.as_mapping())
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| Some((k.as_str()?.to_string(), yaml_scalar_as_string(v)?)))
                .collect()
        })
        .unwrap_or_default()
}

/// Pull `peerDependenciesMeta` into our structured form. Only the
/// `optional` flag round-trips through aube's model; other keys in
/// the meta block (if any) are ignored.
fn collect_peer_meta(block: &yaml_serde::Mapping) -> BTreeMap<String, crate::PeerDepMeta> {
    block
        .get("peerDependenciesMeta")
        .and_then(|v| v.as_mapping())
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| {
                    let name = k.as_str()?.to_string();
                    let meta = v.as_mapping()?;
                    let optional = meta
                        .get("optional")
                        .and_then(|o| o.as_bool())
                        .unwrap_or(false);
                    Some((name, crate::PeerDepMeta { optional }))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Resolve a `file:`-protocol body like `./vendor/foo` or
/// `./vendor/foo-1.0.0.tgz#hash` to a [`LocalSource`]. The fragment
/// (`#...`) that berry appends to pin the imported checksum is
/// stripped — aube's `LocalSource` records the path only, and the
/// checksum round-trips via `yarn_checksum`.
pub(super) fn file_protocol_source(body: &str) -> LocalSource {
    let path = PathBuf::from(strip_hash_fragment(body));
    let is_tarball = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("tgz") || e.eq_ignore_ascii_case("gz"));
    if is_tarball {
        LocalSource::Tarball(path)
    } else {
        LocalSource::Directory(path)
    }
}

/// Extract the local patch file from Yarn's `patch:` protocol body.
///
/// Berry encodes patched npm packages as
/// `name@patch:name@npm%3Aversion#./.yarn/patches/name.patch::version=...`.
/// Aube applies the patch through its existing git-diff materializer, so
/// the only part needed here is the project-relative path after `#`.
///
/// Returns `None` for builtin-compat patches — yarn's internal shims for
/// packages like `resolve`/`typescript`, which carry no real patch file
/// and resolve from the companion `npm:` block. These appear bare
/// (`builtin<compat/resolve>`, `~builtin<...>`) or with a `<qualifier>!`
/// prefix (`optional!builtin<compat/resolve>` — the common berry form via
/// eslint-plugin-import/webpack/rollup). The qualifier is stripped before
/// the builtin check so the `!`-qualified target isn't mistaken for a
/// filesystem path.
fn patch_protocol_path(body: &str) -> Option<String> {
    let (_, after_hash) = body.split_once('#')?;
    let path = after_hash
        .split_once("::")
        .map(|(p, _)| p)
        .unwrap_or(after_hash);
    // A `<qualifier>!builtin<...>` target carries one or more `!`-joined
    // qualifiers (e.g. `optional!`) ahead of the selector. Strip up to and
    // including the last `!` so the builtin check sees the bare selector.
    let selector = path.rsplit_once('!').map(|(_, s)| s).unwrap_or(path);
    if selector.is_empty() || selector.starts_with("builtin<") || selector.starts_with("~builtin<")
    {
        return None;
    }
    Some(path.to_string())
}

fn patch_spec_path(spec: &str) -> Option<String> {
    let (_, protocol, body) = parse_berry_spec(spec)?;
    (protocol == "patch").then(|| patch_protocol_path(body))?
}

fn patch_spec_matches(
    spec: &str,
    pkg: &LockedPackage,
    patched_dependencies: &BTreeMap<String, String>,
) -> bool {
    let Some(path) = patch_spec_path(spec) else {
        return false;
    };
    patched_dependencies
        .get(&pkg.spec_key())
        .is_some_and(|expected| expected == &path)
}

pub(super) fn strip_hash_fragment(s: &str) -> &str {
    s.split_once('#').map(|(a, _)| a).unwrap_or(s)
}

/// If the URL has a `#<commit>` suffix, return `<commit>`. Used for
/// git-over-http berry specs that pin the resolved commit after `#`.
fn extract_commit_hash(url: &str) -> Option<String> {
    url.split_once('#')
        .and_then(|(_, b)| crate::normalize_git_fragment(b))
}

fn strip_commit_hash(url: &str) -> String {
    strip_hash_fragment(url).to_string()
}

/// Classify a remote URL in berry's resolution field as either a git
/// repo (if it has a commit hash suffix or a `.git` path) or a plain
/// tarball download. Checksum / integrity lives on the `checksum:`
/// field and round-trips through `yarn_checksum`.
fn classify_remote(url: &str, _block: &yaml_serde::Mapping) -> Option<LocalSource> {
    if url.ends_with(".git") || url.contains(".git#") {
        Some(LocalSource::Git(GitSource {
            url: strip_commit_hash(url),
            committish: None,
            resolved: extract_commit_hash(url).unwrap_or_default(),
            integrity: None,
            subpath: None,
        }))
    } else {
        Some(LocalSource::RemoteTarball(RemoteTarballSource {
            url: strip_commit_hash(url),
            integrity: String::new(),
            git_hosted: false,
        }))
    }
}

// ---------------------------------------------------------------------------
// Writer: flat LockfileGraph → yarn berry lockfile
// ---------------------------------------------------------------------------

/// Serialize a [`LockfileGraph`] as a yarn berry (v2+) lockfile.
///
/// Output targets yarn 4's `__metadata.version: 8` / `cacheKey: 10c0`
/// (accepted by yarn 3.x too; yarn 2.x is functionally extinct). The
/// block shape — one entry per canonical `(name, version)` with a
/// comma-separated header of all specifiers that resolve to it — is
/// identical to the classic writer's, just reformatted as YAML with
/// `resolution:` / `checksum:` / `languageName` / `linkType` fields.
///
/// ## What round-trips
///
/// `yarn_checksum` (parsed from `checksum:`), patch specs, peer-dep
/// metadata, and all resolved transitive edges make it through parse →
/// write → parse unchanged.
///
/// ## What doesn't
///
/// - Peer-contextualized variants collapse onto a single `name@version`
///   block; berry's native encoding uses `virtual:` keys to keep them
///   distinct but aube's graph model doesn't, matching our pnpm/npm
///   writers.
/// - Packages the resolver produced fresh (no berry parse to source
///   `yarn_checksum` from) are written without a `checksum:` field.
///   Yarn's default `checksumBehavior: throw` populates missing
///   checksums on the next install against its own cache.
pub fn write_berry(
    path: &Path,
    graph: &LockfileGraph,
    manifest: &aube_manifest::PackageJson,
) -> Result<(), Error> {
    // Collapse peer-context variants to one entry per canonical
    // `(name, version)` — same as the classic writer. The canonical
    // key is `name@version`; we need it to look up a package's extra
    // specifiers in the same map.
    let canonical = crate::build_canonical_map(graph);

    // `extra_specs[canonical_key]` is the set of range-form
    // specifiers (e.g. `"foo@npm:^1.0.0"`) that should appear in the
    // block header alongside the exact `foo@npm:1.2.3` one. Collecting
    // them keeps the header compatible with what yarn itself would
    // produce: every spec that resolves to the same (name, version)
    // gets folded into one block, so transitive lookups of a declared
    // range find a matching header.
    let mut extra_specs: BTreeMap<String, std::collections::BTreeSet<String>> = BTreeMap::new();
    let mut patch_specs: BTreeMap<String, String> = BTreeMap::new();
    let manifest_ranges: Vec<(String, String)> = manifest
        .dependencies
        .iter()
        .chain(manifest.dev_dependencies.iter())
        .chain(manifest.optional_dependencies.iter())
        .chain(manifest.peer_dependencies.iter())
        .map(|(n, r)| (n.clone(), r.clone()))
        .collect();
    let format_berry_spec = |dep_name: &str, range: &str| {
        if range_has_protocol(range) {
            format!("{dep_name}@{range}")
        } else {
            format!("{dep_name}@npm:{range}")
        }
    };
    for dep in graph.importers.get(".").into_iter().flatten() {
        let canonical_key = crate::npm::canonical_key_from_dep_path(&dep.dep_path);
        if !canonical.contains_key(&canonical_key) {
            continue;
        }
        let Some((_, range)) = manifest_ranges.iter().find(|(n, _)| n == &dep.name) else {
            continue;
        };
        let manifest_spec = format_berry_spec(&dep.name, range);
        let pkg = canonical.get(&canonical_key).copied().unwrap();
        if patch_spec_matches(&manifest_spec, pkg, &graph.patched_dependencies) {
            patch_specs.insert(canonical_key.clone(), manifest_spec.clone());
        }
        let exact_spec = berry_exact_spec(pkg, &graph.patched_dependencies, &patch_specs);
        if manifest_spec != exact_spec {
            extra_specs
                .entry(canonical_key)
                .or_default()
                .insert(manifest_spec);
        }
    }
    // Harvest transitive declared ranges, same shape as the classic
    // writer. Berry specs always carry a protocol (`npm:`, `workspace:`,
    // `patch:` …); bare ranges get the default `npm:` prefix.
    for pkg in canonical.values() {
        for (dep_name, range) in &pkg.declared_dependencies {
            let Some(resolved_value) = pkg.dependencies.get(dep_name) else {
                continue;
            };
            let target = crate::npm::child_canonical_key(dep_name, resolved_value);
            let Some(target_pkg) = canonical.get(&target) else {
                continue;
            };
            let manifest_spec = format_berry_spec(dep_name, range);
            if patch_spec_matches(&manifest_spec, target_pkg, &graph.patched_dependencies) {
                patch_specs.insert(target.clone(), manifest_spec.clone());
            }
            let exact_spec =
                berry_exact_spec(target_pkg, &graph.patched_dependencies, &patch_specs);
            if manifest_spec != exact_spec {
                extra_specs.entry(target).or_default().insert(manifest_spec);
            }
        }
    }

    let mut out = String::with_capacity(canonical.len().saturating_mul(256).max(4096));
    out.push_str("# This file is generated by running \"yarn install\" inside your project.\n");
    out.push_str("# Manual changes might be lost - proceed with caution!\n\n");
    // `__metadata.version: 10` is what yarn 4.x writes (yarn 3.x emitted 8).
    // The block shape is identical across both; matching the current major's
    // value is what lets `yarn install --immutable` accept the file without
    // rewriting the header. `cacheKey: 10c0` matches the registry-tarball
    // cache generation yarn 3.6+/4.x use.
    out.push_str("__metadata:\n  version: 10\n  cacheKey: 10c0\n\n");

    // Yarn sorts every block — including the root workspace entry — by its
    // header descriptor string (`ms@npm:^2.1.3` < `na-fixture@workspace:.` <
    // `supports-color@npm:^7.1.0`). Our `canonical` map is keyed by
    // `name@version`, a different order, so buffer each block with its header
    // as the sort key and emit them sorted. Matching yarn's ordering is what
    // keeps `--immutable` zero-churn.
    let mut blocks: Vec<(String, String)> = Vec::with_capacity(canonical.len() + 1);

    // Root workspace importer block (`<name>@workspace:.`). Yarn always
    // records the root project as a `workspace:` entry carrying its declared
    // dependency ranges; omitting it makes `--immutable` reject the file.
    {
        let mut block = String::new();
        let key = write_berry_root_workspace(&mut block, manifest);
        blocks.push((key, block));
    }

    for (canonical_key, pkg) in &canonical {
        let mut out = String::new();
        // Yarn's block header lists every *specifier* that resolves to this
        // package — the declared ranges (`chalk@npm:^4.1.2`), not the exact
        // `name@npm:version` resolution (that lives in `resolution:`). Build
        // the header from the collected range specs; only when none exists
        // (a package nothing in this graph references by range — rare, e.g.
        // an orphaned transitive) do we fall back to the exact spec so the
        // block still has a parseable header.
        let exact_spec = berry_exact_spec(pkg, &graph.patched_dependencies, &patch_specs);
        let mut header_specs: Vec<String> = Vec::new();
        if let Some(extras) = extra_specs.get(canonical_key) {
            for s in extras {
                if !header_specs.contains(s) {
                    header_specs.push(s.clone());
                }
            }
        }
        if header_specs.is_empty() {
            header_specs.push(exact_spec.clone());
        }
        // Header and `resolution:` both carry spec strings that may
        // contain `:`, `/`, `@`, `#`, or — for `patch:` / file-path
        // sources — backslashes and quotes. Route both through
        // `quote_yaml_scalar` so escaping matches the rest of the
        // writer. For the multi-spec header we quote the joined
        // `", "`-separated list as one string, same as berry.
        let header_inner = header_specs.join(", ");
        // Yarn sorts blocks by the first (lowest) descriptor in the header.
        let sort_key = header_specs.first().cloned().unwrap_or_default();
        out.push_str(&quote_yaml_scalar(&header_inner));
        out.push_str(":\n");

        // Scalar fields. Yarn writes `version` and `checksum` *unquoted*
        // (`version: 4.1.2`, `checksum: 10c0/…`) but quotes `resolution`
        // (it carries a `:` protocol separator). Matching that quoting is
        // what makes `--immutable` zero-churn.
        out.push_str("  version: ");
        out.push_str(&pkg.version);
        out.push('\n');
        out.push_str("  resolution: ");
        out.push_str(&quote_yaml_scalar(&exact_spec));
        out.push('\n');

        // Dependencies / peerDependencies / peerDependenciesMeta /
        // optionalDependencies: nested YAML mappings with
        // `name: "npm:<range-or-version>"` values. Resolved
        // transitive deps collapse to the exact version of the target
        // block (the resolver produced the graph, so the key always
        // exists in `canonical`).
        write_berry_dep_map(
            &mut out,
            "dependencies",
            &pkg.dependencies,
            &pkg.declared_dependencies,
            &canonical,
            &graph.patched_dependencies,
            &patch_specs,
        );
        write_berry_dep_map(
            &mut out,
            "optionalDependencies",
            &pkg.optional_dependencies,
            &pkg.declared_dependencies,
            &canonical,
            &graph.patched_dependencies,
            &patch_specs,
        );
        write_berry_peer_deps(&mut out, &pkg.peer_dependencies);
        write_berry_peer_meta(&mut out, &pkg.peer_dependencies_meta);
        // `bin:` map — binary name → relative executable path. Yarn 4 emits
        // this (from the package manifest's `bin`) ahead of `checksum`. It's
        // modeled on `LockedPackage.bin`; the berry reader doesn't populate it
        // (so a pure berry→berry round-trip is unaffected), but a graph
        // resolved from the registry carries real bins and must round-trip
        // them through the berry writer like yarn does.
        write_berry_bin(&mut out, &pkg.bin);

        if let Some(checksum) = &pkg.yarn_checksum {
            // Yarn writes the checksum unquoted (`checksum: 10c0/…`).
            out.push_str("  checksum: ");
            out.push_str(checksum);
            out.push('\n');
        }
        out.push_str("  languageName: node\n");
        // `linkType: soft` means "just symlink, don't materialize into
        // the virtual store" — what berry uses for local directory refs
        // (`link:`, `portal:`, and `workspace:`). `hard` is the default
        // for registry packages and generated/cache-backed sources.
        let link_type = match &pkg.local_source {
            Some(LocalSource::Link(_) | LocalSource::Portal(_)) => "soft",
            _ => "hard",
        };
        out.push_str("  linkType: ");
        out.push_str(link_type);
        out.push('\n');
        blocks.push((sort_key, out));
    }

    // Emit blocks sorted by header descriptor, separated by a blank line —
    // yarn's exact layout. The trailing `\n` after the metadata header plus
    // one blank line before each block gives the `\n\n`-delimited shape
    // without a trailing blank line at EOF.
    blocks.sort_by(|a, b| a.0.cmp(&b.0));
    for (i, (_, block)) in blocks.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(block);
    }

    crate::atomic_write_lockfile(path, out.as_bytes())?;
    Ok(())
}

/// The canonical header-spec used for a berry block's first
/// specifier and its `resolution:` field. Registry packages take the
/// form `name@npm:version`; non-registry sources use the protocol
/// recorded in `local_source`.
fn berry_exact_spec(
    pkg: &LockedPackage,
    patched_dependencies: &BTreeMap<String, String>,
    patch_specs: &BTreeMap<String, String>,
) -> String {
    let spec_key = pkg.spec_key();
    match (&pkg.local_source, patched_dependencies.get(&spec_key)) {
        (None, Some(_)) if patch_specs.contains_key(&spec_key) => patch_specs[&spec_key].clone(),
        (None, Some(path)) => {
            format!(
                "{}@patch:{}@npm%3A{}#{}",
                pkg.name, pkg.name, pkg.version, path
            )
        }
        (None, None) => format!("{}@npm:{}", pkg.name, pkg.version),
        (Some(src), _) => format!("{}@{}", pkg.name, src.specifier()),
    }
}

fn write_berry_dep_map(
    out: &mut String,
    section: &str,
    deps: &BTreeMap<String, String>,
    declared: &BTreeMap<String, String>,
    canonical: &BTreeMap<String, &LockedPackage>,
    patched_dependencies: &BTreeMap<String, String>,
    patch_specs: &BTreeMap<String, String>,
) {
    // Only emit edges whose target survives in `canonical`; the
    // graph-level filter layer (e.g. `--prod` prune) may have dropped
    // packages that dev-only edges still reference.
    //
    // Prefer the declared range from the package's own manifest (what
    // berry itself writes — `chalk: "npm:^4.1.0"`) over the resolved
    // pin. Falls back to `npm:<version>` when the declared range is
    // unknown (e.g. a pnpm-sourced graph being re-emitted as yarn).
    let resolved: Vec<(&str, String)> = deps
        .iter()
        .filter_map(|(n, v)| {
            let key = crate::npm::child_canonical_key(n, v);
            let target = canonical.get(&key)?;
            let spec_body = match &target.local_source {
                None => {
                    let body = declared.get(n).cloned().unwrap_or_else(|| {
                        if patched_dependencies.contains_key(&target.spec_key()) {
                            let exact = berry_exact_spec(target, patched_dependencies, patch_specs);
                            exact
                                .strip_prefix(&format!("{n}@"))
                                .unwrap_or(&exact)
                                .to_string()
                        } else {
                            crate::npm::dep_value_as_version(n, v).to_string()
                        }
                    });
                    // Declared ranges may already carry a protocol
                    // (`npm:^4`, `workspace:*`, `patch:…`) — don't
                    // double-prefix those. Bare ranges like `^4.1.0`
                    // get the default `npm:` protocol.
                    if body.contains(':') {
                        body
                    } else {
                        format!("npm:{body}")
                    }
                }
                Some(src) => src.specifier(),
            };
            Some((n.as_str(), spec_body))
        })
        .collect();
    if resolved.is_empty() {
        return;
    }
    out.push_str("  ");
    out.push_str(section);
    out.push_str(":\n");
    for (name, body) in resolved {
        out.push_str("    ");
        out.push_str(&quote_yaml_key(name));
        out.push_str(": ");
        out.push_str(&quote_yaml_scalar(&body));
        out.push('\n');
    }
}

fn write_berry_peer_deps(out: &mut String, peer: &BTreeMap<String, String>) {
    if peer.is_empty() {
        return;
    }
    out.push_str("  peerDependencies:\n");
    for (name, range) in peer {
        out.push_str("    ");
        out.push_str(&quote_yaml_key(name));
        out.push_str(": ");
        // Peer ranges are bare semver (`^18.3.1`) with no protocol `:`, which
        // yarn writes unquoted; only quote when the value would misparse.
        out.push_str(&quote_yaml_value(range));
        out.push('\n');
    }
}

/// Emit yarn 4's `bin:` mapping (binary name → relative executable path).
/// Empty-key placeholder entries (synthesized by pnpm's `hasBin: true`
/// collapse) are skipped so they don't render as `"": …`. Same nested-map
/// indentation and key/value quoting as the dep maps.
fn write_berry_bin(out: &mut String, bin: &BTreeMap<String, String>) {
    let real: Vec<(&String, &String)> = bin.iter().filter(|(k, _)| !k.is_empty()).collect();
    if real.is_empty() {
        return;
    }
    out.push_str("  bin:\n");
    for (name, path) in real {
        out.push_str("    ");
        out.push_str(&quote_yaml_key(name));
        out.push_str(": ");
        out.push_str(&quote_yaml_value(path));
        out.push('\n');
    }
}

fn write_berry_peer_meta(out: &mut String, meta: &BTreeMap<String, crate::PeerDepMeta>) {
    if meta.is_empty() {
        return;
    }
    out.push_str("  peerDependenciesMeta:\n");
    for (name, m) in meta {
        out.push_str("    ");
        out.push_str(&quote_yaml_key(name));
        out.push_str(":\n");
        out.push_str("      optional: ");
        out.push_str(if m.optional { "true" } else { "false" });
        out.push('\n');
    }
}

/// Quote a YAML scalar so it round-trips through a standard YAML
/// parser regardless of punctuation in the content (`:`, `^`, `*`,
/// `@`, protocol prefixes, leading digits). Berry itself quotes
/// liberally; we do too. Double quotes are backslash-escaped.
fn quote_yaml_scalar(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// Render a YAML scalar *value* the way yarn does: quote only when the value
/// would otherwise misparse (a `:` protocol separator, a leading reserved
/// indicator, surrounding whitespace, a comment-starting ` #`). Bare semver
/// ranges (`^18.3.1`, `~1.1.4`, `>=2`) clear all of these and stay unquoted,
/// matching yarn's `peerDependencies` rendering. The same `key_needs_quoting`
/// rules apply to value heads.
fn quote_yaml_value(s: &str) -> String {
    if key_needs_quoting(s) {
        quote_yaml_scalar(s)
    } else {
        s.to_string()
    }
}

/// Render a dependency-map key the way yarn does: scoped names (`@scope/x`,
/// leading `@` is a YAML reserved indicator) are quoted; bare unscoped names
/// are emitted unquoted (`color-convert:`). Matching yarn's exact key quoting
/// is what keeps `--immutable` zero-churn — yarn re-emits unquoted keys and a
/// blanket-quoted file would read as "modified".
fn quote_yaml_key(s: &str) -> String {
    if key_needs_quoting(s) {
        quote_yaml_scalar(s)
    } else {
        s.to_string()
    }
}

/// A YAML mapping key needs quoting when it would otherwise misparse: a
/// leading reserved indicator (`@`, `*`, `&`, `?`, `|`, `>`, `%`, `` ` ``,
/// `!`, `#`, `-`, quotes, brackets/braces), an embedded `: ` or flow
/// punctuation, or surrounding whitespace. Plain package names
/// (`color-convert`, `is-odd`) clear all of these and stay bare, matching
/// yarn's own output.
fn key_needs_quoting(s: &str) -> bool {
    if s.is_empty() {
        return true;
    }
    let first = s.as_bytes()[0];
    if matches!(
        first,
        b'@' | b'*'
            | b'&'
            | b'?'
            | b'|'
            | b'>'
            | b'%'
            | b'`'
            | b'!'
            | b'#'
            | b'-'
            | b'['
            | b']'
            | b'{'
            | b'}'
            | b','
            | b'"'
            | b'\''
            | b' '
    ) {
        return true;
    }
    if s.ends_with(' ') {
        return true;
    }
    // A `:` anywhere makes the scalar ambiguous against a nested mapping; a
    // `#` can start a comment. Package names never contain either, so this
    // only fires on pathological inputs — quote them to be safe.
    s.contains(':') || s.contains(" #")
}

/// Emit the root project's `<name>@workspace:.` block. Yarn always records
/// the root importer as a workspace entry carrying its declared dependency
/// ranges, `version: 0.0.0-use.local`, `languageName: unknown`, `linkType:
/// soft`. The manifest is the source of truth for the dependency ranges.
fn write_berry_root_workspace(out: &mut String, manifest: &aube_manifest::PackageJson) -> String {
    let name = manifest.name.as_deref().unwrap_or("root");
    let header = format!("{name}@workspace:.");
    out.push_str(&quote_yaml_scalar(&header));
    out.push_str(":\n  version: 0.0.0-use.local\n  resolution: ");
    out.push_str(&quote_yaml_scalar(&header));
    out.push('\n');

    let emit_section = |out: &mut String, section: &str, deps: &BTreeMap<String, String>| {
        if deps.is_empty() {
            return;
        }
        out.push_str("  ");
        out.push_str(section);
        out.push_str(":\n");
        for (n, range) in deps {
            out.push_str("    ");
            out.push_str(&quote_yaml_key(n));
            out.push_str(": ");
            let spec = if range_has_protocol(range) {
                range.clone()
            } else {
                format!("npm:{range}")
            };
            out.push_str(&quote_yaml_scalar(&spec));
            out.push('\n');
        }
    };
    // Yarn nests dependencies / devDependencies into one `dependencies:`
    // map on the workspace block (the prod/dev split lives in package.json,
    // not the lockfile importer). optionalDependencies and peer metadata
    // get their own sections.
    let mut runtime: BTreeMap<String, String> = BTreeMap::new();
    for (n, r) in manifest
        .dependencies
        .iter()
        .chain(manifest.dev_dependencies.iter())
    {
        runtime.insert(n.clone(), r.clone());
    }
    emit_section(out, "dependencies", &runtime);
    let optional: BTreeMap<String, String> = manifest
        .optional_dependencies
        .iter()
        .map(|(n, r)| (n.clone(), r.clone()))
        .collect();
    emit_section(out, "optionalDependencies", &optional);

    out.push_str("  languageName: unknown\n  linkType: soft\n");
    header
}
