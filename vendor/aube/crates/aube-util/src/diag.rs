/*!
 * Cold-install deep diagnostics for `aube install`.
 *
 * Activation paths:
 *
 * - CLI flag `aube install --diag <summary|trace|live|full>`,
 *   which builds a [`DiagConfig`] and calls [`init_with_config`].
 * - Env vars for programmatic / CI use:
 *   - `AUBE_DIAG_FILE=<path>` writes JSONL events to a file.
 *   - `AUBE_DIAG_PRINT=1` prints every recorded span to stderr.
 *   - `AUBE_DIAG_THRESHOLD_MS=<n>` filters live prints to spans
 *     whose duration is at least `n` milliseconds.
 *   - `AUBE_DIAG_SUMMARY=1` enables the end-of-run aggregate table only.
 *   - `AUBE_DIAG_CRITPATH=1` retains per-event records for the
 *     critical-path / lifecycle / what-if / starvation analyzers.
 *
 * Event wire format (JSONL, one event per line):
 *
 * `{"t":<ms>,"cat":"<category>","name":"<name>","dur":<ms>,"meta":<obj>?}`
 *
 * Field semantics:
 *
 * - `t` elapsed milliseconds since the recorder was initialized.
 * - `cat` category bucket (e.g. `resolver`, `registry`, `fetch`,
 *   `store`, `linker`, `materialize`, `install`, `install_phase`,
 *   `lockfile`, `manifest`, `starvation`, `channel`, `sample`).
 * - `name` event identifier within the category.
 * - `dur` duration in milliseconds; zero for instant markers.
 * - `meta` optional inline JSON object with structured context.
 *   Embedded strings are escaped via [`jstr`].
 *
 * Recording primitives:
 *
 * - [`Span::new`] for scope-bracketed timings (RAII; emits on drop).
 *   Prefer [`Span::with_meta_fn`] over [`Span::with_meta`] so the
 *   `format!` work is skipped when diagnostics are disabled.
 * - [`event_lazy`] / [`instant_lazy`] for one-shot events with
 *   closure-deferred metadata; same disabled-path no-op behaviour.
 * - [`event`] / [`instant`] for events whose metadata is either
 *   absent or already available without allocation.
 *
 * Concurrency tracking:
 *
 * - [`inflight`] returns an [`InflightGuard`] which increments on
 *   creation and decrements on drop. The sampler emits a
 *   `cat=sample,name=concurrency` event every 50 ms recording each
 *   [`Slot`]'s in-flight count.
 *
 * Permit-holder attribution:
 *
 * - [`register_holder`] records the package currently holding a
 *   [`Slot`]. [`attribute_wait`] queries the active holders when a
 *   waiter blocks for at least 50 ms and emits a `starvation` event
 *   naming the blockers.
 */

use std::cmp::Ordering as CmpOrdering;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

struct Recorder {
    start: Instant,
    file: Option<Mutex<BufWriter<File>>>,
    print_stderr: bool,
    threshold_ms: u64,
    summary: bool,
    track_events: bool,
    in_flight_packuments: AtomicU64,
    in_flight_tarballs: AtomicU64,
    in_flight_imports: AtomicU64,
    in_flight_links: AtomicU64,
    in_flight_decode: AtomicU64,
    event_count: AtomicU64,
    aggregates: Mutex<AggMap>,
    /// In-memory event log used by the critical-path / lifecycle / what-if
    /// / starvation analyzers. Allocated up front to its [`EVENTS_CAP`]
    /// capacity when `track_events` is on so the emit hot path never
    /// pays a reallocation under the mutex lock.
    events: Mutex<Vec<EventRec>>,
}

/**
 * Closed enumeration of event categories.
 *
 * Centralizing the category set lets the type system guarantee that
 * every emit site uses one of the recognized buckets, eliminating the
 * silent-typo-becomes-orphan-row class of bugs that the old `&str`
 * signature allowed. Each variant carries a stable wire identifier
 * accessible via [`Category::wire`].
 *
 * Adding a new category is a one-line enum extension; analyzer filters
 * such as [`is_envelope`] and the `--diag` summary printer stay in
 * sync because they match against the enum rather than re-typing
 * literals at every site.
 */
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Category {
    Resolver,
    Registry,
    Fetch,
    Store,
    Linker,
    Materialize,
    Install,
    InstallPhase,
    Lockfile,
    Manifest,
    Starvation,
    Channel,
    Sample,
    Frozen,
    Script,
    Kernel,
}

impl Category {
    /**
     * Stable wire identifier emitted as the `cat` field in JSONL traces
     * and used as the row key in the end-of-run summary table. The
     * mapping is part of the public output contract and must not
     * change once a release ships a value.
     */
    pub const fn wire(self) -> &'static str {
        match self {
            Category::Resolver => "resolver",
            Category::Registry => "registry",
            Category::Fetch => "fetch",
            Category::Store => "store",
            Category::Linker => "linker",
            Category::Materialize => "materialize",
            Category::Install => "install",
            Category::InstallPhase => "install_phase",
            Category::Lockfile => "lockfile",
            Category::Manifest => "manifest",
            Category::Starvation => "starvation",
            Category::Channel => "channel",
            Category::Sample => "sample",
            Category::Frozen => "frozen",
            Category::Script => "script",
            Category::Kernel => "kernel",
        }
    }
}

/**
 * Per-bucket key for aggregate stats. Uses the typed [`Category`] enum
 * and a `&'static str` name so map lookups are pointer comparisons and
 * insert costs no allocation.
 */
