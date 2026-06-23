use super::berry::{file_protocol_source, strip_hash_fragment};
use crate::{DepType, DirectDep, Error, LocalSource, LockedPackage, LockfileGraph};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Parse a yarn classic (v1) lockfile's pre-read contents.
pub(super) fn parse_classic_str(
    path: &Path,
    content: &str,
    manifest: &aube_manifest::PackageJson,
) -> Result<LockfileGraph, Error> {
    let blocks = tokenize_blocks(content).map_err(|e| Error::parse(path, e))?;

    // spec_to_dep_path maps each specifier (e.g. "is-odd@^3.0.0") to its
    // resolved dep_path ("is-odd@3.0.1"). Used for resolving direct deps
    // from package.json ranges and transitive dep references.
    let mut spec_to_dep_path: BTreeMap<String, String> = BTreeMap::new();
    let mut packages: BTreeMap<String, LockedPackage> = BTreeMap::new();

    for block in &blocks {
        let version = block
            .fields
            .get("version")
            .ok_or_else(|| {
                Error::parse(
                    path,
                    format!("yarn.lock block {:?} has no version", block.specs),
                )
            })?
            .clone();

        // All specs in the key map to the same resolved package.
        // Extract the package name from the first spec.
        let name = parse_spec_name(&block.specs[0]).ok_or_else(|| {
            Error::parse(
                path,
                format!(
                    "could not parse package name from yarn.lock spec '{}'",
                    block.specs[0]
                ),
            )
        })?;
        // npm-protocol alias: `<alias>@npm:<real-name>@<version>`. `name`
        // stays the alias (matches the npm parser's convention — it keys
        // node_modules/<alias>/ and is what consumers refer to); the real
        // registry name lives in `alias_of` so registry_name() returns it.
        // Scan every spec — our writer emits the canonical `name@version`
        // first and the npm-alias spec alongside it, so checking only
        // specs[0] would miss the alias on round-trips.
        let alias_of = block
            .specs
            .iter()
            .find_map(|s| parse_npm_alias_real_name(s))
            .filter(|real| real.as_str() != name);

        // A `link:` / `file:` / `portal:` spec points at a local on-disk
        // package, not a registry one — yarn records `version "0.0.0"`
        // and no `resolved` URL. Without a `LocalSource` the linker
        // treats it as a registry dep and builds a
        // `<name>/-/<name>-0.0.0.tgz` URL that 404s, aborting the whole
        // install. The dep_path is keyed by `LocalSource::dep_path` (the
        // FS-safe hashed form) exactly like the berry parser keys its
        // local packages.
        let local_source = block
            .specs
            .iter()
            .find_map(|s| classic_local_source(s, &name));
        let dep_path = match &local_source {
            Some(src) => src.dep_path(&name),
            None => format!("{name}@{version}"),
        };

        for spec in &block.specs {
            spec_to_dep_path.insert(spec.clone(), dep_path.clone());
        }

        // Only insert the first occurrence; dedup is fine because yarn.lock
        // already guarantees unique (name, version) entries.
        if !packages.contains_key(&dep_path) {
            // Yarn records the declared ranges on each block's
            // `dependencies:` subsection exactly as they appear in the
            // package's own manifest — preserve them so re-emit keeps
            // the original specifiers.
            let declared: BTreeMap<String, String> = block
                .dependencies
                .iter()
                .map(|(n, r)| (n.clone(), r.clone()))
                .collect();
            packages.insert(
                dep_path.clone(),
                LockedPackage {
                    name: name.clone(),
                    version: version.clone(),
                    integrity: block.fields.get("integrity").cloned(),
                    // Store raw "name@range" pairs for now; resolve below.
                    dependencies: block
                        .dependencies
                        .iter()
                        .map(|(n, r)| (n.clone(), format!("{n}@{r}")))
                        .collect(),
                    dep_path,
                    declared_dependencies: declared,
                    alias_of: alias_of.clone(),
                    local_source: local_source.clone(),
                    ..Default::default()
                },
            );
        }
    }

    // Second pass: resolve transitive dep references to dep_paths.
    let mut resolved: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for (dep_path, pkg) in &packages {
        let mut deps: BTreeMap<String, String> = BTreeMap::new();
        for (name, raw_spec) in &pkg.dependencies {
            if let Some(resolved_path) = spec_to_dep_path.get(raw_spec) {
                deps.insert(
                    name.clone(),
                    crate::npm::dep_path_tail(name, resolved_path).to_string(),
                );
            }
        }
        resolved.insert(dep_path.clone(), deps);
    }
    for (dep_path, deps) in resolved {
        if let Some(pkg) = packages.get_mut(&dep_path) {
            pkg.dependencies = deps;
        }
    }

    // Build direct deps from the manifest, cross-referencing against spec_to_dep_path.
    let mut direct: Vec<DirectDep> = Vec::new();
    let push_direct = |name: &str, range: &str, dep_type: DepType, direct: &mut Vec<DirectDep>| {
        let spec = format!("{name}@{range}");
        if let Some(dep_path) = spec_to_dep_path.get(&spec) {
            direct.push(DirectDep {
                name: name.to_string(),
                dep_path: dep_path.clone(),
                dep_type,
                specifier: None,
            });
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

    // A yarn v1 yarn.lock is a flat resolution list with NO workspace /
    // importer structure — workspace membership lives only in the root
    // package.json `workspaces` globs plus the on-disk member
    // package.json files. Without reconstructing the members here, a
    // yarn-source workspace converts to a single lone `.` importer and
    // the target PM frozen-rejects (pnpm ERR_PNPM_OUTDATED_LOCKFILE on a
    // child package.json, npm "Missing" members, bun "lockfile had
    // changes"). Discover each member from the globs + disk and build
    // its importer, cross-referencing its declared ranges against the
    // flat resolution list the same way the root importer is built.
    if let Some(workspaces) = &manifest.workspaces {
        let project_dir = path.parent().unwrap_or_else(|| Path::new("."));
        for member_dir in discover_workspace_members(project_dir, workspaces.patterns()) {
            let member_pj_path = project_dir.join(&member_dir).join("package.json");
            let Ok(member_pj) = aube_manifest::PackageJson::from_path(&member_pj_path) else {
                continue;
            };
            let mut member_direct: Vec<DirectDep> = Vec::new();
            let mut push_member = |name: &str, range: &str, dep_type: DepType| {
                let spec = format!("{name}@{range}");
                if let Some(dep_path) = spec_to_dep_path.get(&spec) {
                    member_direct.push(DirectDep {
                        name: name.to_string(),
                        dep_path: dep_path.clone(),
                        dep_type,
                        specifier: Some(range.to_string()),
                    });
                }
            };
            for (name, range) in &member_pj.dependencies {
                push_member(name, range, DepType::Production);
            }
            for (name, range) in &member_pj.dev_dependencies {
                push_member(name, range, DepType::Dev);
            }
            for (name, range) in &member_pj.optional_dependencies {
                push_member(name, range, DepType::Optional);
            }
            importers.insert(member_dir, member_direct);
        }
    }

    Ok(LockfileGraph {
        importers,
        packages,
        ..Default::default()
    })
}

/// Expand the root manifest's `workspaces` globs against the on-disk
/// tree, returning each member's project-relative directory (POSIX
/// `/`-separated, the importer-key form) that contains a `package.json`.
/// Mirrors npm/yarn-classic workspace globbing: a `packages/*` pattern
/// matches direct child directories; an explicit `packages/app` matches
/// that one directory.
fn discover_workspace_members(project_dir: &Path, patterns: &[String]) -> Vec<String> {
    let mut members: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for pattern in patterns {
        // Negation patterns (`!packages/excluded`) and the root itself
        // aren't member sources here.
        if pattern.starts_with('!') || pattern == "." {
            continue;
        }
        let glob_pat = project_dir.join(pattern);
        let Some(glob_str) = glob_pat.to_str() else {
            continue;
        };
        let Ok(paths) = glob::glob(glob_str) else {
            continue;
        };
        for entry in paths.flatten() {
            if !entry.is_dir() || !entry.join("package.json").is_file() {
                continue;
            }
            let Ok(rel) = entry.strip_prefix(project_dir) else {
                continue;
            };
            // Importer keys are POSIX-relative (`packages/app`), never
            // the host's `\`-separated form.
            let rel_posix = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            if !rel_posix.is_empty() {
                members.insert(rel_posix);
            }
        }
    }
    members.into_iter().collect()
}

#[derive(Debug)]
struct Block {
    /// Specifier keys: each is a "name@range" string.
    specs: Vec<String>,
    /// Flat scalar fields (version, resolved, integrity, etc.)
    fields: BTreeMap<String, String>,
    /// Nested dependencies section: name -> range
    dependencies: BTreeMap<String, String>,
}

/// Tokenize the yarn.lock body into blocks. This is a line-based parser that
/// recognizes:
/// - Comments (`# …`) and blank lines
/// - Header lines ending in `:` (block keys)
/// - Fields indented with 2 spaces
/// - A special nested `dependencies:` section indented with 4 spaces
fn tokenize_blocks(content: &str) -> Result<Vec<Block>, String> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut current: Option<Block> = None;
    let mut in_deps = false;

    for (lineno, raw_line) in content.lines().enumerate() {
        let line_num = lineno + 1;

        // Strip trailing whitespace but preserve leading indentation
        let line = raw_line.trim_end();
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }

        let indent = line.len() - line.trim_start().len();

        // Top-level: block header (one or more comma-separated specs ending in `:`)
        if indent == 0 {
            if let Some(b) = current.take() {
                blocks.push(b);
            }
            in_deps = false;

            let header = line.trim_end_matches(':').trim();
            if !line.ends_with(':') {
                return Err(format!(
                    "line {line_num}: expected block header ending in ':', got '{line}'"
                ));
            }

            let specs = parse_header_specs(header).map_err(|e| format!("line {line_num}: {e}"))?;
            current = Some(Block {
                specs,
                fields: BTreeMap::new(),
                dependencies: BTreeMap::new(),
            });
            continue;
        }

        let block = current.as_mut().ok_or_else(|| {
            format!("line {line_num}: unexpected indented content before any block header")
        })?;

        if indent == 2 {
            in_deps = false;
            let body = line.trim_start();

            // Check for nested section markers (e.g. `dependencies:`)
            if body.ends_with(':') {
                let section = body.trim_end_matches(':').trim();
                if section == "dependencies"
                    || section == "optionalDependencies"
                    || section == "peerDependencies"
                {
                    // Only track `dependencies:` for our resolution graph; ignore others.
                    in_deps = section == "dependencies";
                    continue;
                }
                // Unknown 2-space section header — ignore.
                continue;
            }

            let (key, value) = split_key_value(body)
                .ok_or_else(|| format!("line {line_num}: could not parse '{body}'"))?;
            block.fields.insert(key, value);
        } else if indent >= 4 && in_deps {
            let body = line.trim_start();
            let (name, range) = split_key_value(body)
                .ok_or_else(|| format!("line {line_num}: could not parse dep '{body}'"))?;
            block.dependencies.insert(name, range);
        }
        // Deeper indents outside `dependencies:` are ignored.
    }

    if let Some(b) = current.take() {
        blocks.push(b);
    }

    Ok(blocks)
}

