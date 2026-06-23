//! `aube query` — select packages from the resolved dependency graph.
//!
//! This is a local-only first slice of vlt-style dependency selectors. It
//! reads the lockfile, walks packages reachable from the selected importers,
//! and filters them with simple selector predicates. No registry or security
//! service calls are made.

use aube_lockfile::{DepType, DirectDep, LocalSource, LockedPackage, LockfileGraph};
use clap::Args;
use miette::{Context, IntoDiagnostic, miette};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub const AFTER_LONG_HELP: &str = "\
Inspired by vlt's dependency selector model, but currently local-only:
selectors read aube's lockfile graph without registry or security-service calls.

Examples:

  # Every reachable package
  $ aube query '*'

  # Exact package name
  $ aube query '[name=react]'

  # Direct prod dependencies with install scripts
  $ aube query ':prod:scripts'

  # Local file/link/git/tarball dependencies
  $ aube query ':type(file), :type(link), :type(git), :type(remote)'

  # Machine-readable
  $ aube query ':bin' --json
";

#[derive(Debug, Args)]
pub struct QueryArgs {
    /// Selector expression.
    ///
    /// Supports `*`, bare package names, `[name=value]`,
    /// `[version=value]`, `[license=value]`, `[depPath=value]`,
    /// `[source=value]`, `:prod`, `:dev`, `:optional`, `:peer`,
    /// `:transitive`, `:scripts`, `:bin`, `:deprecated`,
    /// `:license(value)`, and `:type(value)`.
    pub selector: String,

    /// Only match devDependency roots and their transitive deps.
    #[arg(short = 'D', long, conflicts_with = "prod")]
    pub dev: bool,

    /// Only match production/optional roots and their transitive deps.
    #[arg(
        short = 'P',
        long,
        conflicts_with = "dev",
        visible_alias = "production"
    )]
    pub prod: bool,

    /// Emit a JSON array instead of the default text layout.
    #[arg(long, conflicts_with = "parseable")]
    pub json: bool,

    /// Emit tab-separated rows: dep_path, name, version, source, flags.
    #[arg(long)]
    pub parseable: bool,
}

pub async fn run(
    args: QueryArgs,
    filter: aube_workspace::selector::EffectiveFilter,
) -> miette::Result<()> {
    // Workspace root wins over the nearest project root so `aube query`
    // run from inside `packages/foo/` reads the lockfile + manifest at
    // the workspace root instead of the subpackage (which has no
    // lockfile of its own).
    let read_from = crate::dirs::workspace_or_project_root()?;
    // Yaml-only workspace roots have no root `package.json`; the
    // lockfile parser only uses the manifest to classify yarn.lock
    // direct deps, so a default manifest is fine for the read path.
    let manifest = super::load_manifest_or_default(&read_from)?;
    let graph = match aube_lockfile::parse_lockfile(&read_from, &manifest) {
        Ok(graph) => graph,
        Err(aube_lockfile::Error::NotFound(_)) => {
            eprintln!(
                "No lockfile found. Run `{}` first.",
                aube_util::cmd("install")
            );
            return Ok(());
        }
        Err(e) => return Err(miette::Report::new(e)).wrap_err("failed to parse lockfile"),
    };
    let selector = Selector::parse(&args.selector)?;
    let dep_filter = super::DepFilter::from_flags(args.prod, args.dev);
    let importers = selected_importers(&read_from, &filter)?;
    let entries = collect_entries(&graph, dep_filter, importers.as_ref());
    let matches: Vec<_> = entries
        .iter()
        .filter(|entry| selector.matches(entry))
        .collect();

    if args.json {
        print_json(&matches)?;
    } else if args.parseable {
        print_parseable(&matches);
    } else {
        print_default(&matches);
    }
    Ok(())
}