type AggKey = (Category, &'static str);

/**
 * Per-bucket aggregate tallies.
 *
 * Field reference:
 *
 * - `count` number of events recorded for this `(category, name)`.
 * - `sum_ns` cumulative duration in nanoseconds. Rescaled to ms at
 *   output time; the underlying ns precision survives summing across
 *   millions of sub-microsecond spans without overflow.
 * - `max_ns` largest single-event duration observed for this bucket.
 */
#[derive(Default, Clone, Copy, Debug)]
struct AggVal {
    count: u64,
    sum_ns: u128,
    max_ns: u128,
}

/**
 * Per-`(category, name)` aggregate map maintained while diag is active.
 * Keyed by [`AggKey`] with [`AggVal`] tallies. Used by [`print_summary`].
 */
type AggMap = std::collections::BTreeMap<AggKey, AggVal>;

/**
 * In-memory event record retained when `track_events` is on. The
 * critical-path, lifecycle, what-if, and starvation analyzers iterate
 * these records after the install completes.
 *
 * `cat` and `name` are typed/static rather than owned `String`; this
 * makes [`EventRec::clone`] a pointer copy and cuts the per-event
 * allocation budget by two heap entries (~56 bytes typical) on hot
 * installs that emit hundreds of thousands of records.
 */
#[derive(Clone)]
struct EventRec {
    cat: Category,
    name: &'static str,
    start_ms: f64,
    end_ms: f64,
    pkg_id: Option<String>,
    meta: Option<String>,
}

/**
 * Hard cap on the per-event in-memory log when `track_events` is on.
 *
 * Each entry retains the event's category, name, optional package id,
 * and optional metadata string. At ~256 bytes amortized per entry the
 * cap holds the worst-case footprint to a few hundred MiB on hostile
 * inputs while still capturing more than enough to drive the analyzers
 * for any realistic install (the largest fixture observed produces
 * ~95k events).
 */
const EVENTS_CAP: usize = 1_000_000;

static RECORDER: OnceLock<Option<Recorder>> = OnceLock::new();

/**
 * Fast-path activation flag set after [`init_with_config`] populates
 * [`RECORDER`]. The hot-path [`enabled`] check loads this atomic
 * directly instead of indirecting through the `OnceLock` discriminant
 * plus the `Option` match, which matters at the millions-of-calls
 * scale a busy install reaches. `Relaxed` ordering is sufficient: the
 * first observed `true` triggers the heavyweight code path which then
 * re-acquires the recorder via [`rec`] under standard `OnceLock`
 * happens-before semantics.
 */
static ENABLED: AtomicBool = AtomicBool::new(false);

/**
 * Configuration knobs for the diagnostics recorder.
 *
 * Two construction paths are supported:
 *   - The `--diag <mode>` CLI flag in the binary builds a [`DiagConfig`]
 *     directly and passes it to [`init_with_config`].
 *   - The env-var driven path uses [`DiagConfig::from_env`] which reads
 *     `AUBE_DIAG_*` vars. Returns `None` when no diag is requested.
 *
 * Field reference:
 *   `file`           Optional sink for the JSONL event stream.
 *   `print_stderr`   When `true`, every recorded span is also printed
 *                    to stderr (filtered by `threshold_ms`).
 *   `summary`        Enable the end-of-run aggregate table.
 *   `track_events`   Retain per-event records in memory for the
 *                    [`print_critical_path`] / [`print_pkg_lifecycle`]
 *                    / [`print_what_if`] / [`print_starvation`]
 *                    analyzers. Costs memory proportional to event count.
 *   `threshold_ms`   Minimum span duration (in ms) for the live stderr
 *                    printer to emit. Ignored when `print_stderr` is `false`.
 */
#[derive(Default, Clone)]
pub struct DiagConfig {
    pub file: Option<PathBuf>,
    pub print_stderr: bool,
    pub summary: bool,
    pub track_events: bool,
    pub threshold_ms: u64,
}

impl DiagConfig {
    /**
     * Build a [`DiagConfig`] from the `AUBE_DIAG_*` environment variables.
     *
     * Returns `None` when none of `AUBE_DIAG_FILE`, `AUBE_DIAG_PRINT`,
     * `AUBE_DIAG_SUMMARY`, or `AUBE_DIAG_CRITPATH` is set, meaning the
     * caller should leave diagnostics off entirely.
     *
     * `summary` is implied whenever the recorder is alive at all;
     * `track_events` is gated on `AUBE_DIAG_CRITPATH` because the
     * per-event log is the costly bit.
     */
    pub fn from_env() -> Option<Self> {
        let file = crate::env::embedder_env("DIAG_FILE").map(PathBuf::from);
        let print = crate::env::embedder_env("DIAG_PRINT").is_some();
        let summary_env = crate::env::embedder_env("DIAG_SUMMARY").is_some();
        let critpath_env = crate::env::embedder_env("DIAG_CRITPATH").is_some();
        if file.is_none() && !print && !summary_env && !critpath_env {
            return None;
        }
        let threshold_ms = crate::env::embedder_env("DIAG_THRESHOLD_MS")
            .as_deref()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        Some(Self {
            file,
            print_stderr: print,
            // Summary table emits whenever the recorder is alive.
            summary: true,
            // Critical-path / lifecycle / what-if / starvation analysis requires
            // retaining per-event records, which is more memory-intensive.
            track_events: critpath_env,
            threshold_ms,
        })
    }
}

/**
 * Initialize the recorder from `AUBE_DIAG_*` environment variables.
 *
 * No-op when no relevant env var is set. Provided as the env-driven entry
 * point for callers that do not parse the `--diag` CLI flag (CI scripts,
 * external tooling). Internally delegates to [`init_with_config`] with
 * the result of [`DiagConfig::from_env`].
 *
 * Idempotent: subsequent calls after the first do nothing.
 */
pub fn init() {
    init_with_config(DiagConfig::from_env());
}

/**
 * Validate that a `--diag-file` / `AUBE_DIAG_FILE` path is safe to
 * truncate-and-write.
 *
 * Rejects:
 *   - UNC paths (`\\server\share\...`) and Windows device namespaces
 *     (`\\.\...`, `\\?\...`) so that an attacker-supplied env var can
 *     not target named pipes, raw devices, or remote shares.
 *   - NTFS alternate data streams (any `:` after the volume on
 *     Windows). On non-Windows targets `:` is allowed for normal use.
 *   - Reserved Windows device names in the final filename
 *     (`CON`, `PRN`, `AUX`, `NUL`, `COM1`-`COM9`, `LPT1`-`LPT9`).
 *
 * Path traversal beyond the working directory is permitted because
 * legitimate uses include CI scratch dirs and the system temp folder.
 * Operators can additionally constrain via the file-permission model.
 *
 * Returns `Err(reason)` with a human-readable message on rejection.
 */
fn validate_diag_path(path: &std::path::Path) -> Result<(), &'static str> {
    let s = path.to_string_lossy();
    // `\\…` covers Windows UNC, device (`\\.\…`), and verbatim (`\\?\…`)
    // namespaces and is rejected on every platform. `//…` is reserved
    // by POSIX (XBD §4.13) and is the wire form of UNC under MSYS / Git
    // Bash on Windows; reject it only on Windows so legitimate Unix
    // paths starting with `//` (POSIX permits any double-slash prefix
    // to be interpreted by the implementation) are not blocked.
    if s.starts_with(r"\\") {
        return Err("UNC and device paths are not permitted");
    }
    #[cfg(windows)]
    if s.starts_with("//") {
        return Err("UNC and device paths are not permitted");
    }
    #[cfg(windows)]
    {
        // Strip volume root (e.g. "C:") then reject any further colon use,
        // which would address an alternate data stream.
        let after_volume = match s.as_bytes() {
            [_, b':', b'/' | b'\\', rest @ ..] | [_, b':', rest @ ..] => {
                std::str::from_utf8(rest).unwrap_or(s.as_ref())
            }
            _ => s.as_ref(),
        };
        if after_volume.contains(':') {
            return Err("alternate data stream paths are not permitted");
        }
        if let Some(stem) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_ascii_uppercase)
        {
            const RESERVED: &[&str] = &[
                "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
                "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8",
                "LPT9",
            ];
            if RESERVED.contains(&stem.as_str()) {
                return Err("reserved Windows device name");
            }
        }
    }
    Ok(())
}

/**
 * Initialize the recorder from an explicit [`DiagConfig`].
 *
 * Passing `None` keeps diagnostics off. Passing `Some(cfg)` opens the
 * configured JSONL file (when `cfg.file` is set) and records the install
 * start instant against which all event timestamps are measured.
 *
 * If `cfg.file` is set but rejected by [`validate_diag_path`] or fails
 * to open, the recorder still activates without a file sink and a
 * stderr warning is emitted. The diag run continues so that summary,
 * critpath, and other in-memory analyzers still produce output.
 *
 * Idempotent: only the first call wins. Subsequent calls (including from
 * spawned tasks) are no-ops.
 *
 * The CLI binary calls this from `main` after argument parsing; library
 * callers using env-driven configuration should prefer [`init`].
 */
pub fn init_with_config(cfg: Option<DiagConfig>) {
    RECORDER.get_or_init(|| {
        let cfg = cfg?;
        let file = cfg.file.and_then(|p| match validate_diag_path(&p) {
            Ok(()) => match File::create(&p) {
                Ok(f) => Some(Mutex::new(BufWriter::with_capacity(64 * 1024, f))),
                Err(err) => {
                    eprintln!("[diag] could not open trace file {}: {err}", p.display());
                    None
                }
            },
            Err(reason) => {
                eprintln!(
                    "[diag] refusing to write trace file {}: {reason}",
                    p.display()
                );
                None
            }
        });
        if cfg.print_stderr {
            eprintln!(
                "[diag] active threshold={}ms summary={}",
                cfg.threshold_ms, cfg.summary
            );
        }
        // Track-events allocates the worst-case event log eagerly.
        // Rationale: with `EVENTS_CAP` ≈ 1M and ~256 B amortized per
        // record, growing by doubling under the mutex lock would copy
        // ~134 MiB at 524k entries while every emitter blocks. One
        // upfront allocation keeps emit latency predictable.
        let events_cap = if cfg.track_events { EVENTS_CAP } else { 0 };
        let recorder = Recorder {
            start: Instant::now(),
            file,
            print_stderr: cfg.print_stderr,
            threshold_ms: cfg.threshold_ms,
            summary: cfg.summary,
            track_events: cfg.track_events,
            in_flight_packuments: AtomicU64::new(0),
            in_flight_tarballs: AtomicU64::new(0),
            in_flight_imports: AtomicU64::new(0),
            in_flight_links: AtomicU64::new(0),
            in_flight_decode: AtomicU64::new(0),
            event_count: AtomicU64::new(0),
            aggregates: Mutex::new(std::collections::BTreeMap::new()),
            events: Mutex::new(Vec::with_capacity(events_cap)),
        };
        // Publish the active flag last so [`enabled`] starts returning
        // `true` only after the recorder is fully constructed and
        // visible through [`RECORDER`].
        ENABLED.store(true, Ordering::Relaxed);
        Some(recorder)
    });
}

