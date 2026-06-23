//! CI-mode progress: append-only line on a ~2s heartbeat with a
//! left-aligned bar and stats on the right. No spinners, no child
//! rows, no redraws — shape safe for GitHub Actions / plain pipes,
//! where cursor-control escapes get stripped and each animation frame
//! would otherwise land as its own log line.
//!
//! `CiState` owns the heartbeat thread; callers in `super` poke
//! atomic counters (`resolved`, `reused`, `downloaded`,
//! `downloaded_bytes`, `estimated_bytes`) and the heartbeat renders
//! from those snapshots. See `super::InstallProgress` for how TTY vs
//! CI is selected and how these counters are updated.

use clx::style;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

/// How often the CI heartbeat thread wakes to check whether to print a
/// progress line. Kept long enough that a 142-package fetch produces a
/// handful of lines, not a flood.
pub(super) const CI_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);

/// Fallback width used when the terminal size can't be detected and
/// `$COLUMNS` isn't set. 80 is the historical terminal default and
/// renders cleanly even when the CI log viewer clips long lines.
const DEFAULT_BAR_WIDTH: usize = 80;

/// Hard floor on the bar width. Below this the label text won't fit
/// inside the bar and we'd start losing data.
const MIN_BAR_WIDTH: usize = 40;

/// Hard ceiling so a ridiculously wide terminal doesn't produce a
/// 200-column bar that the CI log viewer wraps awkwardly.
const MAX_BAR_WIDTH: usize = 120;

/// Width of the standalone progress bar in CI mode. Small on purpose:
/// the bar is an indicator, the numbers next to it carry the precise
/// state. Wider bars dominate narrow terminals and waste columns the
/// rate / ETA segments need.
const CI_BAR_WIDTH: usize = 15;

/// CI-mode shared state. Owns the heartbeat thread.
///
/// The status line has a fixed-width bar followed by a label that
/// adapts to the current phase: in `resolving` it shows "N pkgs ·
/// resolving · ETA …"; in `fetching` it shows
/// "cur/total pkgs · downloaded[/ ~estimated] · rate · ETA"; in
/// `linking` the rate / ETA segments drop out and the word "linking"
/// takes their place. Reprinted only when the rendered string
/// actually changes since the previous line.
pub(super) struct CiState {
    phase: AtomicUsize,
    pub(super) resolved: AtomicUsize,
    /// Best-effort estimate of the eventual resolved-package count.
    /// Seeded once from any existing lockfile on disk (re-install,
    /// `package.json` change, foreign lockfile) and raised by the
    /// resolver's BFS-frontier signal as new transitives are
    /// discovered. Only ever increases — `fetch_max` semantics — so
    /// a transient frontier shrink (a packument fetch completing
    /// without enqueueing children) can't snap the displayed
    /// denominator backward. Zero until the first signal lands, in
    /// which case the resolving-phase bar falls back to the empty-bar
    /// placeholder.
    pub(super) target_total: AtomicUsize,
    pub(super) reused: AtomicUsize,
    pub(super) downloaded: AtomicUsize,
    pub(super) downloaded_bytes: AtomicU64,
    /// Running sum of `dist.unpackedSize` from packuments seen during
    /// the streaming resolve. `0` until the first packument with the
    /// field arrives, and stays `0` on the lockfile fast path (where
    /// no packument fetch happens). The display gates the
    /// `/ ~13.8 MB` estimated-total segment on this being non-zero.
    pub(super) estimated_bytes: AtomicU64,
    /// Snapshot of `reused + downloaded` at the moment
    /// `set_phase("fetching")` first fires. Used as the baseline for
    /// the fetch-window ETA so the displayed estimate reflects
    /// per-package throughput *during fetching*, not the inflated
    /// install-elapsed denominator that includes lockfile parse and
    /// resolve time. `usize::MAX` sentinel = "not captured yet".
    completed_at_fetch_start: AtomicUsize,
    start: Instant,
    /// Captured the first time `set_phase("fetching")` is called. Used
    /// as the denominator for the transfer rate so it measures network
    /// throughput during the fetch window, not `bytes / (resolve_time +
    /// fetch_time)`. `OnceLock` makes the first-writer-wins semantics
    /// explicit without a mutex.
    fetch_start: OnceLock<Instant>,
    /// The last rendered line we actually wrote. Dedup on the rendered
    /// string (not the raw counter tuple) so changes that round to the
    /// same display — e.g. a byte delta that stays in the same MB
    /// bucket, or a phase change when phase isn't in the render — stay
    /// quiet instead of reprinting an identical line.
    last_printed: Mutex<String>,
    /// Whether the heartbeat has ever emitted a progress line. Stays
    /// `false` for fast installs that finish before the first 2s tick
    /// — `print_install_summary` then takes the no-bar single-line
    /// fast-mode path instead of writing a final framed bar.
    pub(super) shown: AtomicBool,
    done: AtomicBool,
    /// Live `InstallProgress` clone count. Incremented in `Clone`,
    /// decremented in `Drop`. When it hits zero the last clone is gone
    /// and we tear down. We can't use `Arc::strong_count` for this
    /// because the heartbeat thread owns its own strong `Arc<CiState>`
    /// for the entire run.
    pub(super) alive: AtomicUsize,
    /// Signals the heartbeat thread to wake early on shutdown. Phase
    /// transitions deliberately do *not* wake the heartbeat — letting
    /// every phase change punch through the 2s gate would flood the
    /// log with one extra line per phase on every fast install.
    wake: Condvar,
    wake_lock: Mutex<()>,
    /// The heartbeat thread's join handle, taken by `stop()` so the
    /// thread is guaranteed to have exited before the final summary
    /// line is written — no stray tick can appear after `Done in …`.
    heartbeat: Mutex<Option<thread::JoinHandle<()>>>,
}

