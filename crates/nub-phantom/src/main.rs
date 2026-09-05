//! `nub-phantom` — CLI for the phantom-dependency detector.
//!
//! Subcommands:
//!   analyze <pkg>...            Analyze specific packages; print each report.
//!   scan --top <N>              Scan the top-N most-downloaded packages.
//!   scan --from <file>          Scan a newline-delimited package-name list.
//!   scan <pkg>...               Scan the given packages.
//!
//! `--json` emits machine-readable output (the phantom set that feeds the
//! vendored packageExtensions list); otherwise a human summary is printed.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use nub_phantom::classify::{Finding, Verdict};
use nub_phantom::{PackageReport, analyze, fetch};

use serde::Serialize;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("analyze") => cmd_analyze(&args[1..]),
        Some("scan") => cmd_scan(&args[1..]),
        Some("-h" | "--help") | None => {
            eprint!("{USAGE}");
            0
        }
        Some(other) => {
            eprintln!("unknown subcommand: {other}\n\n{USAGE}");
            2
        }
    };
    std::process::exit(code);
}

const USAGE: &str = "\
nub-phantom — detect undeclared (phantom) dependencies of npm packages

USAGE:
  nub-phantom analyze <pkg>...            analyze specific package(s)
  nub-phantom scan --top <N>             scan the top-N most-downloaded packages
  nub-phantom scan --from <file>         scan a newline-delimited name list
  nub-phantom scan <pkg>...              scan the given package(s)

FLAGS:
  --json                 emit JSON (the phantom set) instead of a summary
  --concurrency <N>      parallel fetch/analyze workers (scan; default 8)
";

fn cmd_analyze(args: &[String]) -> i32 {
    let (json, pkgs) = split_flags(args);
    if pkgs.is_empty() {
        eprintln!("analyze: expected at least one package name");
        return 2;
    }
    let client = fetch::client();
    let mut reports = Vec::new();
    for name in &pkgs {
        match analyze(&client, name) {
            Ok(r) => {
                if !json {
                    print_report_human(&r);
                }
                reports.push(r);
            }
            Err(e) => eprintln!("{name}: ERROR {e}"),
        }
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&reports).unwrap_or_default()
        );
    }
    0
}

fn cmd_scan(args: &[String]) -> i32 {
    let (json, rest) = split_flags(args);
    let mut concurrency = 8usize;
    let mut names: Vec<String> = Vec::new();
    let mut it = rest.into_iter().peekable();
    let client = fetch::client();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--top" => {
                let n: usize = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                if n == 0 {
                    eprintln!("scan --top: expected a positive integer");
                    return 2;
                }
                match fetch::top_packages(&client, n) {
                    Ok(list) => names.extend(list),
                    Err(e) => {
                        eprintln!("scan --top: {e}");
                        return 1;
                    }
                }
            }
            "--from" => {
                let path = match it.next() {
                    Some(p) => p,
                    None => {
                        eprintln!("scan --from: expected a file path");
                        return 2;
                    }
                };
                match std::fs::read_to_string(&path) {
                    Ok(s) => names.extend(
                        s.lines()
                            .map(str::trim)
                            .filter(|l| !l.is_empty() && !l.starts_with('#'))
                            .map(String::from),
                    ),
                    Err(e) => {
                        eprintln!("scan --from {path}: {e}");
                        return 1;
                    }
                }
            }
            "--concurrency" => {
                concurrency = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .filter(|&n| n > 0)
                    .unwrap_or(8);
            }
            other => names.push(other.to_string()),
        }
    }

    if names.is_empty() {
        eprintln!("scan: no packages to scan (use --top N, --from FILE, or list names)");
        return 2;
    }

    let (reports, failures) = run_scan(names, concurrency);
    let agg = aggregate(&reports, &failures);
    if json {
        println!("{}", serde_json::to_string_pretty(&agg).unwrap_or_default());
    } else {
        print_scan_human(&agg);
    }
    0
}