/**
 * Reports whether diagnostics are active.
 *
 * Single relaxed atomic load on [`ENABLED`]. Hot-path call sites gate
 * `format!` work behind this check via [`Span::with_meta_fn`],
 * [`event_lazy`], and [`instant_lazy`]. The flag is set after
 * [`init_with_config`] finishes installing the recorder, so a few
 * events emitted during a concurrent first-init may be silently
 * dropped — acceptable for a diagnostics layer.
 */
#[inline]
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/**
 * Internal accessor that returns `Some(&Recorder)` only when diag is
 * configured. All public emit paths short-circuit on `None`.
 */
fn rec() -> Option<&'static Recorder> {
    RECORDER.get().and_then(|o| o.as_ref())
}

/**
 * Emit a recorded event with an explicit duration and an already-built
 * metadata string (or `None` for no metadata).
 *
 * Most call sites should prefer [`event_lazy`] so the metadata `format!`
 * is skipped when diag is disabled. This eager variant is appropriate
 * when the metadata is already available as `&str` without allocation
 * (e.g. a cached buffer) or when the call site is itself behind an
 * [`enabled`] check.
 *
 * The recorder fans the event out to up to three sinks depending on
 * configuration:
 *   - the in-memory aggregate table (always when `summary` is on),
 *   - the in-memory event log (when `track_events` is on and the span
 *     has non-zero duration),
 *   - the JSONL file (when `file` was opened),
 *   - the stderr live printer (when `print_stderr` is on and the span
 *     duration is at least `threshold_ms`).
 */
pub fn event(category: Category, name: &'static str, duration: Duration, meta: Option<&str>) {
    let Some(r) = rec() else { return };
    let t_ms = r.start.elapsed().as_secs_f64() * 1000.0;
    let dur_ms = duration.as_secs_f64() * 1000.0;
    r.event_count.fetch_add(1, Ordering::Relaxed);

    if r.summary {
        let dur_ns = duration.as_nanos();
        let mut agg = r.aggregates.lock().unwrap_or_else(|e| e.into_inner());
        let entry = agg.entry((category, name)).or_default();
        entry.count += 1;
        entry.sum_ns += dur_ns;
        if dur_ns > entry.max_ns {
            entry.max_ns = dur_ns;
        }
    }

    if r.track_events && dur_ms > 0.0 {
        let pkg_id = meta.and_then(extract_pkg_id);
        // Starvation events carry blame metadata that the analyzer
        // re-parses; every other category drops meta after writing
        // since the in-memory log is consumed only by analyzers that
        // do not look at meta.
        let stored_meta = if matches!(category, Category::Starvation) {
            meta.map(|s| s.to_string())
        } else {
            None
        };
        let mut evs = r.events.lock().unwrap_or_else(|e| e.into_inner());
        if evs.len() < EVENTS_CAP {
            evs.push(EventRec {
                cat: category,
                name,
                start_ms: t_ms - dur_ms,
                end_ms: t_ms,
                pkg_id,
                meta: stored_meta,
            });
            if evs.len() == EVENTS_CAP {
                eprintln!(
                    "[diag] event log reached {} entries; further per-event records will be dropped",
                    EVENTS_CAP
                );
            }
        }
    }

    if let Some(file) = &r.file {
        let mut f = file.lock().unwrap_or_else(|e| e.into_inner());
        let cat_wire = category.wire();
        let _ = match meta {
            Some(m) => writeln!(
                f,
                r#"{{"t":{:.3},"cat":"{}","name":"{}","dur":{:.3},"meta":{}}}"#,
                t_ms, cat_wire, name, dur_ms, m
            ),
            None => writeln!(
                f,
                r#"{{"t":{:.3},"cat":"{}","name":"{}","dur":{:.3}}}"#,
                t_ms, cat_wire, name, dur_ms
            ),
        };
    }

    if r.print_stderr && (dur_ms as u64) >= r.threshold_ms {
        let cat_wire = category.wire();
        match meta {
            Some(m) => eprintln!(
                "[diag {:>8.2}ms] {:>10}.{:<28} {:>9.2}ms  {}",
                t_ms, cat_wire, name, dur_ms, m
            ),
            None => eprintln!(
                "[diag {:>8.2}ms] {:>10}.{:<28} {:>9.2}ms",
                t_ms, cat_wire, name, dur_ms
            ),
        }
    }
}

/**
 * Emit an instantaneous marker. Equivalent to [`event`] with a zero
 * duration.
 *
 * Useful for phase boundaries and irreversible state transitions
 * (e.g. `install.resolve_end`, `materialize.drain_rx_begin`).
 */
pub fn instant(category: Category, name: &'static str, meta: Option<&str>) {
    event(category, name, Duration::ZERO, meta);
}

/**
 * Closure-deferred [`instant`]. The metadata `format!` runs only when
 * [`enabled`] returns `true`, so disabled call sites pay only an atomic
 * load.
 */
pub fn instant_lazy<F: FnOnce() -> String>(category: Category, name: &'static str, meta_fn: F) {
    if !enabled() {
        return;
    }
    let meta = meta_fn();
    event(category, name, Duration::ZERO, Some(&meta));
}

/**
 * Closure-deferred [`event`]. The metadata `format!` runs only when
 * [`enabled`] returns `true`, so disabled call sites pay only an atomic
 * load.
 */
pub fn event_lazy<F: FnOnce() -> String>(
    category: Category,
    name: &'static str,
    duration: Duration,
    meta_fn: F,
) {
    if !enabled() {
        return;
    }
    let meta = meta_fn();
    event(category, name, duration, Some(&meta));
}

/**
 * RAII timer for a scoped duration measurement.
 *
 * The span captures `Instant::now()` on construction and emits a
 * recorded event with the elapsed duration when dropped (or when
 * [`Span::finish`] is called explicitly). The category is `&'static str`
 * so it can be cheaply embedded in the JSONL output without allocation.
 *
 * Idiomatic usage at a call site:
 *
 * ```ignore
 * let _diag = aube_util::diag::Span::new("registry", "fetch_packument")
 *     .with_meta_fn(|| format!(r#"{{"name":{}}}"#, jstr(name)));
 * // ... work ...
 * // Drop on scope exit emits the event.
 * ```
 *
 * Always prefer [`Span::with_meta_fn`] over [`Span::with_meta`] so the
 * `format!` allocation is skipped when diag is disabled.
 */
pub struct Span {
    category: Category,
    name: &'static str,
    start: Instant,
    meta: Option<String>,
    finished: bool,
}

impl Span {
    /**
     * Construct a span with the given category and name.
     *
     * Captures the current monotonic [`Instant`] as the start time.
     * The span emits its recorded event on [`Drop`] unless previously
     * finalized via [`Span::finish`]. Both `category` and `name` are
     * compile-time fixed: the call site cannot construct a span with a
     * dynamic name, which keeps the per-event allocation budget at zero
     * and lets aggregate keys be `(Category, &'static str)` pairs.
     */
    pub fn new(category: Category, name: &'static str) -> Self {
        Self {
            category,
            name,
            start: Instant::now(),
            meta: None,
            finished: false,
        }
    }

    /**
     * Attach metadata. The closure runs only when [`enabled`] returns
     * `true`, so the call site pays only an atomic load when
     * diagnostics are off. Sites that already own a built `String` can
     * pass `|| s` — the closure-over-owned-string costs nothing.
     */
    pub fn with_meta<F: FnOnce() -> String>(mut self, f: F) -> Self {
        if enabled() {
            self.meta = Some(f());
        }
        self
    }