/// Parse a header like `"foo@^1.0.0", "foo@^1.1.0"` or `foo@^1.0.0` into specs.
fn parse_header_specs(header: &str) -> Result<Vec<String>, String> {
    let mut specs = Vec::new();
    for raw in header.split(',') {
        let s = raw.trim();
        let unquoted = unquote_yarn_scalar(s);
        if unquoted.is_empty() {
            return Err(format!("empty spec in header '{header}'"));
        }
        specs.push(unquoted.to_string());
    }
    if specs.is_empty() {
        return Err(format!("no specs parsed from header '{header}'"));
    }
    Ok(specs)
}

/// Split a body line like `version "1.2.3"` or `foo "^1.0.0"` into (key, value).
/// Values may be quoted or unquoted.
fn split_key_value(line: &str) -> Option<(String, String)> {
    let (key, rest) = line.split_once(char::is_whitespace)?;
    let value = rest.trim();
    Some((
        unquote_yarn_scalar(key).to_string(),
        unquote_yarn_scalar(value).to_string(),
    ))
}

fn unquote_yarn_scalar(value: &str) -> &str {
    if (value.starts_with('"') && value.ends_with('"') && value.len() >= 2)
        || (value.starts_with('\'') && value.ends_with('\'') && value.len() >= 2)
    {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

/// Extract the package name from a spec like `foo@^1.0.0` or `@scope/pkg@^1.0.0`.
pub(super) fn parse_spec_name(spec: &str) -> Option<String> {
    if let Some(rest) = spec.strip_prefix('@') {
        // Scoped package: find the '@' that comes after the '/'
        let slash = rest.find('/')?;
        let after_slash = &rest[slash + 1..];
        let at = after_slash.find('@')?;
        Some(format!("@{}", &rest[..slash + 1 + at]))
    } else {
        let at = spec.find('@')?;
        Some(spec[..at].to_string())
    }
}

/// Detect a yarn-classic local-package protocol on a spec key and map
/// it to a [`LocalSource`]. Classic encodes the protocol in the spec
/// *range* (`name@link:./path`, `name@file:./path`, `name@portal:./path`)
/// rather than in a separate `resolution:` field the way berry does.
/// Registry and remote (`http(s):`, git) specs return `None` — they
/// resolve through the normal `name@version` path.
fn classic_local_source(spec: &str, name: &str) -> Option<LocalSource> {
    let range = spec.strip_prefix(name)?.strip_prefix('@')?;
    let (protocol, body) = range.split_once(':')?;
    match protocol {
        "link" => Some(LocalSource::Link(PathBuf::from(strip_hash_fragment(body)))),
        "file" => Some(file_protocol_source(body)),
        "portal" => Some(LocalSource::Portal(PathBuf::from(strip_hash_fragment(
            body,
        )))),
        _ => None,
    }
}

/// Detect a yarn npm-protocol alias spec like
/// `<alias>@npm:<real-name>@<version-or-range>` and return the real
/// registry name. Returns `None` for non-aliased specs (the common case).
///
/// Yarn lets a consumer rename a dep on import — `react-loadable: "npm:@docusaurus/react-loadable@5.5.2"`
/// installs `@docusaurus/react-loadable` under `node_modules/react-loadable/`.
/// The lockfile records the alias as the spec key; without surfacing the
/// real name into [`LockedPackage::alias_of`], every registry/store call
/// site would hit the alias-qualified URL and 404.
pub(super) fn parse_npm_alias_real_name(spec: &str) -> Option<String> {
    let after_alias = if let Some(rest) = spec.strip_prefix('@') {
        let slash = rest.find('/')?;
        let after_slash = &rest[slash + 1..];
        let at = after_slash.find('@')?;
        &after_slash[at + 1..]
    } else {
        let at = spec.find('@')?;
        &spec[at + 1..]
    };
    let after_protocol = after_alias.strip_prefix("npm:")?;
    if let Some(rest) = after_protocol.strip_prefix('@') {
        let slash = rest.find('/')?;
        let after_slash = &rest[slash + 1..];
        let at = after_slash.find('@')?;
        Some(format!("@{}", &rest[..slash + 1 + at]))
    } else {
        let at = after_protocol.find('@')?;
        Some(after_protocol[..at].to_string())
    }
}

// ---------------------------------------------------------------------------
// Writer: flat LockfileGraph → yarn.lock v1
// ---------------------------------------------------------------------------

/// Serialize a [`LockfileGraph`] as a yarn v1 lockfile.
///
/// yarn v1 is flat — unlike npm or bun, there's no nested install
/// path. Every `(name, version)` pair gets exactly one block whose
/// header is a comma-separated list of every spec that resolves to
/// it. We always emit the exact `"name@version"` spec (so transitive
/// deps emitted as `bar "2.5.0"` round-trip), and for direct root
/// deps we *also* emit the manifest range spec (e.g. `"bar@^2.0.0"`)
/// so `yarn install` and `aube install` — both of which look up
/// manifest ranges against the block headers — find the entry.
///
/// Transitive deps that arrive through a semver *range* (e.g. `foo`
/// depends on `bar "^2.0.0"`) are still technically lossy: the
/// original range isn't preserved, so if the parent's resolved
/// `bar` version differs from what the lockfile records, reparse
/// will miss. In practice the writer only runs on a graph the
/// resolver just produced, so the resolved versions match the
/// transitive dep keys exactly and reparse finds them.
///
/// Peer-contextualized variants collapse to a single `name@version`
/// entry (yarn v1's data model has no peer context). `resolved` URLs
/// are omitted for the same reason as the npm writer: we don't
/// persist the origin URL. yarn tolerates missing `resolved`.
/// Append a dependency-map key to `out`, quoting it the way yarn v1
/// does. Yarn classic's lockfile parser treats a leading `@` as the
/// start of a scoped-package token and requires the whole key to be a
/// double-quoted string — an unquoted `@scope/name` key produces an
/// `Unknown token … INVALID` parse error. Real yarn v1 emits scoped
/// keys quoted (`"@babel/helper-validator-identifier" "^7.x"`) and
/// bare package names unquoted (`js-tokens "^4.0.0"`); we match that
/// so the lockfile we write parses under `yarn install
/// --frozen-lockfile`.
/// Record `name -> range` into `map` when `range` is a local protocol
/// descriptor (`file:`/`link:`/`portal:`), keeping the first one seen.
/// Used to recover the literal declared range for a local-source block
/// header (see [`write_classic`]).
fn note_local_range<'a>(name: &'a str, range: &'a str, map: &mut BTreeMap<&'a str, &'a str>) {
    if range.starts_with("file:") || range.starts_with("link:") || range.starts_with("portal:") {
        map.entry(name).or_insert(range);
    }
}

/// Record the consumer-declared range for a `name` whose value is a git
/// specifier (`user/repo#ref`, `github:…`, `git+https://…`, …). Used to
/// recover the ORIGINAL git descriptor a yarn v1 block header must carry
/// (see [`write_classic`]). `parse_git_spec` returns `None` for plain
/// semver / `file:` / `link:` / tarball ranges, so this only fires on a
/// genuine git range — never shadowing the local-source path above.
fn note_git_range<'a>(name: &'a str, range: &'a str, map: &mut BTreeMap<&'a str, &'a str>) {
    if crate::parse_git_spec(range).is_some() {
        map.entry(name).or_insert(range);
    }
}

