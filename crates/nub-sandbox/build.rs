//! Bake `data/build-jail-catalog.json` into the crate as `static` Rust.
//!
//! WHY CODEGEN RATHER THAN A RUNTIME PARSE. The catalog is fixed at compile time, so a
//! parse failure is a defect in the tree — not a condition the running jail should ever
//! have to handle. Emitting `&'static` literals means there is no runtime parse, no
//! fallible path to get wrong, and no way for a malformed catalog to reach a user: it
//! fails `cargo build`. `include_str!` + a lazy parse would instead defer the same defect
//! to first use, where the only honest responses are panic or silently jail-less grants.
//!
//! WHY THE VALIDATION LIVES HERE. Moving the tables from Rust literals to a data file
//! opens an editing surface a `static` did not have — a contributor can now write
//! `"siblingDirs": ["../../.."]` in JSON where the equivalent in Rust would have been
//! conspicuous. The checks below re-close that surface at the earliest possible moment,
//! so a catalog that would widen the jail cannot compile. They are deliberately
//! structural; the security RATIONALE (why a host is admissible at all) stays in the
//! crate's unit tests, which can express it against the compiled policy.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::Path;

const CATALOG: &str = "data/build-jail-catalog.json";

fn main() {
    println!("cargo:rerun-if-changed={CATALOG}");
    println!("cargo:rerun-if-changed=build.rs");

    let text = std::fs::read_to_string(CATALOG)
        .unwrap_or_else(|e| fail(&format!("cannot read {CATALOG}: {e}")));
    let catalog: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| fail(&format!("{CATALOG} is not valid JSON: {e}")));

    let out = std::env::var("OUT_DIR").expect("cargo sets OUT_DIR");
    let out = Path::new(&out);
    emit(&out.join("download_hosts.rs"), &gen_hosts(&catalog));
    emit(&out.join("curated_grants.rs"), &gen_grants(&catalog));
    emit(&out.join("system_paths.rs"), &gen_system_paths(&catalog));
    emit(
        &out.join("package_network.rs"),
        &gen_package_network(&catalog),
    );
    // The documentation-only objects generate nothing, so nothing downstream would notice a
    // malformed one. Checking them here is the only thing that stops the catalog's own record
    // of what was REFUSED — the input to every later promotion decision — from rotting into
    // free text. `notGranted.packages` in particular carries a verdict ("this break is
    // correct") that a reader will act on.
    check_refusals(&catalog);
    check_coverage(&catalog);
}

fn emit(path: &Path, src: &str) {
    std::fs::write(path, src)
        .unwrap_or_else(|e| fail(&format!("cannot write {}: {e}", path.display())));
}

// ── $downloads ─────────────────────────────────────────────────────────────────

