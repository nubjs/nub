use super::jsonc::strip_jsonc;
use super::raw::{BunEntry, RawBunLockfile};
use super::source::{
    bin_value_to_map, bun_key_to_alias_name, classify_bun_ident,
    rebase_workspace_scoped_local_source, resolve_nested_bun, resolve_workspace_dep, split_ident,
};
use crate::{DepType, DirectDep, Error, LockedPackage, LockfileGraph, PeerDepMeta};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Parse a bun.lock file into a LockfileGraph.
pub fn parse(path: &Path) -> Result<LockfileGraph, Error> {
    let raw_content = crate::read_lockfile(path)?;
    let cleaned = strip_jsonc(&raw_content);
    // `strip_jsonc` preserves byte offsets, so a serde_json error on
    // `cleaned` points at the same byte in `raw_content`. Feed the
    // raw file into the `NamedSource` so miette renders the user's
    // actual bun.lock (including comments) under the pointer.
    debug_assert_eq!(raw_content.len(), cleaned.len());

    let raw: RawBunLockfile = match serde_json::from_str(&cleaned) {
        Ok(v) => v,
        Err(e) => return Err(Error::parse_json_err(path, raw_content, &e)),
    };

    if raw.lockfile_version != 1 {
        return Err(Error::parse(
            path,
            format!(
                "bun.lock lockfileVersion {} is not supported (expected 1)",
                raw.lockfile_version
            ),
        ));
    }

    // Decode each raw array into a typed BunEntry so later passes don't
    // have to think about bun's per-source-type tuple layouts.
    let mut entries: BTreeMap<String, BunEntry> = BTreeMap::new();
    for (key, value) in &raw.packages {
        let entry = BunEntry::from_array(key, value).map_err(|e| Error::parse(path, e))?;
        entries.insert(key.clone(), entry);
    }
    let mut workspace_scopes: Vec<(&str, &str)> = raw
        .workspaces
        .iter()
        .filter(|(ws_path, _)| !ws_path.is_empty())
        .filter_map(|(ws_path, ws)| {
            ws.extra
                .get("name")
                .and_then(serde_json::Value::as_str)
                .map(|name| (name, ws_path.as_str()))
        })
        .collect();
    workspace_scopes.sort_by_key(|(name, _)| std::cmp::Reverse(name.len()));

    // First pass: parse (name, version) for each entry. bun.lock keys look
    // like the package name ("foo") for the hoisted version, or a nested
    // path ("parent/foo") when multiple versions exist.
    let mut key_info: BTreeMap<String, (String, String)> = BTreeMap::new();
    let mut packages: BTreeMap<String, LockedPackage> = BTreeMap::new();

    for (key, entry) in &entries {
        let Some((raw_name, raw_version)) = split_ident(&entry.ident) else {
            return Err(Error::parse(
                path,
                format!(
                    "could not parse ident '{}' for package '{}'",
                    entry.ident, key
                ),
            ));
        };

        // Detect non-registry specifiers embedded in bun's ident form
        // (`foo@github:user/repo#sha`, `foo@file:./vendor`,
        // `foo@https://…/pkg.tgz`, `foo@workspace:*`, …). The bun key
        // is always the alias-side name; the ident carries the
        // registry identity when bun wrote an npm-alias entry
        // (`foo@npm:real@1.2.3`). Reconstructing a `LocalSource`
        // here keeps the installer from re-routing every such entry
        // through the default registry and either 404-ing or
        // downloading the wrong tarball.
        let alias_name = bun_key_to_alias_name(key);
        let (name, version, local_source, alias_of) = classify_bun_ident(
            &alias_name,
            &raw_name,
            &raw_version,
            entry.integrity.as_deref(),
        )?;
        let local_source = local_source
            .map(|local| rebase_workspace_scoped_local_source(key, local, &workspace_scopes));
        key_info.insert(key.clone(), (name.clone(), version.clone()));

        let dep_path = format!("{name}@{version}");

        // Skip duplicate entries pointing at the same resolved package.
        if packages.contains_key(&dep_path) {
            continue;
        }

        // Collect transitive dep names; resolve to dep_paths in a second pass.
        let mut deps: BTreeMap<String, String> = BTreeMap::new();
        for n in entry
            .meta
            .dependencies
            .keys()
            .chain(entry.meta.optional_dependencies.keys())
        {
            deps.insert(n.clone(), String::new());
        }
        // Track which of those are optionals so the writer can split
        // them back into `optionalDependencies:` instead of dumping
        // everything under `dependencies:` on re-emit.
        let mut optional_deps: BTreeMap<String, String> = BTreeMap::new();
        for n in entry.meta.optional_dependencies.keys() {
            optional_deps.insert(n.clone(), String::new());
        }
        // Preserve bun's per-entry meta ranges (`"^4.1.0"`) so re-emit
        // doesn't collapse them to the resolved pin.
        let mut declared: BTreeMap<String, String> = BTreeMap::new();
        for (k, v) in entry
            .meta
            .dependencies
            .iter()
            .chain(entry.meta.optional_dependencies.iter())
        {
            declared.insert(k.clone(), v.clone());
        }

        // Normalize bun's `bin` meta into the typed BTreeMap while
        // preserving the raw shape (string vs object) on `extra_meta`
        // so the writer can echo the original representation back.
        let bin_map = bin_value_to_map(&name, &entry.meta.bin);
        let mut extra_meta = entry.meta.extra.clone();
        if !matches!(&entry.meta.bin, serde_json::Value::Null) {
            extra_meta.insert("bin".to_string(), entry.meta.bin.clone());
        }
        if !entry.meta.optional_peers.is_empty() {
            extra_meta.insert(
                "optionalPeers".to_string(),
                serde_json::Value::Array(
                    entry
                        .meta
                        .optional_peers
                        .iter()
                        .map(|s| serde_json::Value::String(s.clone()))
                        .collect(),
                ),
            );
        }

        // Peer declarations survive on their typed slot so drift
        // detection sees them; the meta map round-trip survives
        // through `extra_meta` for anything we don't model.
        let peer_dependencies = entry.meta.peer_dependencies.clone();
        let peer_dependencies_meta: BTreeMap<String, PeerDepMeta> = entry
            .meta
            .optional_peers
            .iter()
            .map(|n| (n.clone(), PeerDepMeta { optional: true }))
            .collect();

        packages.insert(
            dep_path.clone(),
            LockedPackage {
                name,
                version,
                integrity: entry.integrity.clone().filter(|s| !s.is_empty()),
                dependencies: deps,
                optional_dependencies: optional_deps,
                peer_dependencies,
                peer_dependencies_meta,
                dep_path,
                local_source,
                alias_of,
                os: entry.meta.os.iter().cloned().collect(),
                cpu: entry.meta.cpu.iter().cloned().collect(),
                libc: entry.meta.libc.iter().cloned().collect(),
                // Carry bun's registry tuple slot 1 (a non-default
                // registry/tarball URL) so re-emit doesn't drop it and
                // re-route a scoped/private dep to the default npm
                // registry on the next resolve. Empty/default → None.
                tarball_url: entry.registry_url.clone(),
                declared_dependencies: declared,
                bin: bin_map,
                extra_meta,
                ..Default::default()
            },
        );
    }

    // Second pass: resolve transitive deps by walking the bun nesting
    // hierarchy — for an entry at key "parent/foo", dep "bar" resolves to
    // "parent/foo/bar" → "parent/bar" → "bar".
    let mut resolved_by_dep_path: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for (key, entry) in &entries {
        let Some((name, version)) = key_info.get(key) else {
            continue;
        };
        let dep_path = format!("{name}@{version}");
        if resolved_by_dep_path.contains_key(&dep_path) {
            continue;
        }

        let mut resolved: BTreeMap<String, String> = BTreeMap::new();
        for dep_name in entry
            .meta
            .dependencies
            .keys()
            .chain(entry.meta.optional_dependencies.keys())
        {
            if let Some(target_key) = resolve_nested_bun(key, dep_name, &key_info)
                && let Some((dname, dver)) = key_info.get(&target_key)
            {
                let target_dep_path = format!("{dname}@{dver}");
                resolved.insert(
                    dep_name.clone(),
                    crate::npm::dep_path_tail(dname, &target_dep_path).to_string(),
                );
            }
        }
        resolved_by_dep_path.insert(dep_path, resolved);
    }
    for (dep_path, deps) in resolved_by_dep_path {
        if let Some(pkg) = packages.get_mut(&dep_path) {
            // Transfer resolved dep_path tails onto `dependencies` (the
            // combined map) and onto `optional_dependencies` for the
            // subset the parser flagged on first pass. Matches the
            // pnpm parser's split so every downstream consumer
            // (linker, writer, drift detection) sees the same shape
            // regardless of source format.
            let mut opts = BTreeMap::new();
            for name in pkg
                .optional_dependencies
                .keys()
                .cloned()
                .collect::<Vec<_>>()
            {
                if let Some(resolved) = deps.get(&name) {
                    opts.insert(name.clone(), resolved.clone());
                }
            }
            pkg.dependencies = deps;
            pkg.optional_dependencies = opts;
        }
    }

    // Workspace importers. bun.lock keys workspace paths as `""` for
    // the root and relative paths (`packages/app`, etc.) for each
    // workspace package. Each importer's direct deps resolve first
    // to a name-scoped override (`app/foo`) or path-scoped override
    // (`packages/app/foo`) when one exists, falling back to the
    // hoisted entry (`foo`). We don't walk intermediate ancestors
    // like `packages/foo` the way `resolve_nested_bun` does for
    // package-nesting — workspace path segments are directories, not
    // package-nesting scopes, so a partial walk could wrongly match a
    // literal npm package named `packages` that has its own nested
    // `foo` entry.
    let mut importers: BTreeMap<String, Vec<DirectDep>> = BTreeMap::new();
    let mut workspace_extra_fields: BTreeMap<String, BTreeMap<String, serde_json::Value>> =
        BTreeMap::new();
    for (ws_path, ws_raw) in &raw.workspaces {
        let importer_path = if ws_path.is_empty() {
            ".".to_string()
        } else {
            ws_path.clone()
        };
        let ws_name = (!ws_path.is_empty())
            .then(|| ws_raw.extra.get("name").and_then(serde_json::Value::as_str))
            .flatten();
        let mut direct: Vec<DirectDep> = Vec::new();
        let push_dep =
            |name: &str, specifier: &str, dep_type: DepType, direct: &mut Vec<DirectDep>| {
                if let Some(target_key) = resolve_workspace_dep(ws_path, ws_name, name, &key_info)
                    && let Some((dname, dver)) = key_info.get(&target_key)
                {
                    direct.push(DirectDep {
                        name: dname.clone(),
                        dep_path: format!("{dname}@{dver}"),
                        dep_type,
                        specifier: Some(specifier.to_string()),
                    });
                }
            };
        for (n, spec) in &ws_raw.dependencies {
            push_dep(n, spec, DepType::Production, &mut direct);
        }
        for (n, spec) in &ws_raw.dev_dependencies {
            push_dep(n, spec, DepType::Dev, &mut direct);
        }
        for (n, spec) in &ws_raw.optional_dependencies {
            push_dep(n, spec, DepType::Optional, &mut direct);
        }
        // Required workspace peers. bun links a workspace's
        // `peerDependencies` entry into that workspace's `node_modules`
        // unless it's listed in `optionalPeers` — so a required peer
        // resolves like a regular direct dep. Walking only
        // dependencies/devDependencies/optionalDependencies above drops
        // it, leaving `packages:` populated but `node_modules/<peer>`
        // missing and downstream imports broken. Skip names already
        // pushed as a regular dep (a dep that's also peer-declared is
        // linked once) and optional peers (bun only links them when
        // some other resolution supplies them, which the regular-dep
        // walk already covers).
        let optional_peers: std::collections::HashSet<&str> = ws_raw
            .extra
            .get("optionalPeers")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str)
            .collect();
        if let Some(peers) = ws_raw
            .extra
            .get("peerDependencies")
            .and_then(serde_json::Value::as_object)
        {
            for (n, spec) in peers {
                if optional_peers.contains(n.as_str()) || direct.iter().any(|d| &d.name == n) {
                    continue;
                }
                let spec = spec.as_str().unwrap_or_default();
                push_dep(n, spec, DepType::Production, &mut direct);
            }
        }
        importers.insert(importer_path.clone(), direct);
        if !ws_raw.extra.is_empty() {
            workspace_extra_fields.insert(importer_path, ws_raw.extra.clone());
        }
    }
    // The `importers` map always needs a `.` entry even when the
    // lockfile omits the `""` workspace entirely (hand-authored
    // fixtures sometimes do).
    importers.entry(".".to_string()).or_default();

    // Translate bun's unnamed `catalog:` / named `catalogs:` blocks
    // into the shared `LockfileGraph.catalogs` shape — outer key is
    // the catalog name (`default` for the unnamed one), inner key is
    // the package name. We don't have a separate resolved version on
    // bun's side, so the `specifier` and `version` track the same
    // value (the declared range); refreshing the catalog at resolve
    // time rewrites `version` to the picked pin.
    let mut catalogs_map: BTreeMap<String, BTreeMap<String, crate::CatalogEntry>> = BTreeMap::new();
    if !raw.catalog.is_empty() {
        let inner = raw
            .catalog
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    crate::CatalogEntry {
                        specifier: v.clone(),
                        version: v.clone(),
                    },
                )
            })
            .collect();
        catalogs_map.insert("default".to_string(), inner);
    }
    for (catalog_name, entries) in &raw.catalogs {
        let inner = entries
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    crate::CatalogEntry {
                        specifier: v.clone(),
                        version: v.clone(),
                    },
                )
            })
            .collect();
        catalogs_map.insert(catalog_name.clone(), inner);
    }

    Ok(LockfileGraph {
        importers,
        packages,
        bun_config_version: Some(raw.config_version),
        overrides: raw.overrides,
        patched_dependencies: raw.patched_dependencies,
        // Preserve bun's insertion order verbatim — dedupe to guard
        // against a hand-authored lockfile with repeats but never
        // reorder, so a re-emit is byte-identical to bun's own output.
        trusted_dependencies: {
            let mut seen = BTreeSet::new();
            let mut out: Vec<String> = Vec::with_capacity(raw.trusted_dependencies.len());
            for name in raw.trusted_dependencies {
                if seen.insert(name.clone()) {
                    out.push(name);
                }
            }
            out
        },
        catalogs: catalogs_map,
        extra_fields: raw.extra,
        workspace_extra_fields,
        ..Default::default()
    })
}