    /**
     * Alias for [`Span::with_meta`] retained for migration. New code
     * should use [`Span::with_meta`] directly; the closure-deferred
     * shape is now the only one offered.
     */
    #[doc(hidden)]
    pub fn with_meta_fn<F: FnOnce() -> String>(self, f: F) -> Self {
        self.with_meta(f)
    }

    /**
     * Finalize the span eagerly rather than waiting for [`Drop`].
     *
     * Useful when emitting the event is correlated with a subsequent
     * span (e.g. boundary markers around a `match` arm).
     */
    pub fn finish(mut self) {
        self.flush();
        self.finished = true;
    }

    /**
     * Internal sink used by both [`Span::finish`] and [`Drop`]. Emits
     * the event via [`event`] when diag is active.
     */
    fn flush(&mut self) {
        if !enabled() {
            return;
        }
        event(
            self.category,
            self.name,
            self.start.elapsed(),
            self.meta.as_deref(),
        );
    }

    /**
     * Snapshot the elapsed duration so far without finalizing the span.
     *
     * The span continues to record and will emit its full duration on
     * drop. Useful for periodic "still alive" probes inside long-running
     * spans.
     */
    pub fn elapsed_ms(&self) -> f64 {
        self.start.elapsed().as_secs_f64() * 1000.0
    }
}

impl Drop for Span {
    /**
     * Emits the recorded event unless [`Span::finish`] was called.
     */
    fn drop(&mut self) {
        if !self.finished {
            self.flush();
        }
    }
}

/**
 * Discriminator for the in-flight counters and permit-holder registry.
 *
 * Variants:
 *
 * - `Pack` packument fetches (resolver phase).
 * - `Tar` tarball downloads (fetch phase).
 * - `Imp` CAS imports (materialize phase).
 * - `Link` linker materialize / symlink work.
 * - `Decode` gzip decompression / tar extraction (CPU bound).
 *
 * The discriminant order matches the index of [`SLOT_COUNT`]-sized
 * arrays used internally for fast O(1) lookup of per-slot mutex-guarded
 * state and atomic counters.
 */
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Slot {
    Pack,
    Tar,
    Imp,
    Link,
    Decode,
}

/// Number of [`Slot`] variants. Used to size the per-slot holder arrays.
pub const SLOT_COUNT: usize = 5;

impl Slot {
    /**
     * Wire-format identifier emitted as the `name` field of starvation
     * events for this slot. Used by [`attribute_wait`] when surfacing
     * permit-wait blame.
     */
    pub const fn wire_name(self) -> &'static str {
        match self {
            Slot::Pack => "packument_sem",
            Slot::Tar => "tarball_sem",
            Slot::Imp => "import_sem",
            Slot::Link => "link_sem",
            Slot::Decode => "decode_sem",
        }
    }

    /**
     * Short JSON key emitted in the periodic `sample.concurrency`
     * event's metadata. Mirrors the variant order in [`Slot`].
     */
    pub const fn sample_key(self) -> &'static str {
        match self {
            Slot::Pack => "pack",
            Slot::Tar => "tar",
            Slot::Imp => "imp",
            Slot::Link => "link",
            Slot::Decode => "decode",
        }
    }
}

/**
 * RAII guard that decrements its [`Slot`]'s in-flight counter on drop.
 *
 * Constructed via [`inflight`]. The 50 ms concurrency sampler reads the
 * counters and emits `cat=sample,name=concurrency` events with the
 * per-slot in-flight totals at sample time.
 */
pub struct InflightGuard {
    slot: Slot,
    /// Whether the matching counter increment ran. Set to `true` only when
    /// [`inflight`] observed an active recorder. The drop site checks
    /// this before decrementing so a guard constructed in the disabled
    /// path or before the recorder was initialized cannot wrap the
    /// counter into a near-`u64::MAX` underflow.
    incremented: bool,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        if !self.incremented {
            return;
        }
        let Some(r) = rec() else { return };
        slot_counter(r, self.slot).fetch_sub(1, Ordering::Relaxed);
    }
}

/// Resolve a slot's atomic counter inside the recorder. Centralized so the
/// `Slot` discriminator is mapped in one place; previous code duplicated
/// the match across `inflight()`, `Drop`, and `sample_concurrency`.
fn slot_counter(r: &Recorder, slot: Slot) -> &AtomicU64 {
    match slot {
        Slot::Pack => &r.in_flight_packuments,
        Slot::Tar => &r.in_flight_tarballs,
        Slot::Imp => &r.in_flight_imports,
        Slot::Link => &r.in_flight_links,
        Slot::Decode => &r.in_flight_decode,
    }
}

/**
 * Increment the in-flight counter for the given [`Slot`] and return a
 * guard that decrements on drop.
 *
 * Always returns a guard, even when diag is disabled, to keep call-site
 * shapes uniform. The guard is a no-op in that case — its `incremented`
 * flag stays `false` so the matching drop does not wrap the counter.
 */
pub fn inflight(slot: Slot) -> InflightGuard {
    let mut incremented = false;
    if let Some(r) = rec() {
        slot_counter(r, slot).fetch_add(1, Ordering::Relaxed);
        incremented = true;
    }
    InflightGuard { slot, incremented }
}

/**
 * Per-[`Slot`] registry of currently-held permits.
 *
 * The slot index of [`Slot`] is used directly to address one of the
 * five mutex-guarded vectors. Callers that hold a permit invoke
 * [`register_holder`] to add their package identifier; the returned
 * [`HolderGuard`] removes the entry on drop.
 *
 * When a waiter on the same slot wants to attribute its wait time, it
 * calls [`attribute_wait`] which snapshots the current holders and
 * emits a `starvation` event naming them.
 */
static HOLDERS: OnceLock<[Mutex<Vec<Arc<str>>>; SLOT_COUNT]> = OnceLock::new();

/**
 * Resolve the per-slot mutex-guarded holder vector, lazy-initializing
 * the [`HOLDERS`] array on first access. Each entry is an [`Arc<str>`]
 * so `clone` is a refcount bump rather than a heap allocation.
 */
fn holders_for(slot: Slot) -> &'static Mutex<Vec<Arc<str>>> {
    let arr = HOLDERS.get_or_init(|| {
        [
            Mutex::new(Vec::new()),
            Mutex::new(Vec::new()),
            Mutex::new(Vec::new()),
            Mutex::new(Vec::new()),
            Mutex::new(Vec::new()),
        ]
    });
    &arr[slot as usize]
}

/**
 * RAII guard that removes a registered holder on drop.
 *
 * Constructed by [`register_holder`]. Removes its entry from the
 * appropriate per-[`Slot`] vector when scope exits. The guard owns
 * its [`Arc<str>`] so `Drop` matches by cheap pointer equality on the
 * common path.
 */
pub struct HolderGuard {
    slot: Slot,
    pkg_id: Arc<str>,
    /// Whether the matching `register_holder` push ran. Mirrors
    /// [`InflightGuard::incremented`]: the drop only attempts removal
    /// when the registration actually happened, so a guard built in the
    /// disabled path or with a poisoned mutex during init does not
    /// silently swap-remove an unrelated holder.
    registered: bool,
}

impl Drop for HolderGuard {
    fn drop(&mut self) {
        if !self.registered {
            return;
        }
        let Ok(mut g) = holders_for(self.slot).lock() else {
            return;
        };
        // Pointer-equality first (the cheap, expected case), fall back
        // to value compare for the rare case where the same `&str`
        // arrived via different `Arc<str>` allocations.
        let pos = g
            .iter()
            .position(|p| Arc::ptr_eq(p, &self.pkg_id))
            .or_else(|| g.iter().position(|p| **p == *self.pkg_id));
        if let Some(pos) = pos {
            g.swap_remove(pos);
        }
    }
}

/**
 * Register `pkg_id` as a current holder of a permit on `slot`.
 *
 * The returned [`HolderGuard`] removes the entry on drop. Always returns
 * a guard, even when diag is disabled, so call sites can use a uniform
 * `let _holder = register_holder(...)` shape regardless of mode.
 *
 * The registry stores the package identifier as [`Arc<str>`]. Call
 * sites that already hold an `Arc<str>` can pass it directly; the
 * `impl Into<Arc<str>>` bound also accepts `String` (one allocation
 * to interleave the data into the Arc) and `&str` (same).
 */
