//! Shared rendering primitives for the install progress UI.
//!
//! CI mode and TTY mode both want the same label content (cur/total
//! pkgs, downloaded / estimated bytes, transfer rate, ETA, phase
//! word) — only the bar and frame differ. The label assembly lives
//! here so a tweak to the segment order or styling lands in both
//! modes without drift.

use super::ci::Snap;
use clx::style;
use std::sync::atomic::{AtomicBool, Ordering};

/// Rough gzip compression ratio for npm tarballs. `dist.unpackedSize`
/// is what aube installs to disk, not what crosses the wire — typical
/// JS/TS code minified through gzip lands around 0.18-0.25×, with
/// already-compressed binaries (prebuilt `.node`, wasm) pushing the
/// average up. 0.20 reflects the central tendency observed on real
/// installs (e.g. a 1230-package tree with 276 MB unpacked downloads
/// ~56 MB compressed = 0.20×). Used solely for the `~13.8 MB` display
/// segment; never persisted to lockfiles or the store. Slightly
/// underestimating is preferred — the bytes segment drops the
/// estimate suffix once running bytes catch up, so a low estimate
/// gracefully disappears, whereas a high one misleads the user about
/// remaining work all the way to the finish line.
const TARBALL_COMPRESSION_RATIO: f64 = 0.20;

/// Build the full `<bar> <label>` line for one heartbeat tick.
/// Returns an empty string when the snapshot has nothing meaningful
/// to show — the heartbeat skips empty lines so a phase=0 snapshot
/// stays quiet instead of printing a blank.
pub(super) fn progress_line(snap: Snap, term_width: usize, bar_width: usize) -> String {
    if snap.phase == 0 {
        return String::new();
    }
    // Compute the clamped numerator once per render so the
    // `WARN_AUBE_PROGRESS_OVERFLOW` warning isn't double-fired across
    // the bar + label call sites; both helpers consume the result via
    // a parameter rather than re-loading the atomics. Resolving phase
    // doesn't display a numerator so we don't bother computing it.
    let completed = if snap.phase == 1 {
        0
    } else {
        clamped_completed(snap)
    };
    let label = label_for(snap, completed);
    if label.is_empty() {
        return String::new();
    }
    let bar = bar_only(snap, bar_width, completed);
    let _ = term_width; // reserved for future right-align/truncate logic
    format!("{bar} {label}")
}

/// Share of the install bar assigned to the resolving phase. The
/// resolver typically accounts for a small fraction of wall time on a
/// network-bound install; reserving ~15% leaves the dominant 80% for
/// fetching and 5% in reserve for linking. The bar fills 0 →
/// `RESOLVE_BAR_WEIGHT` during resolving, then continues monotonically
/// through fetching.
const RESOLVE_BAR_WEIGHT: f64 = 0.15;
/// Share of the install bar assigned to fetching. Resolve + fetch
/// covers 95% of the bar; the final 5% is reserved for linking so the
/// bar never falsely reads "100%" while work is still in flight.
/// Linking doesn't surface per-package progress, so phase 3 holds the
/// fill at the end-of-fetch edge until `finish()` retires the display.
const FETCH_BAR_WEIGHT: f64 = 0.80;

/// Unified install-progress fraction in [0, 1]. Drives both the
/// CI-mode bar (rendered here via [`bar_only`]) and the TTY-mode bar
/// (rendered by clx after the caller scales this to its
/// `progress_current`/`progress_total` integers). The two modes share
/// this function so a tweak to the weighting lands in both renderers.
pub(super) fn unified_progress(snap: Snap, completed: usize) -> f64 {
    let fetch_end = RESOLVE_BAR_WEIGHT + FETCH_BAR_WEIGHT;
    match snap.phase {
        1 if snap.target_total > 0 => {
            let estimate = snap.target_total.max(snap.resolved).max(1) as f64;
            RESOLVE_BAR_WEIGHT * (snap.resolved as f64 / estimate).min(1.0)
        }
        2 => {
            let total = snap.resolved.max(1) as f64;
            let fetch_progress = (completed as f64 / total).min(1.0);
            // Offset by the resolving slice so the bar continues
            // monotonically from where phase 1 left off — but only
            // when a resolving estimate was actually in play. Without
            // one (true first install, no lockfile and no streamed
            // BFS-frontier signal), phase 1 rendered an empty bar,
            // and anchoring fetching at RESOLVE_BAR_WEIGHT would snap
            // the bar upward at the phase boundary. Map the bare
            // fetch progress over [0, fetch_end] in that case so the
            // no-estimate path still tops out where the with-estimate
            // path does — both leave the link slice reserved.
            if snap.target_total > 0 {
                RESOLVE_BAR_WEIGHT + FETCH_BAR_WEIGHT * fetch_progress
            } else {
                fetch_end * fetch_progress
            }
        }
        // Linking has no per-package signal; hold at the fetch-end
        // edge so the bar doesn't claim 100% during the linking window.
        3 => fetch_end,
        // Install complete. The install pipeline has finished every
        // resolve/fetch/link/script step by the time the caller
        // promotes the phase to this terminal state, so the bar can
        // honestly read 100%. Used by `finish()` / `stop()` for one
        // final repaint right before the summary line lands.
        4 => 1.0,
        _ => 0.0,
    }
}