/// Detect the current terminal width for rendering the progress bar.
/// Prefers the `$COLUMNS` env var (set by most shells and honored by
/// GitHub Actions), then falls back to `console::Term::stderr().size()`
/// (works when stderr is a TTY), then a sensible 80-column default.
/// Clamped into `[MIN_BAR_WIDTH, MAX_BAR_WIDTH]`.
pub(super) fn term_width() -> usize {
    let raw = std::env::var("COLUMNS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .or_else(|| {
            let (_rows, cols) = console::Term::stderr().size();
            // `size()` returns (24, 80) as a hardcoded fallback when stderr
            // isn't a TTY — treat that as "unknown" and fall through.
            if cols == 0 { None } else { Some(cols as usize) }
        })
        .unwrap_or(DEFAULT_BAR_WIDTH);
    raw.clamp(MIN_BAR_WIDTH, MAX_BAR_WIDTH)
}

impl CiState {
    pub(super) fn new() -> Self {
        Self {
            phase: AtomicUsize::new(0),
            resolved: AtomicUsize::new(0),
            target_total: AtomicUsize::new(0),
            reused: AtomicUsize::new(0),
            downloaded: AtomicUsize::new(0),
            downloaded_bytes: AtomicU64::new(0),
            estimated_bytes: AtomicU64::new(0),
            completed_at_fetch_start: AtomicUsize::new(usize::MAX),
            start: Instant::now(),
            fetch_start: OnceLock::new(),
            last_printed: Mutex::new(String::new()),
            shown: AtomicBool::new(false),
            done: AtomicBool::new(false),
            alive: AtomicUsize::new(1),
            wake: Condvar::new(),
            wake_lock: Mutex::new(()),
            heartbeat: Mutex::new(None),
        }
    }

    fn snapshot(&self) -> Snap {
        // `fetch_elapsed_ms` is 0 until fetching has started, and
        // frozen at the elapsed-so-far value once it does — so after
        // fetching ends the rate no longer decays, and before it
        // begins we never divide.
        let fetch_elapsed_ms = self
            .fetch_start
            .get()
            .map(|t| t.elapsed().as_millis() as u64)
            .unwrap_or(0);
        let baseline = self.completed_at_fetch_start.load(Ordering::Relaxed);
        Snap {
            phase: self.phase.load(Ordering::Relaxed),
            resolved: self.resolved.load(Ordering::Relaxed),
            target_total: self.target_total.load(Ordering::Relaxed),
            reused: self.reused.load(Ordering::Relaxed),
            downloaded: self.downloaded.load(Ordering::Relaxed),
            bytes: self.downloaded_bytes.load(Ordering::Relaxed),
            estimated: self.estimated_bytes.load(Ordering::Relaxed),
            fetch_elapsed_ms,
            // `usize::MAX` means the baseline hasn't been captured yet
            // (still pre-fetching). Render layer treats that as
            // "ETA …" rather than computing against a missing baseline.
            completed_at_fetch_start: if baseline == usize::MAX {
                None
            } else {
                Some(baseline)
            },
        }
    }

