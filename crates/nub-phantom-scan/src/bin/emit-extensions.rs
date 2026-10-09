//! emit-extensions — convert a `nub-phantom scan --json` report into
//! `packageExtensions`-shaped entries for the vendored
//! `vendor/package-extensions/nub-phantom-extensions.json` member of the
//! unified bundled-defaults set.
//!
//! The scanner records the phantom target *name* (the extension key) but not
//! the importer range, the bucket, or the value, so this emitter applies the
//! policy documented in the unified-extensions plan:
//!   - selector: `pkg@*` — the scanner samples `latest`, so a phantom present
//!     there means the fix hasn't shipped; `*` matches Yarn's "always needs"
//!     class and is safe under `extend_missing` (declared deps win).
//!   - bucket:  a subpath-adapter phantom (hard, reachable only from a non-`.`
//!     `exports` subpath) → `peerDependenciesMeta.<dep>.optional = true` ONLY
//!     (no required `peerDependencies` entry, which would warn for consumers
//!     not using that subpath). A main-graph hard phantom → `dependencies.<dep>`.
//!   - value:   `"*"` — the scanner has no version signal for the target.
//!   - dedup:   skip a finding when a bundled (Yarn ∪ pnpm) selector already
//!     matches `(offender, version)` AND its body declares the same dep in the
//!     same bucket — the curated lists cover it.
//!
//! `Finding` in `nub-phantom-scan` is `Serialize`-only with `pub(crate)`
//! fields, and the scan JSON is emitted by the separate `nub-phantom` eval CLI,
//! so this binary deserializes minimal local mirror structs (with `default` on
//! the fields it doesn't read).
//!
//! Usage:
//!   emit-extensions <scan-report.json> <bundled-union.json> > nub-phantom-extensions.json

use std::collections::BTreeMap;

// --- Mirror of the eval CLI's `ScanReport`/`Offender`/`Finding` (Serialize-only
// in the scan crate), trimmed to the fields this emitter reads. ---
#[derive(serde::Deserialize)]
struct ScanReport {
    offenders: Vec<Offender>,
}

#[derive(serde::Deserialize)]
struct Offender {
    package: String,
    version: String,
    hard_phantoms: Vec<FindingMirror>,
}

#[derive(serde::Deserialize)]
struct FindingMirror {
    package: String,
    verdict: String,
    #[serde(default)]
    from_main: bool,
    #[serde(default)]
    from_subpath: bool,
}

impl FindingMirror {
    /// Mirror of `nub_phantom_scan::Finding::is_subpath_adapter`.
    fn is_subpath_adapter(&self) -> bool {
        self.verdict == "hard-phantom" && self.from_subpath && !self.from_main
    }
}

/// Reimplementation of aube-resolver's `pub(crate) package_selector_matches`
/// (this crate doesn't depend on aube-resolver). A name-only selector matches
/// every version; a `*`/empty range matches any version (even non-semver);
/// otherwise the version must satisfy the range.
fn selector_matches(selector: &str, name: &str, version: &str) -> bool {
    let selector = selector.trim();
    if selector == name {
        return true;
    }
    // Split `name@range`, honoring a leading scope `@`. Mirrors aube's
    // `split_package_selector` (rfind('@'), skip the scope @ at index 0).
    let at = match selector.rfind('@') {
        Some(0) => return false,
        Some(i) => i,
        None => return false, // no @ and not a bare-name match above
    };
    let (sel_name, range) = (&selector[..at], &selector[at + 1..]);
    if sel_name != name || range.is_empty() {
        return sel_name == name && range.is_empty();
    }
    let range = range.trim();
    if range == "*" {
        return true;
    }
    let Ok(r) = node_semver::Range::parse(range) else {
        return false;
    };
    let Ok(v) = node_semver::Version::parse(version) else {
        return false;
    };
    r.satisfies(&v)
}

/// Does `body` already declare `dep` in the bucket this emitter would write?
fn body_covers(body: &serde_json::Value, dep: &str, as_optional_peer: bool) -> bool {
    let Some(obj) = body.as_object() else {
        return false;
    };
    if as_optional_peer {
        obj.get("peerDependenciesMeta")
            .and_then(|v| v.as_object())
            .is_some_and(|m| m.contains_key(dep))
    } else {
        obj.get("dependencies")
            .and_then(|v| v.as_object())
            .is_some_and(|m| m.contains_key(dep))
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let scan_path = args
        .next()
        .expect("usage: emit-extensions <scan-report.json> <bundled-union.json>");
    let union_path = args
        .next()
        .expect("usage: emit-extensions <scan-report.json> <bundled-union.json>");

    let scan_raw = std::fs::read_to_string(&scan_path)
        .unwrap_or_else(|e| panic!("read scan report {scan_path}: {e}"));
    let report: ScanReport = serde_json::from_str(&scan_raw)
        .unwrap_or_else(|e| panic!("parse scan report {scan_path}: {e}"));
    let union_raw = std::fs::read_to_string(&union_path)
        .unwrap_or_else(|e| panic!("read bundled union {union_path}: {e}"));
    let bundled: BTreeMap<String, serde_json::Value> = serde_json::from_str(&union_raw)
        .unwrap_or_else(|e| panic!("parse bundled union {union_path}: {e}"));

    // selector -> body, accumulating multiple phantoms per offender into one body.
    let mut out: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    let mut emitted = 0usize;
    let mut deduped = 0usize;

    for offender in &report.offenders {
        for finding in &offender.hard_phantoms {
            if finding.verdict != "hard-phantom" {
                continue;
            }
            let dep = &finding.package;
            let as_optional_peer = finding.is_subpath_adapter();

            // Dedup: a bundled (Yarn ∪ pnpm) selector that already matches this
            // offender+version AND declares the same dep in the same bucket.
            let covered = bundled.iter().any(|(sel, body)| {
                selector_matches(sel, &offender.package, &offender.version)
                    && body_covers(body, dep, as_optional_peer)
            });
            if covered {
                deduped += 1;
                continue;
            }

            let selector = format!("{}@*", offender.package);
            let body = out.entry(selector).or_insert_with(|| serde_json::json!({}));
            let obj = body.as_object_mut().expect("bodies are objects");
            if as_optional_peer {
                let pmd = obj
                    .entry("peerDependenciesMeta".to_string())
                    .or_insert_with(|| serde_json::json!({}))
                    .as_object_mut()
                    .expect("peerDependenciesMeta is an object");
                pmd.entry(dep.clone())
                    .or_insert_with(|| serde_json::json!({"optional": true}));
            } else {
                let deps = obj
                    .entry("dependencies".to_string())
                    .or_insert_with(|| serde_json::json!({}))
                    .as_object_mut()
                    .expect("dependencies is an object");
                deps.entry(dep.clone())
                    .or_insert_with(|| serde_json::json!("*"));
            }
            emitted += 1;
        }
    }

    let serialized = serde_json::to_string_pretty(&out).expect("serialize output");
    println!("{serialized}");
    eprintln!(
        "emit-extensions: {emitted} emitted, {deduped} deduped (covered by Yarn ∪ pnpm), {} selectors",
        out.len()
    );
}