/// The fixed-width left-aligned bar. Empty portion is dim throughout;
/// the filled portion is cyan across every phase so the bar simply
/// fills as work completes — phase is signalled by the label word
/// (`resolving` / `fetching` / `linking`), not by recoloring the bar.
/// One unified bar across the whole install: resolving fills the
/// leftmost `RESOLVE_BAR_WEIGHT` slice, fetching extends from there
/// toward the right edge, linking holds at the fetch-end edge.
pub(super) fn bar_only(snap: Snap, width: usize, completed: usize) -> String {
    let progress = unified_progress(snap, completed);
    let filled = ((progress * width as f64).round() as usize).min(width);
    let empty = width - filled;
    let fill = "█".repeat(filled);
    let empty = "░".repeat(empty);
    format!("{}{}", style::ecyan(fill), style::edim(empty))
}

/// Just the count segment of the label (`23/142 pkgs`, `1230 pkgs`).
/// Extracted from [`label_for`] so TTY mode can render the same
/// phase-conditional shape via the `count` template prop. Width
/// padding mirrors `label_for`: resolving without an estimate
/// right-aligns to its own running count; everything else aligns to
/// the denominator's digit width.
pub(super) fn count_segment(snap: Snap, completed: usize) -> String {
    match snap.phase {
        1 if snap.target_total > snap.resolved => {
            let cur = pad_count(snap.resolved, snap.target_total);
            format!(
                "{}/{} {}",
                style::ebold(cur),
                style::ebold(snap.target_total),
                style::edim("pkgs"),
            )
        }
        1 => {
            let count = pad_count(snap.resolved, snap.resolved);
            format!("{} {}", style::ebold(count), style::edim("pkgs"))
        }
        2..=4 => {
            let cur = pad_count(completed, snap.resolved);
            format!(
                "{}/{} {}",
                style::ebold(cur),
                style::ebold(snap.resolved),
                style::edim("pkgs"),
            )
        }
        _ => String::new(),
    }
}

/// Phase-specific label content. Format:
///
/// * resolving: `   N pkgs · resolving`
/// * fetching:  `  cur/total pkgs · 4.2 MB / ~13.8 MB · 1.4 MB/s · ETA 5s`
/// * linking:   ` cur/total pkgs · linking`
///
/// Numbers are right-aligned to a min-width-4 column so the right edge
/// of the count stays put across heartbeats — without it, the visible
/// digits jump left every time `snap.resolved` crosses a power of ten
/// during streaming resolve. The ETA segment is omitted entirely when
/// we don't yet have enough fetch-window data to extrapolate, instead
/// of showing a flapping `ETA …` placeholder.
fn label_for(snap: Snap, completed: usize) -> String {
    let dot = format!(" {} ", style::edim("·"));
    match snap.phase {
        1 => {
            let parts = [
                count_segment(snap, completed),
                style::ecyan("resolving").bold().to_string(),
            ];
            parts.join(&dot)
        }
        2 => {
            let mut parts = Vec::with_capacity(4);
            parts.push(count_segment(snap, completed));
            // Skip the bytes segment when nothing has landed and no
            // unpackedSize estimate is available — older publishes
            // and the lockfile fast path both miss the field. Pushing
            // an empty string would produce `pkgs ·  · ETA …` with a
            // doubled separator after the `parts.join` below.
            let seg = bytes_segment(snap);
            if !seg.is_empty() {
                parts.push(seg);
            }
            if let Some(rate) = transfer_rate(snap) {
                parts.push(style::edim(format!("{}/s", format_bytes(rate))).to_string());
            }
            let eta = eta_segment(snap, completed);
            if !eta.is_empty() {
                parts.push(eta);
            }
            parts.join(&dot)
        }
        3 => {
            // Linking has no per-package signal and the post-install
            // summary line already reports total downloaded bytes —
            // duplicating them here just adds visual noise during the
            // brief linking window.
            let parts = [
                count_segment(snap, completed),
                style::ecyan("linking").bold().to_string(),
            ];
            parts.join(&dot)
        }
        // Done. Just the count — the `✓ resolved …` summary line that
        // immediately follows owns the success cue, no need for a
        // phase word here.
        4 => count_segment(snap, completed),
        _ => String::new(),
    }
}

