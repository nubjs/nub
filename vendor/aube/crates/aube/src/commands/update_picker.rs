//! Tri-state interactive update picker (embedder-gated; see
//! `Embedder::rich_update_picker`).
//!
//! One row per outdated direct dep, each carrying horizontal radio cells the
//! user cycles with space / ←→: keep the current version, take the max
//! version inside the manifest range, or take the registry's `latest`
//! dist-tag. Every row defaults to "keep" — an update is an explicit opt-in,
//! never the default. This replaces (under the gating embedder only) the
//! `demand::MultiSelect` picker, which pre-selects everything and can only
//! express one target per invocation; the tri-state rows subsume the
//! `-i` / `-i --latest` split into a single picker.
//!
//! The interactive loop renders on stderr (stdout stays clean for report
//! output, matching the demand picker) and requires a TTY — the caller
//! enforces that before constructing rows.

use std::collections::BTreeSet;
use std::io::Write;

use console::{Key, Term};

/// One pickable dependency. Both targets are always populated on a row
/// that exists — `range_target` with `current` itself when there is no
/// in-range drift, `latest_target` with the range value when the real
/// `latest` is downgrade-guarded — so every row carries a box in every
/// column and cycles in unison. A row offering no real change at all is
/// filtered out by [`build_row`].
#[derive(Debug, Clone)]
pub(crate) struct PickerRow {
    pub key: String,
    pub bucket: &'static str,
    /// The manifest specifier (`^4.1.0`, `~7.5.0`), rendered as a dim
    /// annotation column between the name and the cells.
    pub spec: String,
    pub current: String,
    pub range_target: Option<String>,
    pub latest_target: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PickState {
    Keep,
    Range,
    Latest,
}

/// The confirmed selection: keys to refresh inside their manifest range and
/// keys to bump to the `latest` dist-tag (the caller maps the latter onto
/// the same per-key machinery as an explicit `<pkg>@latest` argument).
#[derive(Debug, Default)]
pub(crate) struct PickerSelection {
    pub in_range: BTreeSet<String>,
    pub to_latest: BTreeSet<String>,
}

impl PickerSelection {
    pub(crate) fn is_empty(&self) -> bool {
        self.in_range.is_empty() && self.to_latest.is_empty()
    }
}

/// Build a picker row from the resolved facts about one dependency, or
/// `None` when there is nothing to offer:
///
/// - The row exists iff the in-range max is a real (non-downgrade) change
///   from `current`, or the `latest` dist-tag is a strict upgrade.
/// - `range_target` is then ALWAYS populated — with `current` itself when
///   the in-range max is current (or would be a downgrade: a lockfile
///   pinned above the manifest range). Duplicating the keep value keeps
///   every row's cells in the same columns and cycling in unison; a
///   no-op "latest in range" pick simply re-resolves to the same version.
/// - `latest_target` = the `latest` dist-tag when it is a strict semver
///   upgrade over `current`; otherwise it DUPLICATES the range value so
///   the cell (and its box) always renders. The duplicate keeps the
///   downgrade guard: a `latest` at-or-below `current` (the prerelease-pin
///   case: current `4.0.0-beta.59`, latest `0.64.0`) is never shown, and
///   [`build_selection`] routes a value-equal latest pick through the
///   in-range machinery so the `latest` dist-tag can't be applied through
///   the duplicate. An unparseable `current` is treated the same way.
pub(crate) fn build_row(
    key: &str,
    bucket: &'static str,
    spec: &str,
    current: &str,
    wanted: Option<&str>,
    latest: Option<&str>,
) -> Option<PickerRow> {
    let semver_downgrade = |target: &str| {
        matches!(
            (
                node_semver::Version::parse(current),
                node_semver::Version::parse(target),
            ),
            (Ok(cur), Ok(new)) if new <= cur
        )
    };
    let range_target = wanted
        .filter(|w| !semver_downgrade(w))
        .unwrap_or(current)
        .to_string();
    let real_latest = latest.filter(|l| {
        let (Ok(cur), Ok(new)) = (
            node_semver::Version::parse(current),
            node_semver::Version::parse(l),
        ) else {
            return false;
        };
        new > cur
    });
    if range_target == current && real_latest.is_none() {
        return None;
    }
    let latest_target = real_latest.unwrap_or(&range_target).to_string();
    Some(PickerRow {
        key: key.to_string(),
        bucket,
        spec: spec.to_string(),
        current: current.to_string(),
        range_target: Some(range_target),
        latest_target: Some(latest_target),
    })
}

impl PickerRow {
    /// Display width of the title column: `name@spec` (the `@spec` part
    /// renders dim), or just the name when there is no spec.
    fn title_w(&self) -> usize {
        self.key.len()
            + if self.spec.is_empty() {
                0
            } else {
                1 + self.spec.len()
            }
    }