/// Split out the boolean `--json` flag; return it plus the remaining args.
fn split_flags(args: &[String]) -> (bool, Vec<String>) {
    let mut json = false;
    let mut rest = Vec::new();
    for a in args {
        if a == "--json" {
            json = true;
        } else {
            rest.push(a.clone());
        }
    }
    (json, rest)
}

/// Fetch+analyze `names` across `concurrency` worker threads (each with its own
/// HTTP client). A failed package is recorded, never fatal. Progress goes to
/// stderr so `--json` stdout stays clean.
fn run_scan(names: Vec<String>, concurrency: usize) -> (Vec<PackageReport>, Vec<(String, String)>) {
    let total = names.len();
    let queue = Arc::new(Mutex::new(names.into_iter()));
    let reports = Arc::new(Mutex::new(Vec::new()));
    let failures = Arc::new(Mutex::new(Vec::new()));
    let done = Arc::new(AtomicUsize::new(0));

    std::thread::scope(|scope| {
        for _ in 0..concurrency.max(1) {
            let queue = Arc::clone(&queue);
            let reports = Arc::clone(&reports);
            let failures = Arc::clone(&failures);
            let done = Arc::clone(&done);
            scope.spawn(move || {
                let client = fetch::client();
                loop {
                    let next = queue.lock().unwrap().next();
                    let Some(name) = next else { break };
                    match analyze(&client, &name) {
                        Ok(r) => reports.lock().unwrap().push(r),
                        Err(e) => failures.lock().unwrap().push((name.clone(), e)),
                    }
                    let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                    if n.is_multiple_of(25) || n == total {
                        eprintln!("  scanned {n}/{total}");
                    }
                }
            });
        }
    });

    let reports = Arc::try_unwrap(reports).unwrap().into_inner().unwrap();
    let failures = Arc::try_unwrap(failures).unwrap().into_inner().unwrap();
    (reports, failures)
}

/// The scan rollup: the empirical phantom set + the over-count the naive view
/// would have reported.
#[derive(Debug, Serialize)]
struct ScanReport {
    scanned_ok: usize,
    failed: Vec<Failure>,
    /// Packages that hard-phantom-import at least one undeclared package. These
    /// are the packageExtensions candidates (add the declaration to package X).
    offenders: Vec<Offender>,
    /// Every hard-phantom target, ranked by how many scanned packages import it.
    phantom_targets: Vec<PhantomTarget>,
    totals: Totals,
}

#[derive(Debug, Serialize)]
struct Failure {
    package: String,
    error: String,
}

#[derive(Debug, Serialize)]
struct Offender {
    package: String,
    version: String,
    hard_phantoms: Vec<Finding>,
    soft_phantoms: Vec<Finding>,
    /// The subset of `hard_phantoms` that are the subpath-adapter class
    /// (consumer-provided backend, reached only via a `<pkg>/<adapter>` subpath).
    subpath_adapter_phantoms: Vec<Finding>,
    /// The subset of `hard_phantoms` that are the legacy deep-path class — reached
    /// only through a published file no declared surface references, importable
    /// because the package ships no `exports` map. Lower confidence than the other
    /// two classes: real per Node resolution, but speculative as to whether any
    /// consumer takes that path.
    deep_path_phantoms: Vec<Finding>,
}

#[derive(Debug, Serialize)]
struct PhantomTarget {
    target: String,
    importer_count: usize,
    importers: Vec<String>,
}

#[derive(Debug, Serialize)]
struct Totals {
    packages_with_hard_phantom: usize,
    total_hard_phantom_edges: usize,
    /// Distinct undeclared packages classified as HARD phantoms across the scan.
    distinct_hard_phantom_targets: usize,
    /// THE BLAST-RADIUS METRIC: how many scanned packages exhibit the
    /// subpath-adapter class (a `<pkg>/<adapter>` subpath statically imports an
    /// undeclared consumer-provided backend) — the GVS-default-breaking pattern.
    packages_with_subpath_adapter: usize,
    /// Subpath-adapter phantom edges across the scan.
    subpath_adapter_edges: usize,
    /// Packages exhibiting the legacy deep-path class.
    packages_with_deep_path: usize,
    /// Legacy deep-path phantom edges across the scan.
    deep_path_edges: usize,
    /// The complement: hard phantoms reachable from the main graph (accidental
    /// undeclared deps — "genuine junk", not the adapter or deep-path classes).
    main_graph_hard_edges: usize,
    /// How many findings a NAIVE detector (undeclared incl. optional peers + soft
    /// loads) would have flagged as phantoms across the scan…
    naive_phantom_flags: usize,
    /// …vs. how many the real classifier keeps as hard phantoms. The gap is the
    /// over-count avoided (optional peers + soft loads).
    real_hard_phantom_flags: usize,
    optional_peers_excluded: usize,
    soft_phantoms_excluded: usize,
}