fn gen_hosts(catalog: &serde_json::Value) -> String {
    let entries = array(catalog, "networkHosts");
    let mut hosts = Vec::new();
    let mut with_residual = Vec::new();
    let mut seen = BTreeSet::new();

    for (i, entry) in entries.iter().enumerate() {
        let at = format!("networkHosts[{i}]");
        let host = string(entry, "host", &at);
        require_provenance(entry, &at);

        // A host admitted DESPITE a write-capable or multi-tenant exposure must say so, and
        // the declaration is generated so a test can hold the two in agreement. v1 refused
        // this class outright; v2 admits some of it on best-effort grounds, and the honest
        // form of that reversal is a NAMED residual per host rather than a quietly wider
        // list. `residual` is optional overall and REQUIRED for the shapes checked in
        // `no_undeclared_residual_host_reached_downloads`.
        if entry.get("residual").is_some() {
            string(entry, "residual", &at);
            with_residual.push(host.clone());
        }

        // Wildcard-freedom is the set's structural anti-exfiltration property: the proxy
        // resolves the name itself, so a `*.suffix` member would let a confined script put
        // chosen bytes in a DNS label and leak them without sending a payload.
        if host.contains('*') {
            fail(&format!(
                "{at}: `{host}` — $downloads must stay wildcard-free; a subdomain wildcard \
                 admits DNS-label exfiltration under the same hostname"
            ));
        }
        if host.is_empty() || host.contains('/') || host.contains(':') {
            fail(&format!("{at}: `{host}` is not a bare hostname"));
        }
        if !seen.insert(host.clone()) {
            fail(&format!("{at}: `{host}` is listed twice"));
        }
        hosts.push(host);
    }

    // The refused list is documentation, but a PR that moves an entry INTO the allowlist
    // while leaving its rejection rationale in place is a contradiction the reader would
    // have to resolve by guessing. Fail instead.
    for (i, entry) in array_at(catalog, &["notGranted", "hosts"])
        .iter()
        .enumerate()
    {
        let at = format!("notGranted.hosts[{i}]");
        let host = string(entry, "host", &at);
        string(entry, "reason", &at);
        string(entry, "detail", &at);
        // Held to the SAME provenance bar as an admitted host. A refusal is the input to a
        // later promotion decision, and an unevidenced one is worse than no entry: it reads
        // as a settled verdict while carrying nothing a reviewer could re-check.
        // `observedUrl` is what a later reviewer re-checks the refusal against.
        require_provenance(entry, &at);
        string(entry, "requester", &at);
        string(entry, "observedUrl", &at);
        if seen.contains(&host) {
            fail(&format!(
                "{at}: `{host}` is in networkHosts AND recorded as refused — one of the two \
                 is wrong"
            ));
        }
    }

    let mut src = String::from(
        "// @generated by build.rs from data/build-jail-catalog.json — do not edit.\n\
         /// The curated install-time artifact hosts — the build jail's egress allowlist and\n\
         /// the `$downloads` net token. Membership criteria, per-host provenance, and the\n\
         /// record of what was refused live in `data/build-jail-catalog.json`.\n\
         pub const DOWNLOAD_HOSTS: &[&str] = &[\n",
    );
    for host in &hosts {
        let _ = writeln!(src, "    {host:?},");
    }
    src.push_str("];\n");
    let _ = write!(
        src,
        "/// The subset of [`DOWNLOAD_HOSTS`] admitted with a NAMED residual exposure — a write\n\
         /// route or a multi-tenant namespace under the same hostname, accepted because the\n\
         /// jail is defense in depth and refusing the host costs more than the residual. Each\n\
         /// entry's `residual` field in the catalog states what remains and what bounds it.\n\
         pub const DOWNLOAD_HOSTS_WITH_RESIDUAL: &[&str] = &{with_residual:?};\n"
    );
    src
}

// ── the curated per-package grant table ────────────────────────────────────────