/// Right-align `count` to a column at least 4 wide and at least as
/// wide as `total`'s digit count. The min-4 floor keeps the column
/// stable for installs up to 9999 packages even before the total is
/// known (resolving phase passes `count == total == snap.resolved`).
fn pad_count(count: usize, total: usize) -> String {
    let width = total.to_string().len().max(4);
    format!("{count:>width$}")
}

/// `4.2 MB` running, optionally `4.2 MB / ~13.8 MB` when the
/// estimated total is known. The estimate is computed by
/// [`estimated_total_download`], which blends a static `unpacked ×
/// ratio` fallback with an extrapolation from observed bytes so it
/// converges to the real total as the install progresses. Drops the
/// estimate suffix once the running total has caught up — at that
/// point we know the actual total and the estimate is just noise.
fn bytes_segment(snap: Snap) -> String {
    let expected_to_download = snap.resolved.saturating_sub(snap.reused);
    let estimated_download = estimated_total_download(
        snap.estimated,
        snap.bytes,
        snap.downloaded,
        expected_to_download,
    );
    if estimated_download > snap.bytes && snap.bytes > 0 {
        format!(
            "{} / ~{}",
            style::ebold(format_bytes(snap.bytes)),
            style::edim(format_bytes(estimated_download)),
        )
    } else if snap.bytes > 0 {
        style::ebold(format_bytes(snap.bytes)).to_string()
    } else if estimated_download > 0 {
        // Fetching just started but no bytes have landed — show the
        // estimated size so the user has a sense of total scope.
        format!("~{}", style::edim(format_bytes(estimated_download)),)
    } else {
        // No bytes, no estimate. Avoid emitting a stray `0 B` segment
        // that would just be visual noise.
        String::new()
    }
}

/// Minimum completed downloads before we trust the observed
/// bytes-per-package average. Below this the sample is too noisy
/// (large packages skew the early estimate) and the static
/// `unpacked × ratio` fallback wins. 20 is roughly where per-package
/// variance averages out on real npm trees.
const OBSERVED_SAMPLE_FLOOR: usize = 20;

/// Estimate the total bytes the user will download over the install.
/// Blends two estimators:
///   * **static** — `unpacked × TARBALL_COMPRESSION_RATIO`. Available
///     from the first packument; biased by per-package compressibility
///     variance.
///   * **observed** — `bytes_so_far × expected_total / downloaded_so_far`.
///     Linear extrapolation from real data; converges to the true total
///     as `downloaded` approaches `expected_total`.
///
/// The blend weight is `sqrt(downloaded / expected_total)`, so the
/// observed signal ramps in smoothly (50% weight at 25% complete,
/// 71% weight at 50% complete) instead of whiplashing on the first
/// few samples. Late in the install the observed estimate dominates
/// and the displayed `~XX MB` converges to the real download total.
///
/// `expected_total_pkgs` should be `resolved - reused` — cached
/// packages contribute zero download bytes, so including them inflates
/// the extrapolation.
pub(super) fn estimated_total_download(
    unpacked: u64,
    bytes_done: u64,
    downloaded_pkgs: usize,
    expected_total_pkgs: usize,
) -> u64 {
    let static_estimate = (unpacked as f64 * TARBALL_COMPRESSION_RATIO) as u64;
    if downloaded_pkgs < OBSERVED_SAMPLE_FLOOR || expected_total_pkgs == 0 {
        return static_estimate;
    }
    let observed_avg = bytes_done as f64 / downloaded_pkgs as f64;
    let extrapolated = observed_avg * expected_total_pkgs as f64;
    let frac = (downloaded_pkgs as f64 / expected_total_pkgs as f64).clamp(0.0, 1.0);
    let weight = frac.sqrt();
    let blended = (1.0 - weight) * static_estimate as f64 + weight * extrapolated;
    blended as u64
}

