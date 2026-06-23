//! `aube peers check` — inspect peer-dependency resolution.
//!
//! Reads the lockfile and reports any package whose declared peer
//! dependencies are missing from its resolved context, or whose resolved
//! peer version doesn't satisfy the declared range. Mirrors
//! `pnpm peers check` (added in pnpm v11).
//!
//! This is a pure read — no network, no filesystem mutation, no project
//! lock. Exits non-zero when at least one unmet or missing peer is
//! reported, so it's CI-friendly.

use aube_lockfile::LockfileGraph;
use clap::{Args, Subcommand};
use std::collections::BTreeMap;

pub const CHECK_AFTER_LONG_HELP: &str = "\
Examples:

  $ aube peers check
  All peer dependencies are satisfied.

  # With issues
  $ aube peers check
  1 unmet, 1 missing peer dependencies:

  ├─┬ @emotion/react@11.11.4
  │ └── ✕ unmet peer react@>=16.8: found 17.0.2
  └─┬ react-dom@18.2.0
    └── ✕ missing peer react@^18.0.0

  # Machine-readable
  $ aube peers check --json
";

#[derive(Debug, Args)]
pub struct PeersArgs {
    #[command(subcommand)]
    pub command: PeersCommand,
}

#[derive(Debug, Subcommand)]
pub enum PeersCommand {
    /// Check for unmet and missing peer-dependency issues by reading the
    /// lockfile.
    ///
    /// Exits with status 1 if any issue is reported.
    #[command(after_long_help = CHECK_AFTER_LONG_HELP)]
    Check(PeersCheckArgs),
}

#[derive(Debug, Args)]
pub struct PeersCheckArgs {
    /// Emit a JSON report instead of the human-readable tree.
    #[arg(long)]
    pub json: bool,
}

pub async fn run(args: PeersArgs) -> miette::Result<Option<i32>> {
    match args.command {
        PeersCommand::Check(a) => check(a).await,
    }
}

async fn check(args: PeersCheckArgs) -> miette::Result<Option<i32>> {
    let cwd = crate::dirs::project_root()?;

    let manifest = super::load_manifest(&cwd.join("package.json"))?;

    let graph = super::load_graph(
        &cwd,
        &manifest,
        &format!(
            "no lockfile found\nhelp: run `{}` first",
            aube_util::cmd("install")
        ),
    )?;

    let issues = collect_issues(&graph, &cwd);

    if args.json {
        print_json(&issues);
    } else {
        print_human(&issues);
    }

    if !issues.is_empty() {
        // pnpm-compat: exit 1 when any peer-dependency issue is found.
        // Return the code for the binary's single `std::process::exit`
        // rather than terminating here, keeping the command embed-safe.
        return Ok(Some(1));
    }
    Ok(None)
}

#[derive(Debug, Clone)]
struct Issue {
    consumer_name: String,
    consumer_version: String,
    peer_name: String,
    peer_range: String,
    kind: IssueKind,
}

#[derive(Debug, Clone)]
enum IssueKind {
    /// Peer is declared but not resolved in the consumer's context, and
    /// it isn't marked optional.
    Missing,
    /// Peer is resolved but the version doesn't satisfy the declared range.
    Unmet { found: String },
    /// Resolved version or range failed to parse — surfaced so the user
    /// can spot a malformed lockfile rather than silently dropping it.
    Unparseable { found: String },
}

/// Resolve a workspace `link:`/`portal:`/`file:` peer tail to the
/// linked package's version. pnpm records a peer satisfied by a
/// workspace member as `link:<path>` (e.g. `vue@link:packages/vue`) in
/// the consumer's lockfile `dependencies`; that path tail is not a
/// semver version, so it must be resolved to the linked member's
/// `package.json` version before the satisfaction check. Returns None
/// when the spec isn't a local link or the target version is
/// unreadable (caller then falls back to the raw tail → Unparseable).
fn link_target_version(tail: &str, project_root: &std::path::Path) -> Option<String> {
    let rel = tail
        .strip_prefix("link:")
        .or_else(|| tail.strip_prefix("portal:"))
        .or_else(|| tail.strip_prefix("file:"))?;
    let manifest_path = project_root.join(rel).join("package.json");
    let pj = super::load_manifest(&manifest_path).ok()?;
    pj.version
}