fn gen_grants(catalog: &serde_json::Value) -> String {
    let entries = array(catalog, "packageGrants");
    let mut seen = BTreeSet::new();
    let mut src = String::from(
        "// @generated by build.rs from data/build-jail-catalog.json — do not edit.\n\
         static CURATED_GRANTS: &[(&str, CuratedGrant)] = &[\n",
    );

    for (i, entry) in entries.iter().enumerate() {
        let at = format!("packageGrants[{i}]");
        let name = string(entry, "package", &at);
        require_provenance(entry, &at);
        string(entry, "mechanism", &at);
        if !seen.insert(name.clone()) {
            fail(&format!("{at}: `{name}` is listed twice"));
        }

        let siblings = opt_strings(entry, "siblingDirs", &at);
        for dir in &siblings {
            // A sibling grant names ONE entry of the package's own enclosing node_modules.
            // Anything with a separator or a traversal component is not a sibling — it is a
            // path out of the subtree the grant is bounded by.
            if dir.is_empty()
                || dir == "."
                || dir == ".."
                || dir.contains('/')
                || dir.contains('\\')
            {
                fail(&format!(
                    "{at}: siblingDirs entry `{dir}` must be a single directory NAME — a \
                     separator or traversal component escapes the enclosing node_modules \
                     the grant is bounded by"
                ));
            }
        }

        let reads = opt_strings(entry, "projectReads", &at);
        for rel in &reads {
            require_project_relative(rel, "projectReads", &at);
        }

        let nodes = opt_strings(entry, "projectNodes", &at);
        for rel in &nodes {
            require_project_relative(rel, "projectNodes", &at);
        }

        let writes = match entry.get("projectWrites") {
            None | Some(serde_json::Value::Null) => "ProjectWrites::None".to_string(),
            Some(w) => {
                // Exactly one of the two shapes, never both: they resolve against different
                // authorities (nub's literal vs the consumer's manifest) and a reader could not
                // tell which one bounds the grant if an entry carried each.
                let manifest = w.get("manifestField");
                let literal = w.get("literal");
                match (manifest, literal) {
                    (Some(_), Some(_)) => fail(&format!(
                        "{at}: projectWrites carries both `manifestField` and `literal` — pick the \
                         one that bounds this package's write"
                    )),
                    (Some(field), None) => {
                        let keys = string_array(field, "projectWrites.manifestField", &at);
                        format!("ProjectWrites::ManifestField(&{keys:?})")
                    }
                    (None, Some(paths)) => {
                        let paths = string_array(paths, "projectWrites.literal", &at);
                        for rel in &paths {
                            require_project_relative(rel, "projectWrites.literal", &at);
                        }
                        format!("ProjectWrites::Literal(&{paths:?})")
                    }
                    (None, None) => fail(&format!(
                        "{at}: projectWrites must be {{\"manifestField\": [..]}} or \
                         {{\"literal\": [..]}} — the only two shapes the compiler implements"
                    )),
                }
            }
        };

        let cwd = entry
            .get("projectCwd")
            .map(|v| {
                v.as_bool()
                    .unwrap_or_else(|| fail(&format!("{at}: projectCwd must be a boolean")))
            })
            .unwrap_or(false);

        require_failure_mode(entry, &at);

        let _ = write!(
            src,
            "    (\n        {name:?},\n        CuratedGrant {{\n            \
             sibling_dirs: &{siblings:?},\n            project_reads: &{reads:?},\n            \
             project_nodes: &{nodes:?},\n            project_writes: {writes},\n            \
             project_cwd: {cwd},\n        }},\n    ),\n"
        );
    }

    src.push_str("];\n");
    src
}

// ── systemPaths (recorded, not yet enforced) ───────────────────────────────────

/// The per-platform system read paths. Generated even though nothing folds them into the jail
/// surface yet: the alternative is that the recommendation lives only in prose, where it drifts
/// from the evidence beside it and nobody notices. Codegen also enforces the shape now rather
/// than at whatever later date the grant is wired in.
fn gen_system_paths(catalog: &serde_json::Value) -> String {
    let mut src = String::from(
        "// @generated by build.rs from data/build-jail-catalog.json — do not edit.\n",
    );
    for (key, konst) in [
        ("linux", "SYSTEM_READ_PATHS_LINUX"),
        ("macos", "SYSTEM_READ_PATHS_MACOS"),
        ("windows", "SYSTEM_READ_PATHS_WINDOWS"),
    ] {
        let entries = array_at(catalog, &["systemPaths", "platforms", key]);
        if entries.is_empty() {
            fail(&format!(
                "systemPaths.platforms.{key} must be a non-empty array"
            ));
        }
        let mut seen = BTreeSet::new();
        let mut paths = Vec::new();
        for (i, entry) in entries.iter().enumerate() {
            let at = format!("systemPaths.platforms.{key}[{i}]");
            let path = string(entry, "path", &at);
            // The `why` is the whole value of the entry — a bare path list would be
            // indistinguishable from a guess a year from now.
            string(entry, "why", &at);
            require_system_path(&path, &at);
            if !seen.insert(path.clone()) {
                fail(&format!("{at}: `{path}` is listed twice"));
            }
            paths.push(path);
        }
        let _ = write!(
            src,
            "/// System library and header paths the build jail should grant READ on {key}.\n\
             /// RECORDED, NOT YET ENFORCED — see `systemPaths.status` in the catalog.\n\
             /// An entry may be `$NAME`- or `%NAME%`-anchored; the EMBEDDER resolves those.\n\
             pub const {konst}: &[&str] = &{paths:?};\n"
        );
    }
    src
}