    fn render(snap: Snap) -> String {
        super::render::progress_line(snap, term_width(), CI_BAR_WIDTH)
    }

    /// Render the one-line header banner that prints once above the
    /// first heartbeat-emitted progress line. Plain whitespace
    /// alignment, no frame: `aube VERSION by jdx.dev` for standalone
    /// aube, `<display_name> VERSION` for an embedder. The vendor
    /// attribution is gated by `product_banner` so the engine brand
    /// never leaks into a host's install output.
    fn render_header() -> String {
        super::product_banner("")
    }

    pub(super) fn spawn_heartbeat(state: &Arc<Self>) {
        let thread_state = state.clone();
        let handle = thread::spawn(move || {
            let state = thread_state;
            loop {
                let guard = state.wake_lock.lock().unwrap();
                // Re-check `done` *before* sleeping. `stop()` sets `done`
                // and then `notify_all()`s without holding `wake_lock`, so
                // a notification that races with the tick body would
                // otherwise be lost and the thread would sleep a full
                // `CI_HEARTBEAT_INTERVAL` before noticing shutdown.
                if state.done.load(Ordering::Relaxed) {
                    break;
                }
                let (guard, _timeout) = state
                    .wake
                    .wait_timeout(guard, CI_HEARTBEAT_INTERVAL)
                    .unwrap();
                drop(guard);
                if state.done.load(Ordering::Relaxed) {
                    break;
                }
                let snap = state.snapshot();
                // Don't make noise until an install is actually underway.
                // Until then there's nothing to bar-graph and no reason to
                // print anything — a no-op install should remain
                // completely silent.
                if snap.resolved == 0 || snap.phase == 0 {
                    continue;
                }
                let line = Self::render(snap);
                if line.is_empty() {
                    continue;
                }
                let mut last = state.last_printed.lock().unwrap();
                if *last == line {
                    // Same rendered line as before — stay quiet.
                    continue;
                }
                *last = line.clone();
                drop(last);
                // First time we actually print, emit the unframed
                // `aube VERSION by jdx.dev` header above the bar so
                // the CI log shows the aube banner. Only printed
                // once per install — `shown` flips true here.
                if !state.shown.swap(true, Ordering::Relaxed) {
                    let _ = writeln!(std::io::stderr(), "{}", Self::render_header());
                }
                let _ = writeln!(std::io::stderr(), "{line}");
            }
        });
        *state.heartbeat.lock().unwrap() = Some(handle);
    }