pub fn register_holder(slot: Slot, pkg_id: impl AsRef<str>) -> HolderGuard {
    let pkg_id: Arc<str> = Arc::from(pkg_id.as_ref());
    let mut registered = false;
    if rec().is_some()
        && let Ok(mut g) = holders_for(slot).lock()
    {
        g.push(Arc::clone(&pkg_id));
        registered = true;
    }
    HolderGuard {
        slot,
        pkg_id,
        registered,
    }
}

/**
 * Record a starvation event for `waiter` blocking on `slot` for `wait`.
 *
 * Only emits when `wait` is at least 50 ms; shorter waits are
 * statistical noise. The emitted event names every package currently
 * registered as a holder via [`register_holder`], giving the analyzer
 * a list of plausible blockers to attribute the wait to.
 */
pub fn attribute_wait(slot: Slot, waiter: &str, wait: Duration) {
    if rec().is_none() {
        return;
    }
    if wait.as_millis() < 50 {
        return;
    }
    // Snapshot the current holders by cloning Arc handles inside the
    // lock — no string allocations under the mutex.
    let names: Vec<Arc<str>> = {
        let g = holders_for(slot).lock().unwrap_or_else(|e| e.into_inner());
        g.clone()
    };
    let holders = if names.is_empty() {
        "<none>".to_string()
    } else {
        let mut s = String::with_capacity(names.len() * 32);
        for (i, n) in names.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(n);
        }
        // Walk back to a UTF-8 char boundary before truncating so a
        // multi-byte holder name that straddles byte 200 (e.g. CJK or
        // emoji in a scoped registry) does not panic. Cap is 200 bytes
        // of payload + 3 bytes for the trailing ellipsis.
        if s.len() > 200 {
            let mut end = 200;
            while end > 0 && !s.is_char_boundary(end) {
                end -= 1;
            }
            s.truncate(end);
            s.push('…');
        }
        s
    };
    event(
        Category::Starvation,
        slot.wire_name(),
        wait,
        Some(&format!(
            r#"{{"waiter":{},"holders":{}}}"#,
            jstr(waiter),
            jstr(&holders)
        )),
    );
}

/// Track an mpsc channel's fill ratio. Register at construction with `register_channel`,
/// then a background sampler reads `Sender::capacity()` every 100ms and emits events.
static CHANNELS: OnceLock<Mutex<Vec<ChannelTracker>>> = OnceLock::new();

struct ChannelTracker {
    name: &'static str,
    capacity: usize,
    sender_capacity_fn: Box<dyn Fn() -> usize + Send + Sync>,
}

pub fn register_channel<T: Send + Sync + 'static>(
    name: &'static str,
    sender: &tokio::sync::mpsc::Sender<T>,
    capacity: usize,
) {
    if rec().is_none() {
        return;
    }
    let weak = sender.downgrade();
    let tracker = ChannelTracker {
        name,
        capacity,
        sender_capacity_fn: Box::new(move || weak.upgrade().map(|s| s.capacity()).unwrap_or(0)),
    };
    let lock = CHANNELS.get_or_init(|| Mutex::new(Vec::new()));
    lock.lock().unwrap_or_else(|e| e.into_inner()).push(tracker);
}

pub fn sample_channels() {
    let Some(lock) = CHANNELS.get() else { return };
    let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    for t in guard.iter() {
        let remaining = (t.sender_capacity_fn)();
        let used = t.capacity.saturating_sub(remaining);
        let fill = (used as f64 / t.capacity.max(1) as f64) * 100.0;
        instant(
            Category::Channel,
            t.name,
            Some(&format!(
                r#"{{"used":{},"cap":{},"fill_pct":{:.1}}}"#,
                used, t.capacity, fill
            )),
        );
    }
}

/// All slots in declaration order. Iterating this is the canonical way
/// to fan a per-slot operation across every counter without writing
/// out the discriminator literals.
const ALL_SLOTS: [Slot; SLOT_COUNT] = [Slot::Pack, Slot::Tar, Slot::Imp, Slot::Link, Slot::Decode];