/// A system path is absolute, or anchored on an env var the embedder resolves. Globs are
/// rejected for the same reason a wildcard host is: `subtree_globs` adds the recursion, so a
/// `*` here can only widen a rule past the directory the entry names.
fn require_system_path(path: &str, at: &str) {
    if path.contains('*') || path.contains('?') {
        fail(&format!(
            "{at}: `{path}` must not contain a glob — subtree recursion is added by the \
             compiler, so a pattern here can only widen the grant"
        ));
    }
    let anchored = path.starts_with('$') || path.starts_with('%');
    let absolute = path.starts_with('/')
        || path
            .as_bytes()
            .get(1)
            .is_some_and(|b| *b == b':' && path.as_bytes()[0].is_ascii_alphabetic());
    if !anchored && !absolute {
        fail(&format!(
            "{at}: `{path}` must be absolute, or anchored on `$VAR` / `%VAR%`"
        ));
    }
}

// ── per-package network ────────────────────────────────────────────────────────

/// The per-package egress table: which packages may reach the network, and how much.
///
/// THE HOST LISTS ARE NOT AUTHORED HERE — they are INVERTED from `networkHosts[].fetchedBy`,
/// which already records, per host, the packages observed to fetch it. Restating the same
/// facts package-first would be two sources that drift; inverting them at build time means a
/// host entry and this table cannot disagree.
///
/// `packageNetwork.full` is the one thing authored package-first, because it is the one
/// thing a host list cannot express: a package whose host set is undetermined, unenumerable,
/// or chosen at RUNTIME. `full` supersedes any host list the inversion produced for the same
/// package (`node-libcurl` legitimately appears under both `github.com` and `nodejs.org`).
fn gen_package_network(catalog: &serde_json::Value) -> String {
    let mut hosts: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for entry in array(catalog, "networkHosts") {
        let host = string(entry, "host", "networkHosts");
        for pkg in string_array(
            entry.get("fetchedBy").unwrap_or(&serde_json::Value::Null),
            "fetchedBy",
            "networkHosts",
        ) {
            hosts.entry(pkg).or_default().push(host.clone());
        }
    }

    let refused: BTreeSet<String> = array_at(catalog, &["notGranted", "packages"])
        .iter()
        .enumerate()
        .map(|(i, e)| string(e, "package", &format!("notGranted.packages[{i}]")))
        .collect();

    let mut full = Vec::new();
    for (i, entry) in array_at(catalog, &["packageNetwork", "full"])
        .iter()
        .enumerate()
    {
        let at = format!("packageNetwork.full[{i}]");
        let name = string(entry, "package", &at);
        string(entry, "versions", &at);
        string(entry, "why", &at);
        require_provenance(entry, &at);
        // The reason must be one of the three shapes a host list genuinely cannot express.
        // Without this the value degenerates into "we did not finish the analysis", which is
        // a different thing and belongs in `notGranted.deferred`.
        const REASONS: &[&str] = &["unenumerable", "undetermined", "runtime-selected"];
        let reason = string(entry, "reason", &at);
        if !REASONS.contains(&reason.as_str()) {
            fail(&format!(
                "{at}: reason `{reason}` is not one of {REASONS:?}"
            ));
        }
        // A package refused on the merits gets NOTHING. The two axes are independent — a
        // package can want network and still be refused filesystem access — but a refusal
        // means the dependency should not have the access at all, and handing it unrestricted
        // egress would invert that.
        if refused.contains(&name) {
            fail(&format!(
                "{at}: `{name}` is recorded in notGranted.packages — a package refused on the \
                 merits must not also receive full network"
            ));
        }
        // `full` supersedes, so the inverted host list would be dead data.
        hosts.remove(&name);
        full.push(name);
    }

    let mut src = String::from(
        "// @generated by build.rs from data/build-jail-catalog.json — do not edit.\n\
         /// Packages granted UNRESTRICTED egress: their host set is undetermined,\n\
         /// unenumerable, or selected at runtime, so a host list cannot express it.\n\
         pub const PACKAGE_NETWORK_FULL: &[&str] = &",
    );
    let _ = writeln!(src, "{full:?};");
    src.push_str(
        "/// Per-package host allowlists, INVERTED from `networkHosts[].fetchedBy` so the two\n\
         /// cannot drift. Sorted by package name; a package with `full` is absent here.\n\
         pub const PACKAGE_NETWORK_HOSTS: &[(&str, &[&str])] = &[\n",
    );
    for (pkg, mut hs) in hosts {
        hs.sort();
        hs.dedup();
        let _ = writeln!(src, "    ({pkg:?}, &{hs:?}),");
    }
    src.push_str("];\n");
    src
}