    pub(super) fn set_phase(&self, phase: &str) {
        // Map the free-form phase label from `install::run` onto the fixed
        // 1=resolving / 2=fetching / 3=linking counter. Unknown labels
        // leave the counter alone.
        let n = match phase {
            "resolving" => 1,
            "fetching" => 2,
            "linking" => 3,
            _ => return,
        };
        if n == 2 {
            // First-writer-wins; a second "fetching" transition (shouldn't
            // happen but defend against it) doesn't reset the rate window.
            let _ = self.fetch_start.set(Instant::now());
            // Capture the completion baseline for the fetch-window ETA.
            // `compare_exchange` so a duplicate phase=2 transition doesn't
            // overwrite the original snapshot (matches `fetch_start` first-
            // writer-wins semantics).
            let completed =
                self.reused.load(Ordering::Relaxed) + self.downloaded.load(Ordering::Relaxed);
            let _ = self.completed_at_fetch_start.compare_exchange(
                usize::MAX,
                completed,
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
        }
        self.phase.store(n, Ordering::Relaxed);
        // Phase transitions deliberately do *not* notify the heartbeat:
        // a sub-2s install runs through resolving → fetching → linking
        // in tens of milliseconds, and waking the heartbeat on every
        // transition would defeat the fast-mode quiet path. The next
        // natural 2s tick (or `stop()`) picks up the new phase.
    }

    /// Stop the heartbeat and (optionally) write the final summary.
    ///
    /// We `join()` the heartbeat thread *before* writing the summary
    /// line so there's no race where a heartbeat tick lands after the
    /// summary. Idempotent via `done.swap`: the second caller (Drop
    /// after explicit `finish()`, etc.) finds `done == true` and
    /// returns without doing anything.
    pub(super) fn stop(&self, print_summary: bool) {
        if self.done.swap(true, Ordering::Relaxed) {
            return;
        }
        self.wake.notify_all();
        if let Some(handle) = self.heartbeat.lock().unwrap().take() {
            let _ = handle.join();
        }
        if !print_summary {
            return;
        }
        // If the heartbeat never printed anything (fast install, no-op,
        // or error before the first tick), stay completely silent — the
        // separate `print_install_summary` call writes the single-line
        // fast-mode summary.
        if !self.shown.load(Ordering::Relaxed) {
            return;
        }
        // One snapshot for both the final bar and the summary stats —
        // taking two separate snapshots would let a concurrent
        // `FetchRow::drop` land between them and desync the numbers.
        let mut snap = self.snapshot();
        // Promote to the terminal "done" phase so the final bar
        // renders at 100% and drops the linking-phase label. The
        // mid-work 95% cap is about not lying while linking is in
        // flight; by the time we reach `stop()` the install is fully
        // complete and the bar should reflect that before the summary
        // line lands.
        snap.phase = 4;
        // Defensive: keep `reused + downloaded <= resolved` so the final
        // bar *and* the summary line below stay internally consistent
        // even if a counter race over-credited `reused`. Trim reused,
        // never downloaded — downloaded reflects real network work.
        // Mirrors the live bar's `clamped_completed` clamp and the
        // `clamp_reused_to` rebase `set_total` applies.
        snap.reused = snap
            .reused
            .min(snap.resolved.saturating_sub(snap.downloaded));
        // Emit one final bar so CI logs end on a complete snapshot
        // even when the last heartbeat was skipped (fast fetch+link
        // between ticks). Skipped if it would duplicate the previous
        // line.
        let final_bar = Self::render(snap);
        if !final_bar.is_empty() {
            let mut last = self.last_printed.lock().unwrap();
            if *last != final_bar {
                *last = final_bar.clone();
                drop(last);
                let _ = writeln!(std::io::stderr(), "{final_bar}");
            }
        }
        // Final stats line: elapsed time plus the full resolve / reuse /
        // download breakdown, color-styled so the green check stands
        // out against the dim text of the timing. No `aube` prefix —
        // the header line above already identified the install.
        // Drops the `downloaded N (X B)` segment entirely when nothing
        // was downloaded (warm cache); same with the parenthesized
        // byte count when the download count itself is non-zero but
        // the byte total is — `0 B` is just noise.
        let elapsed = self.start.elapsed();
        let mut summary = format!(
            "{} resolved {} · reused {}",
            style::egreen("✓").bold(),
            style::ecyan(snap.resolved).bold(),
            style::ecyan(snap.reused).bold(),
        );
        if snap.downloaded > 0 || snap.bytes > 0 {
            summary.push_str(&format!(
                " · downloaded {}",
                style::ecyan(snap.downloaded).bold()
            ));
            if snap.bytes > 0 {
                summary.push_str(&format!(
                    " ({})",
                    style::edim(super::render::format_bytes(snap.bytes))
                ));
            }
        }
        summary.push_str(&format!(" in {}", style::edim(format_duration(elapsed))));
        let _ = writeln!(std::io::stderr(), "{summary}");
    }
}

/// Snapshot of the atomic counters at one heartbeat tick.
#[derive(Clone, Copy)]
pub(super) struct Snap {
    pub(super) phase: usize,
    pub(super) resolved: usize,
    /// Best-effort estimate of the final resolved count: lockfile
    /// peek plus the BFS-frontier high-water mark. `0` means no
    /// estimate is available yet; render falls back to the pre-floor
    /// empty bar.
    pub(super) target_total: usize,
    pub(super) reused: usize,
    pub(super) downloaded: usize,
    pub(super) bytes: u64,
    pub(super) estimated: u64,
    pub(super) fetch_elapsed_ms: u64,
    /// Numerator (`reused + downloaded`) at the moment fetching
    /// started. `None` until phase=2 first fires; render layer falls
    /// back to `ETA …` while it's missing.
    pub(super) completed_at_fetch_start: Option<usize>,
}

/// Format an elapsed duration compactly: sub-second → `240ms`,
/// sub-minute → `4.0s`, otherwise `3m12s`. Matches how most package
/// managers render install time in their summary lines.
pub(super) fn format_duration(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", d.as_secs_f64())
    } else {
        let total = d.as_secs();
        format!("{}m{:02}s", total / 60, total % 60)
    }
}
