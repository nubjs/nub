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
//! is also why it depends on nothing but `std`, `serde_json` and `semver`: `build.rs` cannot
//! see the crate, so any `crate::` reference here would break the build-script copy, and every
//! external crate it names has to be declared as a build-dependency as well as a dependency.
//!
//! Parsing yields OWNED data rather than the `&'static` the compiled tables use, because a
//! runtime-loaded catalog has no static backing. `build.rs` codegens `&'static` literals from
//! it; the override leaks it. Neither borrows from the JSON text.

use std::collections::BTreeSet;
use std::path::{Component, Path};

/// Where a package's project WRITE targets come from — the two spellings of
/// `projectWrites`, which are distinguished because they carry different authorship.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectWriteSource {
    /// A dotted field path read out of the CONSUMER's root manifest. nub owns the field
    /// NAME, the consumer owns the value.
    ManifestField(Vec<String>),
    /// Project-relative subtrees nub names outright, for a package that writes to a
    /// location IT defines rather than one the consumer configures. `.git/hooks` is the
    /// case: a hook installer's whole function is to write there, and no manifest field
    /// carries the answer because the path is git's, not the consumer's.
    Literal(Vec<String>),
}

/// One `$HOME`-anchored artifact cache a package downloads into, keyed by the package's
/// OWN documented override variable.
///
/// PER-OS, because the default this has to reproduce is per-OS: `cachedir('Cypress')` is
/// `~/Library/Caches/Cypress` on macOS and `$XDG_CACHE_HOME/Cypress` on Linux. An entry may
/// omit a platform, which means nub has measured nothing there and grants nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomePath {
    /// The package's own documented cache-override environment variable.
    pub env: String,
    pub macos: Option<String>,
    pub linux: Option<String>,
    pub windows: Option<String>,
}

/// One package's exception, exactly as the catalog spells it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageGrant {
    pub package: String,
    /// The semver range this grant is scoped to, or `None` for every version.
    pub versions: Option<String>,
    /// The TERMINAL tier: this package's lifecycle scripts see the whole filesystem,
    /// read and write. See [`parse_full_disk`] for what it is and why it is not an
    /// escalation.
    pub full_disk: bool,
    pub sibling_dirs: Vec<String>,
    /// `$HOME`-anchored artifact caches, with the env var that redirects each one.
    pub home_paths: Vec<HomePath>,
    /// Chains of package NAMES whose resolved directories the package may write.
    /// `[["prisma"], ["prisma", "@prisma/engines"]]` means "the `prisma` this package
    /// resolves, and the `@prisma/engines` THAT package resolves".
    pub dependency_dirs: Vec<Vec<String>>,
    pub project_reads: Vec<String>,
    /// Where the entry's project writes come from, if it has any.
    pub project_writes: Option<ProjectWriteSource>,
    pub project_cwd: bool,
}

/// One package admitted to the network, and the versions the admission covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageNetworkGrant {
    pub package: String,
    /// The semver range this grant is scoped to, or `None` for every version.
    pub versions: Option<String>,
}

/// A parsed, fully-validated catalog. Three tables, and only TWO of them are consulted by
/// the build jail — a distinction worth stating here because collapsing it has misled
/// readers repeatedly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Catalog {
    /// The hosts nub's own prefetcher may GET from, and the `$downloads` set in the
    /// `nub sandbox` policy language. **NOT a build-jail egress filter**: the jail gates
    /// egress per package as a BOOLEAN and starts no proxy, so this list neither widens nor
    /// narrows what a confined script can reach. Expect it to stay tiny while
    /// `package_network_allowed` grows into the hundreds — they measure different things.
    /// In written order.
    pub download_hosts: Vec<String>,
    pub package_grants: Vec<PackageGrant>,
    /// Packages permitted egress — ALL of it, to any host, or none. SORTED by package name
    /// and deduplicated so the lookup may binary-search.
    pub package_network_allowed: Vec<PackageNetworkGrant>,
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

// ── networkHosts: what NUB fetches, not what a jailed script may reach ─────────

