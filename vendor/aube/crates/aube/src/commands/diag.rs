/*!
 * `aube diag` — post-hoc analysis of saved diagnostic traces.
 *
 * Two sub-commands:
 *
 * - `analyze A.jsonl` renders the per-(category, name) aggregate
 *   table from a saved trace, sorted by descending cumulative
 *   duration.
 * - `compare A.jsonl B.jsonl` diffs two saved traces. Per-operation
 *   regressions are sorted by absolute change in cumulative duration,
 *   with a Mann-Whitney U significance column for distribution-level
 *   confidence.
 *
 * Inputs are JSONL traces produced by `aube install --diag full`. The
 * parser hand-rolls field extraction via [`json_str`] and [`json_num`]
 * so the analyzer has no `serde` dependency and stays robust to minor
 * schema additions.
 */

use clap::{Args, Subcommand};
use miette::{IntoDiagnostic, Result, WrapErr};
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Args)]
pub struct DiagArgs {
    #[command(subcommand)]
    pub command: DiagCommand,
}

#[derive(Debug, Subcommand)]
pub enum DiagCommand {
    /// Show critical path / starvation / per-pkg lifecycle from a saved trace.
    Analyze {
        /// Trace JSONL
        path: PathBuf,
    },
    /// Diff two diag JSONL traces and surface per-operation regressions.
    Compare {
        /// Baseline trace
        a: PathBuf,
        /// Comparison trace
        b: PathBuf,
        /// Minimum |Δsum_ms| to surface (default 50)
        #[arg(long, default_value_t = 50.0)]
        min_delta_ms: f64,
        /// Minimum |%change| to surface (default 10)
        #[arg(long, default_value_t = 10.0)]
        min_pct: f64,
    },
}

#[derive(Default, Clone)]
struct Stat {
    n: u64,
    sum: f64,
    max: f64,
    samples: Vec<f64>,
}

type AggKey = (String, String);
type AggMap = BTreeMap<AggKey, Stat>;

fn read_aggregates(path: &PathBuf) -> Result<(AggMap, f64)> {
    let content = std::fs::read_to_string(path)
        .into_diagnostic()
        .wrap_err_with(|| format!("read {}", path.display()))?;
    let mut agg: AggMap = BTreeMap::new();
    let mut total_ms = 0.0_f64;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let cat = json_str(line, "cat").unwrap_or_default();
        let name = json_str(line, "name").unwrap_or_default();
        let dur = json_num(line, "dur").unwrap_or(0.0);
        if cat == "install" && name == "total" {
            total_ms = total_ms.max(dur);
        }
        let entry = agg.entry((cat, name)).or_default();
        entry.n += 1;
        entry.sum += dur;
        if dur > entry.max {
            entry.max = dur;
        }
        // Cap at 4096 samples per op to keep memory bounded
        if entry.samples.len() < 4096 && dur > 0.0 {
            entry.samples.push(dur);
        }
    }
    Ok((agg, total_ms))
}

/// Mann-Whitney U test (large-sample normal approximation).
/// Returns z-score; |z|>1.96 ≈ p<0.05 two-sided.
fn mann_whitney_z(a: &[f64], b: &[f64]) -> f64 {
    let n1 = a.len();
    let n2 = b.len();
    if n1 < 5 || n2 < 5 {
        return 0.0;
    }
    let mut all: Vec<(f64, u8)> = Vec::with_capacity(n1 + n2);
    for &x in a {
        all.push((x, 0));
    }
    for &x in b {
        all.push((x, 1));
    }
    all.sort_by(|x, y| x.0.partial_cmp(&y.0).unwrap_or(Ordering::Equal));
    // Assign average ranks
    let mut ranks = vec![0.0_f64; all.len()];
    let mut i = 0;
    while i < all.len() {
        let mut j = i;
        while j + 1 < all.len() && all[j + 1].0 == all[i].0 {
            j += 1;
        }
        let avg_rank = ((i + j) as f64) / 2.0 + 1.0;
        for r in &mut ranks[i..=j] {
            *r = avg_rank;
        }
        i = j + 1;
    }
    let r1: f64 = all
        .iter()
        .zip(ranks.iter())
        .filter(|((_, g), _)| *g == 0)
        .map(|(_, r)| *r)
        .sum();
    let u1 = r1 - (n1 * (n1 + 1) / 2) as f64;
    let mu = (n1 * n2) as f64 / 2.0;
    let sigma = ((n1 * n2 * (n1 + n2 + 1)) as f64 / 12.0).sqrt();
    if sigma == 0.0 {
        return 0.0;
    }
    (u1 - mu) / sigma
}

/**
 * Parse a JSON string field by linear scan with proper escape handling.
 *
 * The previous shape stopped at the first `"` byte regardless of
 * whether it was escaped. Field values containing a literal quote
 * (emitted as `\"` by `jstr`) silently mis-parsed. Walks the byte
 * stream and counts consecutive `\` before each `"`; an odd count
 * means the quote is escaped, an even count (including zero) means
 * it terminates the field.
 */
fn json_str(line: &str, field: &str) -> Option<String> {
    let needle = format!("\"{field}\":\"");
    let i = line.find(&needle)?;
    let after = &line[i + needle.len()..];
    let bytes = after.as_bytes();
    let mut idx = 0usize;
    while idx < bytes.len() {
        if bytes[idx] == b'"' {
            let mut bs = 0usize;
            let mut j = idx;
            while j > 0 && bytes[j - 1] == b'\\' {
                bs += 1;
                j -= 1;
            }
            if bs.is_multiple_of(2) {
                return Some(after[..idx].to_string());
            }
        }
        idx += 1;
    }
    None
}

