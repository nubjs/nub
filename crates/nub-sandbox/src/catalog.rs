//! The build-jail catalog parser and its validations — ONE implementation, compiled twice.
//!
//! WHY THIS FILE EXISTS SEPARATELY FROM `build.rs`. The validations below are the security
//! bar for a catalog: they reject a `siblingDirs` entry that escapes the enclosing
//! `node_modules`, a `projectReads` entry that traverses out of the project, a wildcard
//! `$downloads` host, and an entry with no provenance. `build.rs` runs them so a bad catalog
//! fails `cargo build`. The dev-only runtime override (`crate::catalog_override`) must run
//! the SAME ones at load time — `data/README.md` states the rule outright: "the parser for a
//! fetched catalog is the same one as for the baked-in file, with the same build-time
//! validations re-run at load time".
//!
//! A second copy would drift, and a drifted validator on this surface is worse than none: the
//! runtime copy would be the one silently missing a check. So `build.rs` pulls this file in
//! with `#[path]` and the crate pulls it in as a module — one source, two compilations. That
//! is also why it depends on nothing but `std` and `serde_json`: `build.rs` cannot see the
//! crate, so any `crate::` reference here would break the build-script copy.
//!
//! Parsing yields OWNED data rather than the `&'static` the compiled tables use, because a
//! runtime-loaded catalog has no static backing. `build.rs` codegens `&'static` literals from
//! it; the override leaks it. Neither borrows from the JSON text.

use std::collections::BTreeSet;
use std::path::{Component, Path};

/// One package's exception, exactly as the catalog spells it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageGrant {
    pub package: String,
    pub sibling_dirs: Vec<String>,
    /// Chains of package NAMES whose resolved directories the package may write.
    /// `[["prisma"], ["prisma", "@prisma/engines"]]` means "the `prisma` this package
    /// resolves, and the `@prisma/engines` THAT package resolves".
    pub dependency_dirs: Vec<Vec<String>>,
    pub project_reads: Vec<String>,
    /// The dotted field path of a `projectWrites.manifestField`, if the entry has one.
    pub project_writes: Option<Vec<String>>,
    pub project_cwd: bool,
}

/// A parsed, fully-validated catalog: the three tables the jail actually consults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Catalog {
    /// `$downloads` — the egress allowlist, in written order.
    pub download_hosts: Vec<String>,
    pub package_grants: Vec<PackageGrant>,
    /// Packages permitted egress, SORTED and deduplicated so the lookup may binary-search.
    pub package_network_allowed: Vec<String>,
}

/// Parse and validate a catalog document. Every rejection names the offending path so a
/// contributor sees which entry to fix, whether it surfaced from `cargo build` or from a
/// dev override at startup.
pub fn parse(text: &str) -> Result<Catalog, String> {
    let catalog: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("not valid JSON: {e}"))?;

    let download_hosts = parse_hosts(&catalog)?;
    let package_grants = parse_grants(&catalog)?;
    let package_network_allowed = parse_package_network(&catalog)?;

    Ok(Catalog {
        download_hosts,
        package_grants,
        package_network_allowed,
    })
}

// ── $downloads ─────────────────────────────────────────────────────────────────

fn parse_hosts(catalog: &serde_json::Value) -> Result<Vec<String>, String> {
    let entries = array(catalog, "networkHosts")?;
    let mut hosts = Vec::new();
    let mut seen = BTreeSet::new();

    for (i, entry) in entries.iter().enumerate() {
        let at = format!("networkHosts[{i}]");
        let host = string(entry, "host", &at)?;
        require_provenance(entry, &at)?;

        // Wildcard-freedom is the set's structural anti-exfiltration property: the proxy
        // resolves the name itself, so a `*.suffix` member would let a confined script put
        // chosen bytes in a DNS label and leak them without sending a payload.
        if host.contains('*') {
            return Err(format!(
                "{at}: `{host}` — $downloads must stay wildcard-free; a subdomain wildcard \
                 admits DNS-label exfiltration under the same hostname"
            ));
        }
        if host.is_empty() || host.contains('/') || host.contains(':') {
            return Err(format!("{at}: `{host}` is not a bare hostname"));
        }
        if !seen.insert(host.clone()) {
            return Err(format!("{at}: `{host}` is listed twice"));
        }
        hosts.push(host);
    }

    // The refused list is documentation, but a PR that moves an entry INTO the allowlist
    // while leaving its rejection rationale in place is a contradiction the reader would
    // have to resolve by guessing. Fail instead.
    for (i, entry) in array_at(catalog, &["notGranted", "hosts"])?
        .iter()
        .enumerate()
    {
        let at = format!("notGranted.hosts[{i}]");
        let host = string(entry, "host", &at)?;
        string(entry, "reason", &at)?;
        string(entry, "detail", &at)?;
        // Held to the SAME provenance bar as an admitted host. A refusal is the input to a
        // later promotion decision, and an unevidenced one is worse than no entry: it reads
        // as a settled verdict while carrying nothing a reviewer could re-check.
        // `observedUrl` is the field a path-scoped grant would have to be written against.
        require_provenance(entry, &at)?;
        string(entry, "requester", &at)?;
        string(entry, "observedUrl", &at)?;
        if seen.contains(&host) {
            return Err(format!(
                "{at}: `{host}` is in networkHosts AND recorded as refused — one of the two \
                 is wrong"
            ));
        }
    }

    Ok(hosts)
}

