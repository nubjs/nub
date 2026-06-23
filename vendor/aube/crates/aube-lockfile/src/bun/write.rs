use crate::{DirectDep, Error, LocalSource, LockedPackage, LockfileGraph};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Serialize a [`LockfileGraph`] as a bun v1 text lockfile.
///
/// Shares the hoist + nest algorithm with the npm writer via
/// [`crate::npm::build_hoist_tree`]. The segment list per entry is
/// rendered as bun's slash-delimited key form (`foo` or `parent/foo`),
/// and each entry body is a 4-tuple array
/// `[ident, resolved, metadata, integrity]` matching the parser.
///
/// Non-root workspace importers are emitted under their relative
/// project paths (e.g. `packages/app`) by reading each
/// `{importer}/package.json` from disk. The `packages` section is
/// built from the union of every importer's direct deps so workspace-
/// only transitive deps still get keyed into the hoist tree; workspace
/// packages themselves (identified by a `LocalSource::Link`) are
/// filtered out because bun tracks them separately in `workspaces`.
///
/// Lossy areas (same family as the npm writer):
///   - `resolved` is written as an empty string — we don't persist
///     origin URLs in [`LockedPackage`]. bun reparse is unaffected
///     because its parser explicitly ignores field 1.
///   - Peer-contextualized variants collapse to a single
///     `name@version` entry.
pub fn write(
    path: &Path,
    graph: &LockfileGraph,
    manifest: &aube_manifest::PackageJson,
) -> Result<(), Error> {
    use serde_json::{Value, json};

    // Canonicalize to one entry per (name, version). Skip workspace
    // packages (LocalSource::Link) — bun tracks those via the
    // `workspaces` map, not as top-level `packages` entries.
    let mut canonical: BTreeMap<String, &LockedPackage> = BTreeMap::new();
    for pkg in graph.packages.values() {
        if matches!(pkg.local_source, Some(LocalSource::Link(_))) {
            continue;
        }
        canonical.entry(pkg.spec_key()).or_insert(pkg);
        // Git- and url-sourced packages are referenced by their hashed
        // FS-safe dep_path (`ms@git+<hash>`) in importer DirectDeps and
        // parent dependency maps, which never matches the
        // `name@version` spec key. Alias them under the dep_path-
        // derived canonical key too, or the hoist tree drops them and
        // the entry vanishes from `packages` (bun then fails frozen
        // installs with "Failed to resolve root prod dependency").
        canonical
            .entry(crate::npm::canonical_key_from_dep_path(&pkg.dep_path))
            .or_insert(pkg);
    }

    // Build the hoist tree from every importer's direct deps (not just
    // the root's), so transitive deps declared only by a non-root
    // workspace still appear in the `packages` section. Skip
    // workspace-link deps for the same reason as the canonical filter.
    //
    // Dedupe by package name so duplicate direct deps across
    // workspaces don't confuse `build_hoist_tree` — its root-seeding
    // loop silently drops any queue entry whose segs already exist in
    // `placed`, which would mean the second workspace's transitive
    // deps never get walked. `graph.importers` is a BTreeMap, so `.`
    // iterates first and wins conflicts. When two workspaces declare
    // the same dep at different versions we still collapse to a
    // single top-level entry (the first-seen version); a proper fix
    // would emit `<workspace>/<dep>` nested entries per-workspace,
    // which is out of scope here.
    let mut all_roots: Vec<DirectDep> = Vec::new();
    let mut seen_names: BTreeSet<String> = BTreeSet::new();
    for deps in graph.importers.values() {
        for d in deps {
            if matches!(
                graph
                    .packages
                    .get(&d.dep_path)
                    .and_then(|p| p.local_source.as_ref()),
                Some(LocalSource::Link(_))
            ) {
                continue;
            }
            if !seen_names.insert(d.name.clone()) {
                continue;
            }
            all_roots.push(d.clone());
        }
    }
    let tree = crate::npm::build_hoist_tree(&canonical, &all_roots);

    // Non-root workspaces are read fresh from disk because the caller
    // doesn't thread them through — the root manifest is the only one
    // that might carry unsaved edits (from `aube add` / `remove`).
    // Silently falling back to an empty manifest when a read fails
    // keeps the writer best-effort: a missing workspace package.json
    // is odd but not fatal.
    let project_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut workspace_manifests: BTreeMap<String, aube_manifest::PackageJson> = BTreeMap::new();
    for importer_path in graph.importers.keys() {
        if importer_path == "." {
            continue;
        }
        let pj_path = project_dir.join(importer_path).join("package.json");
        let pj = aube_manifest::PackageJson::from_path(&pj_path).unwrap_or_default();
        workspace_manifests.insert(importer_path.clone(), pj);
    }

    // Build the `workspaces[path]` object for each importer.
    //
    // bun's root entry carries only `name` + dep sections (the root's
    // `version`/`bin`/`peerDependenciesMeta` live in the adjacent
    // `package.json`, so duplicating them into the lockfile would
    // produce a gratuitous diff against bun's own output). Non-root
    // entries carry the full picture — `version`, `bin`, dep sections,
    // and `optionalPeers` (bun's compact list form of
    // `peerDependenciesMeta[name].optional`) — because bun treats the
    // lockfile as authoritative for workspace resolution and doesn't
    // re-read every workspace package.json on install.
    //
    // Returns ordered `(key, value)` pairs rather than a `Map` so the
    // hand-written JSONC emitter can render them in bun's field order.
    fn build_workspace_pairs(
        pj: &aube_manifest::PackageJson,
        is_root: bool,
        ws_extras: Option<&BTreeMap<String, Value>>,
    ) -> Vec<(String, Value)> {
        let mut pairs: Vec<(String, Value)> = Vec::new();
        if let Some(name) = &pj.name {
            pairs.push(("name".to_string(), json!(name)));
        }
        if !is_root {
            if let Some(version) = &pj.version {
                pairs.push(("version".to_string(), json!(version)));
            }
            if let Some(bin) = pj.extra.get("bin") {
                pairs.push(("bin".to_string(), bin.clone()));
            }
        }
        if !pj.dependencies.is_empty() {
            pairs.push(("dependencies".to_string(), json!(pj.dependencies)));
        }
        if !pj.dev_dependencies.is_empty() {
            pairs.push(("devDependencies".to_string(), json!(pj.dev_dependencies)));
        }
        if !pj.optional_dependencies.is_empty() {
            pairs.push((
                "optionalDependencies".to_string(),
                json!(pj.optional_dependencies),
            ));
        }
        if !pj.peer_dependencies.is_empty() {
            pairs.push(("peerDependencies".to_string(), json!(pj.peer_dependencies)));
        }
        if !is_root
            && let Some(meta) = pj
                .extra
                .get("peerDependenciesMeta")
                .and_then(Value::as_object)
        {
            // `serde_json::Map` is workspace-configured with
            // `preserve_order`, so `iter()` yields insertion order.
            // bun emits `optionalPeers` alphabetized — sort here to
            // match, otherwise a package.json that declares
            // `peerDependenciesMeta` keys out of order would round-
            // trip to a different byte sequence than bun produces.
            let mut optional_peer_names: Vec<&String> = meta
                .iter()
                .filter(|(_, v)| v.get("optional").and_then(Value::as_bool).unwrap_or(false))
                .map(|(k, _)| k)
                .collect();
            optional_peer_names.sort();
            if !optional_peer_names.is_empty() {
                let optional_peers: Vec<Value> = optional_peer_names
                    .into_iter()
                    .map(|k| Value::String(k.clone()))
                    .collect();
                pairs.push(("optionalPeers".to_string(), Value::Array(optional_peers)));
            }
        }
        // Re-emit unknown workspace fields (anything bun writes that
        // we don't model above) so a bun-side roundtrip preserves
        // them verbatim. Skip keys we've already rendered to avoid
        // duplicating the serde-flatten collision with typed fields.
        if let Some(extras) = ws_extras {
            let already: BTreeSet<String> = pairs.iter().map(|(k, _)| k.clone()).collect();
            for (k, v) in extras {
                if already.contains(k) {
                    continue;
                }
                pairs.push((k.clone(), v.clone()));
            }
        }
        pairs
    }

    let mut workspace_pairs: Vec<(String, Vec<(String, Value)>)> = Vec::new();
    workspace_pairs.push((
        "".to_string(),
        build_workspace_pairs(manifest, true, graph.workspace_extra_fields.get(".")),
    ));
    for (importer_path, pj) in &workspace_manifests {
        let extras = graph.workspace_extra_fields.get(importer_path);
        workspace_pairs.push((
            importer_path.clone(),
            build_workspace_pairs(pj, false, extras),
        ));
    }

    let mut package_entries: Vec<(String, Value)> = Vec::new();
    for (segs, canonical_key) in &tree {
        let Some(pkg) = canonical.get(canonical_key).copied() else {
            continue;
        };

        // Bun's key form: `foo` (hoisted) or `parent/foo` (nested).
        // Scoped names like `@scope/name` already carry their own
        // internal `/` and are joined wholesale — bun's parser
        // recognizes `@`-prefixed segments as a single unit.
        let bun_key = segs.join("/");

        // Metadata object: transitive deps keyed by name → declared
        // range (e.g. `"^4.1.0"`). Fall back to the resolved pin when
        // the declared range is unknown — happens for lockfiles that
        // came through a format without declared ranges (pnpm's
        // `snapshots:` stores pins only). Filter out deps we don't
        // have a canonical entry for (e.g. dropped optional deps).
        //
        // Split the combined `dependencies` map back into
        // `dependencies` + `optionalDependencies` on emission so
        // packages that originally declared optionals round-trip
        // through bun's parser with the same classification.
        let mut deps_obj = serde_json::Map::new();
        let mut opt_deps_obj = serde_json::Map::new();
        for (dep_name, dep_value) in &pkg.dependencies {
            let key = crate::npm::child_canonical_key(dep_name, dep_value);
            if !canonical.contains_key(&key) {
                continue;
            }
            let rendered = pkg
                .declared_dependencies
                .get(dep_name)
                .cloned()
                .unwrap_or_else(|| {
                    crate::npm::dep_value_as_version(dep_name, dep_value).to_string()
                });
            if pkg.optional_dependencies.contains_key(dep_name) {
                opt_deps_obj.insert(dep_name.clone(), Value::String(rendered));
            } else {
                deps_obj.insert(dep_name.clone(), Value::String(rendered));
            }
        }
        let mut meta = serde_json::Map::new();
        if !deps_obj.is_empty() {
            meta.insert("dependencies".to_string(), Value::Object(deps_obj));
        }
        if !opt_deps_obj.is_empty() {
            meta.insert(
                "optionalDependencies".to_string(),
                Value::Object(opt_deps_obj),
            );
        }
        // Peer declarations survive on bun's per-entry meta.
        // Collapsing them into `dependencies` on re-emit is one of
        // the reported parity bugs, so round-trip through the typed
        // slot.
        if !pkg.peer_dependencies.is_empty() {
            let map: serde_json::Map<String, Value> = pkg
                .peer_dependencies
                .iter()
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                .collect();
            meta.insert("peerDependencies".to_string(), Value::Object(map));
        }
        // `optionalPeers` is bun's compact list form — derive from
        // `peer_dependencies_meta` when present, fall back to any
        // original extra_meta["optionalPeers"] array.
        let optional_peer_names: Vec<String> = pkg
            .peer_dependencies_meta
            .iter()
            .filter(|(_, v)| v.optional)
            .map(|(k, _)| k.clone())
            .collect();
        if !optional_peer_names.is_empty() {
            let mut sorted = optional_peer_names.clone();
            sorted.sort();
            let arr: Vec<Value> = sorted.into_iter().map(Value::String).collect();
            meta.insert("optionalPeers".to_string(), Value::Array(arr));
        }
        // Preserve optional-platform packages' filter metadata so
        // bun's platform-aware resolution still has what it needs
        // on the next install. bun's meta field order is
        // `os → cpu → libc → bin` (see `writePackageInfoObject` in
        // bun's `bun.lock.zig`), so emit the platform filters before
        // `bin` — otherwise a package carrying both round-trips to a
        // different byte sequence than bun produces.
        if !pkg.os.is_empty() {
            let arr: Vec<Value> = pkg.os.iter().map(|s| Value::String(s.clone())).collect();
            meta.insert("os".to_string(), Value::Array(arr));
        }
        if !pkg.cpu.is_empty() {
            let arr: Vec<Value> = pkg.cpu.iter().map(|s| Value::String(s.clone())).collect();
            meta.insert("cpu".to_string(), Value::Array(arr));
        }
        if !pkg.libc.is_empty() {
            let arr: Vec<Value> = pkg.libc.iter().map(|s| Value::String(s.clone())).collect();
            meta.insert("libc".to_string(), Value::Array(arr));
        }
        // Preserve the full `bin:` map — bun's meta block records
        // executables by name so `bun install --frozen-lockfile` can
        // recreate the `.bin` shims without re-reading each tarball's
        // manifest. pnpm collapses this to `hasBin: true`; we keep
        // both representations on `LockedPackage.bin` so either
        // writer can render byte-identical output. bun emits `bin`
        // LAST in the meta object (after os/cpu/libc), so it's
        // inserted here, after the platform filters above.
        //
        // Prefer the original shape captured in `extra_meta["bin"]`
        // (string vs object) so a bun-authored lockfile that wrote
        // `"bin": "./foo"` doesn't round-trip to `"bin": {"foo": "./foo"}`.
        // Skip empty-key entries — those are the placeholder bins
        // pnpm's lockfile synthesizes when it knows `hasBin: true`
        // but has no paths.
        if let Some(raw_bin) = pkg.extra_meta.get("bin")
            && !matches!(raw_bin, Value::Null)
        {
            meta.insert("bin".to_string(), raw_bin.clone());
        } else {
            let real_bins: serde_json::Map<String, Value> = pkg
                .bin
                .iter()
                .filter(|(k, _)| !k.is_empty())
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                .collect();
            if !real_bins.is_empty() {
                meta.insert("bin".to_string(), Value::Object(real_bins));
            }
        }
        // Extras: anything bun wrote on the meta block that we don't
        // model on `LockedPackage` (e.g. `deprecated`,
        // `hasInstallScript`). Skip keys we've already rendered to
        // avoid duplicate slots — the serde-flatten capture would
        // include them only if the typed slot was missing.
        const MODELED_META_KEYS: &[&str] = &[
            "dependencies",
            "optionalDependencies",
            "peerDependencies",
            "optionalPeers",
            "bin",
            "os",
            "cpu",
            "libc",
        ];
        for (k, v) in &pkg.extra_meta {
            if MODELED_META_KEYS.contains(&k.as_str()) {
                continue;
            }
            meta.insert(k.clone(), v.clone());
        }

        // npm-alias identity: bun writes the *registry* name and
        // resolved version as the ident when the hoist key is an
        // alias (`foo-alias: [bar@1.2.3, ...]`), not the alias name.
        // Aube's earlier writer emitted `{name}@{version}` which
        // collapsed to the alias name and produced a gratuitous diff
        // against bun's own output.
        let ident_name = pkg.alias_of.as_deref().unwrap_or(&pkg.name);
        // A hosted-git dependency the resolver fetched through a codeload
        // archive (or that pnpm recorded the same way) arrives as a
        // `RemoteTarball { git_hosted: true }` rather than a
        // `LocalSource::Git`. bun writes such a dep in its *git* form
        // (`name@github:owner/repo#<sha>` key + `owner-repo-sha` repo-tag),
        // and a cold-cache `bun install --frozen-lockfile` rejects the
        // registry-shaped collapse with `IntegrityCheckFailed` (it fetches
        // from GitHub and the registry tarball's integrity doesn't match).
        // Normalize the stand-in tarball back to a git source so the git
        // branch below renders bun's accepted form.
        let hosted_git = match pkg.local_source.as_ref() {
            Some(LocalSource::RemoteTarball(rt)) => rt.as_hosted_git_source(),
            _ => None,
        };
        let git_source = match pkg.local_source.as_ref() {
            Some(LocalSource::Git(git)) => Some(git.clone()),
            _ => hosted_git,
        };
        let entry = if let Some(git) = git_source.as_ref() {
            // bun's git tuple: `[ident, {meta}, "<owner>-<repo>-<commit>",
            // integrity]` — no registry-URL slot, and the repo-tag string
            // (bun's cache key) is required: bun 1.3.14 rejects a frozen
            // install without it. The ident keeps the git specifier form
            // (`ms@github:vercel/ms#<commit>`); a bun-authored lockfile
            // round-trips its raw ident tail via `pkg.version`, while a
            // fresh resolve carries the real semver there and the ident
            // is rebuilt from the git source.
            let version_is_git_ident = pkg.version.starts_with("github:")
                || pkg.version.starts_with("git+")
                || pkg.version.starts_with("git://")
                || pkg.version.starts_with("git@");
            // bun pins git idents to the SHORT (7-char) commit sha and
            // silently skips the package on install when the ident
            // carries the full 40-char form (bun 1.3.14 exits 0 but
            // materializes nothing) — truncate exactly like bun does.
            // Non-sha committishes (tags, branches) pass through.
            let commit = if git.resolved.is_empty() {
                git.committish.clone().unwrap_or_default()
            } else {
                git.resolved.clone()
            };
            let commit = if commit.len() == 40 && commit.chars().all(|c| c.is_ascii_hexdigit()) {
                commit[..7].to_string()
            } else {
                commit
            };
            let ident_tail = if version_is_git_ident {
                pkg.version.clone()
            } else if let Some(hosted) = crate::parse_hosted_git(&git.url) {
                let shorthand = match hosted.host {
                    crate::HostedGitHost::GitHub => "github",
                    crate::HostedGitHost::GitLab => "gitlab",
                    crate::HostedGitHost::Bitbucket => "bitbucket",
                };
                format!("{shorthand}:{}/{}#{commit}", hosted.owner, hosted.repo)
            } else if git.url.starts_with("git://") || git.url.starts_with("git+") {
                format!("{}#{commit}", git.url)
            } else {
                format!("git+{}#{commit}", git.url)
            };
            // Tag commit: prefer the committish embedded in a round-
            // tripped ident (bun pins short SHAs there) so the tag and
            // ident stay in step; full SHAs are accepted too.
            let tag_commit = ident_tail
                .rsplit_once('#')
                .map(|(_, c)| c.to_string())
                .unwrap_or(commit);
            let repo_tag = match crate::parse_hosted_git(&git.url) {
                Some(hosted) => format!("{}-{}-{tag_commit}", hosted.owner, hosted.repo),
                None => {
                    let stem = git
                        .url
                        .trim_end_matches('/')
                        .rsplit('/')
                        .next()
                        .unwrap_or("repo")
                        .trim_end_matches(".git");
                    format!("{stem}-{tag_commit}")
                }
            };
            let mut elems = vec![
                Value::String(format!("{ident_name}@{ident_tail}")),
                Value::Object(meta),
                Value::String(repo_tag),
            ];
            // The integrity element is bun's hash of the artifact *it*
            // fetches and is verified on cold installs (bun 1.3.14
            // fails IntegrityCheckFailed on a mismatch), so only a
            // value that round-tripped from a bun-authored lockfile may
            // be re-emitted. Fresh resolves carry aube's own tarball
            // SRI on `pkg.integrity`, which hashes a different artifact
            // — omit the element instead (bun accepts the 3-tuple).
            if version_is_git_ident
                && let Some(integrity) = pkg.integrity.clone().filter(|s| !s.is_empty())
            {
                elems.push(Value::String(integrity));
            }
            Value::Array(elems)
        } else {
            let ident = format!("{}@{}", ident_name, pkg.version);
            let integrity = pkg.integrity.clone().unwrap_or_default();
            // Slot 1 is bun's registry/tarball URL. A non-default
            // registry preserves the full URL here; the default registry
            // is the empty string (bun's "use default" marker). Re-emit
            // whatever the parse carried on `tarball_url` so a
            // scoped/private-registry bun.lock round-trips without
            // re-routing to the default npm registry.
            let registry = pkg.tarball_url.clone().unwrap_or_default();
            Value::Array(vec![
                Value::String(ident),
                Value::String(registry),
                Value::Object(meta),
                Value::String(integrity),
            ])
        };
        package_entries.push((bun_key, entry));
    }

    // Workspace packages live as `[name@workspace:path]` entries
    // alongside the registry packages — bun's `bun install
    // --frozen-lockfile` walks them out of `packages:` to wire up
    // workspace deps without re-reading every workspace package.json.
    // Dropping them on rewrite produces a lockfile that errors
    // "Cannot find package" on subsequent installs.
    //
    // Tuple shape: `[ident]` when the workspace declares no deps,
    // `[ident, { meta }]` when it does. No empty-string slot, no
    // integrity — bun's parser keys off element type, not position.
    //
    // Workspace deps may reference *other* workspace packages
    // (`app` → `lib` via `workspace:*`). Those targets aren't in
    // `canonical` (which excludes `LocalSource::Link`), so build a
    // separate set of workspace dep_paths and accept either when
    // checking whether a dep target is reachable.
    let workspace_dep_paths: BTreeSet<String> = graph
        .packages
        .values()
        .filter(|p| matches!(p.local_source, Some(LocalSource::Link(_))))
        .map(|p| p.dep_path.clone())
        .collect();
    let mut emitted_workspace_keys: BTreeSet<String> = BTreeSet::new();
    for pkg in graph.packages.values() {
        let Some(LocalSource::Link(rel_path)) = pkg.local_source.as_ref() else {
            continue;
        };
        let key = pkg.alias_of.as_deref().unwrap_or(&pkg.name).to_string();
        if !emitted_workspace_keys.insert(key.clone()) {
            continue;
        }
        // Build the ident as `name@workspace:<spec>`. Prefer the
        // original specifier captured on `version` (bun-roundtripped
        // graphs carry `version = "workspace:packages/app"`), and
        // fall back to the `LocalSource::Link` path for graphs
        // synthesized by aube's resolver where `version` is the
        // workspace's real semver. The ident must always reflect
        // the workspace specifier so bun's parser routes the entry
        // into its workspace logic.
        let ident_name = pkg.alias_of.as_deref().unwrap_or(&pkg.name);
        let workspace_spec = if pkg.version.starts_with("workspace:") {
            pkg.version
                .strip_prefix("workspace:")
                .unwrap_or("*")
                .to_string()
        } else {
            let path_str = rel_path.to_string_lossy();
            if path_str.is_empty() || path_str == "." {
                "*".to_string()
            } else {
                path_str.into_owned()
            }
        };
        let ident = format!("{ident_name}@workspace:{workspace_spec}");

        let mut deps_obj = serde_json::Map::new();
        let mut opt_deps_obj = serde_json::Map::new();
        for (dep_name, dep_value) in &pkg.dependencies {
            let canonical_key = crate::npm::child_canonical_key(dep_name, dep_value);
            if !canonical.contains_key(&canonical_key)
                && !workspace_dep_paths.contains(&canonical_key)
            {
                continue;
            }
            let rendered = pkg
                .declared_dependencies
                .get(dep_name)
                .cloned()
                .unwrap_or_else(|| {
                    crate::npm::dep_value_as_version(dep_name, dep_value).to_string()
                });
            if pkg.optional_dependencies.contains_key(dep_name) {
                opt_deps_obj.insert(dep_name.clone(), Value::String(rendered));
            } else {
                deps_obj.insert(dep_name.clone(), Value::String(rendered));
            }
        }
        let entry = if deps_obj.is_empty() && opt_deps_obj.is_empty() {
            Value::Array(vec![Value::String(ident)])
        } else {
            let mut meta = serde_json::Map::new();
            if !deps_obj.is_empty() {
                meta.insert("dependencies".to_string(), Value::Object(deps_obj));
            }
            if !opt_deps_obj.is_empty() {
                meta.insert(
                    "optionalDependencies".to_string(),
                    Value::Object(opt_deps_obj),
                );
            }
            Value::Array(vec![Value::String(ident), Value::Object(meta)])
        };
        package_entries.push((key, entry));
    }
    package_entries.sort_by(|a, b| a.0.cmp(&b.0));

    // Echo back the parsed `configVersion` (default 1 for older v1.1
    // lockfiles that predate the field) so a bun-bumped value round-
    // trips instead of silently downgrading on re-emit.
    let config_version = graph.bun_config_version.unwrap_or(1);

    // Collect top-level blocks bun understands natively. Overrides /
    // catalog / catalogs / patchedDependencies / trustedDependencies
    // are all round-tripped from the parsed graph; anything else the
    // lockfile carried drops through `graph.extra_fields`.
    // bun's native top-level block order is
    // `trustedDependencies → patchedDependencies → overrides →
    // catalog → catalogs` (see `bun.lock.zig`'s writer, which emits
    // them in that sequence between `workspaces` and `packages`).
    // Push in the same order so a bun-authored lockfile round-trips
    // byte-identically rather than reshuffling the metadata blocks.
    let mut top_level_extras: Vec<(String, Value)> = Vec::new();
    if !graph.trusted_dependencies.is_empty() {
        let arr: Vec<Value> = graph
            .trusted_dependencies
            .iter()
            .map(|s| Value::String(s.clone()))
            .collect();
        top_level_extras.push(("trustedDependencies".to_string(), Value::Array(arr)));
    }
    if !graph.patched_dependencies.is_empty() {
        let mut obj = serde_json::Map::new();
        for (k, v) in &graph.patched_dependencies {
            obj.insert(k.clone(), Value::String(v.clone()));
        }
        top_level_extras.push(("patchedDependencies".to_string(), Value::Object(obj)));
    }
    if !graph.overrides.is_empty() {
        let mut obj = serde_json::Map::new();
        for (k, v) in &graph.overrides {
            obj.insert(k.clone(), Value::String(v.clone()));
        }
        top_level_extras.push(("overrides".to_string(), Value::Object(obj)));
    }
    if let Some(default_catalog) = graph.catalogs.get("default") {
        let mut obj = serde_json::Map::new();
        for (k, v) in default_catalog {
            obj.insert(k.clone(), Value::String(v.specifier.clone()));
        }
        if !obj.is_empty() {
            top_level_extras.push(("catalog".to_string(), Value::Object(obj)));
        }
    }
    let named_catalogs: BTreeMap<&String, &BTreeMap<String, crate::CatalogEntry>> = graph
        .catalogs
        .iter()
        .filter(|(k, _)| k.as_str() != "default")
        .collect();
    if !named_catalogs.is_empty() {
        let mut outer = serde_json::Map::new();
        for (name, entries) in named_catalogs {
            let mut inner = serde_json::Map::new();
            for (k, v) in entries {
                inner.insert(k.clone(), Value::String(v.specifier.clone()));
            }
            outer.insert(name.clone(), Value::Object(inner));
        }
        top_level_extras.push(("catalogs".to_string(), Value::Object(outer)));
    }
    // Finally, anything else the parser stashed in `extra_fields`
    // (future bun bumps or hand-authored blocks we don't model).
    const MODELED_TOP_KEYS: &[&str] = &[
        "lockfileVersion",
        "configVersion",
        "workspaces",
        "packages",
        "overrides",
        "patchedDependencies",
        "trustedDependencies",
        "catalog",
        "catalogs",
    ];
    for (k, v) in &graph.extra_fields {
        if MODELED_TOP_KEYS.contains(&k.as_str()) {
            continue;
        }
        top_level_extras.push((k.clone(), v.clone()));
    }

    let body = format_bun_lockfile(
        &workspace_pairs,
        &package_entries,
        config_version,
        &top_level_extras,
    );
    crate::atomic_write_lockfile(path, body.as_bytes())?;
    Ok(())
}