    fn has(&self, state: PickState) -> bool {
        match state {
            PickState::Keep => true,
            PickState::Range => self.range_target.is_some(),
            PickState::Latest => self.latest_target.is_some(),
        }
    }

    /// Cycle forward through the row's available states, wrapping.
    fn next_state(&self, state: PickState) -> PickState {
        let order = [PickState::Keep, PickState::Range, PickState::Latest];
        let start = order.iter().position(|s| *s == state).unwrap_or(0);
        for step in 1..=order.len() {
            let candidate = order[(start + step) % order.len()];
            if self.has(candidate) {
                return candidate;
            }
        }
        PickState::Keep
    }

    fn prev_state(&self, state: PickState) -> PickState {
        let order = [PickState::Keep, PickState::Range, PickState::Latest];
        let start = order.iter().position(|s| *s == state).unwrap_or(0);
        for step in 1..=order.len() {
            let candidate = order[(start + order.len() - step) % order.len()];
            if self.has(candidate) {
                return candidate;
            }
        }
        PickState::Keep
    }

    /// The most conservative update the row offers (range before latest).
    fn first_update_state(&self) -> Option<PickState> {
        if self.range_target.is_some() {
            Some(PickState::Range)
        } else if self.latest_target.is_some() {
            Some(PickState::Latest)
        } else {
            None
        }
    }

    /// The most aggressive update the row offers (latest before range).
    fn max_update_state(&self) -> Option<PickState> {
        if self.latest_target.is_some() {
            Some(PickState::Latest)
        } else if self.range_target.is_some() {
            Some(PickState::Range)
        } else {
            None
        }
    }
}

/// `a` cycles the whole (visible) set through three phases:
/// all-keep → all-conservative-update → all-aggressive-update → all-keep.
/// The two update phases collapse into one when no visible row offers both
/// a range and a latest target.
fn cycle_all(rows: &[PickerRow], states: &mut [PickState], visible: &[usize]) {
    let all_keep = visible.iter().all(|&i| states[i] == PickState::Keep);
    let all_max = visible
        .iter()
        .all(|&i| Some(states[i]) == rows[i].max_update_state());
    for &i in visible {
        states[i] = if all_keep {
            rows[i].first_update_state().unwrap_or(PickState::Keep)
        } else if all_max {
            PickState::Keep
        } else {
            rows[i].max_update_state().unwrap_or(PickState::Keep)
        };
    }
}

/// Column widths, computed from raw (uncolored) strings so ANSI escapes
/// never skew the padding — the whole frame must read as clean table
/// columns.
struct Layout {
    name_w: usize,
    cur_w: usize,
    range_w: usize,
    latest_w: usize,
}

impl Layout {
    fn of(rows: &[PickerRow]) -> Self {
        let max = |it: &mut dyn Iterator<Item = usize>| it.max().unwrap_or(0);
        Layout {
            name_w: max(&mut rows.iter().map(|r| r.title_w())),
            cur_w: max(&mut rows.iter().map(|r| r.current.len())),
            range_w: max(&mut rows
                .iter()
                .filter_map(|r| r.range_target.as_ref().map(String::len))),
            latest_w: max(&mut rows
                .iter()
                .filter_map(|r| r.latest_target.as_ref().map(String::len))),
        }
    }

