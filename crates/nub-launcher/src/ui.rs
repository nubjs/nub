//! The launcher's first-run terminal UI — a centered box with a spinner.
//!
//! A compiled binary's cold start spends seconds on invisible work (decompressing
//! the embedded Node, or provisioning one) before the app writes its first byte.
//! This fills that gap, and it lives in the launcher rather than in JS because the
//! whole point is to be on screen BEFORE Node exists.
//!
//! Three presentations, chosen once at startup and never revisited:
//!   - **Silent** — stderr is not a terminal, or the artifact was built with
//!     `--no-install-message`. Not one byte is written, so piped output and CI
//!     logs are byte-identical to a run with no UI at all.
//!   - **Line** — a terminal we can't safely place a box in (size unknown, too
//!     small, `TERM=dumb`, Windows where we can't query the console). One plain
//!     line, the pre-box behavior.
//!   - **Boxed** — the full treatment on the alternate screen buffer.
//!
//! The alternate screen is what makes viewport centering possible AND what makes
//! teardown exact: leaving it restores the user's scrollback byte-for-byte, so
//! the app's own first line lands on a terminal that looks untouched. The cost is
//! that an abnormal exit would strand the terminal there, so the guard below
//! re-arms on every fatal signal — including SIGABRT, which is how this binary
//! dies from a panic (`panic = "abort"`).

use std::cell::{Cell, RefCell};
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Spinner period. 12.5 fps reads as smooth and costs nothing.
const FRAME: Duration = Duration::from_millis(80);
/// Work that finishes faster than this never paints, so a warm-ish cold start
/// doesn't flash a box for one frame.
const FIRST_PAINT: Duration = Duration::from_millis(120);

/// Horizontal padding between the border and the content column.
const PAD: usize = 3;
/// Content column floor — narrower than this and the progress bar is a joke.
const MIN_CONTENT: usize = 16;
/// Border, blank, message, progress, blank, border.
const BOX_H: usize = 6;
/// `MIN_CONTENT` + padding + borders + a 2-column margin each side.
const MIN_COLS: u16 = (MIN_CONTENT + 2 * PAD + 2 + 4) as u16;
/// The box plus a row of breathing room above and below.
const MIN_ROWS: u16 = (BOX_H + 2) as u16;

const ENTER: &str = "\x1b[?1049h\x1b[?25l\x1b[2J";
const LEAVE: &str = "\x1b[?25h\x1b[?1049l";

// ---- presentation decision ----------------------------------------------------

/// What the terminal will accept. Split out from [`FirstRun::new`] so the
/// decision below is a pure function the tests can drive.
pub(crate) struct Caps {
    pub tty: bool,
    pub size: Option<(u16, u16)>,
    pub unicode: bool,
    pub color: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Plan {
    Silent,
    Line(String),
    Boxed(Layout),
}

/// Box geometry, resolved against one viewport size. Recomputed on resize.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Layout {
    text: String,
    content_w: usize,
    box_w: usize,
    /// 0-based cell of the top-left corner.
    left: usize,
    top: usize,
    unicode: bool,
    color: bool,
}

pub(crate) fn plan(text: Option<&str>, caps: &Caps) -> Plan {
    let Some(raw) = text else {
        return Plan::Silent;
    };
    let text = sanitize(raw);
    if text.is_empty() || !caps.tty {
        return Plan::Silent;
    }
    let Some((cols, rows)) = caps.size else {
        return Plan::Line(text);
    };
    if cols < MIN_COLS || rows < MIN_ROWS {
        return Plan::Line(text);
    }
    Plan::Boxed(Layout::compute(text, cols, rows, caps.unicode, caps.color))
}