// ── the curated per-package grant table ────────────────────────────────────────

fn parse_grants(catalog: &serde_json::Value) -> Result<Vec<PackageGrant>, String> {
    let entries = array(catalog, "packageGrants")?;
    let mut seen = BTreeSet::new();
    let mut grants = Vec::new();

    for (i, entry) in entries.iter().enumerate() {
        let at = format!("packageGrants[{i}]");
        let package = string(entry, "package", &at)?;
        require_provenance(entry, &at)?;
        string(entry, "mechanism", &at)?;
        if !seen.insert(package.clone()) {
            return Err(format!("{at}: `{package}` is listed twice"));
        }

        let sibling_dirs = opt_strings(entry, "siblingDirs", &at)?;
        for dir in &sibling_dirs {
            // A sibling grant names ONE entry of the package's own enclosing node_modules.
            // Anything with a separator or a traversal component is not a sibling — it is a
            // path out of the subtree the grant is bounded by.
            if dir.is_empty()
                || dir == "."
                || dir == ".."
                || dir.contains('/')
                || dir.contains('\\')
            {
                return Err(format!(
                    "{at}: siblingDirs entry `{dir}` must be a single directory NAME — a \
                     separator or traversal component escapes the enclosing node_modules \
                     the grant is bounded by"
                ));
            }
        }

        let dependency_dirs = parse_dependency_dirs(entry, &at)?;

        let project_reads = opt_strings(entry, "projectReads", &at)?;
        for rel in &project_reads {
            require_project_relative(rel, "projectReads", &at)?;
        }

        let project_writes = match entry.get("projectWrites") {
            None | Some(serde_json::Value::Null) => None,
            Some(w) => {
                let field = w
                    .get("manifestField")
                    .and_then(serde_json::Value::as_array)
                    .ok_or_else(|| {
                        format!(
                            "{at}: projectWrites must be {{\"manifestField\": [..]}} — the only \
                             shape the compiler implements"
                        )
                    })?;
                if field.is_empty() {
                    return Err(format!(
                        "{at}: projectWrites.manifestField must name a field"
                    ));
                }
                let mut keys = Vec::with_capacity(field.len());
                for k in field {
                    keys.push(
                        k.as_str()
                            .ok_or_else(|| format!("{at}: manifestField entries must be strings"))?
                            .to_string(),
                    );
                }
                Some(keys)
            }
        };

        let project_cwd = match entry.get("projectCwd") {
            None => false,
            Some(v) => v
                .as_bool()
                .ok_or_else(|| format!("{at}: projectCwd must be a boolean"))?,
        };

        grants.push(PackageGrant {
            package,
            sibling_dirs,
            dependency_dirs,
            project_reads,
            project_writes,
            project_cwd,
        });
    }

    Ok(grants)
}

/// Validate `dependencyDirs` — a list of package-NAME chains, never paths.
///
/// The distinction from `siblingDirs` is the whole security argument. A sibling is a name
/// JOINED to a directory, so a separator in it escapes that directory and the check above
/// rejects one. A dependency chain is never joined: each element is looked up the way Node
/// would, so the only directories it can reach are ones the granted package can already
/// `require`. Rejecting a separator here therefore enforces "this is a name, not a path" —
/// the single scoped `@scope/name` slash is the one exception, because that IS the name.
fn parse_dependency_dirs(entry: &serde_json::Value, at: &str) -> Result<Vec<Vec<String>>, String> {
    let Some(value) = entry.get("dependencyDirs") else {
        return Ok(Vec::new());
    };
    let chains = value
        .as_array()
        .ok_or_else(|| format!("{at}: dependencyDirs must be an array of package-name chains"))?;
    let mut out = Vec::with_capacity(chains.len());
    for (j, chain) in chains.iter().enumerate() {
        let at = format!("{at}.dependencyDirs[{j}]");
        let names = chain
            .as_array()
            .ok_or_else(|| format!("{at}: each chain is an ARRAY of names, e.g. [\"prisma\"]"))?;
        if names.is_empty() {
            return Err(format!("{at}: a chain must name at least one package"));
        }
        let mut resolved = Vec::with_capacity(names.len());
        for n in names {
            let name = n
                .as_str()
                .ok_or_else(|| format!("{at}: chain entries must be strings"))?;
            require_package_name(name, &at)?;
            resolved.push(name.to_string());
        }
        out.push(resolved);
    }
    Ok(out)
}