/// Parse `networkHosts` — the hosts nub's own prefetcher may GET from, outside the jail.
/// Nothing here constrains a confined script: build-jail egress is a per-package boolean
/// (see `compiler::preset::build_jail_net`). Kept adjacent to `packageNetwork` in one file
/// because they are edited together, NOT because one filters the other.
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
        let versions = parse_version_range(entry, &at)?;
        let full_disk = parse_full_disk(entry, &at)?;

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
        let home_paths = parse_home_paths(entry, &at)?;

        let project_reads = opt_strings(entry, "projectReads", &at)?;
        for rel in &project_reads {
            require_project_relative(rel, "projectReads", &at)?;
        }

        let project_writes = match entry.get("projectWrites") {
            None | Some(serde_json::Value::Null) => None,
            Some(w) => Some(parse_project_writes(w, &at)?),
        };

        let project_cwd = match entry.get("projectCwd") {
            None => false,
            Some(v) => v
                .as_bool()
                .ok_or_else(|| format!("{at}: projectCwd must be a boolean"))?,
        };

        grants.push(PackageGrant {
            package,
            versions,
            full_disk,
            sibling_dirs,
            home_paths,
            dependency_dirs,
            project_reads,
            project_writes,
            project_cwd,
        });
    }

    Ok(grants)
}

/// The optional semver range scoping an entry to the versions its measurement covers.
///
/// ABSENT MEANS EVERY VERSION, and that default is what makes the field safe to add to a
/// catalog whose entries were all measured on one version and granted to all of them: an
/// unscoped entry means exactly what it meant before this field was parsed. A range is
/// therefore an act of narrowing that someone measured a BOUNDARY for — `esbuild`'s `<0.13.0`
/// is the version `optionalDependencies` landed in, above which the package resolves a
/// prebuilt binary and opens no socket — never a restatement of "we happened to test 1.2.3".
/// The version a measurement ran against is `versionsObserved`, which is prose and gates
/// nothing.
///
/// A malformed range FAILS THE BUILD rather than degrading to unscoped. The two silent
/// readings are both worse than a compile error: treating it as "all versions" widens a grant
/// its author meant to narrow, and treating it as "no versions" makes the entry inert while
/// still reading as present.
fn parse_version_range(entry: &serde_json::Value, at: &str) -> Result<Option<String>, String> {
    // A prose note is allowed to sit beside the range, but only as its own key. The two were
    // one field until 2026-07-31, which is precisely the trap this split closes: every entry
    // carried a `versions` string that looked like a condition and constrained nothing.
    match entry.get("versionsObserved") {
        None | Some(serde_json::Value::Null) => {}
        Some(v) => {
            v.as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| format!("{at}: `versionsObserved` must be a non-empty string"))?;
        }
    }

    let Some(value) = entry.get("versions") else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let text = value
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            format!("{at}: `versions` must be a semver range string, e.g. \"<0.13.0\"")
        })?;
    semver::VersionReq::parse(text).map_err(|e| {
        format!(
            "{at}: versions `{text}` is not a semver range ({e}) — a note about which \
             versions were measured belongs in `versionsObserved`, which constrains nothing"
        )
    })?;
    Ok(Some(text.to_string()))
}

