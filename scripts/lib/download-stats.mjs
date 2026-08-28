// Pure data logic for nub's download statistics, shared by scripts/download-stats.mjs
// (the CLI) and scripts/lib/download-chart.mjs (the renderer). Everything here is
// side-effect free and unit-tested in scripts/download-stats.test.mjs, so the
// aggregation rules that decide what a number MEANS live in one place:
//
//   - Weeks are UTC Monday-anchored and carry a `complete` flag. npm's counts lag
//     ~2 days, so the trailing week is always short; quoting it as a weekly number
//     reads as a cliff that never happened. Headline figures use the last COMPLETE week.
//   - GitHub asset counters are cumulative-only, and every release ships a `.sha256`
//     beside each tarball that installers also fetch. Summing raw asset counts
//     overstates the channel by ~50%, so assets are classified before they are added.

const DAY_MS = 86_400_000;

export const isDay = (s) => /^\d{4}-\d{2}-\d{2}$/.test(s);

export function addDays(day, n) {
  return new Date(Date.parse(`${day}T00:00:00Z`) + n * DAY_MS).toISOString().slice(0, 10);
}

/** UTC Monday of the week containing `day`. */
export function weekStart(day) {
  const dow = (new Date(`${day}T00:00:00Z`).getUTCDay() + 6) % 7; // 0 = Monday
  return addDays(day, -dow);
}

/**
 * Roll daily rows into Monday-anchored weeks.
 * @param rows [{ day: "YYYY-MM-DD", counts: { [series]: number } }]
 * @returns [{ week_start, week_end, days, complete, counts }]
 */
export function rollupWeekly(rows) {
  const weeks = new Map();
  for (const { day, counts } of rows) {
    const start = weekStart(day);
    let w = weeks.get(start);
    if (!w) weeks.set(start, (w = { week_start: start, week_end: day, days: 0, gaps: 0, counts: {} }));
    w.week_end = day > w.week_end ? day : w.week_end;
    w.days += 1;
    if (isGapDay({ counts })) w.gaps += 1;
    for (const [k, v] of Object.entries(counts)) w.counts[k] = (w.counts[k] ?? 0) + v;
  }
  return [...weeks.values()]
    .sort((a, b) => a.week_start.localeCompare(b.week_start))
    .map((w) => ({ ...w, complete: w.days === 7 }));
}

/**
 * npm occasionally reports a day as zero for EVERY package at once — a batch-job gap,
 * not a day on which nobody installed anything. Measured on this series: 2026-07-12 and
 * 2026-08-14 both read 0 across all nine packages while their neighbours ran 5k-9k.
 *
 * Such a day is MISSING, not zero. It is never imputed into a total (that would invent
 * downloads), but it must be kept out of any rate or mean, where a false zero silently
 * biases the answer down — it cost the August projection 6% before this existed. The
 * test is deliberately strict: every series zero. A day where only the meta package
 * reads zero while platform packages report is left alone, being the ambiguous case.
 */
export function isGapDay(row) {
  const vals = Object.values(row.counts);
  return vals.length > 0 && vals.every((v) => v === 0);
}

/** First day of the calendar month containing `day`. */
export function monthStart(day) {
  return `${day.slice(0, 7)}-01`;
}

export function daysInMonth(day) {
  const [y, m] = day.split("-").map(Number);
  return new Date(Date.UTC(y, m, 0)).getUTCDate();
}

/**
 * Roll daily rows into calendar months (UTC), mirroring rollupWeekly's shape so the
 * renderer can draw either granularity from one code path.
 */
export function rollupMonthly(rows) {
  const months = new Map();
  for (const { day, counts } of rows) {
    const start = monthStart(day);
    let m = months.get(start);
    if (!m) months.set(start, (m = { month_start: start, month_end: day, days: 0, gaps: 0, counts: {} }));
    m.month_end = day > m.month_end ? day : m.month_end;
    m.days += 1;
    if (isGapDay({ counts })) m.gaps += 1;
    for (const [k, v] of Object.entries(counts)) m.counts[k] = (m.counts[k] ?? 0) + v;
  }
  return [...months.values()]
    .sort((a, b) => a.month_start.localeCompare(b.month_start))
    .map((m) => ({ ...m, expected_days: daysInMonth(m.month_start), complete: m.days === daysInMonth(m.month_start) }));
}

/**
 * Project a month-in-progress to its full-month total.
 *
 * Two choices here are load-bearing and both differ from the obvious approach:
 *
 * 1. Remaining days are counted from the LAST OBSERVED DAY to the month end, never as
 *    `expected_days - observed_days`. The first month of a package's life is short
 *    because publishing started mid-month, and those missing days are in the PAST —
 *    the naive subtraction would "project" 26 days that already happened and roughly
 *    sextuple the figure. This form yields 0 for such a month, which is correct.
 * 2. The run rate is the trailing 7-day mean, not the month-to-date mean. npm traffic
 *    is heavily CI-driven, so weekdays run far above weekends; a 7-day window holds
 *    exactly one of each weekday and cancels that cycle, while a month-to-date mean
 *    inherits whatever weekday mix the elapsed part happened to contain.
 *
 * Returns null when the month needs no projection (already complete, or its last
 * observed day IS the month end).
 */
export function projectMonth(bucket, dailyRows, series, window = 7) {
  const lastDay = Number(bucket.month_end.slice(8, 10));
  const daysRemaining = bucket.expected_days - lastDay;
  if (daysRemaining <= 0) return null;

  // The window stays a CALENDAR window (so it holds one of each weekday), but the mean
  // is taken only over the days npm actually reported — a gap day dilutes, never counts.
  const trailing = dailyRows.slice(-Math.min(window, dailyRows.length));
  const reported = trailing.filter((r) => !isGapDay(r));
  if (!reported.length) return null;
  const rate = reported.reduce((sum, r) => sum + (r.counts[series] ?? 0), 0) / reported.length;
  const measured = bucket.counts[series] ?? 0;
  const projected = Math.round(rate * daysRemaining);
  return {
    measured,
    projected,
    total: measured + projected,
    daysRemaining,
    daysObserved: lastDay,
    rate: Math.round(rate),
    window: trailing.length,
    windowReported: reported.length,
  };
}