/// `ETA 5s` once we have enough fetch-window data to extrapolate;
/// empty string while we don't (the caller drops the segment so the
/// label doesn't carry a flapping `ETA …` placeholder). Uses
/// *fetch-window* throughput (completions since `set_phase("fetching")`
/// divided by `fetch_elapsed_ms`) so the estimate reflects per-package
/// work-rate during fetching, not the inflated install-elapsed
/// denominator that would include lockfile parse and resolve time.
fn eta_segment(snap: Snap, completed: usize) -> String {
    if completed >= snap.resolved {
        return String::new();
    }
    let Some(baseline) = snap.completed_at_fetch_start else {
        return String::new();
    };
    let fetch_completed = completed.saturating_sub(baseline);
    if fetch_completed == 0 || snap.fetch_elapsed_ms == 0 {
        return String::new();
    }
    let remaining = snap.resolved - completed;
    let eta_ms = snap.fetch_elapsed_ms.saturating_mul(remaining as u64) / fetch_completed as u64;
    style::edim(format!(
        "ETA {}",
        format_duration(std::time::Duration::from_millis(eta_ms))
    ))
    .to_string()
}

/// Bytes-per-second over the fetching window only. Returns `None`
/// when no bytes have landed or the fetch window hasn't opened yet —
/// the rate segment is then dropped from the label.
fn transfer_rate(snap: Snap) -> Option<u64> {
    if snap.bytes == 0 || snap.fetch_elapsed_ms == 0 {
        return None;
    }
    Some(snap.bytes.saturating_mul(1000) / snap.fetch_elapsed_ms)
}

/// Process-wide latch: once the overflow warning has fired, every
/// subsequent render skips it. The bookkeeping condition tends to
/// recur across multiple heartbeats once tripped — without this
/// gate the CLI would log dozens of identical warnings to stderr,
/// drowning out the actual install output. One warning per CLI
/// session is enough to flag the regression for diagnosis.
static OVERFLOW_WARNED: AtomicBool = AtomicBool::new(false);

/// Defensive clamp: numerator can never exceed denominator. The two
/// known sources of overrun (the catch-up bookkeeping bug and
/// streamed-then-pruned packages) are fixed at their roots, but if a
/// new code path regresses we want the display to stay sane and the
/// `WARN_AUBE_PROGRESS_OVERFLOW` warning to fire — once.
fn clamped_completed(snap: Snap) -> usize {
    let raw = snap.reused + snap.downloaded;
    if raw > snap.resolved && snap.resolved > 0 && !OVERFLOW_WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            code = aube_codes::warnings::WARN_AUBE_PROGRESS_OVERFLOW,
            raw_completed = raw,
            resolved = snap.resolved,
            "progress numerator exceeded resolved-package denominator; clamping display"
        );
    }
    raw.min(snap.resolved)
}

/// Format a byte count using the same SI units pnpm / npm show: `B`,
/// `kB`, `MB`, `GB`. Decimal (1000-based) because that's what every
/// package manager uses for on-the-wire sizes.
pub(crate) fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1_000;
    const MB: u64 = 1_000_000;
    const GB: u64 = 1_000_000_000;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.0} kB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// Format an elapsed duration compactly. Mirrors `ci::format_duration`