// ── the documentation-only objects ─────────────────────────────────────────────

/// `notGranted.packages` records a VERDICT — that a measured break is the jail working — so it
/// is held to the same provenance bar as a grant. A refusal a reader cannot re-check is worse
/// than no entry, because it reads as settled while carrying nothing.
fn check_refusals(catalog: &serde_json::Value) {
    let granted: BTreeSet<String> = array(catalog, "packageGrants")
        .iter()
        .enumerate()
        .map(|(i, e)| string(e, "package", &format!("packageGrants[{i}]")))
        .collect();

    for (i, entry) in array_at(catalog, &["notGranted", "packages"])
        .iter()
        .enumerate()
    {
        let at = format!("notGranted.packages[{i}]");
        let name = string(entry, "package", &at);
        string(entry, "reason", &at);
        string(entry, "wants", &at);
        string(entry, "detail", &at);
        string(entry, "versions", &at);
        require_provenance(entry, &at);
        require_failure_mode(entry, &at);
        if granted.contains(&name) {
            fail(&format!(
                "{at}: `{name}` is in packageGrants AND recorded as refused — one of the two is \
                 wrong"
            ));
        }
    }

    for (i, entry) in array_at(catalog, &["notGranted", "deferred"])
        .iter()
        .enumerate()
    {
        let at = format!("notGranted.deferred[{i}]");
        let name = string(entry, "package", &at);
        string(entry, "scope", &at);
        string(entry, "why", &at);
        require_failure_mode(entry, &at);
        if granted.contains(&name) {
            fail(&format!(
                "{at}: `{name}` is deferred AND granted — a grant supersedes a deferral, so \
                 remove the deferral"
            ));
        }
    }
}

/// The coverage table is DERIVED data in a hand-edited file, which is exactly the shape that
/// drifts silently. Pinning each row's count to the host entry's own `fetchedBy` length is what
/// keeps the percentages in `corpus` honest as hosts are added.
fn check_coverage(catalog: &serde_json::Value) {
    let mut counts = std::collections::BTreeMap::new();
    for (i, entry) in array(catalog, "networkHosts").iter().enumerate() {
        let at = format!("networkHosts[{i}]");
        let host = string(entry, "host", &at);
        string(entry, "artifact", &at);
        let n = string_array(
            entry
                .get("fetchedBy")
                .unwrap_or_else(|| fail(&format!("{at}: `fetchedBy` is required"))),
            "fetchedBy",
            &at,
        )
        .len();
        counts.insert(host, n);
    }
    let refused: BTreeSet<String> = array_at(catalog, &["notGranted", "hosts"])
        .iter()
        .enumerate()
        .map(|(i, e)| string(e, "host", &format!("notGranted.hosts[{i}]")))
        .collect();

    for (i, row) in array_at(catalog, &["corpus", "networkCoverage"])
        .iter()
        .enumerate()
    {
        let at = format!("corpus.networkCoverage[{i}]");
        let host = string(row, "host", &at);
        let claimed = row
            .get("packages")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_else(|| fail(&format!("{at}: `packages` must be a number")));
        match counts.get(&host) {
            // A refused host has no `fetchedBy` to check against; its coverage row is the
            // record of what the refusal COSTS, which is the point of keeping the row.
            None if refused.contains(&host) => {}
            None => fail(&format!(
                "{at}: `{host}` is in neither networkHosts nor notGranted.hosts"
            )),
            Some(actual) if *actual as u64 != claimed => fail(&format!(
                "{at}: `{host}` claims {claimed} packages but its networkHosts entry lists \
                 {actual} in `fetchedBy`"
            )),
            Some(_) => {}
        }
    }
}

/// The failure a DENIAL produces. `SILENT` is why the field exists: an exit-code measurement
/// cannot see it, so it has to be carried as data or it is lost.
fn require_failure_mode(entry: &serde_json::Value, at: &str) {
    const MODES: &[&str] = &["LOUD", "SILENT", "GRACEFUL", "UNDETERMINED"];
    let mode = string(entry, "failureMode", at);
    if !MODES.contains(&mode.as_str()) {
        fail(&format!(
            "{at}: failureMode `{mode}` is not one of {MODES:?} — normalize the prose onto the \
             enum and keep the nuance in `observed`"
        ));
    }
}