impl Layout {
    fn compute(text: String, cols: u16, rows: u16, unicode: bool, color: bool) -> Self {
        let (cols, rows) = (cols as usize, rows as usize);
        // 4 = the margin, 2 = the borders. Guaranteed >= MIN_CONTENT by MIN_COLS.
        let max_content = cols - 4 - 2 - 2 * PAD;
        // "<spinner><2 spaces><text>" — the spinner is fixed-width, so centering
        // the group as a whole never jitters between frames.
        let text = if 3 + display_width(&text) > max_content {
            truncate(&text, max_content - 3, unicode)
        } else {
            text
        };
        let content_w = (3 + display_width(&text)).clamp(MIN_CONTENT, max_content);
        let box_w = content_w + 2 * PAD + 2;
        Self {
            text,
            content_w,
            box_w,
            left: (cols - box_w) / 2,
            top: (rows - BOX_H) / 2,
            unicode,
            color,
        }
    }
}

// ---- rendering ----------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Sty {
    Plain,
    Dim,
    Accent,
}

impl Sty {
    fn escape(self) -> &'static str {
        match self {
            Sty::Plain => "",
            Sty::Dim => "\x1b[2m",
            Sty::Accent => "\x1b[36m",
        }
    }
}

/// One rendered box row as styled runs. Kept as segments rather than a
/// pre-escaped string so the width invariant stays checkable in tests.
type Row = Vec<(Sty, String)>;

struct Glyphs {
    tl: char,
    tr: char,
    bl: char,
    br: char,
    h: char,
    v: char,
    bar_on: char,
    bar_off: char,
    spin: &'static [&'static str],
}

const UNICODE_GLYPHS: Glyphs = Glyphs {
    tl: '╭',
    tr: '╮',
    bl: '╰',
    br: '╯',
    h: '─',
    v: '│',
    bar_on: '━',
    bar_off: '─',
    spin: &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"],
};

const ASCII_GLYPHS: Glyphs = Glyphs {
    tl: '+',
    tr: '+',
    bl: '+',
    br: '+',
    h: '-',
    v: '|',
    bar_on: '#',
    bar_off: '-',
    spin: &["|", "/", "-", "\\"],
};

impl Layout {
    fn glyphs(&self) -> &'static Glyphs {
        if self.unicode {
            &UNICODE_GLYPHS
        } else {
            &ASCII_GLYPHS
        }
    }

    fn rows(&self, frame: usize, done: u64, total: u64) -> Vec<Row> {
        let g = self.glyphs();
        let rule: String = std::iter::repeat_n(g.h, self.box_w - 2).collect();
        let edge = |l: char, r: char| -> Row { vec![(Sty::Dim, format!("{l}{rule}{r}"))] };
        let wrap = |mid: Row| -> Row {
            let mut row = vec![(Sty::Dim, g.v.to_string()), (Sty::Plain, " ".repeat(PAD))];
            row.extend(mid);
            row.push((Sty::Plain, " ".repeat(PAD)));
            row.push((Sty::Dim, g.v.to_string()));
            row
        };
        let blank = || wrap(vec![(Sty::Plain, " ".repeat(self.content_w))]);

        vec![
            edge(g.tl, g.tr),
            blank(),
            wrap(self.message_row(frame)),
            wrap(self.progress_row(done, total)),
            blank(),
            edge(g.bl, g.br),
        ]
    }

    /// `<spinner>  <text>`, centered in the content column.
    fn message_row(&self, frame: usize) -> Row {
        let g = self.glyphs();
        let spin = g.spin[frame % g.spin.len()];
        let used = 3 + display_width(&self.text);
        let lead = (self.content_w.saturating_sub(used)) / 2;
        let trail = self.content_w.saturating_sub(used + lead);
        vec![
            (Sty::Plain, " ".repeat(lead)),
            (Sty::Accent, spin.to_string()),
            (Sty::Plain, format!("  {}{}", self.text, " ".repeat(trail))),
        ]
    }

    /// A determinate bar when the byte total is known, a byte counter when only
    /// the progress is (a download with no content-length), blank otherwise.
    /// Never a fabricated percentage.
    fn progress_row(&self, done: u64, total: u64) -> Row {
        let g = self.glyphs();
        if total > 0 {
            let bar_w = self.content_w - 6;
            let pct = ((done.min(total) as u128 * 100) / total as u128) as usize;
            let on = bar_w * pct / 100;
            return vec![
                (Sty::Plain, g.bar_on.to_string().repeat(on)),
                (Sty::Dim, g.bar_off.to_string().repeat(bar_w - on)),
                (Sty::Dim, format!("  {pct:>3}%")),
            ];
        }
        if done > 0 {
            let label = human_bytes(done);
            let w = display_width(&label);
            if w <= self.content_w {
                let lead = (self.content_w - w) / 2;
                return vec![(
                    Sty::Dim,
                    format!(
                        "{}{label}{}",
                        " ".repeat(lead),
                        " ".repeat(self.content_w - w - lead)
                    ),
                )];
            }
        }
        vec![(Sty::Plain, " ".repeat(self.content_w))]
    }

    /// One frame as a single escape-laden string: absolute cursor placement per
    /// row, no clearing. Rows are always exactly `box_w` cells, so overwriting
    /// in place leaves no residue.
    fn frame(&self, n: usize, done: u64, total: u64) -> String {
        let mut out = String::with_capacity(self.box_w * BOX_H * 2);
        for (i, row) in self.rows(n, done, total).iter().enumerate() {
            out.push_str(&format!("\x1b[{};{}H", self.top + 1 + i, self.left + 1));
            for (sty, s) in row {
                if !self.color || *sty == Sty::Plain {
                    out.push_str(s);
                } else {
                    out.push_str(sty.escape());
                    out.push_str(s);
                    out.push_str("\x1b[0m");
                }
            }
        }
        out
    }
}