/// The `resolved "<url>"` value yarn v1 writes for a git dependency.
///
/// yarn's `resolved` form depends on how the dependency was DECLARED, not
/// just where it resolves. Empirically (yarn 1.22), a hosted *shorthand*
/// (`github:owner/repo#ref`, `owner/repo#ref`) resolves to the flat codeload
/// tarball (`https://codeload.github.com/<owner>/<repo>/tar.gz/<sha>`), while
/// a *URL* git spec (`git+https://…#<sha>`, `git+ssh://…`, `git://…`) is
/// echoed back VERBATIM as the `resolved` URL, fragment and all. Emitting a
/// codeload tarball for a `git+https` declaration makes yarn's GitFetcher
/// fail `Invariant Violation: Commit hash required` on a frozen install,
/// because it expects the commit on the URL fragment.
///
/// `declared` is the original descriptor recovered from the manifest (the
/// block-header range). `None` only when there is no resolved commit to pin
/// (the source never got an ls-remote pass) — the caller omits `resolved`.
fn yarn_git_resolved(git: &crate::GitSource, declared: &str) -> Option<String> {
    if git.resolved.is_empty() {
        return None;
    }
    // A URL-form git declaration is echoed verbatim. `parse_git_spec`
    // returns a URL only for genuine URL specs (it expands bare/`github:`
    // shorthands into a hosted URL but reports them here via the shorthand
    // check below), so gate on the literal declared prefix instead.
    let is_url_form = declared.starts_with("git+")
        || declared.starts_with("git://")
        || declared.starts_with("git@")
        || declared.starts_with("ssh://")
        || ((declared.starts_with("https://") || declared.starts_with("http://"))
            && declared.contains(".git"));
    if is_url_form {
        // The declaration already carries the committish fragment yarn
        // needs (`…#<sha>`); echo it unchanged.
        return Some(declared.to_string());
    }
    if let Some(hosted) = crate::parse_hosted_git(&git.url)
        && let Some(tarball) = hosted.tarball_url(&git.resolved)
    {
        return Some(tarball);
    }
    let base = git.url.strip_prefix("git+").unwrap_or(&git.url);
    Some(format!("{base}#{}", git.resolved))
}