    // Full column DISPLAY widths (box + space + padded version), floored
    // by the column's header label so a short version column never
    // squeezes its heading. Zero when no row carries that cell, so the
    // column (and its header) is omitted entirely. Counted in chars, not
    // bytes — the `■`/`□` glyphs are multibyte.
    fn keep_col_w(&self) -> usize {
        (2 + self.cur_w).max(HDR_KEEP.len())
    }
    fn range_col_w(&self) -> usize {
        if self.range_w == 0 {
            0
        } else {
            (2 + self.range_w).max(HDR_RANGE.len())
        }
    }
    fn latest_col_w(&self) -> usize {
        if self.latest_w == 0 {
            0
        } else {
            (2 + self.latest_w).max(HDR_LATEST.len())
        }
    }
}

// Column headings; the cells below them carry only box + version.
const HDR_KEEP: &str = "keep";
const HDR_RANGE: &str = "latest in range";
const HDR_LATEST: &str = "latest";

/// Render one radio cell: box + version, no label (the column heading
/// carries the label). Selected cells get a plain filled box (`■`);
/// unselected cells a dimmed hollow box — bun's checkbox glyphs. The
/// version keeps whatever coloring the caller computed (the semver-diff
/// colors carry the severity signal). `raw_w`/`col_w` are display widths:
/// the version is already padded to the column's version width, so the
/// trailing pad tops the cell up to the full column width.
fn cell(version: &str, raw_w: usize, col_w: usize, selected: bool) -> String {
    use clx::style;
    let pad = " ".repeat(col_w.saturating_sub(raw_w));
    if selected {
        format!("■ {version}{pad}")
    } else {
        format!("{} {version}{pad}", style::estyle("□").dim())
    }
}

fn format_row(row: &PickerRow, state: PickState, layout: &Layout, focused: bool) -> String {
    use clx::style;
    let cursor = if focused {
        format!("{} ", style::ecyan("❯"))
    } else {
        "  ".to_string()
    };
    let title_pad = " ".repeat(layout.name_w.saturating_sub(row.title_w()));
    let name = if row.spec.is_empty() {
        format!("{}{title_pad}", row.key)
    } else {
        format!(
            "{}{}{title_pad}",
            row.key,
            style::estyle(format!("@{}", row.spec)).dim()
        )
    };
    let current_padded = format!("{:<w$}", row.current, w = layout.cur_w);
    let keep_selected = state == PickState::Keep;
    let keep_version = if keep_selected {
        current_padded
    } else {
        style::estyle(current_padded).dim().to_string()
    };
    let mut line = format!(
        "{cursor}  {name}  {}",
        cell(
            &keep_version,
            2 + layout.cur_w,
            layout.keep_col_w(),
            keep_selected
        )
    );
    if layout.range_col_w() > 0 {
        line.push_str("  ");
        match &row.range_target {
            Some(target) => {
                let version =
                    super::outdated::colorize_diff(&row.current, target, layout.range_w, true);
                line.push_str(&cell(
                    &version,
                    2 + layout.range_w,
                    layout.range_col_w(),
                    state == PickState::Range,
                ));
            }
            None => line.push_str(&" ".repeat(layout.range_col_w())),
        }
    }
    if layout.latest_col_w() > 0 {
        line.push_str("  ");
        match &row.latest_target {
            Some(target) => {
                let version =
                    super::outdated::colorize_diff(&row.current, target, layout.latest_w, true);
                line.push_str(&cell(
                    &version,
                    2 + layout.latest_w,
                    layout.latest_col_w(),
                    state == PickState::Latest,
                ));
            }
            None => line.push_str(&" ".repeat(layout.latest_col_w())),
        }
    }
    line
}

/// The dim column-heading line rendered once under the title: blank over
/// the name column, then each existing column's label at its cells' start.
fn header_line(layout: &Layout) -> String {
    let mut line = " ".repeat(4 + layout.name_w + 2);
    line.push_str(&format!("{:<w$}", HDR_KEEP, w = layout.keep_col_w()));
    if layout.range_col_w() > 0 {
        line.push_str("  ");
        line.push_str(&format!("{:<w$}", HDR_RANGE, w = layout.range_col_w()));
    }
    if layout.latest_col_w() > 0 {
        line.push_str("  ");
        line.push_str(HDR_LATEST);
    }
    line
}

/// Stable presentation order for the manifest buckets.
fn bucket_rank(bucket: &str) -> usize {
    match bucket {
        "dependencies" => 0,
        "devDependencies" => 1,
        _ => 2,
    }
}

fn visible_indices(rows: &[PickerRow], filter: &str) -> Vec<usize> {
    if filter.is_empty() {
        return (0..rows.len()).collect();
    }
    let needle = filter.to_lowercase();
    rows.iter()
        .enumerate()
        .filter(|(_, r)| r.key.to_lowercase().contains(&needle))
        .map(|(i, _)| i)
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn render_frame(
    rows: &[PickerRow],
    states: &[PickState],
    visible: &[usize],
    cursor: usize,
    layout: &Layout,
    filtering: bool,
    filter: &str,
    term_width: usize,
) -> String {
    use clx::style;
    let mut out = String::new();
    let push_line = |line: String, out: &mut String| {
        // Wrapped lines would break the clear-and-redraw line accounting, so
        // hard-truncate to the terminal width (ANSI-aware).
        if term_width > 0 {
            out.push_str(&console::truncate_str(&line, term_width, "…"));
        } else {
            out.push_str(&line);
        }
        out.push('\n');
    };
    push_line(
        style::estyle("Choose dependency updates")
            .bold()
            .to_string(),
        &mut out,
    );
    push_line(
        style::estyle(header_line(layout)).dim().to_string(),
        &mut out,
    );
    let mut prev_bucket: Option<&str> = None;
    for (pos, &idx) in visible.iter().enumerate() {
        let row = &rows[idx];
        if prev_bucket != Some(row.bucket) {
            push_line(format!("  {}", style::estyle(row.bucket).dim()), &mut out);
            prev_bucket = Some(row.bucket);
        }
        push_line(
            format_row(row, states[idx], layout, pos == cursor),
            &mut out,
        );
    }
    if visible.is_empty() {
        push_line(style::estyle("  (no matches)").dim().to_string(), &mut out);
    }
    if filtering {
        push_line(format!("/{filter}"), &mut out);
    } else if !filter.is_empty() {
        push_line(
            style::estyle(format!("/{filter}")).dim().to_string(),
            &mut out,
        );
    }
    push_line(
        style::estyle(
            "↑/↓ move · space/←/→ cycle · a cycle all · / filter · enter apply · esc cancel",
        )
        .dim()
        .to_string(),
        &mut out,
    );
    out
}

/// Restores the cursor even on an early `?` return or panic unwind.
struct CursorGuard<'a>(&'a Term);
impl Drop for CursorGuard<'_> {
    fn drop(&mut self) {
        let _ = self.0.show_cursor();
    }
}