/// to avoid a cross-module call from the inline summary path; kept
/// as a single function so future tweaks land in one place.
pub(super) fn format_duration(d: std::time::Duration) -> String {
    super::ci::format_duration(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(phase: usize, resolved: usize, completed: usize, bytes: u64, estimated: u64) -> Snap {
        Snap {
            phase,
            resolved,
            target_total: 0,
            reused: completed,
            downloaded: 0,
            bytes,
            estimated,
            fetch_elapsed_ms: 3_000,
            // Tests model an install where fetching started at zero
            // completions; the eta_segment then derives its rate from
            // `completed - 0 / fetch_elapsed_ms`.
            completed_at_fetch_start: Some(0),
        }
    }

    fn strip_ansi(s: &str) -> String {
        // Strip simple SGR sequences for assertion stability (env-dependent
        // colors_enabled would otherwise break expected-string tests).
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' && chars.peek() == Some(&'[') {
                chars.next();
                for esc_c in chars.by_ref() {
                    if esc_c.is_ascii_alphabetic() {
                        break;
                    }
                }
                continue;
            }
            out.push(c);
        }
        out
    }

    #[test]
    fn resolving_phase_shows_count_without_eta_placeholder() {
        let line = strip_ansi(&progress_line(snap(1, 89, 0, 0, 0), 80, 15));
        assert!(line.contains("89 pkgs"), "got: {line}");
        assert!(line.contains("resolving"), "got: {line}");
        assert!(
            !line.contains("ETA"),
            "no ETA placeholder in resolving: {line}"
        );
    }

    #[test]
    fn resolving_phase_pads_count_for_stable_column() {
        let small = strip_ansi(&progress_line(snap(1, 5, 0, 0, 0), 80, 15));
        let big = strip_ansi(&progress_line(snap(1, 1237, 0, 0, 0), 80, 15));
        // Both lines should align "pkgs" at the same column (min-width-4
        // pad). Exact substring match catches a regression where
        // padding gets applied to only one of the two.
        assert!(small.contains("   5 pkgs"), "got: {small}");
        assert!(big.contains("1237 pkgs"), "got: {big}");
    }

    #[test]
    fn done_phase_fills_bar_and_drops_phase_word() {
        // After `finish()` / `stop()` the bar should paint at full
        // width — the install is genuinely complete. The phase word
        // drops because the `✓ resolved …` summary line that follows
        // owns the success cue.
        let mut s = snap(2, 1230, 1230, 56_000_000, 0);
        s.phase = 4;
        s.reused = 0;
        s.downloaded = 1230;
        let bar = strip_ansi(&bar_only(s, 15, 1230));
        assert_eq!(
            bar.matches('\u{2588}').count(),
            15,
            "done phase must fill the bar: {bar}"
        );
        let line = strip_ansi(&progress_line(s, 80, 15));
        assert!(line.contains("1230/1230 pkgs"), "got: {line}");
        assert!(!line.contains("linking"), "no phase word at done: {line}");
        assert!(!line.contains("fetching"), "no phase word at done: {line}");
    }

    #[test]
    fn linking_phase_omits_byte_total() {
        // Linking is brief and the post-install summary line carries
        // the downloaded-bytes total — showing it inline during
        // linking is duplicate noise.
        let line = strip_ansi(&progress_line(
            snap(3, 142, 142, 13_800_000, 13_800_000),
            80,
            15,
        ));
        assert!(line.contains("linking"), "got: {line}");
        assert!(
            !line.contains("MB"),
            "byte total must drop in linking: {line}"
        );
    }

    #[test]
    fn fetching_phase_shows_bytes_and_estimate() {
        // Estimated unpacked = 69 MB → 0.20× = ~13.8 MB compressed,
        // which exceeds the 4.2 MB downloaded so far so the
        // `/ ~estimated` segment renders.
        let line = strip_ansi(&progress_line(
            snap(2, 142, 23, 4_200_000, 69_000_000),
            80,
            15,
        ));
        assert!(line.contains("23/142 pkgs"), "got: {line}");
        assert!(line.contains("4.2 MB"), "got: {line}");
        assert!(line.contains("~13.8 MB"), "got: {line}");
    }

    #[test]
    fn fetching_phase_drops_estimate_when_running_exceeds_it() {
        // Estimated unpacked × 0.20 (≈ 2.76 MB) is below the running
        // 4.2 MB, so the `/ ~estimated` segment is dropped — at that
        // point the running figure is the better number anyway.
        let line = strip_ansi(&progress_line(
            snap(2, 142, 23, 4_200_000, 13_800_000),
            80,
            15,
        ));
        assert!(line.contains("4.2 MB"), "got: {line}");
        assert!(!line.contains("~"), "estimate should drop: {line}");
    }

    #[test]
    fn linking_phase_drops_rate_and_eta() {
        let line = strip_ansi(&progress_line(
            snap(3, 142, 142, 13_800_000, 13_800_000),
            80,
            15,
        ));
        assert!(line.contains("142/142"), "got: {line}");
        assert!(line.contains("linking"), "got: {line}");
        assert!(!line.contains("MB/s"), "rate must drop in linking: {line}");
        assert!(!line.contains("ETA"), "eta must drop in linking: {line}");
    }

    #[test]
    fn resolving_with_target_total_shows_cur_total_and_filled_bar() {
        let mut s = snap(1, 500, 0, 0, 0);
        s.target_total = 1230;
        let line = strip_ansi(&progress_line(s, 80, 15));
        assert!(line.contains("500/1230 pkgs"), "got: {line}");
        assert!(line.contains("resolving"), "got: {line}");
        // Bar should have at least one filled cell once
        // target_total > resolved > 0.
        assert!(line.contains('\u{2588}'), "expected filled fill: {line}");
        assert!(line.contains('\u{2591}'), "expected empty fill: {line}");
    }

    #[test]
    fn resolving_without_target_total_keeps_bare_count() {
        let line = strip_ansi(&progress_line(snap(1, 500, 0, 0, 0), 80, 15));
        // No target_total → fall back to original "N pkgs" shape
        // with an empty bar so we don't fake a denominator.
        assert!(line.contains("500 pkgs"), "got: {line}");
        assert!(!line.contains("/"), "no cur/total without estimate: {line}");
        assert!(
            !line.contains('\u{2588}'),
            "no fill without estimate: {line}"
        );
    }

    #[test]
    fn resolving_target_total_undershoot_caps_at_resolve_weight() {
        // Resolved already exceeded a stale estimate (e.g. lockfile
        // undershot because the user added a big new subtree). The
        // bar's resolving slice caps at `RESOLVE_BAR_WEIGHT`; phase 2
        // takes over for the remaining fill. Label shows the bare
        // count rather than `cur/total` with cur > total.
        let mut s = snap(1, 1300, 0, 0, 0);
        s.target_total = 1230;
        let line = strip_ansi(&progress_line(s, 80, 15));
        assert!(line.contains("1300 pkgs"), "got: {line}");
        // Bar is in resolving phase — at most ~RESOLVE_BAR_WEIGHT (~15%)
        // of the 15-cell bar can be filled, so the empty portion is
        // still present.
        assert!(
            line.contains('\u{2591}'),
            "resolving bar must not extend past its slice: {line}"
        );
    }

    #[test]
    fn fetch_start_without_estimate_does_not_jump_to_resolve_offset() {
        // No estimate was ever provided (no lockfile, BFS-frontier
        // signal never raised the floor): resolving rendered empty,
        // and fetching at completed=0 must also start empty rather
        // than snap up to RESOLVE_BAR_WEIGHT.
        let empty_resolve = snap(1, 0, 0, 0, 0);
        let resolve_bar = strip_ansi(&bar_only(empty_resolve, 15, 0));
        assert_eq!(
            resolve_bar.matches('\u{2588}').count(),
            0,
            "resolving without estimate must render empty: {resolve_bar}"
        );

        let fetch_start = snap(2, 142, 0, 0, 0);
        let fetch_bar = strip_ansi(&bar_only(fetch_start, 15, 0));
        assert_eq!(
            fetch_bar.matches('\u{2588}').count(),
            0,
            "fetch start without estimate must not snap to RESOLVE_BAR_WEIGHT: {fetch_bar}"
        );

        // Fetching tops out at the resolve+fetch edge (95% of width)
        // even without an estimate, leaving the link slice reserved.
        // On a 15-cell bar that's round(0.95 * 15) = 14 filled cells.
        let fetch_end = snap(2, 142, 142, 0, 0);
        let end_bar = strip_ansi(&bar_only(fetch_end, 15, 142));
        assert_eq!(
            end_bar.matches('\u{2588}').count(),
            14,
            "fetch end without estimate must cap at fetch-end edge: {end_bar}"
        );
    }

    #[test]
    fn unified_bar_continues_from_resolve_into_fetch() {
        // End of resolving with a 1:1 estimate hits the resolve-slice
        // edge; phase 2 starts at the same fill level and grows from
        // there, so the bar progresses monotonically across phases.
        // target_total carries forward through the phase change — the
        // atomic isn't cleared at the boundary — so phase 2 picks up
        // the offset.
        let mut end_resolve = snap(1, 1230, 0, 0, 0);
        end_resolve.target_total = 1230;
        let resolve_bar = strip_ansi(&bar_only(end_resolve, 15, 0));
        let resolve_filled = resolve_bar.matches('\u{2588}').count();

        let mut start_fetch = snap(2, 1230, 0, 0, 0);
        start_fetch.target_total = 1230;
        let fetch_bar = strip_ansi(&bar_only(start_fetch, 15, 0));
        let fetch_filled = fetch_bar.matches('\u{2588}').count();

        // Phase 2 at completed=0 starts at exactly the resolving
        // slice's edge (or one cell higher from rounding), never
        // backs up.
        assert!(
            fetch_filled >= resolve_filled,
            "fetch start ({fetch_filled}) must not regress below resolve end ({resolve_filled})"
        );
        // Fetch end caps at the resolve+fetch edge (95%); the final
        // link slice stays reserved so the bar doesn't read "100%"
        // mid-work. round(0.95 * 15) = 14 cells on a 15-wide bar.
        let mut end_fetch = snap(2, 1230, 1230, 0, 0);
        end_fetch.target_total = 1230;
        let end_fetch_bar = strip_ansi(&bar_only(end_fetch, 15, 1230));
        assert_eq!(
            end_fetch_bar.matches('\u{2588}').count(),
            14,
            "got: {end_fetch_bar}"
        );
    }

    #[test]
    fn linking_phase_holds_below_full() {
        // Phase 3 must not paint a 100%-filled bar — linking is still
        // doing work and a full bar would lie to the user. The fill
        // holds at the resolve+fetch edge (95% of width); on a
        // 15-wide bar that's 14 filled cells with 1 empty reserve.
        let mut s = snap(3, 1230, 1230, 13_800_000, 13_800_000);
        s.target_total = 1230;
        let bar = strip_ansi(&bar_only(s, 15, 1230));
        assert_eq!(
            bar.matches('\u{2588}').count(),
            14,
            "linking must reserve the final cell: {bar}"
        );
        assert_eq!(
            bar.matches('\u{2591}').count(),
            1,
            "linking must keep one empty cell: {bar}"
        );
    }

    #[test]
    fn estimate_falls_back_to_static_below_sample_floor() {
        // Only a handful of packages downloaded — too few to trust
        // the observed average. Stays on `unpacked × ratio`.
        let estimate = estimated_total_download(100_000_000, 5_000_000, 5, 100);
        assert_eq!(estimate, 20_000_000, "static fallback expected");
    }

    #[test]
    fn estimate_converges_to_observed_late_in_install() {
        // User's reported install: 1230/1237 downloaded, 56 MB so far,
        // 276 MB unpacked sum. Without dynamic blending the displayed
        // estimate would stick at 276 × 0.20 = 55.2 MB — close, but
        // would also stay at 82.8 MB on the old 0.30 ratio. With
        // blending the observed signal dominates at 99% complete and
        // the estimate converges to the real ~56 MB regardless of
        // which static ratio we shipped.
        let estimate = estimated_total_download(276_000_000, 56_000_000, 1230, 1237);
        // Observed extrapolation: 56MB / 1230 × 1237 ≈ 56.3 MB.
        // Blend weight at this completion is sqrt(1230/1237) ≈ 0.997,
        // so the blend is essentially the observed value.
        assert!(
            (55_000_000..58_000_000).contains(&estimate),
            "expected ~56 MB convergence, got {estimate}"
        );
    }

    #[test]
    fn estimate_corrects_when_static_is_way_off() {
        // Registry overstated `unpackedSize` by 3×: static would give
        // 60 MB but the actual install only downloads ~55 MB. Late
        // in install the observed signal pulls the estimate back to
        // reality.
        let estimate = estimated_total_download(300_000_000, 50_000_000, 90, 100);
        assert!(
            (54_000_000..58_000_000).contains(&estimate),
            "expected dynamic correction below static overshoot, got {estimate}"
        );
    }

    #[test]
    fn clamps_overflow_to_resolved() {
        let mut s = snap(2, 5, 7, 0, 0);
        s.reused = 7;
        let line = strip_ansi(&progress_line(s, 80, 15));
        assert!(line.contains("5/5 pkgs"), "got: {line}");
    }
}