fn push_classic_dep_key(out: &mut String, key: &str) {
    if key.starts_with('@') {
        out.push('"');
        out.push_str(key);
        out.push('"');
    } else {
        out.push_str(key);
    }
}

pub fn write_classic(
    path: &Path,
    graph: &LockfileGraph,
    manifest: &aube_manifest::PackageJson,
) -> Result<(), Error> {
    // Collapse peer-context variants: one entry per canonical (name, version).
    let canonical = crate::build_canonical_map(graph);

    // Collect every spec that points at a canonical `(name, version)` —
    // both root-manifest ranges *and* transitive declared ranges from
    // every other package's `declared_dependencies`. Yarn groups all
    // specs resolving to the same (name, version) under one block
    // header (`is-number@^6.0.0, is-number@~6.0.1:`), and reparse of
    // a transitive `bar "^2.0.0"` needs `bar@^2.0.0` to appear in
    // some block's header to find the right canonical entry.
    //
    // Keyed by canonical key; values are the extra range-form spec
    // strings to emit alongside the exact `"name@version"` one.
    // Deduped per canonical so identical ranges coming from multiple
    // consumers collapse.
    let mut extra_specs: BTreeMap<String, std::collections::BTreeSet<String>> = BTreeMap::new();
    let root_importer_specs = manifest
        .dependencies
        .iter()
        .chain(manifest.dev_dependencies.iter())
        .chain(manifest.optional_dependencies.iter())
        .chain(manifest.peer_dependencies.iter());
    for dep in graph.importers.get(".").into_iter().flatten() {
        let canonical_key = crate::npm::canonical_key_from_dep_path(&dep.dep_path);
        if !canonical.contains_key(&canonical_key) {
            continue;
        }
        // Look up the range the manifest currently uses for this dep.
        let range = root_importer_specs
            .clone()
            .find(|(n, _)| n.as_str() == dep.name.as_str())
            .map(|(_, r)| r.clone());
        if let Some(range) = range {
            let spec = format!("{}@{range}", dep.name);
            if spec != canonical_key {
                extra_specs.entry(canonical_key).or_default().insert(spec);
            }
        }
    }
    // Harvest transitive declared ranges. Each package's
    // `declared_dependencies[name] = range` is the range its own
    // manifest uses; the canonical the range resolves to is whatever
    // the resolver already placed under `pkg.dependencies[name]`.
    for pkg in canonical.values() {
        for (dep_name, range) in &pkg.declared_dependencies {
            let Some(resolved_value) = pkg.dependencies.get(dep_name) else {
                continue;
            };
            let target = crate::npm::child_canonical_key(dep_name, resolved_value);
            if !canonical.contains_key(&target) {
                continue;
            }
            let spec = format!("{dep_name}@{range}");
            if spec != target {
                extra_specs.entry(target).or_default().insert(spec);
            }
        }
    }

    // Map each local-source package name to the literal protocol range
    // its consumer declared (`file:./local-pkg`, `link:../sibling`).
    // Yarn keys a local block by the *descriptor the consumer wrote*,
    // and its `--frozen-lockfile` check compares that descriptor against
    // the manifest byte-for-byte: a synthesized `file:local-pkg` (from a
    // pnpm-lock conversion that dropped the `./`) wouldn't match the
    // manifest's `file:./local-pkg`, so yarn demands a rewrite. We
    // recover the exact declared range — from the root manifest for
    // direct deps, from a parent's `declared_dependencies` for
    // transitive ones — and prefer it over the source's reconstructed
    // specifier.
    let mut declared_local_range: BTreeMap<&str, &str> = BTreeMap::new();
    // Map each git-source package name to the ORIGINAL git specifier its
    // consumer declared (`vercel/ms#4ff48cec`, `github:user/repo#tag`,
    // `git+https://…`). Yarn v1 keys a git block by the descriptor the
    // manifest wrote and its `--frozen-lockfile` check matches that
    // descriptor against the manifest — so emitting the *expanded* resolved
    // URL (`ssh://git@github.com/vercel/ms.git#<40-char-sha>`, what an npm
    // lockfile's `resolved` carries) makes yarn reject the file with "Your
    // lockfile needs to be updated". We recover the user-written range from
    // the root manifest (direct deps) or a parent's `declared_dependencies`
    // (transitive) and key the block by it, mirroring the `file:`/`link:`
    // recovery above. A git source whose range we CAN'T recover is refused
    // outright (below) rather than written in the unmatchable expanded form.
    let mut declared_git_range: BTreeMap<&str, &str> = BTreeMap::new();
    for (name, range) in manifest
        .dependencies
        .iter()
        .chain(manifest.dev_dependencies.iter())
        .chain(manifest.optional_dependencies.iter())
    {
        note_local_range(name, range, &mut declared_local_range);
        note_git_range(name, range, &mut declared_git_range);
    }
    for pkg in canonical.values() {
        for (name, range) in &pkg.declared_dependencies {
            note_local_range(name, range, &mut declared_local_range);
            note_git_range(name, range, &mut declared_git_range);
        }
    }

    let mut out = String::with_capacity(canonical.len().saturating_mul(256).max(4096));
    out.push_str("# THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.\n");
    out.push_str("# yarn lockfile v1\n\n\n");

    for (canonical_key, pkg) in &canonical {
        // A `file:`/`link:`/`portal:` local-source package is keyed by
        // its protocol descriptor, not `name@version`. Real yarn v1
        // writes `"local-utils@file:./local-pkg":` (the range its
        // consumer declared), with `version` but no `resolved`/
        // `integrity` — keying it `"local-utils@0.0.0"` instead makes
        // `yarn install --frozen-lockfile` reject the file with "Your
        // lockfile needs to be updated", because yarn can't reconcile
        // the `file:` range in package.json against a `name@version`
        // header. We reproduce yarn's local-source header exactly.
        // A git source is keyed by the ORIGINAL git descriptor the
        // consumer declared (`ms@vercel/ms#4ff48cec`) and carries a
        // `resolved "<codeload tarball URL>"` line — exactly what yarn v1
        // writes for a hosted git dep. We must recover the user-written
        // range: keying the block by the expanded resolved URL (the npm
        // lockfile's `resolved`) leaves yarn unable to match it against the
        // manifest's range, so `--frozen-lockfile` rejects the file. If the
        // range can't be recovered, refuse rather than emit the broken
        // expanded form (the never-silently-write-a-yarn-rejected-lockfile
        // bar). `git_resolved` is the `resolved` URL when we can derive one.
        // A hosted-git dependency fetched through a codeload archive (or
        // recorded that way by pnpm v9+) arrives as a
        // `RemoteTarball { git_hosted: true }` rather than a
        // `LocalSource::Git`. yarn v1 keys such a dep by the declared git
        // spec with a `resolved "<codeload tarball URL>"` line — keying it
        // by the tarball URL with `version "0.0.0"` (the local-source
        // fallback below) makes `yarn install --frozen-lockfile` reject the
        // file. Normalize the stand-in tarball back to a git source so the
        // git branch renders yarn's accepted form.
        let hosted_git = match &pkg.local_source {
            Some(LocalSource::RemoteTarball(rt)) => rt.as_hosted_git_source(),
            _ => None,
        };
        let git_source = match &pkg.local_source {
            Some(LocalSource::Git(git)) => Some(git.clone()),
            _ => hosted_git,
        };
        let mut git_resolved: Option<String> = None;
        let header = if let Some(git) = &git_source {
            let range = declared_git_range.get(pkg.name.as_str()).ok_or_else(|| {
                Error::parse(
                    path,
                    format!(
                        "dependency `{}` is a git dependency whose original \
                         specifier could not be recovered from package.json, so it \
                         can't be written to a yarn v1 lockfile that yarn would \
                         accept. Declare it in package.json (e.g. \
                         `\"{}\": \"<owner>/<repo>#<ref>\"`) before migrating to yarn.",
                        pkg.name, pkg.name
                    ),
                )
            })?;
            git_resolved = yarn_git_resolved(git, range);
            format!("{}@{}", pkg.name, range)
        } else if let Some(src) = &pkg.local_source {
            // `file:`/`link:`/`portal:` — keyed by the declared protocol
            // descriptor (recovered, else the source's reconstructed spec).
            let range = declared_local_range
                .get(pkg.name.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| src.specifier());
            format!("{}@{}", pkg.name, range)
        } else {
            canonical_key.clone()
        };

        // Header: `"name@version"[, "name@range"]*:` — always start
        // with the exact spec so transitive reparse works, then
        // append any manifest range specs pointing at this entry.
        out.push('"');
        out.push_str(&header);
        out.push('"');
        if let Some(extras) = extra_specs.get(canonical_key) {
            for spec in extras {
                out.push_str(", \"");
                out.push_str(spec);
                out.push('"');
            }
        }
        out.push_str(":\n");

        // `  version "..."`
        out.push_str("  version \"");
        out.push_str(&pkg.version);
        out.push_str("\"\n");

        // A git source carries `resolved "<codeload tarball URL>"`; a
        // `file:`/`link:`/`portal:` source carries no `resolved`/`integrity`
        // (yarn v1 emits only `version` for those); a registry source
        // carries its integrity.
        if let Some(resolved) = &git_resolved {
            out.push_str("  resolved \"");
            out.push_str(resolved);
            out.push_str("\"\n");
        } else if pkg.local_source.is_none()
            && let Some(integ) = &pkg.integrity
        {
            out.push_str("  integrity ");
            out.push_str(integ);
            out.push('\n');
        }

        // `  dependencies:` block — prefer the declared range from the
        // package's own manifest (what yarn itself writes) over the
        // resolved pin. Falls back to the pin when the source
        // lockfile didn't carry declared ranges (e.g. pnpm → yarn).
        let nonempty_deps: BTreeMap<&str, String> = pkg
            .dependencies
            .iter()
            .filter_map(|(n, v)| {
                let key = crate::npm::child_canonical_key(n, v);
                if !canonical.contains_key(&key) {
                    return None;
                }
                let rendered = pkg
                    .declared_dependencies
                    .get(n)
                    .cloned()
                    .unwrap_or_else(|| crate::npm::dep_value_as_version(n, v).to_string());
                Some((n.as_str(), rendered))
            })
            .collect();
        if !nonempty_deps.is_empty() {
            out.push_str("  dependencies:\n");
            for (dep_name, dep_version) in &nonempty_deps {
                out.push_str("    ");
                push_classic_dep_key(&mut out, dep_name);
                out.push_str(" \"");
                out.push_str(dep_version);
                out.push_str("\"\n");
            }
        }

        out.push('\n');
    }

    crate::atomic_write_lockfile(path, out.as_bytes())?;
    Ok(())
}