/// The last rung of the grant ladder: this package's lifecycle scripts get the whole
/// filesystem, read and write.
///
/// WHY THIS IS A REDUCTION AND NOT AN ESCALATION. Outside nub a lifecycle script already
/// runs with the user's COMPLETE authority — every file the user can touch, plus the
/// network, plus their environment. `fullDisk` withholds two of those three: the env axis
/// still scrubs the credential family and redirects `HOME`, and egress is still decided by
/// `packageNetwork`, which this field does not touch. So the widest thing this grant can
/// produce is strictly narrower than `npm install` on the same package.
///
/// PACKAGE IDENTITY REMAINS THE ENTIRE GATE, exactly as for every narrower field. The key
/// is aube's installer-resolved `registry_name()` (see the module docs on `curated`), an
/// UNCATALOGUED package gets nothing whatsoever, and a dependency has no spelling by which
/// it can name itself in here. Adding a terminal rung does not weaken that; it only means a
/// package nub has already chosen to trust by name is trusted the way the ecosystem already
/// trusts it.
///
/// WHY IT EXISTS. Without it, a package that fails under every targeted grant is an open
/// investigation — and a catalog campaign that has to root-cause its own tail never
/// finishes. With it, that package is one catalog line, so 100% compatibility is reachable
/// by construction and scope-narrowing becomes an optimisation to do LATER, from a green
/// baseline, rather than a prerequisite.
///
/// `evidence: "measured"` IS REQUIRED, and it is the only enforceable form of "the narrower
/// grants were tried first". A `policy` full-disk row would be a guess that hands over the
/// disk; `vendor-documented` and `source-read` establish what a package INTENDS, never that
/// every narrower cell of the grant ladder was run and failed. Only a measurement can, so
/// the widest tier is the one tier that may not be taken on anything else. The prose naming
/// which rungs were tried belongs in `mechanism`, which every grant already carries.
///
/// An explicit `false` is REJECTED rather than read as absent. The refusal channel is
/// `notGranted`; a `"fullDisk": false` in a grant entry reads as a deliberate denial the
/// schema has no such meaning for, and silently treating it as absence is how a reader ends
/// up believing a field constrains something it does not — the exact trap `versions` was in
/// until it was made to parse.
fn parse_full_disk(entry: &serde_json::Value, at: &str) -> Result<bool, String> {
    let Some(value) = entry.get("fullDisk") else {
        return Ok(false);
    };
    match value.as_bool() {
        Some(true) => {}
        Some(false) => {
            return Err(format!(
                "{at}: `fullDisk` is present and false — omit the key entirely, or record \
                 the refusal under `notGranted`"
            ));
        }
        None => return Err(format!("{at}: `fullDisk` must be the boolean true")),
    }
    let evidence = string(entry, "evidence", at)?;
    if evidence != "measured" {
        return Err(format!(
            "{at}: a `fullDisk` grant requires evidence `measured`, not `{evidence}` — it is \
             the widest tier, and only a measurement can establish that every narrower grant \
             was tried and insufficient"
        ));
    }
    Ok(true)
}

/// Validate `homePaths` — the `$HOME`-anchored artifact caches, one entry per override var.
///
/// THE PATH IS WRITTEN IN THE SURFACE PATTERN LANGUAGE, restricted to the two `$HOME`-family
/// anchors. `~/…` is the user's home and `$cache/…` is the platform cache root, which is what
/// makes an entry track `XDG_CACHE_HOME` on Linux and `%LOCALAPPDATA%` on Windows for free —
/// the packages this exists for compute their defaults from exactly those roots, so an entry
/// spelled any other way would aim at a directory the tool does not read back at run time.
/// The closed anchor set is also the bound: `$tmp` is the jail's own per-run scratch, a bare
/// absolute path would let the catalog name any directory on the machine, and a
/// project-relative path is already `projectWrites`.
fn parse_home_paths(entry: &serde_json::Value, at: &str) -> Result<Vec<HomePath>, String> {
    let Some(value) = entry.get("homePaths") else {
        return Ok(Vec::new());
    };
    let items = value.as_array().ok_or_else(|| {
        format!("{at}: homePaths must be an array of {{env, macos?, linux?, windows?}}")
    })?;
    let mut seen = BTreeSet::new();
    let mut out = Vec::with_capacity(items.len());
    for (j, item) in items.iter().enumerate() {
        let at = format!("{at}.homePaths[{j}]");
        let env = string(item, "env", &at)?;
        require_env_name(&env, &at)?;
        if !seen.insert(env.clone()) {
            return Err(format!("{at}: `{env}` is listed twice for one package"));
        }
        let mut per_os = [None, None, None];
        for (slot, key) in per_os.iter_mut().zip(["macos", "linux", "windows"]) {
            match item.get(key) {
                None | Some(serde_json::Value::Null) => {}
                Some(v) => {
                    let path = v
                        .as_str()
                        .filter(|s| !s.trim().is_empty())
                        .ok_or_else(|| format!("{at}: `{key}` must be a non-empty string"))?;
                    require_home_anchored(path, key, &at)?;
                    *slot = Some(path.to_string());
                }
            }
        }
        let [macos, linux, windows] = per_os;
        if macos.is_none() && linux.is_none() && windows.is_none() {
            return Err(format!(
                "{at}: name at least one of `macos`/`linux`/`windows` — an entry with none \
                 grants nothing anywhere"
            ));
        }
        out.push(HomePath {
            env,
            macos,
            linux,
            windows,
        });
    }
    Ok(out)
}