fn aggregate(reports: &[PackageReport], failures: &[(String, String)]) -> ScanReport {
    let mut offenders = Vec::new();
    let mut target_importers: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut total_hard_edges = 0usize;
    let mut naive = 0usize;
    let mut real_hard = 0usize;
    let mut optional_peers = 0usize;
    let mut soft = 0usize;
    let mut adapter_pkgs = 0usize;
    let mut adapter_edges = 0usize;
    let mut deep_pkgs = 0usize;
    let mut deep_edges = 0usize;

    for r in reports {
        naive += r.naive_phantom_count();
        let hard: Vec<Finding> = r.hard_phantoms().cloned().collect();
        let adapters: Vec<Finding> = r.subpath_adapter_phantoms().cloned().collect();
        let deep: Vec<Finding> = r.deep_path_phantoms().cloned().collect();
        let soft_ph: Vec<Finding> = r
            .findings
            .iter()
            .filter(|f| f.verdict == Verdict::SoftPhantom)
            .cloned()
            .collect();
        optional_peers += r.count(Verdict::DeclaredOptionalPeer);
        soft += soft_ph.len();
        real_hard += hard.len();
        total_hard_edges += hard.len();
        adapter_edges += adapters.len();
        if !adapters.is_empty() {
            adapter_pkgs += 1;
        }
        deep_edges += deep.len();
        if !deep.is_empty() {
            deep_pkgs += 1;
        }
        for f in &hard {
            target_importers
                .entry(f.package.clone())
                .or_default()
                .push(r.name.clone());
        }
        if !hard.is_empty() || !soft_ph.is_empty() {
            offenders.push(Offender {
                package: r.name.clone(),
                version: r.version.clone(),
                hard_phantoms: hard,
                soft_phantoms: soft_ph,
                subpath_adapter_phantoms: adapters,
                deep_path_phantoms: deep,
            });
        }
    }

    let mut phantom_targets: Vec<PhantomTarget> = target_importers
        .into_iter()
        .map(|(target, mut importers)| {
            importers.sort();
            importers.dedup();
            PhantomTarget {
                importer_count: importers.len(),
                target,
                importers,
            }
        })
        .collect();
    phantom_targets.sort_by(|a, b| {
        b.importer_count
            .cmp(&a.importer_count)
            .then(a.target.cmp(&b.target))
    });

    offenders.sort_by(|a, b| {
        b.hard_phantoms
            .len()
            .cmp(&a.hard_phantoms.len())
            .then(a.package.cmp(&b.package))
    });

    ScanReport {
        scanned_ok: reports.len(),
        failed: failures
            .iter()
            .map(|(p, e)| Failure {
                package: p.clone(),
                error: e.clone(),
            })
            .collect(),
        totals: Totals {
            packages_with_hard_phantom: offenders
                .iter()
                .filter(|o| !o.hard_phantoms.is_empty())
                .count(),
            total_hard_phantom_edges: total_hard_edges,
            distinct_hard_phantom_targets: phantom_targets.len(),
            packages_with_subpath_adapter: adapter_pkgs,
            subpath_adapter_edges: adapter_edges,
            packages_with_deep_path: deep_pkgs,
            deep_path_edges: deep_edges,
            main_graph_hard_edges: total_hard_edges - adapter_edges - deep_edges,
            naive_phantom_flags: naive,
            real_hard_phantom_flags: real_hard,
            optional_peers_excluded: optional_peers,
            soft_phantoms_excluded: soft,
        },
        offenders,
        phantom_targets,
    }
}

