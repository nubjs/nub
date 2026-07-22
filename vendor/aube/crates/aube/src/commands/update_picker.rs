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

/// One pickable dependency. `range_target` / `latest_target` are only
/// populated when they represent a real, non-downgrade change from
/// `current`; a row with neither is filtered out by [`build_row`].
#[derive(Debug, Clone)]
pub(crate) struct PickerRow {
    pub key: String,
    pub bucket: &'static str,
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
/// - `range_target` = the in-range max when it differs from `current` and
///   is not a semver downgrade (a lockfile pinned above the manifest range
///   would otherwise get a downgrade offered as "range"). When either side
///   fails to parse, plain inequality decides — the resolver may still
///   move it, so hiding it would lie.
/// - `latest_target` = the `latest` dist-tag when it is a strict semver
///   upgrade over `current`. A `latest` at-or-below `current` (the
///   prerelease-pin case: current `4.0.0-beta.59`, latest `0.64.0`) is
///   dropped rather than offered as a downgrade — the per-cell form of the
///   `preserve_pin` guard. An unparseable `current` also drops the cell,
///   since the ordering can't be established.
/// - When both targets are the same version the row collapses to a single
///   `latest` cell (the more informative label).
pub(crate) fn build_row(
    key: &str,
    bucket: &'static str,
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
    let mut range_target = wanted
        .filter(|w| *w != current && !semver_downgrade(w))
        .map(str::to_owned);
    let latest_target = latest
        .filter(|l| {
            let (Ok(cur), Ok(new)) = (
                node_semver::Version::parse(current),
                node_semver::Version::parse(l),
            ) else {
                return false;
            };
            new > cur
        })
        .map(str::to_owned);
    if latest_target.is_some() && latest_target == range_target {
        range_target = None;
    }
    if range_target.is_none() && latest_target.is_none() {
        return None;
    }
    Some(PickerRow {
        key: key.to_string(),
        bucket,
        current: current.to_string(),
        range_target,
        latest_target,
    })
}

impl PickerRow {
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
            name_w: max(&mut rows.iter().map(|r| r.key.len())),
            cur_w: max(&mut rows.iter().map(|r| r.current.len())),
            range_w: max(&mut rows
                .iter()
                .filter_map(|r| r.range_target.as_ref().map(String::len))),
            latest_w: max(&mut rows
                .iter()
                .filter_map(|r| r.latest_target.as_ref().map(String::len))),
        }
    }

    // Full raw cell DISPLAY widths (glyph + label + padded version), zero
    // when no row carries that cell so the column is omitted entirely.
    // Counted in chars, not bytes — the `■`/`□` glyphs are multibyte.
    fn range_cell_w(&self) -> usize {
        if self.range_w == 0 {
            0
        } else {
            "□ range ".chars().count() + self.range_w
        }
    }
    fn latest_cell_w(&self) -> usize {
        if self.latest_w == 0 {
            0
        } else {
            "□ latest ".chars().count() + self.latest_w
        }
    }
}

/// Marker color for a selected update cell: the same severity the version
/// tail carries (pnpm's colorize-semver-diff palette), so the filled box
/// reads as "how big is this jump" at a glance — the treatment bun's
/// interactive updater uses for its checkbox. Falls back to plain when
/// either side doesn't parse.
fn severity_box(current: &str, target: &str) -> String {
    use clx::style;
    let (Ok(cur), Ok(new)) = (
        node_semver::Version::parse(current),
        node_semver::Version::parse(target),
    ) else {
        return "■".to_string();
    };
    let styled = style::estyle("■");
    if cur.major != new.major {
        styled.red()
    } else if cur.minor != new.minor {
        styled.cyan()
    } else if cur.patch != new.patch {
        styled.green()
    } else {
        styled.magenta()
    }
    .to_string()
}

/// Render one radio cell. Selected cells carry a filled box (`■`, colored
/// by bump severity for update cells — bun's checkbox glyphs); unselected
/// cells get a dimmed hollow box and label. The version keeps whatever
/// coloring the caller computed (semver-diff colors carry meaning
/// regardless of selection).
fn cell(label: &str, version: &str, selected: bool, marker: Option<&str>) -> String {
    use clx::style;
    if selected {
        format!("{} {label} {version}", marker.unwrap_or("■"))
    } else {
        format!(
            "{} {} {version}",
            style::estyle("□").dim(),
            style::estyle(label).dim(),
        )
    }
}