// ---- shared state -------------------------------------------------------------

/// Progress the render thread reads and the work threads write. `total == 0`
/// means indeterminate.
struct State {
    stop: Mutex<bool>,
    wake: Condvar,
    done: AtomicU64,
    total: AtomicU64,
    /// A file whose growth IS the progress (a shell-out download). Polled by the
    /// render thread; cheaper and more honest than parsing curl's meter.
    watch: Mutex<Option<PathBuf>>,
}

impl State {
    fn new() -> Self {
        Self {
            stop: Mutex::new(false),
            wake: Condvar::new(),
            done: AtomicU64::new(0),
            total: AtomicU64::new(0),
            watch: Mutex::new(None),
        }
    }

    /// Sleep up to `d`, waking immediately on stop. Returns whether to stop.
    /// Loops to the deadline so a spurious wakeup can't shorten the hold-off.
    fn park(&self, d: Duration) -> bool {
        let deadline = Instant::now() + d;
        let mut guard = self.stop.lock().unwrap_or_else(|e| e.into_inner());
        while !*guard {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let (next, _) = self
                .wake
                .wait_timeout(guard, deadline - now)
                .unwrap_or_else(|e| e.into_inner());
            guard = next;
        }
        *guard
    }

    fn halt(&self) {
        *self.stop.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.wake.notify_all();
    }
}

/// The write half of the progress channel, handed to whatever is doing the work.
/// A no-op when the presentation is Silent or Line — the calls stay unconditional
/// at the call sites.
#[derive(Clone)]
pub struct Progress(Arc<State>);

impl Progress {
    pub fn set_total(&self, total: u64) {
        self.0.done.store(0, Ordering::Relaxed);
        self.0.total.store(total, Ordering::Relaxed);
    }

    pub fn add(&self, n: u64) {
        self.0.done.fetch_add(n, Ordering::Relaxed);
    }

    /// Track `path`'s size instead of an explicit counter — for work done by a
    /// child process that only reports through the file it is writing.
    pub fn watch_file(&self, path: &Path) {
        self.reset();
        *self.0.watch.lock().unwrap_or_else(|e| e.into_inner()) = Some(path.to_path_buf());
    }

