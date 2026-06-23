use crate::{Error, GitSource, LocalSource, RemoteTarballSource};
use aube_util::path::normalize_lexical;
use std::collections::BTreeMap;
use std::path::{Component, Path};

/// Extract the alias name bun uses as the hoist key. bun's `packages`
/// key is `<alias_name>` hoisted or `<parent>/<alias_name>` nested,
/// where `alias_name` matches `package.json`'s dep key verbatim.
pub(super) fn bun_key_to_alias_name(key: &str) -> String {
    if let Some(last_slash) = key.rfind('/') {
        // Scoped names like `@scope/name` are a single unit — if the
        // slice before the last slash is itself `@scope`, keep the
        // whole suffix.
        let tail_start = key[..last_slash].rfind('/').map(|i| i + 1).unwrap_or(0);
        if key[tail_start..last_slash].starts_with('@') {
            key[tail_start..].to_string()
        } else {
            key[last_slash + 1..].to_string()
        }
    } else {
        key.to_string()
    }
}

/// Classify a bun ident's version tail as a registry pin, an npm alias
/// target, or a non-registry source (git, file, link, workspace, http
/// tarball). Returns `(name, version, local_source, alias_of)`.
///
/// - `alias_name` is the hoist key (bun's left-hand side).
/// - `raw_name` / `raw_version` come from `split_ident()` on the ident
///   (the right-hand side of the tuple's position 0).
///
/// The alias name wins as `LockedPackage.name` whenever it differs
/// from the ident's name (npm-alias case). `alias_of` records the
/// registry-side name only then.
pub(super) fn classify_bun_ident(
    alias_name: &str,
    raw_name: &str,
    raw_version: &str,
    integrity: Option<&str>,
) -> Result<(String, String, Option<LocalSource>, Option<String>), Error> {
    // npm-alias tail: bun writes the registry identity into the ident,
    // so the raw name is the real registry name and the alias key is
    // the hoist name.
    let alias_of = if alias_name != raw_name {
        Some(raw_name.to_string())
    } else {
        None
    };
    let name = alias_name.to_string();

    // Non-registry tails.
    if raw_version.starts_with("workspace:") {
        let rel = raw_version.strip_prefix("workspace:").unwrap_or("");
        // `workspace:*` / `workspace:^` / `workspace:~` are version-
        // range selectors, not directory paths — a `PathBuf::from("*")`
        // would silently become `{project_root}/*` under any caller
        // that does `project_root.join(link.path())`. Bun's `packages`
        // entries for workspace members use root-relative paths like
        // `workspace:packages/lib`, so keep slash-bearing tails as paths.
        // Otherwise fall back to `.` so range selectors point at the
        // workspace root and the caller resolves the actual location
        // from the graph's workspace map.
        let is_path = rel.starts_with('.') || rel.starts_with('/') || rel.contains('/');
        let path_buf = std::path::PathBuf::from(if rel.is_empty() || !is_path { "." } else { rel });
        return Ok((
            name,
            raw_version.to_string(),
            Some(LocalSource::Link(path_buf)),
            alias_of,
        ));
    }
    if let Some(rest) = raw_version.strip_prefix("github:") {
        let (url, committish) = split_committish(rest);
        return Ok((
            name,
            raw_version.to_string(),
            Some(LocalSource::Git(GitSource {
                url: format!("https://github.com/{url}.git"),
                committish: committish.clone(),
                resolved: committish.unwrap_or_default(),
                integrity: None,
                subpath: None,
            })),
            alias_of,
        ));
    }
    if (raw_version.starts_with("git+")
        || raw_version.starts_with("git://")
        || raw_version.starts_with("git@"))
        && let Some((url, committish, subpath)) = crate::parse_git_spec(raw_version)
    {
        return Ok((
            name,
            raw_version.to_string(),
            Some(LocalSource::Git(GitSource {
                url,
                committish: committish.clone(),
                resolved: committish.unwrap_or_default(),
                integrity: None,
                subpath,
            })),
            alias_of,
        ));
    }
    if raw_version.starts_with("http://") || raw_version.starts_with("https://") {
        // Mirror the sibling `LockedPackage.integrity` hash onto the
        // `RemoteTarballSource` so consumers of
        // `local_source.specifier()` or integrity-verification paths
        // see the same value — leaving it empty would make the two
        // fields drift apart for the same entry.
        return Ok((
            name,
            raw_version.to_string(),
            Some(LocalSource::RemoteTarball(RemoteTarballSource {
                url: raw_version.to_string(),
                integrity: integrity.map(str::to_string).unwrap_or_default(),
                git_hosted: false,
            })),
            alias_of,
        ));
    }
    if let Some(rest) = raw_version.strip_prefix("file:") {
        let rel = std::path::PathBuf::from(rest);
        let kind = if LocalSource::path_looks_like_tarball(&rel) {
            LocalSource::Tarball(rel)
        } else {
            LocalSource::Directory(rel)
        };
        return Ok((name, raw_version.to_string(), Some(kind), alias_of));
    }
    let raw_path = std::path::PathBuf::from(raw_version);
    if LocalSource::path_looks_like_tarball(&raw_path) {
        return Ok((
            name,
            raw_version.to_string(),
            Some(LocalSource::Tarball(raw_path)),
            alias_of,
        ));
    }
    if let Some(rest) = raw_version.strip_prefix("link:") {
        return Ok((
            name,
            raw_version.to_string(),
            Some(LocalSource::Link(std::path::PathBuf::from(rest))),
            alias_of,
        ));
    }
    // Plain registry pin.
    Ok((name, raw_version.to_string(), None, alias_of))
}