/// An environment variable nub may SET for the confined child, refusing the names whose value
/// the jail itself decides.
///
/// The reserved set is not stylistic. `HOME`/`USERPROFILE` are what
/// `preset::compile_build_jail` points at the private jail home — a `homePaths` entry
/// overwriting one would undo the redirect the whole build jail rests on — and the rest are
/// the roots this very field expands against (`XDG_CACHE_HOME`, `LOCALAPPDATA`, `TMPDIR`) or
/// resolution paths a grant has no business steering (`PATH`, `NODE_OPTIONS`, `NODE_PATH`).
fn require_env_name(name: &str, at: &str) -> Result<(), String> {
    const RESERVED: &[&str] = &[
        "HOME",
        "USERPROFILE",
        "PATH",
        "TMPDIR",
        "TEMP",
        "TMP",
        "APPDATA",
        "LOCALAPPDATA",
        "XDG_CACHE_HOME",
        "NODE_OPTIONS",
        "NODE_PATH",
    ];
    // SCREAMING_SNAKE only. Every package's documented override is spelled that way, and the
    // restriction makes the Windows case-insensitive env insert unambiguous rather than
    // something a catalog entry could produce two spellings of.
    let shaped = name.starts_with(|c: char| c.is_ascii_uppercase())
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    if !shaped {
        return Err(format!(
            "{at}: env `{name}` must be an UPPERCASE_SNAKE environment variable name"
        ));
    }
    if RESERVED.contains(&name) {
        return Err(format!(
            "{at}: env `{name}` is reserved — the jail decides its value"
        ));
    }
    Ok(())
}

/// The home-relative roots a `homePaths` target may hang off — the platform CACHE directories
/// and nothing else. `$cache/` is already a cache root by construction (`XDG_CACHE_HOME`,
/// `%LOCALAPPDATA%`, else `~/.cache`); the two `~/`-relative spellings are the conventions a
/// tool computing its own default from `homedir()` lands on, which is the case the whole field
/// exists for (puppeteer's `~/.cache/puppeteer`, Cypress's `~/Library/Caches/Cypress`).
const HOME_CACHE_ROOTS: &[&str] = &["$cache/", "~/.cache/", "~/Library/Caches/"];

/// A path anchored at `~/` or `$cache/`, under a CACHE root, with no traversal and no glob
/// metacharacter.
///
/// ⛔ THE CACHE-ROOT BOUND IS WHAT MAKES THIS GRANT SAFE, and until now nothing enforced it.
/// A `homePaths` entry is a LIVE read-write grant on the user's real `$HOME`, handed to a
/// dependency's lifecycle script for the whole run — the one place the jail deliberately
/// reaches outside itself. [`super::compiler::curated::CuratedGrant::home_paths`] argues that
/// is safe because NUB authors the path, so it can never be the persistence vector a
/// copy-the-private-home-out design would open: `~/.zshrc`, `~/.config/git/config` with
/// `core.hooksPath`, `~/Library/LaunchAgents/*`. That argument is about which directories may
/// be named, and the anchor check alone does not make it — `~/.ssh` and `~/Library/LaunchAgents`
/// are both `~/`-anchored, traversal-free and glob-free, so both were accepted. The catalog is
/// an edited text surface (see this module's header), so the invariant has to be a rule here
/// rather than a property of the entries that happen to be in the file today.
///
/// Both shipped entries are cache-rooted already, so this rejects nothing that exists; what it
/// buys is that the next entry cannot quietly be somewhere else.
fn require_home_anchored(path: &str, key: &str, at: &str) -> Result<(), String> {
    let rest = path
        .strip_prefix("~/")
        .or_else(|| path.strip_prefix("$cache/"))
        .ok_or_else(|| {
            format!(
                "{at}: {key} path `{path}` must start with `~/` or `$cache/` — those are the \
                 only anchors a home-cache grant may hang off"
            )
        })?;
    if rest.trim().is_empty() {
        return Err(format!(
            "{at}: {key} path `{path}` names the anchor itself, never a directory under it"
        ));
    }
    if path.contains('*') || path.contains('?') || path.contains('\\') {
        return Err(format!(
            "{at}: {key} path `{path}` must be a literal forward-slash path — no glob \
             metacharacter, no backslash"
        ));
    }
    if Path::new(rest)
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return Err(format!(
            "{at}: {key} path `{path}` traverses out of its anchor"
        ));
    }
    // AFTER the traversal check, so `~/.cache/../.ssh` cannot pass by prefix alone.
    if !HOME_CACHE_ROOTS.iter().any(|r| path.starts_with(r)) {
        return Err(format!(
            "{at}: {key} path `{path}` must name a directory under a cache root \
             ({}) — a homePaths entry is a live write on the real $HOME, and anywhere \
             else is a persistence vector",
            HOME_CACHE_ROOTS.join(", ")
        ));
    }
    Ok(())
}