/// A project-relative subtree, checked before the runtime clamp ever sees it. The clamp in
/// `curated::contained` is the enforcing check and stays; this one turns a traversal that
/// the clamp would silently DROP into a visible build failure, so a contributor learns the
/// grant does not work instead of shipping one that quietly does nothing.
fn require_project_relative(rel: &str, field: &str, at: &str) {
    let p = Path::new(rel);
    if rel.is_empty() || p.is_absolute() || rel.starts_with('/') || rel.starts_with('\\') {
        fail(&format!(
            "{at}: {field} entry `{rel}` must be project-relative"
        ));
    }
    if p.components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        fail(&format!(
            "{at}: {field} entry `{rel}` traverses out of the project root"
        ));
    }
}

/// Every entry carries how it was learned. This is what lets a later reader tell a grant
/// measured against a real denial from one taken on a vendor's word — the two are not
/// equally strong, and the catalog would lose that distinction the moment an entry could
/// be added without stating it.
fn require_provenance(entry: &serde_json::Value, at: &str) {
    const KINDS: &[&str] = &["measured", "vendor-documented", "source-read"];
    let evidence = string(entry, "evidence", at);
    if !KINDS.contains(&evidence.as_str()) {
        fail(&format!(
            "{at}: evidence `{evidence}` is not one of {KINDS:?}"
        ));
    }
    if string(entry, "observed", at).len() < 20 {
        fail(&format!(
            "{at}: `observed` must state what was actually seen, not a placeholder"
        ));
    }
    string(entry, "platform", at);
}

// ── helpers ────────────────────────────────────────────────────────────────────

fn array<'a>(catalog: &'a serde_json::Value, key: &str) -> &'a Vec<serde_json::Value> {
    catalog
        .get(key)
        .and_then(serde_json::Value::as_array)
        .unwrap_or_else(|| fail(&format!("{CATALOG}: `{key}` must be an array")))
}

fn array_at<'a>(catalog: &'a serde_json::Value, path: &[&str]) -> &'a [serde_json::Value] {
    let mut node = catalog;
    for key in path {
        match node.get(key) {
            Some(next) => node = next,
            None => return &[],
        }
    }
    node.as_array()
        .map(Vec::as_slice)
        .unwrap_or_else(|| fail(&format!("{CATALOG}: `{}` must be an array", path.join("."))))
}

fn string(entry: &serde_json::Value, key: &str, at: &str) -> String {
    entry
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| {
            fail(&format!(
                "{at}: `{key}` is required and must be a non-empty string"
            ))
        })
}

/// A non-empty array of non-empty strings, checked at the VALUE rather than by key so it can
/// serve both a named field and a nested one (`projectWrites.literal`).
fn string_array(value: &serde_json::Value, what: &str, at: &str) -> Vec<String> {
    let items = value
        .as_array()
        .unwrap_or_else(|| fail(&format!("{at}: `{what}` must be an array of strings")));
    if items.is_empty() {
        fail(&format!("{at}: `{what}` must not be empty"));
    }
    items
        .iter()
        .map(|v| {
            v.as_str()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| {
                    fail(&format!("{at}: `{what}` entries must be non-empty strings"))
                })
                .to_string()
        })
        .collect()
}

fn opt_strings(entry: &serde_json::Value, key: &str, at: &str) -> Vec<String> {
    match entry.get(key) {
        None | Some(serde_json::Value::Null) => Vec::new(),
        Some(v) => v
            .as_array()
            .unwrap_or_else(|| fail(&format!("{at}: `{key}` must be an array of strings")))
            .iter()
            .map(|s| {
                s.as_str()
                    .unwrap_or_else(|| fail(&format!("{at}: `{key}` entries must be strings")))
                    .to_string()
            })
            .collect(),
    }
}

fn fail(msg: &str) -> ! {
    // Printed as a cargo error so it surfaces at the top of the build output rather than
    // buried in a panic backtrace.
    println!("cargo:warning={msg}");
    panic!("build-jail catalog is invalid — {msg}");
}