    /// Back to indeterminate — the spinner alone.
    pub fn reset(&self) {
        self.0.done.store(0, Ordering::Relaxed);
        self.0.total.store(0, Ordering::Relaxed);
        *self.0.watch.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// A `Read` that reports what it consumed. Wrapping the COMPRESSED side of the
/// zstd decoder is what makes embed-shape progress determinate: the payload's
/// compressed length is known exactly, and the decoder consumes it linearly.
pub struct Counting<R> {
    inner: R,
    progress: Progress,
}

impl<R> Counting<R> {
    pub fn new(inner: R, progress: Progress) -> Self {
        Self { inner, progress }
    }
}

impl<R: Read> Read for Counting<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.progress.add(n as u64);
        Ok(n)
    }
}

// ---- the handle the launcher holds --------------------------------------------

/// The one-shot first-run notice. The launcher owns the terminal before it does
/// any work, so a cold run announces itself instead of going silent for seconds;
/// a warm run never constructs a reason to start.
pub struct FirstRun {
    plan: Plan,
    state: Arc<State>,
    started: Cell<bool>,
    thread: RefCell<Option<JoinHandle<()>>>,
}

impl FirstRun {
    pub fn new(text: Option<&str>) -> Self {
        Self::with_caps(text, detect_caps())
    }

    pub(crate) fn with_caps(text: Option<&str>, caps: Caps) -> Self {
        Self {
            plan: plan(text, &caps),
            state: Arc::new(State::new()),
            started: Cell::new(false),
            thread: RefCell::new(None),
        }
    }

    /// Announce the cold path. Idempotent: extraction and provisioning both call
    /// it, and a run that does both starts one UI.
    pub fn announce(&self) {
        if self.started.replace(true) {
            return;
        }
        match &self.plan {
            Plan::Silent => {}
            Plan::Line(text) => {
                let mut err = std::io::stderr().lock();
                let _ = writeln!(err, "{text}");
            }
            Plan::Boxed(layout) => {
                let layout = layout.clone();
                let state = Arc::clone(&self.state);
                *self.thread.borrow_mut() = std::thread::Builder::new()
                    .name("nub-first-run".into())
                    .spawn(move || render(layout, state))
                    .ok();
            }
        }
    }

    pub fn progress(&self) -> Progress {
        Progress(Arc::clone(&self.state))
    }

    /// Whether the box currently owns the terminal — the signal for a child
    /// process to keep its own progress meter to itself.
    pub fn owns_terminal(&self) -> bool {
        self.started.get() && matches!(self.plan, Plan::Boxed(_))
    }

    /// Stop rendering and hand the terminal back. Must complete before the app's
    /// own output starts; `Drop` covers the error paths.
    pub fn finish(&self) {
        let handle = self.thread.borrow_mut().take();
        if let Some(handle) = handle {
            self.state.halt();
            let _ = handle.join();
        }
    }
}

impl Drop for FirstRun {
    fn drop(&mut self) {
        self.finish();
    }
}

// ---- the render thread --------------------------------------------------------

fn render(mut layout: Layout, state: Arc<State>) {
    // Hold off before touching the screen at all: if the work beats this, the
    // user never sees a flash, and the alternate screen is never entered.
    if state.park(FIRST_PAINT) {
        return;
    }

    let screen = Screen::enter();
    let mut size = viewport();
    let mut frame = 0usize;
    loop {
        let now = viewport();
        if now != size {
            size = now;
            if let Some((cols, rows)) = now {
                if cols >= MIN_COLS && rows >= MIN_ROWS {
                    layout = Layout::compute(
                        layout.text.clone(),
                        cols,
                        rows,
                        layout.unicode,
                        layout.color,
                    );
                    screen.write("\x1b[2J");
                }
            }
        }

        let total = state.total.load(Ordering::Relaxed);
        // Cloned out rather than stat'd under the lock: the work thread must never
        // block on this thread's filesystem call.
        let watched = state
            .watch
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let done = match watched {
            Some(path) => std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
            None => state.done.load(Ordering::Relaxed),
        };
        screen.write(&layout.frame(frame, done, total));

        frame += 1;
        if state.park(FRAME) {
            break;
        }
    }
}