/// Reject anything that is not an ordinary npm package name.
///
/// `node_modules` is refused by name: the walk joins `node_modules/<name>`, so admitting it
/// would grant the virtual store and `.bin` — the two directories the whole grant table is
/// bounded away from (`.bin` is run UNCONFINED later; the store holds every dependency's
/// source before it executes).
fn require_package_name(name: &str, at: &str) -> Result<(), String> {
    let bad =
        |why: &str| -> Result<(), String> { Err(format!("{at}: package name `{name}` {why}")) };
    if name.is_empty() {
        return bad("is empty");
    }
    if name == "node_modules" {
        return bad("names the virtual store and `.bin`, never a package");
    }
    if name.starts_with('.') || name.contains('\\') {
        return bad("must be a plain package name — no traversal, no backslash");
    }
    let segments: Vec<&str> = name.split('/').collect();
    let unscoped = match segments.as_slice() {
        [one] => *one,
        [scope, rest] if scope.starts_with('@') && scope.len() > 1 => *rest,
        _ => return bad("must be `name` or `@scope/name`"),
    };
    if unscoped.is_empty() || unscoped.starts_with('.') {
        return bad("must be `name` or `@scope/name`");
    }
    Ok(())
}

// ── the per-package egress table ───────────────────────────────────────────────

/// Which packages may reach the network, as a NAME SET. Egress is a per-package BOOLEAN: a
/// catalog entry means the grant was ratified by review, and how much network the package
/// then uses is not something this table narrows.
///
/// TWO SOURCES, ONE VERDICT, because a grant can be spelled either way and both mean "on":
///  - `networkHosts[].fetchedBy` — a package named as fetching an admitted host.
///  - `packageNetwork.full` — a package whose host set was never enumerable in the first place.
///
/// `notGranted.packages` OVERRIDES both. A package refused on the merits gets nothing, and it
/// can legitimately appear in `fetchedBy` as an observation of what it *did* fetch — recording
/// the observation must not become a grant. (Real case: `install-peers` is both.)
fn parse_package_network(catalog: &serde_json::Value) -> Result<Vec<String>, String> {
    let mut refused = BTreeSet::new();
    for (i, entry) in array_at(catalog, &["notGranted", "packages"])?
        .iter()
        .enumerate()
    {
        refused.insert(string(
            entry,
            "package",
            &format!("notGranted.packages[{i}]"),
        )?);
    }

    let mut allowed: BTreeSet<String> = BTreeSet::new();
    for entry in array(catalog, "networkHosts")? {
        for pkg in opt_strings(entry, "fetchedBy", "networkHosts")? {
            allowed.insert(pkg);
        }
    }
    for (i, entry) in array_at(catalog, &["packageNetwork", "full"])?
        .iter()
        .enumerate()
    {
        let at = format!("packageNetwork.full[{i}]");
        require_provenance(entry, &at)?;
        allowed.insert(string(entry, "package", &at)?);
    }
    for name in &refused {
        allowed.remove(name);
    }

    Ok(allowed.into_iter().collect())
}

/// A project-relative subtree, checked before the runtime clamp ever sees it. The clamp in
/// `compiler::curated::contained` is the enforcing check and stays; this one turns a traversal
/// that the clamp would silently DROP into a visible rejection, so a contributor learns the
/// grant does not work instead of shipping one that quietly does nothing.
fn require_project_relative(rel: &str, field: &str, at: &str) -> Result<(), String> {
    let p = Path::new(rel);
    if rel.is_empty() || p.is_absolute() || rel.starts_with('/') || rel.starts_with('\\') {
        return Err(format!(
            "{at}: {field} entry `{rel}` must be project-relative"
        ));
    }
    if p.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(format!(
            "{at}: {field} entry `{rel}` traverses out of the project root"
        ));
    }
    Ok(())
}

/// Every entry carries how it was learned. This is what lets a later reader tell a grant
/// measured against a real denial from one taken on a vendor's word — the two are not
/// equally strong, and the catalog would lose that distinction the moment an entry could
/// be added without stating it.
fn require_provenance(entry: &serde_json::Value, at: &str) -> Result<(), String> {
    const KINDS: &[&str] = &["measured", "vendor-documented", "source-read"];
    let evidence = string(entry, "evidence", at)?;
    if !KINDS.contains(&evidence.as_str()) {
        return Err(format!(
            "{at}: evidence `{evidence}` is not one of {KINDS:?}"
        ));
    }
    if string(entry, "observed", at)?.len() < 20 {
        return Err(format!(
            "{at}: `observed` must state what was actually seen, not a placeholder"
        ));
    }
    string(entry, "platform", at)?;
    Ok(())
}