/**
 * Parse a numeric JSON field by linear scan.
 *
 * Locates `"<field>":` then reads up to the next delimiter (`,`, `}`,
 * or whitespace) and parses as `f64`. Non-finite values (`NaN`, `inf`,
 * `-inf`) are rejected because they would poison every `partial_cmp`
 * sort downstream — Rust's `f64::parse` accepts them per IEEE 754,
 * but the analyzer treats them as malformed input.
 */
fn json_num(line: &str, field: &str) -> Option<f64> {
    let needle = format!("\"{field}\":");
    let i = line.find(&needle)?;
    let after = &line[i + needle.len()..];
    let end = after.find([',', '}', ' ']).unwrap_or(after.len());
    let v = after[..end].trim().parse::<f64>().ok()?;
    if v.is_finite() { Some(v) } else { None }
}

pub async fn run(args: DiagArgs) -> Result<()> {
    match args.command {
        DiagCommand::Compare {
            a,
            b,
            min_delta_ms,
            min_pct,
        } => compare(&a, &b, min_delta_ms, min_pct),
        DiagCommand::Analyze { path } => analyze(&path),
    }
}

fn compare(a: &PathBuf, b: &PathBuf, min_delta_ms: f64, min_pct: f64) -> Result<()> {
    let (agg_a, total_a) = read_aggregates(a)?;
    let (agg_b, total_b) = read_aggregates(b)?;

    let mut keys: std::collections::BTreeSet<(String, String)> = std::collections::BTreeSet::new();
    keys.extend(agg_a.keys().cloned());
    keys.extend(agg_b.keys().cloned());

    println!(
        "compare {} ({:.0}ms wall) vs {} ({:.0}ms wall)",
        a.display(),
        total_a,
        b.display(),
        total_b
    );
    println!(
        "delta wall: {:+.0}ms ({:+.1}%)",
        total_b - total_a,
        ((total_b - total_a) / total_a.max(1.0)) * 100.0
    );
    println!();
    println!(
        "{:<14} {:<32} {:>9} {:>9} {:>10} {:>9} {:>9} {:>10} {:>8} {:>7} {:>5}",
        "cat", "name", "n_a", "n_b", "Δn", "sum_a", "sum_b", "Δsum", "Δ%", "mwu_z", "sig"
    );

    let mut rows: Vec<(String, String, Stat, Stat)> = Vec::new();
    for k in &keys {
        let sa = agg_a.get(k).cloned().unwrap_or_default();
        let sb = agg_b.get(k).cloned().unwrap_or_default();
        rows.push((k.0.clone(), k.1.clone(), sa, sb));
    }
    rows.sort_by(|a, b| {
        let da = (b.3.sum - b.2.sum).abs();
        let db = (a.3.sum - a.2.sum).abs();
        da.partial_cmp(&db).unwrap_or(Ordering::Equal)
    });
    for (cat, name, sa, sb) in rows.iter().take(60) {
        let delta_sum = sb.sum - sa.sum;
        let base = sa.sum.max(1.0);
        let pct = (delta_sum / base) * 100.0;
        if delta_sum.abs() < min_delta_ms && pct.abs() < min_pct {
            continue;
        }
        let delta_n = sb.n as i64 - sa.n as i64;
        let z = mann_whitney_z(&sa.samples, &sb.samples);
        let sig = if z.abs() >= 3.29 {
            "***"
        } else if z.abs() >= 2.58 {
            "**"
        } else if z.abs() >= 1.96 {
            "*"
        } else {
            "·"
        };
        println!(
            "{:<14} {:<32} {:>9} {:>9} {:>+10} {:>7.0}ms {:>7.0}ms {:>+8.0}ms {:>+7.1}% {:>+7.2} {:>5}",
            truncate(cat, 14),
            truncate(name, 32),
            sa.n,
            sb.n,
            delta_n,
            sa.sum,
            sb.sum,
            delta_sum,
            pct,
            z,
            sig
        );
    }
    Ok(())
}

fn analyze(path: &PathBuf) -> Result<()> {
    let (agg, total_ms) = read_aggregates(path)?;
    println!("{} ({:.0}ms wall)", path.display(), total_ms);
    println!(
        "{:<14} {:<32} {:>6} {:>9} {:>9} {:>9} {:>7}",
        "cat", "name", "n", "sum_ms", "mean_ms", "max_ms", "%wall"
    );
    let mut rows: Vec<((String, String), Stat)> = agg.into_iter().collect();
    rows.sort_by(|a, b| b.1.sum.partial_cmp(&a.1.sum).unwrap_or(Ordering::Equal));
    for ((cat, name), s) in rows.iter().take(40) {
        let mean = s.sum / (s.n.max(1) as f64);
        let pct = (s.sum / total_ms.max(1.0)) * 100.0;
        println!(
            "{:<14} {:<32} {:>6} {:>9.1} {:>9.2} {:>9.1} {:>6.1}%",
            truncate(cat, 14),
            truncate(name, 32),
            s.n,
            s.sum,
            mean,
            s.max,
            pct
        );
    }
    Ok(())
}

use aube_util::diag::truncate;