/// Validate `projectWrites` — exactly one of `manifestField` or `literal`.
///
/// EXACTLY ONE, rejected rather than merged, because the two differ in who authored the
/// path and a reader has to be able to tell at a glance. `manifestField` grants what the
/// CONSUMER wrote in their own manifest; `literal` grants what NUB wrote in this file. An
/// entry carrying both would be one grant with two provenances and one `observed` string
/// covering neither cleanly.
///
/// A `literal` entry is held to the same project-relative check as `projectReads`: the
/// runtime clamp in `compiler::curated::contained` would silently DROP a traversal, so
/// rejecting it here is what turns an inert grant into a visible build failure.
fn parse_project_writes(w: &serde_json::Value, at: &str) -> Result<ProjectWriteSource, String> {
    let field = w.get("manifestField");
    let literal = w.get("literal");
    match (field, literal) {
        (Some(_), Some(_)) => Err(format!(
            "{at}: projectWrites carries both `manifestField` and `literal` — pick the one \
             that matches who authored the path"
        )),
        (Some(f), None) => {
            let keys = write_targets(f, at, "manifestField")?;
            Ok(ProjectWriteSource::ManifestField(keys))
        }
        (None, Some(l)) => {
            let paths = write_targets(l, at, "literal")?;
            for rel in &paths {
                require_project_relative(rel, "projectWrites.literal", at)?;
            }
            Ok(ProjectWriteSource::Literal(paths))
        }
        (None, None) => Err(format!(
            "{at}: projectWrites must be {{\"manifestField\": [..]}} or {{\"literal\": [..]}}"
        )),
    }
}