// ── helpers ────────────────────────────────────────────────────────────────────

fn array<'a>(catalog: &'a serde_json::Value, key: &str) -> Result<&'a [serde_json::Value], String> {
    catalog
        .get(key)
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| format!("`{key}` must be an array"))
}

fn array_at<'a>(
    catalog: &'a serde_json::Value,
    path: &[&str],
) -> Result<&'a [serde_json::Value], String> {
    let mut node = catalog;
    for key in path {
        match node.get(key) {
            Some(next) => node = next,
            None => return Ok(&[]),
        }
    }
    node.as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| format!("`{}` must be an array", path.join(".")))
}

fn string(entry: &serde_json::Value, key: &str, at: &str) -> Result<String, String> {
    entry
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| format!("{at}: `{key}` is required and must be a non-empty string"))
}

fn opt_strings(entry: &serde_json::Value, key: &str, at: &str) -> Result<Vec<String>, String> {
    match entry.get(key) {
        None | Some(serde_json::Value::Null) => Ok(Vec::new()),
        Some(v) => {
            let items = v
                .as_array()
                .ok_or_else(|| format!("{at}: `{key}` must be an array of strings"))?;
            let mut out = Vec::with_capacity(items.len());
            for s in items {
                out.push(
                    s.as_str()
                        .ok_or_else(|| format!("{at}: `{key}` entries must be strings"))?
                        .to_string(),
                );
            }
            Ok(out)
        }
    }
}

#[cfg(test)]
mod tests {
    /// A REFUSAL BEATS AN OBSERVATION — the clause that decides egress when the catalog
    /// records a package BOTH ways, and the one the binary model made load-bearing: since
    /// `preset::build_jail_net` now compiles an entry straight into a network grant, a
    /// `fetchedBy` observation that outranked a refusal would hand egress to a package
    /// refused on the merits.
    ///
    /// Asserted against a synthetic catalog rather than the shipped one on purpose. The
    /// shipped catalog names no refused package, so the same assertion against it would pass
    /// against a generator that had stopped subtracting entirely — a control already at the
    /// expected value proves nothing. The admitted sibling is what makes this non-vacuous:
    /// it shares the `fetchedBy` array with the refused name, so a parser returning an empty
    /// set fails here instead of looking correct.
    #[test]
    fn a_refused_package_gets_no_grant_however_it_was_observed() {
        let catalog = super::parse(
            r#"{
              "networkHosts": [{
                "host": "registry.example.test",
                "fetchedBy": ["refused-pkg", "admitted-pkg"],
                "evidence": "measured",
                "observed": "both packages resolved this host in the same corpus arm",
                "platform": "linux-x64"
              }],
              "packageGrants": [],
              "notGranted": { "packages": [{ "package": "refused-pkg" }] }
            }"#,
        )
        .expect("the synthetic catalog is valid");

        assert_eq!(
            catalog.package_network_allowed,
            vec!["admitted-pkg".to_string()],
            "the refusal must remove `refused-pkg` and leave its `fetchedBy` sibling admitted"
        );
    }

    /// `dependencyDirs` entries are NAMES the resolver looks up, never paths it joins — so a
    /// spelling that would turn one into a path has to be refused here, where both the build
    /// and the dev override run the same check.
    ///
    /// Paired with an accepted control in the same shape, because a validator that rejected
    /// everything would satisfy the rejection half on its own.
    #[test]
    fn a_dependency_chain_entry_must_be_a_package_name() {
        let catalog = |chain: &str| {
            super::parse(&format!(
                r#"{{
                  "networkHosts": [],
                  "packageGrants": [{{
                    "package": "pkg",
                    "dependencyDirs": [{chain}],
                    "mechanism": "re-execs a sibling CLI that writes into its own package dir",
                    "evidence": "measured",
                    "observed": "EPERM writing the resolved dir",
                    "platform": "macos-arm64"
                  }}],
                  "notGranted": {{}}
                }}"#
            ))
        };

        let ok = catalog(r#"["prisma", "@prisma/engines"]"#)
            .expect("a plain name and a scoped name are both valid");
        assert_eq!(
            ok.package_grants[0].dependency_dirs,
            vec![vec!["prisma".to_string(), "@prisma/engines".to_string()]],
            "the accepted control must actually carry the chain"
        );

        // `..` and `a/b` would escape the resolver's one-hop lookup; `node_modules` names the
        // virtual store and `.bin` rather than any package; a bare chain is not an array.
        for rejected in [
            r#"[".."]"#,
            r#"["a/b"]"#,
            r#"["node_modules"]"#,
            r#"["@scope/a/b"]"#,
            r#"[]"#,
            r#""prisma""#,
        ] {
            assert!(
                catalog(rejected).is_err(),
                "dependencyDirs [{rejected}] must be rejected"
            );
        }
    }
}