fn selected_importers(
    root: &Path,
    filter: &aube_workspace::selector::EffectiveFilter,
) -> miette::Result<Option<BTreeSet<String>>> {
    if filter.is_empty() {
        return Ok(None);
    }

    let workspace_pkgs = aube_workspace::find_workspace_packages(root)
        .map_err(|e| miette!("failed to discover workspace packages: {e}"))?;
    if workspace_pkgs.is_empty() {
        return Err(miette!(
            "{}: --filter requires a workspace root (aube-workspace.yaml, pnpm-workspace.yaml, or package.json with a `workspaces` field) at or above {}",
            aube_util::cmd("query"),
            root.display()
        ));
    }
    let selected =
        aube_workspace::selector::select_workspace_packages(root, &workspace_pkgs, filter)
            .map_err(|e| miette!("invalid --filter selector: {e}"))?;
    if selected.is_empty() {
        return Err(miette!(
            "{}: filter {filter:?} did not match any workspace package",
            aube_util::cmd("query")
        ));
    }

    let mut importers = BTreeSet::new();
    for pkg in &selected {
        importers.insert(super::workspace_importer_path(root, &pkg.dir)?);
    }
    Ok(Some(importers))
}

#[derive(Debug, Clone)]
struct QueryEntry<'g> {
    dep_path: String,
    pkg: &'g LockedPackage,
    direct_types: BTreeSet<QueryDepType>,
    peer: bool,
    transitive: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum QueryDepType {
    Prod,
    Dev,
    Optional,
}

impl QueryDepType {
    fn label(self) -> &'static str {
        match self {
            Self::Prod => "prod",
            Self::Dev => "dev",
            Self::Optional => "optional",
        }
    }
}

fn collect_entries<'g>(
    graph: &'g LockfileGraph,
    filter: super::DepFilter,
    importer_filter: Option<&BTreeSet<String>>,
) -> Vec<QueryEntry<'g>> {
    let mut entries: BTreeMap<String, QueryEntry<'g>> = BTreeMap::new();
    let mut stack = Vec::new();

    for (importer, roots) in &graph.importers {
        if let Some(importers) = importer_filter
            && !importers.contains(importer)
        {
            continue;
        }
        for root in roots {
            if !filter.keeps(root.dep_type) {
                continue;
            }
            let Some(pkg) = graph.get_package(&root.dep_path) else {
                continue;
            };
            let entry = entries
                .entry(root.dep_path.clone())
                .or_insert_with(|| QueryEntry {
                    dep_path: root.dep_path.clone(),
                    pkg,
                    direct_types: BTreeSet::new(),
                    peer: false,
                    transitive: false,
                });
            entry.direct_types.insert(query_dep_type(root));
            stack.push(root.dep_path.clone());
        }
    }

    while let Some(dep_path) = stack.pop() {
        let Some(parent) = graph.get_package(&dep_path) else {
            continue;
        };
        for (name, tail) in &parent.dependencies {
            let child_dep_path = format!("{name}@{tail}");
            let Some(child) = graph.get_package(&child_dep_path) else {
                continue;
            };
            let inserted = !entries.contains_key(&child_dep_path);
            let entry = entries
                .entry(child_dep_path.clone())
                .or_insert_with(|| QueryEntry {
                    dep_path: child_dep_path.clone(),
                    pkg: child,
                    direct_types: BTreeSet::new(),
                    peer: false,
                    transitive: true,
                });
            if parent.peer_dependencies.contains_key(name) {
                entry.peer = true;
            }
            if parent.optional_dependencies.contains_key(name) || child.optional {
                entry.direct_types.insert(QueryDepType::Optional);
            }
            if inserted {
                stack.push(child_dep_path);
            }
        }
    }

    entries.into_values().collect()
}

fn query_dep_type(dep: &DirectDep) -> QueryDepType {
    match dep.dep_type {
        DepType::Production => QueryDepType::Prod,
        DepType::Dev => QueryDepType::Dev,
        DepType::Optional => QueryDepType::Optional,
    }
}

#[derive(Debug, Clone)]
struct Selector {
    groups: Vec<Vec<Predicate>>,
}