fn print_report_human(r: &PackageReport) {
    println!("\n{}@{}  ({} files)", r.name, r.version, r.files_analyzed);
    for f in &r.findings {
        let tag = match f.verdict {
            Verdict::HardPhantom => "PHANTOM  ",
            Verdict::SoftPhantom => "soft     ",
            Verdict::DeclaredOptionalPeer => "opt-peer ",
            Verdict::DeclaredPeer => "peer     ",
            Verdict::Declared => "ok       ",
            Verdict::Builtin => "builtin  ",
            Verdict::SelfRef => "self     ",
            Verdict::DevOnlyDeepPath => "dev-deep ",
        };
        // Only surface the interesting verdicts by default.
        if matches!(
            f.verdict,
            Verdict::HardPhantom | Verdict::SoftPhantom | Verdict::DeclaredOptionalPeer
        ) {
            // A deep-path-only phantom is real per Node resolution but speculative
            // as to whether a consumer takes that path — marked so a reader never
            // has to infer the difference from the provenance bits.
            let via = if f.is_deep_path_only() {
                "  (deep-path only)"
            } else {
                ""
            };
            println!("  {tag} {}  [{}]{via}", f.package, f.specifiers.join(", "));
        }
    }
}

fn print_scan_human(a: &ScanReport) {
    println!("\n=== phantom-dependency scan ===");
    println!("scanned OK:        {}", a.scanned_ok);
    println!("failed:            {}", a.failed.len());
    println!(
        "packages w/ hard phantom: {}",
        a.totals.packages_with_hard_phantom
    );
    println!(
        "hard phantom edges:  {}  (distinct targets: {})",
        a.totals.total_hard_phantom_edges, a.totals.distinct_hard_phantom_targets
    );
    println!(
        "over-count avoided:  naive would flag {} phantoms; real hard = {}  (optional peers {}, soft {} excluded)",
        a.totals.naive_phantom_flags,
        a.totals.real_hard_phantom_flags,
        a.totals.optional_peers_excluded,
        a.totals.soft_phantoms_excluded
    );
    println!(
        "SUBPATH-ADAPTER CLASS (GVS-default blast radius): {} packages, {} edges  |  main-graph junk: {} edges",
        a.totals.packages_with_subpath_adapter,
        a.totals.subpath_adapter_edges,
        a.totals.main_graph_hard_edges
    );
    println!(
        "LEGACY DEEP-PATH CLASS (no exports map; lower confidence): {} packages, {} edges",
        a.totals.packages_with_deep_path, a.totals.deep_path_edges
    );

    println!("\n-- legacy deep-path offenders (reachable only by deep import) --");
    for o in a
        .offenders
        .iter()
        .filter(|o| !o.deep_path_phantoms.is_empty())
    {
        let names: Vec<&str> = o
            .deep_path_phantoms
            .iter()
            .map(|f| f.package.as_str())
            .collect();
        println!("  {}@{}  ->  {}", o.package, o.version, names.join(", "));
    }

    println!("\n-- subpath-adapter offenders (consumer-provided backend, breaks under GVS) --");
    for o in a
        .offenders
        .iter()
        .filter(|o| !o.subpath_adapter_phantoms.is_empty())
    {
        let names: Vec<&str> = o
            .subpath_adapter_phantoms
            .iter()
            .map(|f| f.package.as_str())
            .collect();
        println!("  {}@{}  ->  {}", o.package, o.version, names.join(", "));
    }

    println!("\n-- most-imported phantom targets --");
    for t in a.phantom_targets.iter().take(30) {
        println!("  {:>4}x  {}", t.importer_count, t.target);
    }

    println!("\n-- offenders (hard phantoms) --");
    for o in a
        .offenders
        .iter()
        .filter(|o| !o.hard_phantoms.is_empty())
        .take(60)
    {
        let names: Vec<&str> = o.hard_phantoms.iter().map(|f| f.package.as_str()).collect();
        println!("  {}@{}  ->  {}", o.package, o.version, names.join(", "));
    }
}