pub fn sample_concurrency() {
    let Some(r) = rec() else { return };
    use std::fmt::Write;
    let mut meta = String::with_capacity(80);
    meta.push('{');
    for (idx, slot) in ALL_SLOTS.iter().enumerate() {
        if idx > 0 {
            meta.push(',');
        }
        let value = slot_counter(r, *slot).load(Ordering::Relaxed);
        let _ = write!(meta, r#""{}":{}"#, slot.sample_key(), value);
    }
    meta.push('}');
    instant(Category::Sample, "concurrency", Some(&meta));
}

pub fn spawn_concurrency_sampler() {
    if !enabled() {
        return;
    }
    tokio::spawn(async {
        let mut iv = tokio::time::interval(Duration::from_millis(50));
        iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut tick = 0u32;
        loop {
            iv.tick().await;
            sample_concurrency();
            // Channel sampler runs at 1/4 the rate (200ms)
            tick = tick.wrapping_add(1);
            if tick.is_multiple_of(4) {
                sample_channels();
            }
        }
    });
}

pub fn flush() {
    if let Some(r) = rec() {
        if let Some(file) = &r.file {
            let _ = file.lock().unwrap_or_else(|e| e.into_inner()).flush();
        }
        let n = r.event_count.load(Ordering::Relaxed);
        let total_ms = r.start.elapsed().as_secs_f64() * 1000.0;

        if r.summary {
            print_summary(r, total_ms);
        }

        if r.track_events {
            let evs = r.events.lock().unwrap_or_else(|e| e.into_inner()).clone();
            print_critical_path(&evs, total_ms);
            print_starvation(&evs, total_ms);
            print_what_if(&evs, total_ms);
            print_pkg_lifecycle(&evs, total_ms);
        }

        if r.print_stderr {
            eprintln!(
                "[diag] flushed {} events over {:.1}ms ({:.0}/s)",
                n,
                total_ms,
                (n as f64 / total_ms.max(1.0)) * 1000.0
            );
        }
    }
}

/**
 * Derive a canonical package identifier from inline JSON metadata for
 * cross-event correlation in the per-package lifecycle analyzer.
 *
 * The identifier is always just the `name` field, never `name@version`.
 * Different event categories embed different fields — `task_wait_packument`
 * carries only `name`, while `tarball` carries both `name` and `version`.
 * If the lifecycle analyzer keyed on `name@version` it would treat the
 * resolver wait and the tarball fetch as belonging to two different
 * packages and split the timeline. Keying on `name` alone keeps the
 * lifecycle whole at the cost of conflating multiple resolved versions of
 * the same name (which is rare in a single install).
 *
 * Returns `None` when no `name` field is present (e.g. starvation events
 * which carry `waiter` and `holders` instead).
 */
fn extract_pkg_id(meta: &str) -> Option<String> {
    extract_field(meta, "name")
}

/**
 * Read a JSON string field by substring scan and unescape the result.
 *
 * Locates the field as `"<field>":"`, then walks bytes until an
 * unescaped closing `"`. A `"` is considered escaped when an odd
 * number of `\` precedes it; that handles the `\"` produced by
 * [`jstr`] and the literal-`\\` boundary case (`\\"` is `\` followed
 * by an unescaped quote).
 *
 * The returned string has the standard JSON escape sequences
 * (`\"`, `\\`, `\n`, `\r`, `\t`, `\u00XX`) un-escaped so that the
 * value matches what a real JSON parser would produce.
 */
fn extract_field(meta: &str, field: &str) -> Option<String> {
    let needle = format!("\"{field}\":\"");
    let i = meta.find(&needle)?;
    let after = &meta[i + needle.len()..];
    let bytes = after.as_bytes();
    let mut idx = 0usize;
    while idx < bytes.len() {
        if bytes[idx] == b'"' {
            // Count consecutive `\` immediately before this quote; an
            // odd count means the quote is itself escaped (`\"`), an
            // even count (including zero) means the quote terminates
            // the field.
            let mut bs = 0usize;
            let mut j = idx;
            while j > 0 && bytes[j - 1] == b'\\' {
                bs += 1;
                j -= 1;
            }
            if bs.is_multiple_of(2) {
                return Some(unescape_json_str(&after[..idx]));
            }
        }
        idx += 1;
    }
    None
}

/**
 * Reverse of [`jstr`]. Decodes the standard JSON escape sequences this
 * crate emits: `\"`, `\\`, `\n`, `\r`, `\t`, and `\u00XX` for the
 * control range. Bytes outside those forms pass through unchanged.
 */
fn unescape_json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('u') => {
                // Read exactly four hex digits; on any malformed input
                // emit the raw `\u` and continue rather than panic.
                let mut hex = String::with_capacity(4);
                for _ in 0..4 {
                    if let Some(h) = chars.next() {
                        hex.push(h);
                    }
                }
                if let Ok(code) = u32::from_str_radix(&hex, 16)
                    && let Some(decoded) = char::from_u32(code)
                {
                    out.push(decoded);
                } else {
                    out.push('\\');
                    out.push('u');
                    out.push_str(&hex);
                }
            }
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/**
 * Whether `e` is an envelope/wrapper span that wraps the entire install
 * and would dominate any longest-chain analysis if not filtered out.
 *
 * `install.total`, every `install_phase.*` row, and a small set of
 * named install-pipeline boundaries fall into this category. The
 * critical-path and what-if analyzers operate on leaf events only;
 * keying off this single predicate keeps both in sync.
 */
fn is_envelope(e: &EventRec) -> bool {
    matches!(e.cat, Category::Install | Category::InstallPhase)
        || e.name == "phase_resolve"
        || e.name == "phase_fetch_await"
        || e.name == "phase_materialize_await"
}

/**
 * Compute the longest-duration chain of strictly non-overlapping events
 * in `sorted` (which must already be sorted by `end_ms` ascending).
 *
 * Implements weighted-interval-scheduling DP in O(n log n): for each
 * event at index `i`, binary-search the latest predecessor `j` with
 * `end_ms[j] <= start_ms[i]`, then choose between extending the chain
 * through `j` (`take`) or skipping `i` (`skip`).
 *
 * Returns `(chain, total)` where `chain` is the indices of the
 * selected events and `total` is the summed duration.
 */
fn longest_chain(sorted: &[&EventRec]) -> (Vec<usize>, f64) {
    let n = sorted.len();
    if n == 0 {
        return (Vec::new(), 0.0);
    }
    let ends: Vec<f64> = sorted.iter().map(|e| e.end_ms).collect();
    let mut p: Vec<Option<usize>> = vec![None; n];
    for i in 0..n {
        let s = sorted[i].start_ms;
        let mut lo = 0i64;
        let mut hi = i as i64 - 1;
        let mut found: Option<usize> = None;
        while lo <= hi {
            let mid = ((lo + hi) / 2) as usize;
            if ends[mid] <= s {
                found = Some(mid);
                lo = mid as i64 + 1;
            } else {
                hi = mid as i64 - 1;
            }
        }
        p[i] = found;
    }
    let mut dp: Vec<f64> = vec![0.0; n];
    let mut include: Vec<bool> = vec![false; n];
    for i in 0..n {
        let dur_i = sorted[i].end_ms - sorted[i].start_ms;
        let take = dur_i + p[i].map_or(0.0, |j| dp[j]);
        let skip = if i == 0 { 0.0 } else { dp[i - 1] };
        if take >= skip {
            dp[i] = take;
            include[i] = true;
        } else {
            dp[i] = skip;
        }
    }
    let total = dp[n - 1];
    let mut chain: Vec<usize> = Vec::new();
    let mut i: i64 = n as i64 - 1;
    while i >= 0 {
        let idx = i as usize;
        if include[idx] {
            chain.push(idx);
            i = p[idx].map(|j| j as i64).unwrap_or(-1);
        } else {
            i -= 1;
        }
    }
    chain.reverse();
    (chain, total)
}

fn print_critical_path(events: &[EventRec], total_ms: f64) {
    if events.is_empty() {
        return;
    }
    let mut sorted: Vec<&EventRec> = events.iter().filter(|e| !is_envelope(e)).collect();
    if sorted.is_empty() {
        return;
    }
    sorted.sort_by(|a, b| {
        a.end_ms
            .partial_cmp(&b.end_ms)
            .unwrap_or(CmpOrdering::Equal)
    });
    let (chain, critical_total) = longest_chain(&sorted);

    eprintln!();
    eprintln!(
        "critical path {:.1}ms ({:.0}% of {:.1}ms wall, {} spans)",
        critical_total,
        (critical_total / total_ms.max(1.0)) * 100.0,
        total_ms,
        chain.len()
    );
    eprintln!(
        "{:>4} {:>9} {:>9} {:<14} {:<28} pkg",
        "#", "start", "dur", "cat", "name"
    );
    // Collapse trivial spans (< 1 ms) into a single summary line so the
    // first useful entry of the chain (typically a multi-second packument
    // or tarball wait) appears at or near the top of the rendered list.
    let trivial_threshold = 1.0;
    let mut printed = 0usize;
    let mut trivial_run = 0usize;
    let mut trivial_run_dur = 0.0f64;
    let mut chain_iter = chain.iter().peekable();
    while let Some(&idx) = chain_iter.next() {
        let e = &sorted[idx];
        let dur = e.end_ms - e.start_ms;
        if dur < trivial_threshold {
            trivial_run += 1;
            trivial_run_dur += dur;
            // Flush the run when we hit a non-trivial span or end of chain.
            let next_trivial = chain_iter
                .peek()
                .map(|&&i| (sorted[i].end_ms - sorted[i].start_ms) < trivial_threshold)
                .unwrap_or(false);
            if !next_trivial {
                eprintln!(
                    "  (collapsed {} sub-1ms spans, {:.1}ms total)",
                    trivial_run, trivial_run_dur
                );
                trivial_run = 0;
                trivial_run_dur = 0.0;
            }
            continue;
        }
        if printed >= 40 {
            break;
        }
        printed += 1;
        let pkg = e.pkg_id.as_deref().unwrap_or("");
        eprintln!(
            "{:>4} {:>8.0}ms {:>8.1}ms {:<14} {:<28} {}",
            printed,
            e.start_ms,
            dur,
            truncate(e.cat.wire(), 14),
            truncate(e.name, 28),
            truncate(pkg, 50)
        );
    }

    // Slack analysis: top 10 fattest spans NOT on critical path.
    let on_path: std::collections::HashSet<usize> = chain.iter().copied().collect();
    let mut off_path: Vec<(usize, f64)> = (0..sorted.len())
        .filter(|i| !on_path.contains(i))
        .map(|i| (i, sorted[i].end_ms - sorted[i].start_ms))
        .collect();
    off_path.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(CmpOrdering::Equal));

    eprintln!();
    eprintln!("top off-critical (already-overlapped, recoverable=0):");
    for (idx, dur) in off_path.iter().take(10) {
        let e = &sorted[*idx];
        let pkg = e.pkg_id.as_deref().unwrap_or("");
        eprintln!(
            "  {:>8.1}ms {:<14} {:<28} {}",
            dur,
            truncate(e.cat.wire(), 14),
            truncate(e.name, 28),
            truncate(pkg, 50)
        );
    }
}