/// Owns the alternate screen for as long as it lives. The single writer to
/// stderr while the UI is up, so no frame can interleave with another.
struct Screen;

impl Screen {
    fn enter() -> Self {
        arm_restore_guard();
        let s = Screen;
        s.write(ENTER);
        s
    }

    fn write(&self, s: &str) {
        let mut err = std::io::stderr().lock();
        let _ = err.write_all(s.as_bytes());
        let _ = err.flush();
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        self.write(LEAVE);
        disarm_restore_guard();
    }
}

/// Restore the terminal from a signal handler, then die as the signal intended.
/// Only `write`/`signal`/`raise` run here — all async-signal-safe. SIGABRT is in
/// the set because `panic = "abort"` is how this binary dies from a panic, and
/// `Drop` does not run on that path.
#[cfg(unix)]
mod guard {
    use libc::c_int;

    const FATAL: [c_int; 8] = [
        libc::SIGINT,
        libc::SIGTERM,
        libc::SIGHUP,
        libc::SIGQUIT,
        libc::SIGABRT,
        libc::SIGSEGV,
        libc::SIGBUS,
        libc::SIGILL,
    ];

    extern "C" fn restore(sig: c_int) {
        let bytes = super::LEAVE.as_bytes();
        unsafe {
            libc::write(libc::STDERR_FILENO, bytes.as_ptr().cast(), bytes.len());
            libc::signal(sig, libc::SIG_DFL);
            libc::raise(sig);
        }
    }

    pub(super) fn arm() {
        for sig in FATAL {
            unsafe { libc::signal(sig, restore as *const () as libc::sighandler_t) };
        }
    }

    pub(super) fn disarm() {
        for sig in FATAL {
            unsafe { libc::signal(sig, libc::SIG_DFL) };
        }
    }
}

#[cfg(unix)]
fn arm_restore_guard() {
    guard::arm();
}
#[cfg(unix)]
fn disarm_restore_guard() {
    guard::disarm();
}
#[cfg(not(unix))]
fn arm_restore_guard() {}
#[cfg(not(unix))]
fn disarm_restore_guard() {}

// ---- terminal capability probing ----------------------------------------------

fn detect_caps() -> Caps {
    let tty = std::io::stderr().is_terminal();
    let dumb = std::env::var("TERM").map(|t| t == "dumb").unwrap_or(false);
    Caps {
        tty,
        size: if tty && !dumb { viewport() } else { None },
        unicode: unicode_locale(),
        color: !dumb && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty()),
    }
}

/// The viewport, from stderr — the surface the UI owns. Windows returns `None`:
/// querying the console and enabling VT processing both need Win32 calls this
/// binary does not link, and a box drawn on a console that renders escapes
/// literally is worse than the plain line.
#[cfg(unix)]
fn viewport() -> Option<(u16, u16)> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::ioctl(libc::STDERR_FILENO, libc::TIOCGWINSZ as _, &mut ws) };
    if rc != 0 || ws.ws_col == 0 || ws.ws_row == 0 {
        return None;
    }
    Some((ws.ws_col, ws.ws_row))
}
#[cfg(not(unix))]
fn viewport() -> Option<(u16, u16)> {
    None
}

/// Box-drawing and braille are safe unless the locale explicitly says otherwise
/// (`LANG=C`). Assuming UTF-8 on an unset locale matches every terminal in
/// practice; assuming ASCII would give most macOS users the fallback box.
fn unicode_locale() -> bool {
    for key in ["LC_ALL", "LC_CTYPE", "LANG"] {
        if let Some(v) = std::env::var_os(key) {
            let v = v.to_string_lossy().to_ascii_uppercase();
            if v.is_empty() {
                continue;
            }
            return v.contains("UTF-8") || v.contains("UTF8");
        }
    }
    true
}