/** Running total of `series` across buckets (weeks or months), oldest first. */
export function cumulative(buckets, series) {
  let run = 0;
  return buckets.map((b) => (run += b.counts[series] ?? 0));
}

// --- GitHub release assets -------------------------------------------------

/**
 * Every release ships one `.sha256` per tarball, which installers fetch alongside
 * the binary; `nub-launcher-*` templates are pulled by `nub compile` to cross-compile,
 * not by anyone installing nub. Only `binary` counts as an install.
 */
export function assetKind(name) {
  if (name.endsWith(".sha256")) return "checksum";
  if (name.startsWith("nub-launcher-")) return "launcher";
  return "binary";
}

/** "nub-darwin-arm64.tar.gz" -> "darwin-arm64"; null for anything else. */
export function assetPlatform(name) {
  const m = /^nub-(.+?)\.(?:tar\.gz|zip)$/.exec(name);
  return m ? m[1] : null;
}

/**
 * Collapse the per-asset snapshot rows into one total per snapshot date.
 * @param rows [{ snapshot_date, tag, asset, download_count }]
 */
export function snapshots(rows) {
  const byDate = new Map();
  for (const r of rows) {
    let s = byDate.get(r.snapshot_date);
    if (!s) {
      s = { date: r.snapshot_date, binary: 0, checksum: 0, launcher: 0, byPlatform: {}, tags: new Set() };
      byDate.set(r.snapshot_date, s);
    }
    const n = Number(r.download_count);
    s[assetKind(r.asset)] += n;
    s.tags.add(r.tag);
    const plat = assetKind(r.asset) === "binary" && assetPlatform(r.asset);
    if (plat) s.byPlatform[plat] = (s.byPlatform[plat] ?? 0) + n;
  }
  return [...byDate.values()]
    .sort((a, b) => a.date.localeCompare(b.date))
    .map((s) => ({ ...s, releases: s.tags.size, tags: undefined }));
}

/**
 * Deltas between consecutive snapshots — the only way a GitHub time series exists,
 * since the API exposes cumulative counters and no history. One snapshot yields none.
 */
export function snapshotDeltas(snaps) {
  const out = [];
  for (let i = 1; i < snaps.length; i++) {
    const [a, b] = [snaps[i - 1], snaps[i]];
    out.push({
      from: a.date,
      to: b.date,
      days: Math.round((Date.parse(b.date) - Date.parse(a.date)) / DAY_MS),
      // A release deleted between snapshots (or the canary being recreated) can drive
      // a counter backwards; clamp so a negative never lands in a "downloads" column.
      binary: Math.max(0, b.binary - a.binary),
    });
  }
  return out;
}

/**
 * Spread each snapshot interval's downloads evenly across the days it covers.
 *
 * Snapshot intervals do not respect week boundaries — a run on the 20th and another on
 * the 26th produces one 6-day interval straddling two weeks. Attributing the whole thing
 * to either week silently compares a 6-day GitHub figure against a 2-day npm figure and
 * calls the sum a weekly total. Allocating per-day makes the two channels commensurable;
 * the uniform split is an assumption, which is why weeklyGithub() below will only report
 * a week whose every day an interval actually covers.
 *
 * An interval (from, to] covers the `days` days ending at `to` — `from` itself was
 * already counted by the previous snapshot.
 */
export function allocateDeltasToDays(deltas) {
  const perDay = new Map();
  for (const d of deltas) {
    if (d.days <= 0) continue;
    const rate = d.binary / d.days;
    for (let i = 0; i < d.days; i++) perDay.set(addDays(d.to, -i), rate);
  }
  return perDay;
}

/**
 * A period's GitHub downloads, or null when ANY of its days is unmeasured. Null is the
 * honest answer: a partial period reported as a number reads as a whole one.
 */
export function coveredSum(startDay, dayCount, perDay) {
  let sum = 0;
  for (let i = 0; i < dayCount; i++) {
    const v = perDay.get(addDays(startDay, i));
    if (v === undefined) return null;
    sum += v;
  }
  return Math.round(sum);
}

// --- CSV -------------------------------------------------------------------

// nub's own columns are dates, plain integers and package names — none can contain a
// comma or quote — so a split is sufficient and a quoting parser would be dead code.
export function parseCsv(text) {
  const [head, ...lines] = text.trim().split("\n");
  const cols = head.split(",");
  return lines.filter(Boolean).map((l) => Object.fromEntries(l.split(",").map((v, i) => [cols[i], v])));
}

/**
 * The real package columns of npm-daily.csv, which is `date, <pkgs...>, platforms_total,
 * <meta>_cumulative`. Selecting them by position rather than by an `@` prefix is
 * load-bearing: `@nubjs/nub_cumulative` also starts with `@`, and admitting a RUNNING
 * TOTAL as a series is silently corrupting — a cumulative column is never zero, so
 * isGapDay() can never fire and every reporting gap is treated as a real zero. That
 * bug made a re-render disagree with a fresh fetch by 6% with nothing to show for it.
 */
export function packageColumns(cols) {
  const end = cols.indexOf("platforms_total");
  return cols.slice(1, end === -1 ? cols.length : end);
}

export function toCsv(header, rows) {
  return [header.join(","), ...rows.map((r) => r.join(","))].join("\n") + "\n";
}