/// Hand-written JSONC emitter matching bun 1.2's `bun.lock` style.
///
/// bun's output has an idiosyncratic shape — nested object fields use
/// trailing commas (standard JSONC) except `packages:` itself (the
/// last top-level field, where bun omits the trailing comma and leaves
/// the closing brace bare) — and every `packages:` entry is serialized
/// as a single-line array with a blank separator above. serde_json's
/// `to_string_pretty` can't express any of that, so we build the
/// output by hand.
///
/// `workspaces` is the ordered list of `(path, pairs)` where `path` is
/// the workspace key in `workspaces[]` (`""` for the root,
/// `"packages/app"` for non-root) and `pairs` are the ordered
/// key/value entries inside. `package_entries` are the `packages:`
/// map in BTreeMap order — each is rendered as a single-line
/// `[ident, "", {meta}, integrity]` array.
///
/// `config_version` is echoed back into the output as bun itself does —
/// hardcoding would silently downgrade the field when bun bumps it.
fn format_bun_lockfile(
    workspaces: &[(String, Vec<(String, serde_json::Value)>)],
    package_entries: &[(String, serde_json::Value)],
    config_version: u32,
    top_level_extras: &[(String, serde_json::Value)],
) -> String {
    let mut out = String::with_capacity(8192);
    out.push_str("{\n");
    out.push_str("  \"lockfileVersion\": 1,\n");
    out.push_str(&format!("  \"configVersion\": {config_version},\n"));

    // Workspaces block. Emits root (`""`) first, then each non-root
    // workspace in the order the caller supplied.
    out.push_str("  \"workspaces\": {\n");
    for (path, pairs) in workspaces.iter() {
        out.push_str(&format!(
            "    {}: {{\n",
            serde_json::to_string(path).unwrap()
        ));
        // Keys bun renders as multi-line blocks inside a workspace
        // entry. Other object-valued keys (`bin`) stay inline to
        // match bun's `"bin": { "name": "./path" }` form.
        const MULTILINE_KEYS: &[&str] = &[
            "dependencies",
            "devDependencies",
            "optionalDependencies",
            "peerDependencies",
        ];
        for (k, v) in pairs.iter() {
            let key_str = serde_json::to_string(k).unwrap();
            // bun emits a trailing comma after every workspace-level
            // field, including the last one — `},` closes the block.
            match v {
                serde_json::Value::Object(map)
                    if !map.is_empty() && MULTILINE_KEYS.contains(&k.as_str()) =>
                {
                    out.push_str(&format!("      {key_str}: {{\n"));
                    for (dk, dv) in map {
                        out.push_str(&format!(
                            "        {}: {},\n",
                            serde_json::to_string(dk).unwrap(),
                            inline_json(dv, 0)
                        ));
                    }
                    out.push_str("      },\n");
                }
                _ => {
                    out.push_str(&format!("      {key_str}: {},\n", inline_json(v, 0)));
                }
            }
        }
        // bun emits a trailing comma on every workspace entry,
        // including the last one — the outer `"workspaces"` map's
        // own trailing comma still closes the block below.
        out.push_str("    },\n");
    }
    out.push_str("  },\n");

    // Top-level extras (`overrides`, `catalog`, `catalogs`,
    // `patchedDependencies`, `trustedDependencies`, plus anything
    // the parser captured in `extra_fields`). Emit in the order the
    // caller supplied so a bun-first write preserves bun's own
    // field order on re-read.
    for (k, v) in top_level_extras {
        let key_str = serde_json::to_string(k).unwrap();
        match v {
            serde_json::Value::Object(map) if !map.is_empty() => {
                out.push_str(&format!("  {key_str}: {{\n"));
                for (dk, dv) in map {
                    out.push_str(&format!(
                        "    {}: {},\n",
                        serde_json::to_string(dk).unwrap(),
                        inline_json(dv, 0)
                    ));
                }
                out.push_str("  },\n");
            }
            _ => {
                out.push_str(&format!("  {key_str}: {},\n", inline_json(v, 0)));
            }
        }
    }

    // Packages block. Each entry is its own line; bun separates
    // entries with a blank line (an empty line between every
    // consecutive pair). `packages:` is bun's last top-level field and
    // gets no trailing comma on its closing brace.
    out.push_str("  \"packages\": {\n");
    for (i, (key, entry)) in package_entries.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&format!(
            "    {}: {},\n",
            serde_json::to_string(key).unwrap(),
            inline_json(entry, 0)
        ));
    }
    out.push_str("  }\n");
    out.push_str("}\n");
    out
}