fn collect_issues(graph: &LockfileGraph, project_root: &std::path::Path) -> Vec<Issue> {
    let mut out: Vec<Issue> = Vec::new();
    for pkg in graph.packages.values() {
        for (peer_name, peer_range) in &pkg.peer_dependencies {
            let optional = pkg
                .peer_dependencies_meta
                .get(peer_name)
                .map(|m| m.optional)
                .unwrap_or(false);

            // The resolved peer (if any) lives in the consumer's
            // `dependencies` map after the peer-context pass. Values may
            // carry a nested suffix like "18.2.0(prop-types@15.8.1)" — strip
            // it to get the bare version for semver comparison.
            let resolved_tail = pkg.dependencies.get(peer_name);
            match resolved_tail {
                Some(tail) => {
                    // A workspace `link:`/`portal:`/`file:` tail resolves to
                    // the linked member's manifest version — a workspace
                    // peer is satisfied by the local member, not flagged
                    // unparseable.
                    let resolved_version = link_target_version(tail, project_root);
                    let version_str = resolved_version
                        .as_deref()
                        .unwrap_or_else(|| tail.split_once('(').map(|(v, _)| v).unwrap_or(tail));
                    match (
                        node_semver::Version::parse(version_str),
                        node_semver::Range::parse(peer_range),
                    ) {
                        (Ok(v), Ok(r)) if v.satisfies(&r) => {}
                        (Ok(_), Ok(_)) => out.push(Issue {
                            consumer_name: pkg.name.clone(),
                            consumer_version: pkg.version.clone(),
                            peer_name: peer_name.clone(),
                            peer_range: peer_range.clone(),
                            kind: IssueKind::Unmet {
                                found: version_str.to_string(),
                            },
                        }),
                        _ => out.push(Issue {
                            consumer_name: pkg.name.clone(),
                            consumer_version: pkg.version.clone(),
                            peer_name: peer_name.clone(),
                            peer_range: peer_range.clone(),
                            kind: IssueKind::Unparseable {
                                found: version_str.to_string(),
                            },
                        }),
                    }
                }
                None if !optional => out.push(Issue {
                    consumer_name: pkg.name.clone(),
                    consumer_version: pkg.version.clone(),
                    peer_name: peer_name.clone(),
                    peer_range: peer_range.clone(),
                    kind: IssueKind::Missing,
                }),
                None => {}
            }
        }
    }

    // Stable, deterministic order: by consumer, then peer.
    out.sort_by(|a, b| {
        (&a.consumer_name, &a.consumer_version, &a.peer_name).cmp(&(
            &b.consumer_name,
            &b.consumer_version,
            &b.peer_name,
        ))
    });
    out
}

fn print_human(issues: &[Issue]) {
    if issues.is_empty() {
        println!("All peer dependencies are satisfied.");
        return;
    }

    // Group by consumer (name@version) for the tree-style output.
    let mut groups: BTreeMap<(String, String), Vec<&Issue>> = BTreeMap::new();
    for i in issues {
        groups
            .entry((i.consumer_name.clone(), i.consumer_version.clone()))
            .or_default()
            .push(i);
    }

    let mut n_unmet = 0usize;
    let mut n_missing = 0usize;
    let mut n_unparseable = 0usize;
    for i in issues {
        match &i.kind {
            IssueKind::Missing => n_missing += 1,
            IssueKind::Unmet { .. } => n_unmet += 1,
            IssueKind::Unparseable { .. } => n_unparseable += 1,
        }
    }
    let extra = if n_unparseable > 0 {
        format!(", {n_unparseable} unparseable")
    } else {
        String::new()
    };
    println!("{n_unmet} unmet, {n_missing} missing{extra} peer dependencies:");
    println!();

    let group_count = groups.len();
    for (gi, ((name, version), group)) in groups.iter().enumerate() {
        let last_group = gi + 1 == group_count;
        let group_connector = if last_group { "└─┬" } else { "├─┬" };
        let child_prefix = if last_group { "  " } else { "│ " };
        println!("{group_connector} {name}@{version}");
        for (i, issue) in group.iter().enumerate() {
            let last = i + 1 == group.len();
            let branch = if last { "└──" } else { "├──" };
            match &issue.kind {
                IssueKind::Missing => println!(
                    "{child_prefix}{branch} ✕ missing peer {}@{}",
                    issue.peer_name, issue.peer_range
                ),
                IssueKind::Unmet { found } => println!(
                    "{child_prefix}{branch} ✕ unmet peer {}@{}: found {}",
                    issue.peer_name, issue.peer_range, found
                ),
                IssueKind::Unparseable { found } => println!(
                    "{child_prefix}{branch} ? could not check peer {}@{} (found {found})",
                    issue.peer_name, issue.peer_range
                ),
            }
        }
    }
}