pub(super) fn rebase_workspace_scoped_local_source(
    key: &str,
    local: LocalSource,
    workspace_scopes: &[(&str, &str)],
) -> LocalSource {
    let Some(local_path) = local.path() else {
        return local;
    };
    // Bun may write workspace-name-scoped local entries with
    // root-relative bare paths (`vendor/local-dir`) or
    // importer-relative climbs (`../../vendor/local.tgz`). Only the
    // latter needs rebasing to project-root form.
    if !local_path
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return local;
    }
    let Some((_, ws_path)) = workspace_scopes.iter().find(|(name, _)| {
        key.strip_prefix(*name)
            .is_some_and(|suffix| suffix.starts_with('/'))
    }) else {
        return local;
    };
    let rebased = normalize_lexical(&Path::new(ws_path).join(local_path));
    match local {
        LocalSource::Directory(_) => LocalSource::Directory(rebased),
        LocalSource::Tarball(_) => LocalSource::Tarball(rebased),
        LocalSource::Link(_) => LocalSource::Link(rebased),
        LocalSource::Portal(_) => LocalSource::Portal(rebased),
        LocalSource::Exec(_) => LocalSource::Exec(rebased),
        LocalSource::Git(_) | LocalSource::RemoteTarball(_) => local,
    }
}

pub(super) fn split_committish(spec: &str) -> (String, Option<String>) {
    match spec.rfind('#') {
        Some(i) => (spec[..i].to_string(), Some(spec[i + 1..].to_string())),
        None => (spec.to_string(), None),
    }
}

/// Normalize bun's `bin` meta (either a single-string form or a
/// `{name: path}` object) into the typed BTreeMap LockedPackage uses.
/// String form defaults the bin name to `default_name` (the package
/// name), matching npm's own fallback when `package.json` writes
/// `"bin": "./foo.js"` shorthand.
pub(super) fn bin_value_to_map(
    default_name: &str,
    value: &serde_json::Value,
) -> BTreeMap<String, String> {
    match value {
        serde_json::Value::String(s) => {
            let mut map = BTreeMap::new();
            map.insert(default_name.to_string(), s.clone());
            map
        }
        serde_json::Value::Object(obj) => obj
            .iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect(),
        _ => BTreeMap::new(),
    }
}

/// Resolve a transitive dep from the perspective of a bun.lock entry at
/// key `pkg_key`. bun.lock uses slash-delimited keys for nested overrides:
/// an entry at "parent/foo" means "foo" is nested inside "parent" because
/// the hoisted version didn't satisfy parent's range.
///
/// We walk up the key's ancestors, first checking the package's own nested
/// scope then each ancestor's, finally falling back to the hoisted entry
/// at just the bare `dep_name`.
pub(super) fn resolve_nested_bun(
    pkg_key: &str,
    dep_name: &str,
    key_info: &BTreeMap<String, (String, String)>,
) -> Option<String> {
    let mut base = pkg_key.to_string();
    loop {
        let candidate = if base.is_empty() {
            dep_name.to_string()
        } else {
            format!("{base}/{dep_name}")
        };
        if key_info.contains_key(&candidate) {
            return Some(candidate);
        }
        if base.is_empty() {
            return None;
        }
        // Strip the trailing package segment. For scoped packages we need
        // to strip "@scope/name" as a single unit.
        if let Some(idx) = base.rfind('/') {
            // If the base ends with "@scope/name", we need to check if the
            // segment before the "/" starts with '@' — if so, strip that full
            // "@scope/name" tail. Otherwise strip just the trailing segment.
            let tail_start = base[..idx].rfind('/').map(|i| i + 1).unwrap_or(0);
            if base[tail_start..idx].starts_with('@') {
                base.truncate(tail_start.saturating_sub(1));
            } else {
                base.truncate(idx);
            }
        } else {
            base.clear();
        }
    }
}

/// Resolve a direct dep of a workspace importer at path `ws_path`
/// (e.g. `""` for root, `"packages/app"` for a nested workspace) to
/// its `key_info` key. Checks the workspace-scoped override
/// (`<workspace_name>/<dep_name>`), the path-scoped override
/// (`<ws_path>/<dep_name>`), then the hoisted bare key
/// (`<dep_name>`). Intentionally does *not* walk intermediate
/// ancestors like `packages/<dep_name>` — those are
/// package-nesting keys that belong to `resolve_nested_bun`, and
/// partial matches there could spuriously resolve to a literal npm
/// package named `packages` that happened to carry its own nested
/// entry.
pub(super) fn resolve_workspace_dep(
    ws_path: &str,
    ws_name: Option<&str>,
    dep_name: &str,
    key_info: &BTreeMap<String, (String, String)>,
) -> Option<String> {
    if let Some(ws_name) = ws_name {
        let ws_specific = format!("{ws_name}/{dep_name}");
        if key_info.contains_key(&ws_specific) {
            return Some(ws_specific);
        }
    }
    if !ws_path.is_empty() {
        let ws_specific = format!("{ws_path}/{dep_name}");
        if key_info.contains_key(&ws_specific) {
            return Some(ws_specific);
        }
    }
    if key_info.contains_key(dep_name) {
        return Some(dep_name.to_string());
    }
    None
}

/// Split a bun ident like `foo@1.2.3` or `@scope/pkg@1.2.3` into `(name, version)`.
pub(super) fn split_ident(ident: &str) -> Option<(String, String)> {
    if let Some(rest) = ident.strip_prefix('@') {
        let slash = rest.find('/')?;
        let after_slash = &rest[slash + 1..];
        let at = after_slash.find('@')?;
        let name = format!("@{}", &rest[..slash + 1 + at]);
        let version = after_slash[at + 1..].to_string();
        Some((name, version))
    } else {
        let at = ident.find('@')?;
        Some((ident[..at].to_string(), ident[at + 1..].to_string()))
    }
}
