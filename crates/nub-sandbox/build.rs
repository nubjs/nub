//! Bake `data/build-jail-catalog.json` into the crate as `static` Rust.
//!
//! WHY CODEGEN RATHER THAN A RUNTIME PARSE. The catalog is fixed at compile time, so a
//! parse failure is a defect in the tree — not a condition the running jail should ever
//! have to handle. Emitting `&'static` literals means there is no runtime parse, no
//! fallible path to get wrong, and no way for a malformed catalog to reach a user: it
//! fails `cargo build`. `include_str!` + a lazy parse would instead defer the same defect
//! to first use, where the only honest responses are panic or silently jail-less grants.
//!
//! WHY THE VALIDATION LIVES HERE. The catalog is a text surface a contributor edits
//! directly, so the checks below reject a malformed or widening entry at the earliest
//! possible moment. They are deliberately STRUCTURAL — is this a bare hostname, is this a
//! known access level — because the settled schema has no paths and no free text left to
//! get wrong. The security RATIONALE (why a package is admissible at all) lives in
//! `data/README.md` and, for the pre-migration record, in `wiki/research/`.
//!
//! THE SCHEMA IS A FLAT MAP, and that is the point. Package name → grant, three optional
//! fields. The shape it replaced spread one package's facts across five sibling objects
//! (45 of 161 packages appeared in more than one; `node-libcurl` in five), so a reader had
//! to join them by hand and the generator had to invert one into another. A map makes the
//! duplication structurally impossible.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

const CATALOG: &str = "data/build-jail-catalog.json";

fn main() {
    println!("cargo:rerun-if-changed={CATALOG}");
    println!("cargo:rerun-if-changed=build.rs");

    let text = std::fs::read_to_string(CATALOG)
        .unwrap_or_else(|e| fail(&format!("cannot read {CATALOG}: {e}")));
    let catalog: BTreeMap<String, serde_json::Value> = serde_json::from_str(&text)
        .unwrap_or_else(|e| fail(&format!("{CATALOG} is not a JSON object of grants: {e}")));
    if catalog.is_empty() {
        fail(&format!("{CATALOG} is empty"));
    }

    let grants: Vec<(String, Grant)> = catalog
        .into_iter()
        .map(|(name, value)| {
            let grant = parse(&name, &value);
            (name, grant)
        })
        .collect();

    let out = std::env::var("OUT_DIR").expect("cargo sets OUT_DIR");
    let out = Path::new(&out);
    emit(&out.join("download_hosts.rs"), &gen_hosts(&grants));
    emit(&out.join("curated_grants.rs"), &gen_project(&grants));
    emit(&out.join("package_network.rs"), &gen_network(&grants));
}

fn emit(path: &Path, src: &str) {
    std::fs::write(path, src)
        .unwrap_or_else(|e| fail(&format!("cannot write {}: {e}", path.display())));
}

struct Grant {
    network: bool,
    hosts: Vec<String>,
    project: Option<&'static str>,
}

/// One entry, checked field by field. An unknown key is a hard error rather than ignored:
/// the fields this schema DROPPED (`siblingDirs`, `projectCwd`, `mechanism`, `observed`, …)
/// are exactly what a contributor working from an old example would reach for, and silently
/// accepting one would leave them believing a grant is narrower than it is.
fn parse(name: &str, value: &serde_json::Value) -> Grant {
    let obj = value
        .as_object()
        .unwrap_or_else(|| fail(&format!("{name}: a grant must be a JSON object")));
    for key in obj.keys() {
        if !matches!(key.as_str(), "network" | "hosts" | "project") {
            fail(&format!(
                "{name}: unknown field `{key}` — the schema is `network`, `hosts` and \
                 `project`. Provenance is prose now; it belongs in the write-up, not here"
            ));
        }
    }

    let network = match obj.get("network") {
        None => false,
        Some(serde_json::Value::Bool(true)) => true,
        // `false` is enforcement-identical to absence, and two spellings for one state is
        // how a reader ends up thinking one of them means something.
        Some(_) => fail(&format!(
            "{name}: `network` may only be `true` — omit the field to grant no egress"
        )),
    };

    let hosts = match obj.get("hosts") {
        None => Vec::new(),
        Some(v) => {
            let items = v
                .as_array()
                .unwrap_or_else(|| fail(&format!("{name}: `hosts` must be an array")));
            if items.is_empty() {
                fail(&format!(
                    "{name}: `hosts` must not be empty — omit it if no host was resolved"
                ));
            }
            items
                .iter()
                .map(|item| {
                    let host = item
                        .as_str()
                        .unwrap_or_else(|| fail(&format!("{name}: `hosts` entries are strings")))
                        .trim()
                        .to_string();
                    require_bare_host(name, &host);
                    host
                })
                .collect()
        }
    };
    // A host list without egress is dead data that reads as a live grant, and it would also
    // put a host into `$downloads` on behalf of a package that cannot dial it.
    if !hosts.is_empty() && !network {
        fail(&format!(
            "{name}: `hosts` without `network: true` — the host list is the evidence FOR the \
             egress grant, so one without the other is a half-written entry"
        ));
    }

    let project = match obj.get("project") {
        None => None,
        Some(v) => match v.as_str() {
            Some("read") => Some("read"),
            Some("readwrite") => Some("readwrite"),
            _ => fail(&format!(
                "{name}: `project` must be \"read\" or \"readwrite\""
            )),
        },
    };

    if !network && project.is_none() {
        fail(&format!(
            "{name}: an entry with no `network` and no `project` grants nothing — delete it"
        ));
    }
    Grant {
        network,
        hosts,
        project,
    }
}