fn print_json(issues: &[Issue]) {
    let arr: Vec<serde_json::Value> = issues
        .iter()
        .map(|i| {
            let mut obj = serde_json::Map::new();
            obj.insert(
                "consumer".into(),
                format!("{}@{}", i.consumer_name, i.consumer_version).into(),
            );
            obj.insert("name".into(), i.peer_name.clone().into());
            obj.insert("range".into(), i.peer_range.clone().into());
            let (kind, found) = match &i.kind {
                IssueKind::Missing => ("missing", None),
                IssueKind::Unmet { found } => ("unmet", Some(found.clone())),
                IssueKind::Unparseable { found } => ("unparseable", Some(found.clone())),
            };
            obj.insert("kind".into(), kind.into());
            if let Some(f) = found {
                obj.insert("found".into(), f.into());
            }
            serde_json::Value::Object(obj)
        })
        .collect();
    let json = serde_json::to_string_pretty(&serde_json::Value::Array(arr))
        .unwrap_or_else(|_| "[]".to_string());
    println!("{json}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use aube_lockfile::{LockedPackage, LockfileGraph, PeerDepMeta};
    use std::collections::BTreeMap;

    fn pkg_with_peer(
        name: &str,
        version: &str,
        peer: (&str, &str),
        resolved: Option<&str>,
        optional: bool,
    ) -> LockedPackage {
        let mut peer_dependencies = BTreeMap::new();
        peer_dependencies.insert(peer.0.to_string(), peer.1.to_string());
        let mut peer_dependencies_meta = BTreeMap::new();
        if optional {
            peer_dependencies_meta.insert(peer.0.to_string(), PeerDepMeta { optional: true });
        }
        let mut dependencies = BTreeMap::new();
        if let Some(v) = resolved {
            dependencies.insert(peer.0.to_string(), v.to_string());
        }
        LockedPackage {
            name: name.to_string(),
            version: version.to_string(),
            dependencies,
            peer_dependencies,
            peer_dependencies_meta,
            dep_path: format!("{name}@{version}"),
            ..Default::default()
        }
    }

    fn graph_of(pkgs: Vec<LockedPackage>) -> LockfileGraph {
        let mut packages = BTreeMap::new();
        for p in pkgs {
            packages.insert(p.dep_path.clone(), p);
        }
        LockfileGraph {
            packages,
            ..Default::default()
        }
    }

    // Project root is irrelevant for non-link resolved tails; use the
    // current dir so the helper signature is satisfied.
    fn no_root() -> std::path::PathBuf {
        std::path::PathBuf::from(".")
    }

    #[test]
    fn satisfied_peer_produces_no_issue() {
        let g = graph_of(vec![pkg_with_peer(
            "styled",
            "6.1.0",
            ("react", "^18.0.0"),
            Some("18.2.0"),
            false,
        )]);
        assert!(collect_issues(&g, &no_root()).is_empty());
    }

    #[test]
    fn missing_required_peer_is_reported() {
        let g = graph_of(vec![pkg_with_peer(
            "styled",
            "6.1.0",
            ("react", "^18.0.0"),
            None,
            false,
        )]);
        let issues = collect_issues(&g, &no_root());
        assert_eq!(issues.len(), 1);
        assert!(matches!(issues[0].kind, IssueKind::Missing));
    }

    #[test]
    fn missing_optional_peer_is_silent() {
        let g = graph_of(vec![pkg_with_peer(
            "styled",
            "6.1.0",
            ("react", "^18.0.0"),
            None,
            true,
        )]);
        assert!(collect_issues(&g, &no_root()).is_empty());
    }

    #[test]
    fn unmet_version_is_reported_with_found() {
        let g = graph_of(vec![pkg_with_peer(
            "styled",
            "6.1.0",
            ("react", "^18.0.0"),
            Some("17.0.2"),
            false,
        )]);
        let issues = collect_issues(&g, &no_root());
        assert_eq!(issues.len(), 1);
        match &issues[0].kind {
            IssueKind::Unmet { found } => assert_eq!(found, "17.0.2"),
            other => panic!("expected Unmet, got {other:?}"),
        }
    }

    #[test]
    fn unparseable_version_is_reported() {
        let g = graph_of(vec![pkg_with_peer(
            "styled",
            "6.1.0",
            ("react", "^18.0.0"),
            Some("not-a-semver-version"),
            false,
        )]);
        let issues = collect_issues(&g, &no_root());
        assert_eq!(issues.len(), 1);
        assert!(matches!(issues[0].kind, IssueKind::Unparseable { .. }));
    }

    #[test]
    fn nested_peer_suffix_in_resolved_value_is_stripped() {
        let g = graph_of(vec![pkg_with_peer(
            "styled",
            "6.1.0",
            ("react", "^18.0.0"),
            Some("18.2.0(prop-types@15.8.1)"),
            false,
        )]);
        assert!(collect_issues(&g, &no_root()).is_empty());
    }

    // Regression for the pnpm-11 monorepo case (vuejs/core): a registry
    // package's peer satisfied by a WORKSPACE member is recorded as
    // `link:<path>` in the consumer's lockfile deps (e.g. plugin-vue's
    // `vue: link:packages/vue`). The check must resolve the link to the
    // member's manifest version and verify satisfaction — not report it
    // unparseable.
    #[test]
    fn workspace_link_peer_resolves_to_member_version() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let member_dir = tmp.path().join("packages/vue");
        std::fs::create_dir_all(&member_dir).expect("mkdir member");
        std::fs::write(
            member_dir.join("package.json"),
            r#"{"name":"vue","version":"3.5.38"}"#,
        )
        .expect("write member manifest");

        let g = graph_of(vec![pkg_with_peer(
            "@vitejs/plugin-vue",
            "6.0.7",
            ("vue", "^3.2.25"),
            Some("link:packages/vue"),
            false,
        )]);

        // 3.5.38 satisfies ^3.2.25 → no issue.
        assert!(collect_issues(&g, tmp.path()).is_empty());
    }

    // A workspace-link peer whose member version does NOT satisfy the
    // declared range is a genuine Unmet (resolved, but out of range) —
    // not Unparseable.
    #[test]
    fn workspace_link_peer_out_of_range_is_unmet() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let member_dir = tmp.path().join("packages/vue");
        std::fs::create_dir_all(&member_dir).expect("mkdir member");
        std::fs::write(
            member_dir.join("package.json"),
            r#"{"name":"vue","version":"2.7.0"}"#,
        )
        .expect("write member manifest");

        let g = graph_of(vec![pkg_with_peer(
            "@vitejs/plugin-vue",
            "6.0.7",
            ("vue", "^3.2.25"),
            Some("link:packages/vue"),
            false,
        )]);

        let issues = collect_issues(&g, tmp.path());
        assert_eq!(issues.len(), 1);
        match &issues[0].kind {
            IssueKind::Unmet { found } => assert_eq!(found, "2.7.0"),
            other => panic!("expected Unmet, got {other:?}"),
        }
    }
}