/// Serialize a JSON value inline in bun's spaced style — objects as
/// `{ "k": v, "k2": v2 }` (with a trailing space before `}` and a
/// trailing comma before the close), arrays as `["a", "b"]` (no
/// trailing comma). Recurses into nested objects/arrays.
///
/// `base_indent` is reserved for a future multi-line fallback when an
/// object gets too wide; bun in 1.2 keeps even the larger metadata
/// objects on one line, so we currently ignore it.
fn inline_json(value: &serde_json::Value, _base_indent: usize) -> String {
    use serde_json::Value;
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(_) => serde_json::to_string(value).unwrap(),
        Value::Array(arr) => {
            let parts: Vec<String> = arr.iter().map(|v| inline_json(v, 0)).collect();
            format!("[{}]", parts.join(", "))
        }
        Value::Object(map) => {
            if map.is_empty() {
                return "{}".to_string();
            }
            let parts: Vec<String> = map
                .iter()
                .map(|(k, v)| {
                    format!(
                        "{}: {}",
                        serde_json::to_string(k).unwrap(),
                        inline_json(v, 0)
                    )
                })
                .collect();
            // bun writes `{ k: v, k2: v2 }` — spaces inside, no trailing comma.
            format!("{{ {} }}", parts.join(", "))
        }
    }
}