/// Run the picker. `Ok(None)` means the user cancelled (Ctrl-C / Esc);
/// the caller maps that to exit code 130 via the return path, same as the
/// demand picker.
pub(crate) fn run(mut rows: Vec<PickerRow>) -> std::io::Result<Option<PickerSelection>> {
    rows.sort_by(|a, b| {
        bucket_rank(a.bucket)
            .cmp(&bucket_rank(b.bucket))
            .then_with(|| a.key.cmp(&b.key))
    });
    let layout = Layout::of(&rows);
    let mut states = vec![PickState::Keep; rows.len()];
    let mut filter = String::new();
    let mut filtering = false;
    let mut cursor = 0usize;
    let mut last_lines = 0usize;

    let term = Term::stderr();
    let _guard = CursorGuard(&term);
    term.hide_cursor()?;

    loop {
        let visible = visible_indices(&rows, &filter);
        if cursor >= visible.len() {
            cursor = visible.len().saturating_sub(1);
        }
        let width = term.size().1 as usize;
        let frame = render_frame(
            &rows, &states, &visible, cursor, &layout, filtering, &filter, width,
        );
        term.clear_last_lines(last_lines)?;
        last_lines = frame.lines().count();
        write!(&term, "{frame}")?;

        let key = term.read_key()?;
        let cycle = |states: &mut [PickState], forward: bool| {
            if let Some(&idx) = visible.get(cursor) {
                states[idx] = if forward {
                    rows[idx].next_state(states[idx])
                } else {
                    rows[idx].prev_state(states[idx])
                };
            }
        };
        if filtering {
            match key {
                Key::Enter => filtering = false,
                Key::Escape => {
                    filtering = false;
                    filter.clear();
                }
                Key::Backspace => {
                    filter.pop();
                }
                Key::ArrowDown => cursor = (cursor + 1).min(visible.len().saturating_sub(1)),
                Key::ArrowUp => cursor = cursor.saturating_sub(1),
                Key::ArrowRight | Key::Char(' ') => cycle(&mut states, true),
                Key::ArrowLeft => cycle(&mut states, false),
                Key::CtrlC => {
                    term.clear_last_lines(last_lines)?;
                    return Ok(None);
                }
                Key::Char(c) if !c.is_control() => filter.push(c),
                _ => {}
            }
            continue;
        }
        match key {
            Key::ArrowDown | Key::Char('j') => {
                cursor = (cursor + 1).min(visible.len().saturating_sub(1));
            }
            Key::ArrowUp | Key::Char('k') => cursor = cursor.saturating_sub(1),
            Key::ArrowRight | Key::Char('l') | Key::Char(' ') | Key::Char('x') => {
                cycle(&mut states, true);
            }
            Key::ArrowLeft | Key::Char('h') => cycle(&mut states, false),
            Key::Char('a') => cycle_all(&rows, &mut states, &visible),
            Key::Char('/') => {
                filtering = true;
                filter.clear();
            }
            Key::Escape => {
                if filter.is_empty() {
                    term.clear_last_lines(last_lines)?;
                    return Ok(None);
                }
                filter.clear();
            }
            Key::CtrlC => {
                term.clear_last_lines(last_lines)?;
                return Ok(None);
            }
            Key::Enter => {
                term.clear_last_lines(last_lines)?;
                return Ok(Some(build_selection(&rows, &states)));
            }
            _ => {}
        }
    }
}