fn print_starvation(events: &[EventRec], _total_ms: f64) {
    use std::collections::HashMap;
    let starv: Vec<&EventRec> = events
        .iter()
        .filter(|e| e.cat == Category::Starvation)
        .collect();
    if starv.is_empty() {
        return;
    }
    // Group by sem name. For each, top blamers (holder pkg names appearing in meta).
    let mut by_sem: HashMap<&str, Vec<&EventRec>> = HashMap::new();
    for e in &starv {
        by_sem.entry(e.name).or_default().push(e);
    }
    eprintln!();
    eprintln!("starvation events ({} total):", starv.len());
    eprintln!(
        "{:<16} {:>6} {:>9} {:>9} top_blamers",
        "sem", "n", "sum_ms", "max_ms"
    );
    let mut keys: Vec<&&str> = by_sem.keys().collect();
    keys.sort();
    for k in keys {
        let evs = &by_sem[k];
        let sum: f64 = evs.iter().map(|e| e.end_ms - e.start_ms).sum();
        let max: f64 = evs
            .iter()
            .map(|e| e.end_ms - e.start_ms)
            .fold(0.0_f64, f64::max);
        // Tally holder names from each starvation event's meta.holders (comma-separated).
        let mut blame_count: HashMap<String, u32> = HashMap::new();
        for e in evs {
            let Some(m) = &e.meta else { continue };
            let Some(holders_field) = extract_field(m, "holders") else {
                continue;
            };
            for h in holders_field.split(',') {
                let h = h.trim();
                if h.is_empty() || h == "<none>" {
                    continue;
                }
                *blame_count.entry(h.to_string()).or_insert(0) += 1;
            }
        }
        let mut blamers: Vec<(String, u32)> = blame_count.into_iter().collect();
        blamers.sort_by_key(|b| std::cmp::Reverse(b.1));
        let top: String = blamers
            .iter()
            .take(3)
            .map(|(n, c)| format!("{}({})", truncate(n, 30), c))
            .collect::<Vec<_>>()
            .join(" ");
        eprintln!(
            "{:<16} {:>6} {:>7.0}ms {:>7.0}ms {}",
            k,
            evs.len(),
            sum,
            max,
            top
        );
    }
}

/// What-if causal simulator. For each high-impact (cat,name) bucket, compute how much
/// wall time would drop if that bucket were 25%, 50%, 100% faster — by simulating the
/// span-duration reduction over the critical path only. Off-critical-path spans have
/// zero recoverable wall impact (they were already overlapped).
fn print_what_if(events: &[EventRec], total_ms: f64) {
    use std::collections::HashMap;
    let leaf_events: Vec<&EventRec> = events.iter().filter(|e| !is_envelope(e)).collect();
    if leaf_events.is_empty() {
        return;
    }

    // Compute on-critical-path duration per (cat,name): for each event on the critical
    // path, attribute its duration to the (cat,name) bucket.
    let mut sorted = leaf_events.clone();
    sorted.sort_by(|a, b| {
        a.end_ms
            .partial_cmp(&b.end_ms)
            .unwrap_or(CmpOrdering::Equal)
    });
    let (on_path, critical_total) = longest_chain(&sorted);

    // Sum on-critical-path duration per (cat,name). Keyed by the typed
    // [`AggKey`] so the hash is a pointer compare rather than a string
    // compare; with hundreds of events each, this matters.
    let mut bucket_critical: HashMap<AggKey, f64> = HashMap::new();
    for &idx in &on_path {
        let e = sorted[idx];
        let dur = e.end_ms - e.start_ms;
        *bucket_critical.entry((e.cat, e.name)).or_insert(0.0) += dur;
    }

    // Total off-critical wall per (cat,name); these are 0 recoverable.
    let mut bucket_total: HashMap<AggKey, f64> = HashMap::new();
    for e in &leaf_events {
        *bucket_total.entry((e.cat, e.name)).or_insert(0.0) += e.end_ms - e.start_ms;
    }

    // Sort by on-critical-path contribution
    let mut rows: Vec<(AggKey, f64, f64)> = bucket_critical
        .into_iter()
        .map(|(k, on)| {
            let total_b = bucket_total.get(&k).copied().unwrap_or(0.0);
            (k, on, total_b)
        })
        .collect();
    rows.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(CmpOrdering::Equal));

    eprintln!();
    eprintln!(
        "what-if speedup simulator (critical path total {:.0}ms / wall {:.0}ms)",
        critical_total, total_ms
    );
    eprintln!(
        "{:<14} {:<32} {:>10} {:>10} {:>10} {:>10} {:>10} {:>9}",
        "cat", "name", "on_path", "off_path", "−25%", "−50%", "−100%", "%recoverable"
    );
    for ((cat, name), on, total_b) in rows.iter().take(15) {
        let off = total_b - on;
        let s25 = on * 0.25;
        let s50 = on * 0.50;
        let s100 = on * 1.00;
        let pct_rec = (on / total_ms.max(1.0)) * 100.0;
        eprintln!(
            "{:<14} {:<32} {:>8.0}ms {:>8.0}ms {:>+8.0}ms {:>+8.0}ms {:>+8.0}ms {:>8.1}%",
            truncate(cat.wire(), 14),
            truncate(name, 32),
            on,
            off,
            -s25,
            -s50,
            -s100,
            pct_rec
        );
    }
}

fn print_pkg_lifecycle(events: &[EventRec], total_ms: f64) {
    use std::collections::BTreeMap;
    // Group events by pkg_id, collect (cat, name, dur, start) per pkg.
    let mut by_pkg: BTreeMap<String, Vec<&EventRec>> = BTreeMap::new();
    for e in events {
        if let Some(pkg) = &e.pkg_id {
            by_pkg.entry(pkg.clone()).or_default().push(e);
        }
    }
    if by_pkg.is_empty() {
        return;
    }
    // Score each pkg: total wall (max end - min start). Sort desc.
    let mut scored: Vec<(String, f64, f64, f64, usize)> = by_pkg
        .iter()
        .map(|(pkg, evs)| {
            let min_start = evs.iter().map(|e| e.start_ms).fold(f64::INFINITY, f64::min);
            let max_end = evs.iter().map(|e| e.end_ms).fold(0.0_f64, f64::max);
            let sum_dur: f64 = evs.iter().map(|e| e.end_ms - e.start_ms).sum();
            (pkg.clone(), min_start, max_end, sum_dur, evs.len())
        })
        .collect();
    scored.sort_by(|a, b| {
        (b.2 - b.1)
            .partial_cmp(&(a.2 - a.1))
            .unwrap_or(CmpOrdering::Equal)
    });

    eprintln!();
    eprintln!(
        "per-package lifecycle (top 20 by wall span, {} pkgs total)",
        scored.len()
    );
    eprintln!(
        "{:<48} {:>9} {:>9} {:>8} {:>5}",
        "pkg", "first", "last", "span", "evts"
    );
    for (pkg, min_s, max_e, _sum, n) in scored.iter().take(20) {
        let span = max_e - min_s;
        let pct = (span / total_ms.max(1.0)) * 100.0;
        eprintln!(
            "{:<48} {:>7.0}ms {:>7.0}ms {:>6.0}ms {:>5} {:>4.1}%",
            truncate(pkg, 48),
            min_s,
            max_e,
            span,
            n,
            pct
        );
    }
}

/**
 * Truncate `s` to at most `n` bytes for tabular display, taking care
 * not to slice through the middle of a UTF-8 code-point sequence.
 *
 * The result is suffixed with the ellipsis character `…` (3 bytes)
 * whenever truncation occurred. Falls back to the empty string if
 * the requested cap is too small to fit even the ellipsis.
 */
pub fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let cap = n.saturating_sub(1);
    if cap == 0 {
        return String::new();
    }
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

fn print_summary(r: &Recorder, total_ms: f64) {
    let agg = r.aggregates.lock().unwrap_or_else(|e| e.into_inner());
    let mut rows: Vec<(AggKey, AggVal)> = agg.iter().map(|(k, v)| (*k, *v)).collect();
    drop(agg);
    rows.sort_by_key(|b| std::cmp::Reverse(b.1.sum_ns));

    eprintln!("diag total {:.1}ms", total_ms);
    eprintln!(
        "{:<10} {:<32} {:>6} {:>9} {:>9} {:>9} {:>7}",
        "cat", "name", "n", "sum_ms", "mean_ms", "max_ms", "%wall"
    );
    for ((cat, name), stats) in rows.iter().take(40) {
        let sum_ms = (stats.sum_ns as f64) / 1_000_000.0;
        let mean_ms = sum_ms / (stats.count as f64);
        let max_ms = (stats.max_ns as f64) / 1_000_000.0;
        let pct = (sum_ms / total_ms.max(1.0)) * 100.0;
        eprintln!(
            "{:<10} {:<32} {:>6} {:>9.1} {:>9.2} {:>9.1} {:>6.1}%",
            cat.wire(),
            name,
            stats.count,
            sum_ms,
            mean_ms,
            max_ms,
            pct
        );
    }
}