// ---- text measurement ---------------------------------------------------------

/// Anything that would break the box — newlines, escapes, control bytes —
/// becomes a space. The message is user-supplied at compile time.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_string()
}

/// Terminal cells, not chars. A full `unicode-width` table is not worth a
/// dependency in a size-critical binary; these ranges cover the wide scripts and
/// emoji that would otherwise visibly skew the border.
fn display_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

fn char_width(c: char) -> usize {
    let u = c as u32;
    match u {
        // Combining marks, zero-width joiners, variation selectors.
        0x0300..=0x036F | 0x200B..=0x200F | 0xFE00..=0xFE0F => 0,
        // CJK, Hangul, kana, fullwidth forms, emoji.
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE6F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1F9FF
        | 0x20000..=0x3FFFD => 2,
        _ => 1,
    }
}

/// Truncate to `w` cells inclusive of the ellipsis.
fn truncate(s: &str, w: usize, unicode: bool) -> String {
    let mark = if unicode { "…" } else { "..." };
    let mark_w = display_width(mark);
    if w <= mark_w {
        return mark.chars().take(w).collect();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let cw = char_width(c);
        if used + cw > w - mark_w {
            break;
        }
        out.push(c);
        used += cw;
    }
    out.push_str(mark);
    out
}

fn human_bytes(n: u64) -> String {
    const KB: f64 = 1024.0;
    let b = n as f64;
    if b < KB {
        format!("{n} B")
    } else if b < KB * KB {
        format!("{:.0} KB", b / KB)
    } else if b < KB * KB * KB {
        format!("{:.1} MB", b / (KB * KB))
    } else {
        format!("{:.2} GB", b / (KB * KB * KB))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(tty: bool, size: Option<(u16, u16)>) -> Caps {
        Caps {
            tty,
            size,
            unicode: true,
            color: true,
        }
    }

    fn boxed(plan: Plan) -> Layout {
        match plan {
            Plan::Boxed(l) => l,
            other => panic!("expected a box, got {other:?}"),
        }
    }

    /// The contract the non-TTY case depends on: a pipe, a log, or a CI job sees
    /// NOTHING — no line, no escape byte — and `--no-install-message` is silent
    /// even on a terminal.
    #[test]
    fn silent_off_a_terminal_and_when_disabled() {
        assert_eq!(
            plan(Some("Setting up app"), &caps(false, Some((120, 40)))),
            Plan::Silent
        );
        assert_eq!(plan(None, &caps(true, Some((120, 40)))), Plan::Silent);
        assert_eq!(
            plan(Some("   "), &caps(true, Some((120, 40)))),
            Plan::Silent
        );
    }

    /// A terminal we cannot place a box in degrades to the one plain line rather
    /// than drawing a corrupt box: unknown size (Windows, a failed ioctl), too
    /// few columns, too few rows.
    #[test]
    fn degrades_to_one_line_when_the_box_will_not_fit() {
        let line = Plan::Line("Setting up app".into());
        assert_eq!(plan(Some("Setting up app"), &caps(true, None)), line);
        assert_eq!(
            plan(
                Some("Setting up app"),
                &caps(true, Some((MIN_COLS - 1, 40)))
            ),
            line
        );
        assert_eq!(
            plan(
                Some("Setting up app"),
                &caps(true, Some((120, MIN_ROWS - 1)))
            ),
            line
        );
        // Exactly at the floor, the box is on.
        assert!(matches!(
            plan(
                Some("Setting up app"),
                &caps(true, Some((MIN_COLS, MIN_ROWS)))
            ),
            Plan::Boxed(_)
        ));
    }

    /// Geometry: the box is centered, stays inside the viewport with a margin,
    /// and every rendered row is exactly as wide as the box — the invariant that
    /// keeps in-place repainting from leaving residue.
    #[test]
    fn box_is_centered_and_rows_are_flush() {
        for (cols, rows) in [(MIN_COLS, MIN_ROWS), (80, 24), (200, 60), (43, 11)] {
            let l = boxed(plan(
                Some("Setting up my-app"),
                &caps(true, Some((cols, rows))),
            ));
            assert!(
                l.box_w + 4 <= cols as usize,
                "{cols}x{rows}: box {} overflows",
                l.box_w
            );
            assert_eq!(
                l.left,
                (cols as usize - l.box_w) / 2,
                "{cols}x{rows}: off-center"
            );
            assert_eq!(
                l.top,
                (rows as usize - BOX_H) / 2,
                "{cols}x{rows}: off-center"
            );
            for (done, total) in [(0, 0), (3, 10), (10, 10), (4096, 0)] {
                for row in l.rows(0, done, total) {
                    let text: String = row.iter().map(|(_, s)| s.as_str()).collect();
                    assert_eq!(
                        display_width(&text),
                        l.box_w,
                        "{cols}x{rows} at {done}/{total}: row {text:?} is not {} cells",
                        l.box_w
                    );
                }
            }
        }
    }

    /// A message longer than the viewport is truncated into the box, never
    /// allowed to widen it past the margin.
    #[test]
    fn long_message_is_truncated_to_fit() {
        let l = boxed(plan(Some(&"x".repeat(400)), &caps(true, Some((60, 20)))));
        assert!(l.box_w + 4 <= 60);
        assert!(
            l.text.ends_with('…'),
            "expected an ellipsis, got {:?}",
            l.text
        );
    }

    /// Newlines and escapes in a compile-time-supplied message must not escape
    /// the box.
    #[test]
    fn control_characters_are_neutralized() {
        let l = boxed(plan(
            Some("Setting\nup\x1b[31m app"),
            &caps(true, Some((80, 24))),
        ));
        assert!(!l.text.contains('\n') && !l.text.contains('\x1b'));
    }

    /// Progress is shown only where it is genuinely known: a determinate bar when
    /// a byte total exists, a byte counter when only the numerator does, and
    /// nothing at all otherwise.
    #[test]
    fn progress_row_never_invents_a_percentage() {
        let l = boxed(plan(Some("Setting up app"), &caps(true, Some((80, 24)))));
        let text = |done, total| -> String {
            l.progress_row(done, total)
                .iter()
                .map(|(_, s)| s.as_str())
                .collect()
        };
        assert!(text(0, 0).trim().is_empty());
        assert!(text(50, 200).contains("25%"));
        assert!(text(200, 200).contains("100%"));
        // Overshoot is clamped, not rendered as 137%.
        assert!(text(274, 200).contains("100%"));
        assert_eq!(text(1_572_864, 0).trim(), "1.5 MB");
    }

    #[test]
    fn width_accounts_for_wide_and_zero_width_characters() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("日本語"), 6);
        assert_eq!(display_width("e\u{0301}"), 1);
    }

    /// The non-Unicode locale (`LANG=C`) still gets a box — the ASCII fallback —
    /// with no box-drawing or braille bytes, and the row-width invariant holds
    /// there too.
    #[test]
    fn ascii_fallback_uses_only_ascii_glyphs() {
        let caps = Caps {
            tty: true,
            size: Some((80, 24)),
            unicode: false,
            color: true,
        };
        let l = boxed(plan(Some("Setting up app"), &caps));
        for row in l.rows(2, 3, 10) {
            let text: String = row.iter().map(|(_, s)| s.as_str()).collect();
            assert!(text.is_ascii(), "non-ASCII in fallback row: {text:?}");
            assert_eq!(display_width(&text), l.box_w);
        }
    }
}