/// The confirmed contract: keep-rows drop out entirely, range-rows land in
/// `in_range` (bare-arg semantics), latest-rows land in `to_latest` (the
/// caller routes them as explicit `<pkg>@latest` specs).
///
/// A latest pick whose cell merely duplicates the range value routes as
/// in-range instead: on a downgrade-guarded row the shown version came
/// from the range side, and applying the real `latest` dist-tag there
/// would downgrade past what the cell displayed. (When range and latest
/// genuinely resolve to the same version the two routes are equivalent,
/// so the value check is safe for both duplicate shapes.)
fn build_selection(rows: &[PickerRow], states: &[PickState]) -> PickerSelection {
    let mut selection = PickerSelection::default();
    for (row, state) in rows.iter().zip(states) {
        match state {
            PickState::Keep => {}
            PickState::Range => {
                selection.in_range.insert(row.key.clone());
            }
            PickState::Latest => {
                if row.latest_target == row.range_target {
                    selection.in_range.insert(row.key.clone());
                } else {
                    selection.to_latest.insert(row.key.clone());
                }
            }
        }
    }
    selection
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        key: &str,
        bucket: &'static str,
        current: &str,
        wanted: Option<&str>,
        latest: Option<&str>,
    ) -> Option<PickerRow> {
        build_row(key, bucket, "^1.0.0", current, wanted, latest)
    }

    #[test]
    fn build_row_drops_rows_without_drift() {
        assert!(row("a", "dependencies", "1.2.3", Some("1.2.3"), Some("1.2.3")).is_none());
        assert!(row("a", "dependencies", "1.2.3", None, None).is_none());
    }

    #[test]
    fn build_row_duplicates_equal_targets_and_backfills_range() {
        // wanted == latest: both cells show the same version, so the row
        // cycles through all three states like every other row.
        let r = row("a", "dependencies", "1.2.3", Some("2.0.0"), Some("2.0.0")).unwrap();
        assert_eq!(r.range_target.as_deref(), Some("2.0.0"));
        assert_eq!(r.latest_target.as_deref(), Some("2.0.0"));
        // No in-range drift: the range cell duplicates `current` (a no-op
        // pick) rather than going missing.
        let r = row("a", "dependencies", "1.2.3", Some("1.2.3"), Some("2.0.0")).unwrap();
        assert_eq!(r.range_target.as_deref(), Some("1.2.3"));
        assert_eq!(r.latest_target.as_deref(), Some("2.0.0"));
    }

    #[test]
    fn build_row_masks_latest_downgrade_with_range_duplicate() {
        // The bun screenshot case: current 4.0.0-beta.59, latest dist-tag
        // 0.64.0 — the latest cell must never show the downgrade. It
        // duplicates the in-range refresh instead, so the row still has a
        // box in every column.
        let r = row(
            "@effect/opentelemetry",
            "dependencies",
            "4.0.0-beta.59",
            Some("4.0.0-beta.60"),
            Some("0.64.0"),
        )
        .unwrap();
        assert_eq!(r.range_target.as_deref(), Some("4.0.0-beta.60"));
        assert_eq!(r.latest_target.as_deref(), Some("4.0.0-beta.60"));
    }

    #[test]
    fn build_row_offers_both_targets_when_distinct() {
        let r = row("a", "dependencies", "7.5.4", Some("7.5.9"), Some("7.8.5")).unwrap();
        assert_eq!(r.range_target.as_deref(), Some("7.5.9"));
        assert_eq!(r.latest_target.as_deref(), Some("7.8.5"));
    }

    #[test]
    fn build_row_hides_range_downgrade_but_keeps_unparseable_drift() {
        // A lockfile pinned above the manifest range must not get the lower
        // in-range max offered as an update.
        assert!(row("a", "dependencies", "2.1.0", Some("1.9.0"), None).is_none());
        // Unparseable current: the real latest can't be ordered so the
        // latest cell duplicates the range value, which stays on plain
        // inequality — the resolver may move it.
        let r = row("a", "dependencies", "git-pin", Some("1.2.3"), Some("9.9.9")).unwrap();
        assert_eq!(r.range_target.as_deref(), Some("1.2.3"));
        assert_eq!(r.latest_target.as_deref(), Some("1.2.3"));
    }

    #[test]
    fn selection_splits_states_into_range_and_latest_sets() {
        let rows = vec![
            row("keepme", "dependencies", "1.0.0", Some("1.1.0"), None).unwrap(),
            row(
                "rangey",
                "dependencies",
                "1.0.0",
                Some("1.1.0"),
                Some("2.0.0"),
            )
            .unwrap(),
            row("latesty", "dependencies", "1.0.0", None, Some("3.0.0")).unwrap(),
        ];
        let states = vec![PickState::Keep, PickState::Range, PickState::Latest];
        let sel = build_selection(&rows, &states);
        assert!(!sel.in_range.contains("keepme") && !sel.to_latest.contains("keepme"));
        assert_eq!(sel.in_range.iter().collect::<Vec<_>>(), vec!["rangey"]);
        assert_eq!(sel.to_latest.iter().collect::<Vec<_>>(), vec!["latesty"]);
        assert!(!sel.is_empty());
        assert!(build_selection(&rows, &[PickState::Keep; 3]).is_empty());

        // A latest pick on a downgrade-guarded row (latest cell duplicates
        // the range value) routes through the in-range machinery — applying
        // the real `latest` dist-tag there would downgrade past the version
        // the cell displayed.
        let guarded = vec![
            row(
                "pinned",
                "dependencies",
                "4.0.0-beta.90",
                Some("4.0.0-beta.100"),
                Some("0.64.0"),
            )
            .unwrap(),
        ];
        let sel = build_selection(&guarded, &[PickState::Latest]);
        assert_eq!(sel.in_range.iter().collect::<Vec<_>>(), vec!["pinned"]);
        assert!(sel.to_latest.is_empty());
    }

    #[test]
    fn state_cycle_walks_all_three_states_on_every_row() {
        // Even a downgrade-guarded row cycles through all three states —
        // its latest cell duplicates the range value instead of vanishing.
        for r in [
            row(
                "a",
                "dependencies",
                "4.0.0-beta.90",
                Some("4.0.0-beta.100"),
                Some("0.64.0"),
            )
            .unwrap(),
            row("b", "dependencies", "1.0.0", Some("1.5.0"), Some("2.0.0")).unwrap(),
        ] {
            assert_eq!(r.next_state(PickState::Keep), PickState::Range);
            assert_eq!(r.next_state(PickState::Range), PickState::Latest);
            assert_eq!(r.next_state(PickState::Latest), PickState::Keep);
            assert_eq!(r.prev_state(PickState::Keep), PickState::Latest);
        }
    }

    #[test]
    fn cycle_all_walks_keep_conservative_aggressive_in_unison() {
        let rows = vec![
            row("a", "dependencies", "1.0.0", Some("1.5.0"), Some("2.0.0")).unwrap(),
            // No in-range drift: the range phase is a no-op duplicate for
            // this row, but it still moves column-in-unison with the rest.
            row("b", "dependencies", "1.0.0", None, Some("3.0.0")).unwrap(),
        ];
        let mut states = vec![PickState::Keep; rows.len()];
        let visible: Vec<usize> = (0..rows.len()).collect();

        cycle_all(&rows, &mut states, &visible);
        assert_eq!(states, vec![PickState::Range, PickState::Range]);
        cycle_all(&rows, &mut states, &visible);
        assert_eq!(states, vec![PickState::Latest, PickState::Latest]);
        cycle_all(&rows, &mut states, &visible);
        assert_eq!(states, vec![PickState::Keep, PickState::Keep]);
    }

    /// The maintainer-set bar for this picker: every cell starts at the same
    /// column in every row — clean table columns, regardless of which cells
    /// a row actually has and how long its versions are.
    #[test]
    fn rows_render_as_aligned_columns() {
        let rows = vec![
            row("chalk", "dependencies", "4.1.2", None, Some("5.6.2")).unwrap(),
            row(
                "@effect/opentelemetry",
                "dependencies",
                "4.0.0-beta.99",
                Some("4.0.0-beta.100"),
                Some("0.64.0"),
            )
            .unwrap(),
            row(
                "semver",
                "dependencies",
                "7.5.4",
                Some("7.5.9"),
                Some("7.8.5"),
            )
            .unwrap(),
        ];
        let layout = Layout::of(&rows);
        let rendered: Vec<String> = rows
            .iter()
            .map(|r| {
                console::strip_ansi_codes(&format_row(r, PickState::Keep, &layout, false))
                    .into_owned()
            })
            .collect();
        // Expected column starts in CHARS (display cells), not bytes — the
        // `■`/`□` markers are multibyte, so byte offsets differ between a
        // row with a real cell and one with a blank-filled column even
        // when the columns line up on screen.
        let keep_col = 4 + layout.name_w + 2;
        let range_col = keep_col + layout.keep_col_w() + 2;
        let latest_col = range_col + layout.range_col_w() + 2;
        let boxes = |l: &str| -> Vec<usize> {
            l.chars()
                .enumerate()
                .filter(|(_, c)| *c == '■' || *c == '□')
                .map(|(i, _)| i)
                .collect()
        };
        // Every box sits exactly on its column start, and EVERY row has a
        // box in every column: the chalk row's range cell duplicates
        // `current` (no in-range drift) and the beta pin's latest cell
        // duplicates its range value (downgrade-guarded).
        for line in &rendered {
            assert_eq!(
                boxes(line),
                vec![keep_col, range_col, latest_col],
                "{rendered:#?}"
            );
        }
        // The heading labels sit on the same columns as the boxes below them.
        let header = header_line(&layout);
        let char_col = |l: &str, b: usize| l[..b].chars().count();
        assert_eq!(char_col(&header, header.find(HDR_KEEP).unwrap()), keep_col);
        assert_eq!(
            char_col(&header, header.find(HDR_RANGE).unwrap()),
            range_col
        );
        assert_eq!(
            char_col(&header, header.rfind(HDR_LATEST).unwrap()),
            latest_col
        );
    }
}