impl Selector {
    fn parse(input: &str) -> miette::Result<Self> {
        let groups: Vec<Vec<Predicate>> = split_top_level(input, ',')
            .into_iter()
            .map(|group| parse_group(group.trim()))
            .collect::<miette::Result<_>>()?;
        if groups.is_empty() {
            return Err(miette!("empty query selector"));
        }
        Ok(Self { groups })
    }

    fn matches(&self, entry: &QueryEntry<'_>) -> bool {
        self.groups
            .iter()
            .any(|group| group.iter().all(|pred| pred.matches(entry)))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Predicate {
    Any,
    Name(String),
    Attr { key: String, value: Option<String> },
    Pseudo(Pseudo),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Pseudo {
    Prod,
    Dev,
    Optional,
    Peer,
    Transitive,
    Scripts,
    Bin,
    Deprecated,
    Type(String),
    License(String),
}

impl Predicate {
    fn matches(&self, entry: &QueryEntry<'_>) -> bool {
        match self {
            Self::Any => true,
            Self::Name(name) => entry.pkg.name == *name,
            Self::Attr { key, value } => attr_matches(entry, key, value.as_deref()),
            Self::Pseudo(pseudo) => pseudo.matches(entry),
        }
    }
}

impl Pseudo {
    fn matches(&self, entry: &QueryEntry<'_>) -> bool {
        match self {
            Self::Prod => entry.direct_types.contains(&QueryDepType::Prod),
            Self::Dev => entry.direct_types.contains(&QueryDepType::Dev),
            Self::Optional => entry.direct_types.contains(&QueryDepType::Optional),
            Self::Peer => entry.peer,
            Self::Transitive => entry.transitive,
            Self::Scripts => has_install_script(entry.pkg),
            Self::Bin => !entry.pkg.bin.is_empty(),
            Self::Deprecated => entry.pkg.extra_meta.contains_key("deprecated"),
            Self::Type(kind) => source_kind(entry.pkg) == kind.as_str(),
            Self::License(license) => entry.pkg.license.as_deref() == Some(license.as_str()),
        }
    }
}

fn parse_group(input: &str) -> miette::Result<Vec<Predicate>> {
    let mut out = Vec::new();
    for token in selector_tokens(input)? {
        out.extend(parse_compound_token(&token)?);
    }
    if out.is_empty() {
        return Err(miette!("empty query selector"));
    }
    Ok(out)
}

fn parse_compound_token(input: &str) -> miette::Result<Vec<Predicate>> {
    let mut out = Vec::new();
    let mut rest = input;
    while !rest.is_empty() {
        if let Some(after_star) = rest.strip_prefix('*') {
            out.push(Predicate::Any);
            rest = after_star;
        } else if let Some(after_open) = rest.strip_prefix('[') {
            let Some(close) = after_open.find(']') else {
                return Err(miette!("unterminated attribute selector in {input:?}"));
            };
            out.push(parse_attr(&after_open[..close])?);
            rest = &after_open[close + 1..];
        } else if rest.starts_with(':') {
            out.extend(parse_pseudos(rest)?);
            rest = "";
        } else {
            let name_end = rest.find([':', '[']).unwrap_or(rest.len());
            if name_end == 0 {
                return Err(miette!("invalid query selector token {input:?}"));
            }
            out.push(Predicate::Name(rest[..name_end].to_string()));
            rest = &rest[name_end..];
        }
    }
    Ok(out)
}

fn parse_attr(input: &str) -> miette::Result<Predicate> {
    let (key, value) = input
        .split_once('=')
        .map(|(k, v)| (k.trim(), Some(unquote(v.trim()))))
        .unwrap_or_else(|| (input.trim(), None));
    if key.is_empty() {
        return Err(miette!("empty attribute selector"));
    }
    let key = key.to_ascii_lowercase();
    if value.is_some() && matches!(key.as_str(), "bin" | "deprecated") {
        return Err(miette!("[{key}] does not support value comparisons"));
    }
    Ok(Predicate::Attr { key, value })
}

fn parse_pseudos(input: &str) -> miette::Result<Vec<Predicate>> {
    let mut out = Vec::new();
    let mut rest = input;
    while let Some(after_colon) = rest.strip_prefix(':') {
        let name_end = after_colon.find([':', '(']).unwrap_or(after_colon.len());
        let name = &after_colon[..name_end];
        if name.is_empty() {
            return Err(miette!("empty pseudo selector in {input:?}"));
        }
        rest = &after_colon[name_end..];
        let arg = if let Some(after_open) = rest.strip_prefix('(') {
            let Some(close) = after_open.find(')') else {
                return Err(miette!("unterminated pseudo selector in {input:?}"));
            };
            rest = &after_open[close + 1..];
            Some(unquote(after_open[..close].trim()))
        } else {
            None
        };
        out.push(Predicate::Pseudo(parse_pseudo(name, arg)?));
    }
    if !rest.is_empty() {
        return Err(miette!(
            "invalid pseudo selector tail {rest:?} in {input:?}"
        ));
    }
    Ok(out)
}

fn parse_pseudo(name: &str, arg: Option<String>) -> miette::Result<Pseudo> {
    match name.to_ascii_lowercase().as_str() {
        "prod" | "production" => Ok(Pseudo::Prod),
        "dev" => Ok(Pseudo::Dev),
        "optional" => Ok(Pseudo::Optional),
        "peer" => Ok(Pseudo::Peer),
        "transitive" => Ok(Pseudo::Transitive),
        "scripts" => Ok(Pseudo::Scripts),
        "bin" => Ok(Pseudo::Bin),
        "deprecated" => Ok(Pseudo::Deprecated),
        "type" => Ok(Pseudo::Type(
            arg.ok_or_else(|| miette!(":type() requires an argument"))?,
        )),
        "license" => {
            Ok(Pseudo::License(arg.ok_or_else(|| {
                miette!(":license() requires an argument")
            })?))
        }
        _ => Err(miette!("unsupported query pseudo selector :{name}")),
    }
}

fn selector_tokens(input: &str) -> miette::Result<Vec<String>> {
    let mut tokens = Vec::new();
    let mut start = None;
    let mut bracket_depth = 0usize;
    let mut paren_depth = 0usize;

    for (idx, ch) in input.char_indices() {
        if start.is_none() && !ch.is_whitespace() {
            start = Some(idx);
        }
        match ch {
            '[' => bracket_depth += 1,
            ']' => {
                bracket_depth = bracket_depth
                    .checked_sub(1)
                    .ok_or_else(|| miette!("unmatched `]` in selector {input:?}"))?;
            }
            '(' => paren_depth += 1,
            ')' => {
                paren_depth = paren_depth
                    .checked_sub(1)
                    .ok_or_else(|| miette!("unmatched `)` in selector {input:?}"))?;
            }
            c if c.is_whitespace() && bracket_depth == 0 && paren_depth == 0 => {
                if let Some(s) = start.take() {
                    tokens.push(input[s..idx].to_string());
                }
            }
            _ => {}
        }
    }
    if bracket_depth != 0 || paren_depth != 0 {
        return Err(miette!("unterminated selector {input:?}"));
    }
    if let Some(s) = start {
        tokens.push(input[s..].to_string());
    }
    Ok(tokens)
}

fn split_top_level(input: &str, delimiter: char) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut bracket_depth = 0usize;
    let mut paren_depth = 0usize;
    for (idx, ch) in input.char_indices() {
        match ch {
            '[' => bracket_depth += 1,
            ']' => bracket_depth = bracket_depth.saturating_sub(1),
            '(' => paren_depth += 1,
            ')' => paren_depth = paren_depth.saturating_sub(1),
            c if c == delimiter && bracket_depth == 0 && paren_depth == 0 => {
                out.push(&input[start..idx]);
                start = idx + c.len_utf8();
            }
            _ => {}
        }
    }
    out.push(&input[start..]);
    out.into_iter().filter(|s| !s.trim().is_empty()).collect()
}

fn unquote(input: &str) -> String {
    input
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| input.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(input)
        .to_string()
}

fn attr_matches(entry: &QueryEntry<'_>, key: &str, value: Option<&str>) -> bool {
    match key {
        "name" => value.is_none_or(|v| entry.pkg.name == v),
        "version" => value.is_none_or(|v| entry.pkg.version == v),
        "deppath" | "dep-path" | "dep_path" => value.is_none_or(|v| entry.dep_path == v),
        "license" => entry
            .pkg
            .license
            .as_deref()
            .is_some_and(|l| value.is_none_or(|v| l == v)),
        "source" | "type" => value.is_none_or(|v| source_kind(entry.pkg) == v),
        "bin" => !entry.pkg.bin.is_empty() && value.is_none(),
        "deprecated" => entry.pkg.extra_meta.contains_key("deprecated") && value.is_none(),
        _ => false,
    }
}

fn has_install_script(pkg: &LockedPackage) -> bool {
    pkg.extra_meta
        .get("hasInstallScript")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

fn source_kind(pkg: &LockedPackage) -> &'static str {
    match pkg.local_source.as_ref() {
        None => "registry",
        Some(LocalSource::Directory(_)) => "file",
        Some(LocalSource::Tarball(_)) => "file",
        Some(LocalSource::Link(_)) => "link",
        Some(LocalSource::Portal(_)) => "portal",
        Some(LocalSource::Exec(_)) => "exec",
        Some(LocalSource::Git(_)) => "git",
        Some(LocalSource::RemoteTarball(_)) => "remote",
    }
}

fn flags(entry: &QueryEntry<'_>) -> Vec<&'static str> {
    let mut out: Vec<_> = entry.direct_types.iter().map(|t| t.label()).collect();
    if entry.peer {
        out.push("peer");
    }
    if entry.transitive {
        out.push("transitive");
    }
    if has_install_script(entry.pkg) {
        out.push("scripts");
    }
    if !entry.pkg.bin.is_empty() {
        out.push("bin");
    }
    out
}

fn print_default(matches: &[&QueryEntry<'_>]) {
    for entry in matches {
        let flags = flags(entry);
        if flags.is_empty() {
            println!(
                "{}@{} {} [{}]",
                entry.pkg.name,
                entry.pkg.version,
                entry.dep_path,
                source_kind(entry.pkg)
            );
        } else {
            println!(
                "{}@{} {} [{}; {}]",
                entry.pkg.name,
                entry.pkg.version,
                entry.dep_path,
                source_kind(entry.pkg),
                flags.join(",")
            );
        }
    }
}

fn print_parseable(matches: &[&QueryEntry<'_>]) {
    for entry in matches {
        println!(
            "{}\t{}\t{}\t{}\t{}",
            entry.dep_path,
            entry.pkg.name,
            entry.pkg.version,
            source_kind(entry.pkg),
            flags(entry).join(",")
        );
    }
}

#[derive(Serialize)]
struct JsonEntry<'a> {
    name: &'a str,
    version: &'a str,
    dep_path: &'a str,
    source: &'a str,
    flags: Vec<&'static str>,
    license: Option<&'a str>,
}

fn print_json(matches: &[&QueryEntry<'_>]) -> miette::Result<()> {
    let entries: Vec<_> = matches
        .iter()
        .map(|entry| JsonEntry {
            name: &entry.pkg.name,
            version: &entry.pkg.version,
            dep_path: &entry.dep_path,
            source: source_kind(entry.pkg),
            flags: flags(entry),
            license: entry.pkg.license.as_deref(),
        })
        .collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&entries)
            .into_diagnostic()
            .wrap_err("failed to serialize query output")?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aube_lockfile::DirectDep;

    fn pkg(name: &str, version: &str, deps: &[(&str, &str)]) -> LockedPackage {
        let dependencies = deps
            .iter()
            .map(|(name, tail)| ((*name).to_string(), (*tail).to_string()))
            .collect();
        LockedPackage {
            name: name.to_string(),
            version: version.to_string(),
            dependencies,
            dep_path: format!("{name}@{version}"),
            ..Default::default()
        }
    }

    fn graph() -> LockfileGraph {
        let mut app = pkg("app", "1.0.0", &[("react", "18.2.0")]);
        app.peer_dependencies
            .insert("react".to_string(), "^18".to_string());
        let mut react = pkg("react", "18.2.0", &[]);
        react
            .bin
            .insert("react-bin".to_string(), "bin.js".to_string());
        let mut esbuild = pkg("esbuild", "0.25.0", &[]);
        esbuild.extra_meta.insert(
            "hasInstallScript".to_string(),
            serde_json::Value::Bool(true),
        );

        let mut packages = BTreeMap::new();
        packages.insert("app@1.0.0".to_string(), app);
        packages.insert("react@18.2.0".to_string(), react);
        packages.insert("esbuild@0.25.0".to_string(), esbuild);

        let mut importers = BTreeMap::new();
        importers.insert(
            ".".to_string(),
            vec![
                DirectDep {
                    name: "app".to_string(),
                    dep_path: "app@1.0.0".to_string(),
                    dep_type: DepType::Production,
                    specifier: None,
                },
                DirectDep {
                    name: "esbuild".to_string(),
                    dep_path: "esbuild@0.25.0".to_string(),
                    dep_type: DepType::Dev,
                    specifier: None,
                },
            ],
        );

        LockfileGraph {
            importers,
            packages,
            ..Default::default()
        }
    }

    #[test]
    fn parses_chained_pseudos() {
        let selector = Selector::parse(":prod:scripts, [name=react]").unwrap();
        assert_eq!(selector.groups.len(), 2);
        assert_eq!(
            selector.groups[0],
            vec![
                Predicate::Pseudo(Pseudo::Prod),
                Predicate::Pseudo(Pseudo::Scripts)
            ]
        );
    }

    #[test]
    fn parses_compact_name_and_attr_pseudos() {
        let selector = Selector::parse("react:peer:bin, [name=esbuild]:scripts").unwrap();
        assert_eq!(
            selector.groups[0],
            vec![
                Predicate::Name("react".to_string()),
                Predicate::Pseudo(Pseudo::Peer),
                Predicate::Pseudo(Pseudo::Bin),
            ]
        );
        assert_eq!(
            selector.groups[1],
            vec![
                Predicate::Attr {
                    key: "name".to_string(),
                    value: Some("esbuild".to_string()),
                },
                Predicate::Pseudo(Pseudo::Scripts),
            ]
        );
    }

    #[test]
    fn rejects_value_comparison_for_boolean_attributes() {
        let err = Selector::parse("[bin=react-bin]").unwrap_err().to_string();
        assert!(err.contains("[bin] does not support value comparisons"));
        let err = Selector::parse("[deprecated=true]")
            .unwrap_err()
            .to_string();
        assert!(err.contains("[deprecated] does not support value comparisons"));
    }

    #[test]
    fn matches_peer_and_bin_from_transitive_node() {
        let graph = graph();
        let entries = collect_entries(&graph, super::super::DepFilter::All, None);
        let selector = Selector::parse(":peer:bin").unwrap();
        let matched: Vec<_> = entries
            .iter()
            .filter(|entry| selector.matches(entry))
            .collect();
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].pkg.name, "react");
        assert!(matched[0].transitive);
    }

    #[test]
    fn dep_filter_limits_reachable_roots() {
        let graph = graph();
        let entries = collect_entries(&graph, super::super::DepFilter::ProdOnly, None);
        assert!(entries.iter().any(|entry| entry.pkg.name == "react"));
        assert!(!entries.iter().any(|entry| entry.pkg.name == "esbuild"));
    }
}