fn format_row(row: &PickerRow, state: PickState, layout: &Layout, focused: bool) -> String {
    use clx::style;
    let cursor = if focused {
        format!("{} ", style::ecyan("❯"))
    } else {
        "  ".to_string()
    };
    let name = format!("{:<w$}", row.key, w = layout.name_w);
    let current_padded = format!("{:<w$}", row.current, w = layout.cur_w);
    let keep_selected = state == PickState::Keep;
    let keep_version = if keep_selected {
        current_padded
    } else {
        style::estyle(current_padded).dim().to_string()
    };
    let mut line = format!(
        "{cursor}  {name}  {}",
        cell("keep", &keep_version, keep_selected, None)
    );
    if layout.range_cell_w() > 0 {
        line.push_str("  ");
        match &row.range_target {
            Some(target) => {
                let version =
                    super::outdated::colorize_diff(&row.current, target, layout.range_w, true);
                let marker = severity_box(&row.current, target);
                line.push_str(&cell(
                    "range",
                    &version,
                    state == PickState::Range,
                    Some(&marker),
                ));
            }
            None => line.push_str(&" ".repeat(layout.range_cell_w())),
        }
    }
    if layout.latest_cell_w() > 0 {
        line.push_str("  ");
        match &row.latest_target {
            Some(target) => {
                let version =
                    super::outdated::colorize_diff(&row.current, target, layout.latest_w, true);
                let marker = severity_box(&row.current, target);
                line.push_str(&cell(
                    "latest",
                    &version,
                    state == PickState::Latest,
                    Some(&marker),
                ));
            }
            None => line.push_str(&" ".repeat(layout.latest_cell_w())),
        }
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
fn build_selection(rows: &[PickerRow], states: &[PickState]) -> PickerSelection {
    let mut selection = PickerSelection::default();
    for (row, state) in rows.iter().zip(states) {
        match state {
            PickState::Keep => {}
            PickState::Range => {
                selection.in_range.insert(row.key.clone());
            }
            PickState::Latest => {
                selection.to_latest.insert(row.key.clone());
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
        build_row(key, bucket, current, wanted, latest)
    }

    #[test]
    fn build_row_drops_rows_without_drift() {
        assert!(row("a", "dependencies", "1.2.3", Some("1.2.3"), Some("1.2.3")).is_none());
        assert!(row("a", "dependencies", "1.2.3", None, None).is_none());
    }

    #[test]
    fn build_row_collapses_equal_targets_to_latest() {
        let r = row("a", "dependencies", "1.2.3", Some("2.0.0"), Some("2.0.0")).unwrap();
        assert_eq!(r.range_target, None);
        assert_eq!(r.latest_target.as_deref(), Some("2.0.0"));
    }

    #[test]
    fn build_row_hides_latest_downgrade_for_prerelease_pin() {
        // The bun screenshot case: current 4.0.0-beta.59, latest dist-tag
        // 0.64.0 — the latest cell must not offer the downgrade, while the
        // in-range prerelease refresh is still offered.
        let r = row(
            "@effect/opentelemetry",
            "dependencies",
            "4.0.0-beta.59",
            Some("4.0.0-beta.60"),
            Some("0.64.0"),
        )
        .unwrap();
        assert_eq!(r.range_target.as_deref(), Some("4.0.0-beta.60"));
        assert_eq!(r.latest_target, None);
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
        // Unparseable current: latest is dropped (no ordering), but the
        // range cell stays on plain inequality — the resolver may move it.
        let r = row("a", "dependencies", "git-pin", Some("1.2.3"), Some("9.9.9")).unwrap();
        assert_eq!(r.range_target.as_deref(), Some("1.2.3"));
        assert_eq!(r.latest_target, None);
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
    }

    #[test]
    fn state_cycle_wraps_and_skips_missing_states() {
        let r = row("a", "dependencies", "1.0.0", None, Some("2.0.0")).unwrap();
        assert_eq!(r.next_state(PickState::Keep), PickState::Latest);
        assert_eq!(r.next_state(PickState::Latest), PickState::Keep);
        assert_eq!(r.prev_state(PickState::Keep), PickState::Latest);

        let both = row("b", "dependencies", "1.0.0", Some("1.5.0"), Some("2.0.0")).unwrap();
        assert_eq!(both.next_state(PickState::Keep), PickState::Range);
        assert_eq!(both.next_state(PickState::Range), PickState::Latest);
        assert_eq!(both.next_state(PickState::Latest), PickState::Keep);
        assert_eq!(both.prev_state(PickState::Keep), PickState::Latest);
    }

    #[test]
    fn cycle_all_walks_keep_conservative_aggressive() {
        let rows = vec![
            row("a", "dependencies", "1.0.0", Some("1.5.0"), Some("2.0.0")).unwrap(),
            row("b", "dependencies", "1.0.0", None, Some("3.0.0")).unwrap(),
        ];
        let mut states = vec![PickState::Keep; rows.len()];
        let visible: Vec<usize> = (0..rows.len()).collect();

        cycle_all(&rows, &mut states, &visible);
        assert_eq!(states, vec![PickState::Range, PickState::Latest]);
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
        // Column positions in CHARS (display cells), not bytes — the
        // `■`/`□` markers are multibyte, so byte offsets differ between a
        // row with a real cell and one with a blank-filled column even
        // when the columns line up on screen.
        let char_col = |l: &str, needle: &str| -> Option<usize> {
            l.find(needle).map(|b| l[..b].chars().count())
        };
        let keep_cols: Vec<usize> = rendered
            .iter()
            .map(|l| char_col(l, " keep ").unwrap())
            .collect();
        assert!(keep_cols.windows(2).all(|w| w[0] == w[1]), "{rendered:#?}");
        // range and latest cells each occupy their own fixed column; a row
        // without a range cell blank-fills it so its latest cell still lands
        // in the latest column.
        let range_col = char_col(&rendered[2], " range ").unwrap();
        let latest_cols: Vec<usize> = rendered
            .iter()
            .filter_map(|l| char_col(l, " latest "))
            .collect();
        assert_eq!(latest_cols.len(), 2);
        assert!(
            latest_cols.windows(2).all(|w| w[0] == w[1]),
            "{rendered:#?}"
        );
        assert!(range_col < latest_cols[0]);
    }
}