/// Helper: time a synchronous expression and return (value, duration).
#[inline]
pub fn time_sync<T>(category: Category, name: &'static str, f: impl FnOnce() -> T) -> T {
    if !enabled() {
        return f();
    }
    let start = Instant::now();
    let v = f();
    event(category, name, start.elapsed(), None);
    v
}

/// Escape a string for safe inclusion in a JSON value.
pub fn jstr(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/**
 * Construct a [`Span`] with optional `key = value` metadata pairs.
 *
 * The metadata `format!` runs only when [`enabled`] returns `true`, so
 * call sites pay only the atomic-load fast path when diagnostics are
 * disabled. Values are escaped via [`jstr`].
 *
 * # Examples
 *
 * ```ignore
 * let _diag = diag_span!("registry", "fetch_packument", name = pkg_name);
 * let _diag = diag_span!("fetch", "tarball", name = name, version = ver);
 * ```
 */
#[macro_export]
macro_rules! diag_span {
    ($cat:expr, $name:expr) => {
        $crate::diag::Span::new($cat, $name)
    };
    ($cat:expr, $name:expr, $($k:ident = $v:expr),+ $(,)?) => {{
        $crate::diag::Span::new($cat, $name).with_meta(|| {
            format!(
                "{{{}}}",
                [$(format!("\"{}\":{}", stringify!($k), $crate::diag::jstr(&$v.to_string()))),+].join(",")
            )
        })
    }};
}

/**
 * Emit an instantaneous marker with optional `key = value` metadata.
 *
 * The metadata `format!` runs only when [`enabled`] returns `true`.
 * Values are escaped via [`jstr`].
 */
#[macro_export]
macro_rules! diag_instant {
    ($cat:expr, $name:expr) => {
        $crate::diag::instant($cat, $name, None)
    };
    ($cat:expr, $name:expr, $($k:ident = $v:expr),+ $(,)?) => {{
        $crate::diag::instant_lazy($cat, $name, || {
            format!(
                "{{{}}}",
                [$(format!("\"{}\":{}", stringify!($k), $crate::diag::jstr(&$v.to_string()))),+].join(",")
            )
        });
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `jstr` must round-trip valid JSON for every category of input
    /// the meta builder hands it: ASCII, control chars, embedded
    /// quotes, multi-byte UTF-8.
    #[test]
    fn jstr_escapes_all_categories() {
        assert_eq!(jstr("hi"), "\"hi\"");
        assert_eq!(jstr("a\"b"), "\"a\\\"b\"");
        assert_eq!(jstr("a\\b"), "\"a\\\\b\"");
        assert_eq!(jstr("a\nb\tc\rd"), "\"a\\nb\\tc\\rd\"");
        assert_eq!(jstr("\x01"), "\"\\u0001\"");
        assert_eq!(jstr("café"), "\"café\"");
        assert_eq!(jstr("日本"), "\"日本\"");
    }

    /// `truncate` must never panic on multi-byte boundaries — CJK and
    /// scoped npm package names land mid-codepoint at common widths.
    #[test]
    fn truncate_is_utf8_boundary_safe() {
        let s = "日本語パッケージ";
        for n in 1..=s.len() + 2 {
            let _ = truncate(s, n);
        }
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello", 4), "hel…");
    }

    /// `extract_pkg_id` returns the canonical name (without `@version`)
    /// so the lifecycle analyzer correlates events of one package
    /// across categories that disagree on whether to include version.
    #[test]
    fn extract_pkg_id_returns_name_only() {
        let m = r#"{"name":"lodash","version":"4.17.21"}"#;
        assert_eq!(extract_pkg_id(m).as_deref(), Some("lodash"));
        let m2 = r#"{"name":"lodash"}"#;
        assert_eq!(extract_pkg_id(m2).as_deref(), Some("lodash"));
        let m3 = r#"{"version":"1.0.0"}"#;
        assert!(extract_pkg_id(m3).is_none());
    }

    /// Exercise the longest-chain DP against a brute-force oracle on
    /// small inputs. Catches off-by-one in the `p[i]` binary search,
    /// the take/skip tie-break, and reconstruction edge cases.
    #[test]
    fn longest_chain_matches_brute_force() {
        fn brute(events: &[(f64, f64)]) -> f64 {
            let n = events.len();
            let mut best = 0.0_f64;
            for mask in 0u32..(1u32 << n) {
                let mut picked: Vec<(f64, f64)> = (0..n)
                    .filter(|i| mask & (1 << i) != 0)
                    .map(|i| events[i])
                    .collect();
                picked.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
                let ok = picked.windows(2).all(|w| w[0].1 <= w[1].0);
                if !ok {
                    continue;
                }
                let total: f64 = picked.iter().map(|(s, e)| e - s).sum();
                if total > best {
                    best = total;
                }
            }
            best
        }
        let cases: &[&[(f64, f64)]] = &[
            &[],
            &[(0.0, 10.0)],
            &[(0.0, 10.0), (5.0, 15.0)],
            &[(0.0, 10.0), (10.0, 20.0)],
            &[(0.0, 5.0), (3.0, 8.0), (7.0, 12.0)],
            &[(0.0, 100.0), (10.0, 20.0), (30.0, 40.0)],
        ];
        for case in cases {
            let evs: Vec<EventRec> = case
                .iter()
                .map(|(s, e)| EventRec {
                    cat: Category::Resolver,
                    name: "y",
                    start_ms: *s,
                    end_ms: *e,
                    pkg_id: None,
                    meta: None,
                })
                .collect();
            let mut sorted: Vec<&EventRec> = evs.iter().collect();
            sorted.sort_by(|a, b| {
                a.end_ms
                    .partial_cmp(&b.end_ms)
                    .unwrap_or(CmpOrdering::Equal)
            });
            let (_, total) = longest_chain(&sorted);
            let bf = brute(case);
            assert!(
                (total - bf).abs() < 1e-6,
                "case {case:?}: dp={total} bf={bf}"
            );
        }
    }

    /// `Slot::wire_name` and `sample_key` must agree with the variant
    /// order used internally by `slot_counter` / array indexing.
    #[test]
    fn slot_wire_names_are_distinct_and_stable() {
        let names: Vec<&'static str> = ALL_SLOTS.iter().map(|s| s.wire_name()).collect();
        assert_eq!(
            names,
            vec![
                "packument_sem",
                "tarball_sem",
                "import_sem",
                "link_sem",
                "decode_sem"
            ]
        );
        let keys: Vec<&'static str> = ALL_SLOTS.iter().map(|s| s.sample_key()).collect();
        assert_eq!(keys, vec!["pack", "tar", "imp", "link", "decode"]);
    }

    /// `extract_field` must skip past `\"` produced by [`jstr`]; the
    /// previous naive parser stopped at the first `"` and silently
    /// truncated the value.
    #[test]
    fn extract_field_handles_escaped_quotes() {
        let meta = r#"{"name":"foo\"bar","version":"1.0"}"#;
        assert_eq!(extract_field(meta, "name").as_deref(), Some(r#"foo"bar"#));
        assert_eq!(extract_field(meta, "version").as_deref(), Some("1.0"));
        // Round-trip through jstr/extract preserves an arbitrary value.
        let original = "weird \"name\" with \\ slash and \n newline";
        let wire = format!("{{\"name\":{}}}", jstr(original));
        assert_eq!(extract_field(&wire, "name").as_deref(), Some(original));
    }

    /// `validate_diag_path` rejects Windows-style UNC on every platform
    /// but accepts POSIX double-slash prefixes on non-Windows targets.
    #[test]
    fn validate_diag_path_rejects_unc_only() {
        use std::path::Path;
        assert!(validate_diag_path(Path::new(r"\\srv\share\f.jsonl")).is_err());
        assert!(validate_diag_path(Path::new("./local.jsonl")).is_ok());
        // POSIX `//foo` should be accepted on Unix; on Windows it is
        // rejected as the MSYS/Git-Bash UNC wire form.
        let r = validate_diag_path(Path::new("//tmp/foo.jsonl"));
        if cfg!(windows) {
            assert!(r.is_err());
        } else {
            assert!(r.is_ok());
        }
    }
}