/// Wildcard-freedom is the host set's structural anti-exfiltration property: the egress proxy
/// resolves the name itself, so a `*.suffix` member would let a confined script put chosen
/// bytes in a DNS label and leak them without sending a payload at all.
fn require_bare_host(name: &str, host: &str) {
    if host.contains('*') {
        fail(&format!(
            "{name}: `{host}` — the host set must stay wildcard-free; a subdomain wildcard \
             admits DNS-label exfiltration under the same hostname"
        ));
    }
    if host.is_empty() || host.contains('/') || host.contains(':') || host.contains(' ') {
        fail(&format!(
            "{name}: `{host}` is not a BARE hostname — no scheme, port, path or annotation"
        ));
    }
}

// ── $downloads ─────────────────────────────────────────────────────────────────

/// `$downloads` is the UNION of every entry's `hosts`, sorted.
///
/// Sorted rather than authored-order, because a map has no order to preserve — the shape this
/// replaced was an ordered host array, and a prefix of it was pinned by a test. Determinism is
/// what that test was actually protecting (`download_net_rules` emits one rule per host in
/// list order and the IR is compared byte-wise elsewhere), and sorting supplies it
/// unconditionally.
fn gen_hosts(grants: &[(String, Grant)]) -> String {
    let hosts: BTreeSet<&str> = grants
        .iter()
        .flat_map(|(_, g)| g.hosts.iter().map(String::as_str))
        .collect();
    if hosts.is_empty() {
        fail("no entry names a host — $downloads would be empty");
    }
    let hosts: Vec<&str> = hosts.into_iter().collect();
    format!(
        "// @generated by build.rs from data/build-jail-catalog.json — do not edit.\n\
         /// The install-time artifact hosts — the union of every catalog entry's `hosts`, and\n\
         /// the `$downloads` net token. Per-package `hosts` is EVIDENCE (see\n\
         /// [`super::package_network`]); this union is what a policy author's `$downloads`\n\
         /// expands to.\n\
         pub const DOWNLOAD_HOSTS: &[&str] = &{hosts:?};\n"
    )
}

// ── the per-package project-filesystem grant ───────────────────────────────────

fn gen_project(grants: &[(String, Grant)]) -> String {
    let mut src = String::from(
        "// @generated by build.rs from data/build-jail-catalog.json — do not edit.\n\
         static CURATED_GRANTS: &[(&str, ProjectAccess)] = &[\n",
    );
    for (name, grant) in grants {
        let access = match grant.project {
            None => continue,
            Some("read") => "ProjectAccess::Read",
            Some(_) => "ProjectAccess::ReadWrite",
        };
        let _ = writeln!(src, "    ({name:?}, {access}),");
    }
    src.push_str("];\n");
    src
}

// ── per-package egress ─────────────────────────────────────────────────────────

fn gen_network(grants: &[(String, Grant)]) -> String {
    let mut src = String::from(
        "// @generated by build.rs from data/build-jail-catalog.json — do not edit.\n\
         /// The packages granted egress, sorted. Membership is the whole gate: `network` is an\n\
         /// OS-enforced BOOLEAN, so this list answers the only question the net axis asks.\n\
         pub const NETWORK_GRANTED: &[&str] = &[\n",
    );
    for (name, grant) in grants {
        if grant.network {
            let _ = writeln!(src, "    {name:?},");
        }
    }
    src.push_str("];\n");
    src
}

fn fail(msg: &str) -> ! {
    // Printed as a cargo error so it surfaces at the top of the build output rather than
    // buried in a panic backtrace.
    println!("cargo:warning={msg}");
    panic!("build-jail catalog is invalid — {msg}");
}