fn write_targets(value: &serde_json::Value, at: &str, key: &str) -> Result<Vec<String>, String> {
    let items = value
        .as_array()
        .ok_or_else(|| format!("{at}: projectWrites.{key} must be an array of strings"))?;
    if items.is_empty() {
        return Err(format!(
            "{at}: projectWrites.{key} must name at least one entry"
        ));
    }
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        out.push(
            item.as_str()
                .ok_or_else(|| format!("{at}: projectWrites.{key} entries must be strings"))?
                .to_string(),
        );
    }
    Ok(out)
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
///
/// ONLY `packageNetwork.full` MAY BE VERSION-SCOPED. A `fetchedBy` name is an observation
/// attached to a HOST, carrying no version of its own, so it can only ever mean "every
/// version" — and a package spelled both ways with a range on the `full` side would be an
/// entry that reads as narrowed while the other spelling silently re-widens it. That
/// contradiction is rejected rather than resolved, on the same principle as the refused-host
/// check above: a reader must not have to guess which spelling won.
fn parse_package_network(catalog: &serde_json::Value) -> Result<Vec<PackageNetworkGrant>, String> {
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

    // `BTreeMap` rather than a `Vec` so the result is sorted by name and deduplicated, which
    // is what the compiled table's `binary_search` rests on. The value is the range in force.
    let mut allowed: std::collections::BTreeMap<String, Option<String>> =
        std::collections::BTreeMap::new();
    let mut fetched_by: BTreeSet<String> = BTreeSet::new();
    for entry in array(catalog, "networkHosts")? {
        for pkg in opt_strings(entry, "fetchedBy", "networkHosts")? {
            fetched_by.insert(pkg.clone());
            allowed.insert(pkg, None);
        }
    }
    for (i, entry) in array_at(catalog, &["packageNetwork", "full"])?
        .iter()
        .enumerate()
    {
        let at = format!("packageNetwork.full[{i}]");
        require_provenance(entry, &at)?;
        let package = string(entry, "package", &at)?;
        let versions = parse_version_range(entry, &at)?;
        if versions.is_some() && fetched_by.contains(&package) {
            return Err(format!(
                "{at}: `{package}` is version-scoped here but also named in a \
                 networkHosts[].fetchedBy array, which is unscoped — the two spellings \
                 disagree about which versions are admitted"
            ));
        }
        allowed.insert(package, versions);
    }
    for name in &refused {
        allowed.remove(name);
    }

    Ok(allowed
        .into_iter()
        .map(|(package, versions)| PackageNetworkGrant { package, versions })
        .collect())
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
///
/// `policy` is the odd one out and deliberately so: the other three assert an OBSERVATION,
/// while a policy grant asserts only a JUDGEMENT — "this package is too widely depended on
/// to risk breaking, so it gets egress before anyone measures whether it needs it." It was
/// added 2026-07-31 when egress was granted by download rank, and it exists precisely so
/// that widening cannot hide: `evidence` partitions the catalog into what we saw and what
/// we decided, and a policy row is the one a later measurement should replace. Labelling
/// such a row `measured` would be the actual harm here — it would launder a judgement call
/// into evidence and leave nothing to audit.
fn require_provenance(entry: &serde_json::Value, at: &str) -> Result<(), String> {
    const KINDS: &[&str] = &["measured", "vendor-documented", "source-read", "policy"];
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
            catalog
                .package_network_allowed
                .iter()
                .map(|g| g.package.as_str())
                .collect::<Vec<_>>(),
            vec!["admitted-pkg"],
            "the refusal must remove `refused-pkg` and leave its `fetchedBy` sibling admitted"
        );
    }

    /// `versions` is the one field that NARROWS an entry, so a malformed one must fail the
    /// build rather than degrade — both silent readings are wrong in a different direction
    /// (unscoped widens what its author narrowed; inert leaves a grant that reads as present).
    ///
    /// The accepted control is what makes this non-vacuous, and it also pins the split that
    /// motivated the field: a prose note in `versionsObserved` is fine, the SAME prose in
    /// `versions` is rejected. Every entry in the shipped catalog carried exactly that prose
    /// under the `versions` name until 2026-07-31, unparsed and constraining nothing.
    #[test]
    fn a_version_scope_is_a_real_range_or_the_build_fails() {
        let catalog = |scope: &str| {
            super::parse(&format!(
                r#"{{
                  "networkHosts": [],
                  "packageGrants": [{{
                    "package": "pkg",
                    {scope}
                    "siblingDirs": ["@types"],
                    "mechanism": "copies generated typings into a sibling of its own package dir",
                    "evidence": "measured",
                    "observed": "EPERM writing the sibling directory under the jail",
                    "platform": "macos-arm64"
                  }}],
                  "notGranted": {{}}
                }}"#
            ))
        };

        let scoped = catalog(r#""versions": "<0.13.0","#).expect("a semver range is valid");
        assert_eq!(
            scoped.package_grants[0].versions.as_deref(),
            Some("<0.13.0")
        );
        let unscoped = catalog("").expect("an absent range is valid");
        assert_eq!(
            unscoped.package_grants[0].versions, None,
            "an absent `versions` must stay absent — it is what makes every existing entry \
             keep meaning every version"
        );
        let noted = catalog(r#""versionsObserved": "6.x (7.0.0 dropped the postinstall)","#)
            .expect("prose in versionsObserved is provenance and constrains nothing");
        assert_eq!(noted.package_grants[0].versions, None);

        for rejected in [
            // The prose the field used to hold, in the field that now enforces.
            r#""versions": "6.x (7.0.0 dropped the postinstall entirely)","#,
            r#""versions": "13.x, measured on 13.14.2","#,
            r#""versions": "not a range","#,
            // Shapes with nothing to enforce.
            r#""versions": "","#,
            r#""versions": 13,"#,
            r#""versions": ["<0.13.0"],"#,
            // A note that carries no note.
            r#""versionsObserved": "","#,
        ] {
            assert!(catalog(rejected).is_err(), "{rejected} must be rejected");
        }
    }

    /// A package cannot be admitted unscoped by one spelling and scoped by the other. The
    /// `fetchedBy` observation hangs off a HOST and names no version, so it can only mean
    /// every version — silently letting it outrank a range would make a narrowed entry inert
    /// while it still read as narrowed.
    #[test]
    fn a_scoped_egress_entry_may_not_be_re_widened_by_a_fetched_by_observation() {
        let catalog = |versions: &str| {
            super::parse(&format!(
                r#"{{
                  "networkHosts": [{{
                    "host": "cdn.example.test",
                    "fetchedBy": ["pkg"],
                    "evidence": "measured",
                    "observed": "the package resolved this host during its postinstall",
                    "platform": "linux-x64"
                  }}],
                  "packageGrants": [],
                  "packageNetwork": {{ "full": [{{
                    "package": "pkg",
                    {versions}
                    "evidence": "measured",
                    "observed": "the package resolved this host during its postinstall",
                    "platform": "linux-x64"
                  }}] }},
                  "notGranted": {{}}
                }}"#
            ))
        };
        assert!(catalog(r#""versions": "<2.0.0","#).is_err());
        // The control: the same pair without a range is the ordinary both-spellings case the
        // catalog already contains, and stays admitted exactly once.
        let both = catalog("").expect("an unscoped entry may be spelled both ways");
        assert_eq!(both.package_network_allowed.len(), 1);
        assert_eq!(both.package_network_allowed[0].versions, None);
    }

    /// A `homePaths` entry writes into the user's REAL home, so its two halves — the anchor
    /// and the variable — are the ones a mistake would widen. Both are checked here, where the
    /// build and the dev override run the same code, and both are paired with an accepted
    /// control so a validator that rejected everything could not satisfy the rejection half.
    #[test]
    fn a_home_path_is_anchored_and_never_names_a_reserved_variable() {
        let catalog = |paths: &str| {
            super::parse(&format!(
                r#"{{
                  "networkHosts": [],
                  "packageGrants": [{{
                    "package": "pkg",
                    "homePaths": {paths},
                    "mechanism": "downloads its binary into a $HOME cache it reads back at run time",
                    "evidence": "measured",
                    "observed": "the tool cannot find its own binary after a confined install",
                    "platform": "macos-arm64"
                  }}],
                  "notGranted": {{}}
                }}"#
            ))
        };

        let ok = catalog(
            r#"[{"env": "TOOL_CACHE", "macos": "~/Library/Caches/Tool", "linux": "$cache/Tool"}]"#,
        )
        .expect("both anchors and a per-OS split are valid");
        let got = &ok.package_grants[0].home_paths;
        assert_eq!(got.len(), 1, "the accepted control must carry the entry");
        assert_eq!(got[0].env, "TOOL_CACHE");
        assert_eq!(got[0].macos.as_deref(), Some("~/Library/Caches/Tool"));
        assert_eq!(got[0].linux.as_deref(), Some("$cache/Tool"));
        assert_eq!(
            got[0].windows, None,
            "an omitted platform stays absent — it must not inherit another's path"
        );
        // The third cache root, and the one both shipped entries use. Its own positive control,
        // so the rejection list below cannot be satisfied by a rule that refuses everything.
        catalog(r#"[{"env": "TOOL_CACHE", "macos": "~/.cache/Tool"}]"#)
            .expect("`~/.cache/` is a cache root");

        for rejected in [
            // Anchors outside the closed set: an absolute path names anything on the machine,
            // `$tmp` is the jail's own scratch, and a bare relative is `projectWrites`' job.
            r#"[{"env": "T", "macos": "/etc"}]"#,
            r#"[{"env": "T", "macos": "$tmp/Tool"}]"#,
            r#"[{"env": "T", "macos": "Library/Caches/Tool"}]"#,
            // The anchor itself, and a traversal out of it.
            r#"[{"env": "T", "macos": "~/"}]"#,
            r#"[{"env": "T", "macos": "~/../../etc"}]"#,
            // A glob would widen the rule past the directory the entry names.
            r#"[{"env": "T", "macos": "~/Library/Caches/*"}]"#,
            // ⛔ ANCHORED, TRAVERSAL-FREE, GLOB-FREE — AND STILL A PERSISTENCE VECTOR. These
            // are the paths `CuratedGrant::home_paths` names as the reason it grants a
            // directory nub authored rather than copying the private home out, so a catalog
            // that could name them would give up the argument the field rests on. Each is a
            // live rw grant on the user's real $HOME for the whole lifecycle run.
            r#"[{"env": "T", "macos": "~/.ssh"}]"#,
            r#"[{"env": "T", "macos": "~/.config/git"}]"#,
            r#"[{"env": "T", "macos": "~/Library/LaunchAgents"}]"#,
            r#"[{"env": "T", "macos": "~/.zshrc"}]"#,
            r#"[{"env": "T", "linux": "~/.local/share/Tool"}]"#,
            // The bound is on the WHOLE path, not a prefix: a traversal that starts inside a
            // cache root still lands outside one.
            r#"[{"env": "T", "macos": "~/.cache/../.ssh"}]"#,
            // Variables the jail itself decides.
            r#"[{"env": "HOME", "macos": "~/Library/Caches/Tool"}]"#,
            r#"[{"env": "PATH", "macos": "~/Library/Caches/Tool"}]"#,
            // Shapes with nothing to grant, or two spellings of one variable.
            r#"[{"env": "T"}]"#,
            r#"[{"env": "t", "macos": "~/Tool"}]"#,
            r#"[{"env": "T", "macos": "~/a"}, {"env": "T", "linux": "~/b"}]"#,
            r#"{"env": "T", "macos": "~/Tool"}"#,
        ] {
            assert!(
                catalog(rejected).is_err(),
                "homePaths {rejected} must be rejected"
            );
        }
    }

    /// `fullDisk` is the widest tier the catalog can express, so the two things that keep it
    /// honest are enforced here rather than left to review: it may only be taken on a
    /// MEASUREMENT (nothing else can establish that every narrower rung was tried and
    /// failed), and it may not be spelled as an explicit `false`, which would read as a
    /// refusal the schema has no meaning for.
    ///
    /// Paired with an accepted control, and the control asserts the field actually ARRIVES —
    /// a parser that returned `false` unconditionally would satisfy every rejection below
    /// while making the whole tier inert.
    #[test]
    fn a_full_disk_grant_is_a_measurement_or_it_is_not_a_grant() {
        let catalog = |fields: &str, evidence: &str| {
            super::parse(&format!(
                r#"{{
                  "networkHosts": [],
                  "packageGrants": [{{
                    "package": "pkg",
                    {fields}
                    "mechanism": "every narrower rung was measured and failed; the script writes into another package's store entry",
                    "evidence": "{evidence}",
                    "observed": "fails ungranted and with project+egress together, passes with the whole filesystem",
                    "platform": "darwin-arm64"
                  }}],
                  "notGranted": {{}}
                }}"#
            ))
        };

        let granted = catalog(r#""fullDisk": true,"#, "measured")
            .expect("a measured full-disk grant is valid");
        assert!(
            granted.package_grants[0].full_disk,
            "the accepted control must carry the field — a parser that always returned false \
             would pass every rejection below and grant nothing"
        );
        let absent = catalog("", "measured").expect("an absent key is valid");
        assert!(!absent.package_grants[0].full_disk);

        for (fields, evidence) in [
            // Judgement, vendor prose and source-reading can each say what a package
            // INTENDS; none can say that the narrower rungs were run and failed.
            (r#""fullDisk": true,"#, "policy"),
            (r#""fullDisk": true,"#, "vendor-documented"),
            (r#""fullDisk": true,"#, "source-read"),
            // A refusal has its own channel; this spelling is not it.
            (r#""fullDisk": false,"#, "measured"),
            // Shapes that read as a grant and are not a boolean.
            (r#""fullDisk": "true","#, "measured"),
            (r#""fullDisk": 1,"#, "measured"),
        ] {
            assert!(
                catalog(fields, evidence).is_err(),
                "fullDisk {fields} with evidence {evidence} must be rejected"
            );
        }
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
