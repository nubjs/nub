// Rules that decide what a download number MEANS. Run: node --test scripts/download-stats.test.mjs
import assert from "node:assert/strict";
import { test } from "node:test";
import {
  weekStart,
  rollupWeekly,
  rollupMonthly,
  projectMonth,
  isGapDay,
  daysInMonth,
  packageColumns,
  cumulative,
  assetKind,
  assetPlatform,
  snapshots,
  snapshotDeltas,
  allocateDeltasToDays,
  coveredSum,
  parseCsv,
  toCsv,
} from "./lib/download-stats.mjs";

test("weekStart anchors to the UTC Monday, including from a Sunday", () => {
  // Sunday is the off-by-one: getUTCDay() calls it 0, but it ENDS the ISO week.
  assert.equal(weekStart("2026-08-16"), "2026-08-10", "Sunday must fall back to its own Monday");
  assert.equal(weekStart("2026-08-17"), "2026-08-17", "a Monday is its own week start");
  assert.equal(weekStart("2026-08-19"), "2026-08-17");
});

test("rollupWeekly sums each series per week and flags a short week as partial", () => {
  const rows = [
    { day: "2026-08-14", counts: { a: 1, b: 10 } }, // Fri
    { day: "2026-08-15", counts: { a: 2, b: 20 } }, // Sat
    { day: "2026-08-17", counts: { a: 4, b: 40 } }, // Mon, next week
  ];
  const [w1, w2] = rollupWeekly(rows);
  assert.equal(w1.week_start, "2026-08-10");
  assert.deepEqual(w1.counts, { a: 3, b: 30 });
  assert.equal(w1.days, 2, "only two days observed");
  assert.equal(w1.complete, false, "a week with fewer than 7 observed days is partial");
  assert.equal(w1.week_end, "2026-08-15", "week_end is the last observed day, not the calendar Sunday");
  assert.equal(w2.week_start, "2026-08-17");
  assert.equal(w2.counts.a, 4);
});

test("rollupWeekly marks a week complete only at 7 observed days", () => {
  const rows = Array.from({ length: 7 }, (_, i) => ({
    day: `2026-08-${10 + i}`,
    counts: { a: 1 },
  }));
  const [w] = rollupWeekly(rows);
  assert.equal(w.complete, true, `7 days should be complete, got days=${w.days}`);
  assert.equal(w.counts.a, 7);
});

test("rollupMonthly buckets by calendar month and sizes each month correctly", () => {
  const rows = [
    { day: "2026-06-29", counts: { a: 1 } },
    { day: "2026-06-30", counts: { a: 2 } },
    { day: "2026-07-01", counts: { a: 4 } },
  ];
  const [jun, jul] = rollupMonthly(rows);
  assert.equal(jun.month_start, "2026-06-01");
  assert.equal(jun.counts.a, 3);
  assert.equal(jun.expected_days, 30, "June has 30 days, not a hardcoded 31");
  assert.equal(jun.complete, false, "2 observed days of 30 is not a complete month");
  assert.equal(jul.month_start, "2026-07-01");
  assert.equal(daysInMonth("2026-02-10"), 28, "non-leap February");
  assert.equal(daysInMonth("2028-02-10"), 29, "leap February");
});

test("isGapDay flags only a day that is zero across EVERY series", () => {
  assert.equal(isGapDay({ counts: { a: 0, b: 0 } }), true, "npm-wide reporting gap");
  assert.equal(isGapDay({ counts: { a: 0, b: 5 } }), false, "one series reporting means the day is real");
  assert.equal(isGapDay({ counts: {} }), false, "no series is not a gap");
});

test("projectMonth counts remaining days forward from the last observed day", () => {
  // The first month of a package's life is short because publishing started mid-month.
  // Those absent days are in the PAST — projecting them would invent history.
  const rows = ["2026-05-27", "2026-05-28", "2026-05-29", "2026-05-30", "2026-05-31"].map((day) => ({
    day,
    counts: { a: 100 },
  }));
  const [may] = rollupMonthly(rows);
  assert.equal(may.days, 5);
  assert.equal(may.complete, false, "5 of 31 days");
  assert.equal(
    projectMonth(may, rows, "a"),
    null,
    "the last observed day IS the month end, so there is nothing ahead to project",
  );
});

test("projectMonth extrapolates the remainder at the trailing daily rate", () => {
  // 10 days observed of a 31-day month, flat at 100/day -> 21 days left at 100.
  const rows = Array.from({ length: 10 }, (_, i) => ({
    day: `2026-08-0${i + 1}`.replace("08-010", "08-10"),
    counts: { a: 100 },
  }));
  const [aug] = rollupMonthly(rows);
  const p = projectMonth(aug, rows, "a");
  assert.equal(p.daysObserved, 10);
  assert.equal(p.daysRemaining, 21, "31 - 10");
  assert.equal(p.rate, 100);
  assert.equal(p.projected, 2100);
  assert.equal(p.total, 3100, "1000 measured + 2100 projected");
});

test("projectMonth keeps a reporting gap out of the rate", () => {
  // 7-day window with one npm-wide gap: the mean is over the 6 REPORTED days, so the
  // false zero cannot drag the forecast down.
  const rows = Array.from({ length: 10 }, (_, i) => ({
    day: `2026-08-${String(i + 1).padStart(2, "0")}`,
    counts: { a: i === 8 ? 0 : 100 }, // 2026-08-09 is the gap, inside the trailing window
  }));
  const [aug] = rollupMonthly(rows);
  const p = projectMonth(aug, rows, "a");
  assert.equal(p.window, 7);
  assert.equal(p.windowReported, 6, "the gap day is dropped from the mean");
  assert.equal(p.rate, 100, `a diluted mean would give 86, got ${p.rate}`);
  assert.equal(aug.gaps, 1, "the bucket still reports the gap so a reader can see it");
});

test("cumulative runs a per-series total across weeks", () => {
  const weekly = [{ counts: { a: 5 } }, { counts: { a: 7 } }, { counts: {} }];
  assert.deepEqual(cumulative(weekly, "a"), [5, 12, 12], "a missing series contributes zero, not NaN");
});

test("assetKind separates the checksum and launcher assets from real binaries", () => {
  // Installers fetch the .sha256 beside each binary; counting it overstates the
  // channel by roughly half. Launcher templates are pulled by `nub compile`.
  assert.equal(assetKind("nub-linux-x64.tar.gz"), "binary");
  assert.equal(assetKind("nub-win32-x64.zip"), "binary");
  assert.equal(assetKind("nub-linux-x64.tar.gz.sha256"), "checksum");
  assert.equal(assetKind("nub-launcher-linux-x64"), "launcher");
});

test("assetPlatform reads the platform off both archive shapes and nothing else", () => {
  assert.equal(assetPlatform("nub-darwin-arm64.tar.gz"), "darwin-arm64");
  assert.equal(assetPlatform("nub-linux-x64-musl.tar.gz"), "linux-x64-musl");
  assert.equal(assetPlatform("nub-win32-arm64.zip"), "win32-arm64");
  assert.equal(assetPlatform("nub-linux-x64.tar.gz.sha256"), null, "a checksum is not a platform binary");
});

test("snapshots totals binaries per snapshot date and never folds checksums in", () => {
  const rows = [
    { snapshot_date: "2026-08-01", tag: "v1", asset: "nub-linux-x64.tar.gz", download_count: "100" },
    { snapshot_date: "2026-08-01", tag: "v1", asset: "nub-linux-x64.tar.gz.sha256", download_count: "90" },
    { snapshot_date: "2026-08-01", tag: "v2", asset: "nub-win32-x64.zip", download_count: "5" },
    { snapshot_date: "2026-08-08", tag: "v1", asset: "nub-linux-x64.tar.gz", download_count: "160" },
  ];
  const [first, second] = snapshots(rows);
  assert.equal(first.binary, 105, "binaries only");
  assert.equal(first.checksum, 90, "checksums are counted but kept separate");
  assert.equal(first.releases, 2, "two distinct tags");
  assert.deepEqual(first.byPlatform, { "linux-x64": 100, "win32-x64": 5 });
  assert.equal(second.date, "2026-08-08", "snapshots come back oldest first");
});

test("snapshotDeltas needs two snapshots and never reports a negative interval", () => {
  const one = [{ date: "2026-08-01", binary: 100 }];
  assert.deepEqual(snapshotDeltas(one), [], "one cumulative snapshot is not a time series");

  const three = [
    { date: "2026-08-01", binary: 100 },
    { date: "2026-08-08", binary: 160 },
    { date: "2026-08-15", binary: 150 }, // a deleted release can walk the counter back
  ];
  const d = snapshotDeltas(three);
  assert.deepEqual(
    d.map((x) => [x.from, x.to, x.days, x.binary]),
    [
      ["2026-08-01", "2026-08-08", 7, 60],
      ["2026-08-08", "2026-08-15", 7, 0],
    ],
    "a counter that walks backwards clamps to 0 rather than a negative download count",
  );
});

test("packageColumns excludes the derived columns, cumulative above all", () => {
  // Regression: an `@`-prefix filter also matched `@nubjs/nub_cumulative`, admitting a
  // RUNNING TOTAL as a series. A cumulative column is never zero, so isGapDay() could
  // never fire and every npm reporting gap was scored as a real zero-download day.
  const cols = ["date", "@nubjs/nub", "@nubjs/nub-linux-x64", "platforms_total", "@nubjs/nub_cumulative"];
  assert.deepEqual(packageColumns(cols), ["@nubjs/nub", "@nubjs/nub-linux-x64"]);
  assert.equal(
    packageColumns(cols).includes("@nubjs/nub_cumulative"),
    false,
    "a running total is not a series and must never reach isGapDay",
  );
});

test("allocateDeltasToDays spreads an interval over the days it covers, exclusive of `from`", () => {
  const perDay = allocateDeltasToDays([{ from: "2026-08-20", to: "2026-08-26", days: 6, binary: 600 }]);
  assert.equal(perDay.size, 6);
  assert.equal(perDay.get("2026-08-26"), 100, "the `to` day is covered");
  assert.equal(perDay.get("2026-08-21"), 100, "six days back from `to`");
  assert.equal(
    perDay.get("2026-08-20"),
    undefined,
    "`from` belongs to the PREVIOUS snapshot and must not be counted twice",
  );
});

test("coveredSum reports a period only when every one of its days is covered", () => {
  // The real first interval straddled a week boundary: 6 days over two weeks, so neither
  // week is whole. Reporting either would compare a partial GitHub window against a full
  // npm week.
  const perDay = allocateDeltasToDays([{ from: "2026-08-20", to: "2026-08-26", days: 6, binary: 600 }]);
  assert.equal(coveredSum("2026-08-17", 7, perDay), null, "only 3 of 7 days covered");
  assert.equal(coveredSum("2026-08-24", 7, perDay), null, "only 3 of 7 days covered");

  const full = allocateDeltasToDays([{ from: "2026-08-16", to: "2026-08-23", days: 7, binary: 700 }]);
  assert.equal(coveredSum("2026-08-17", 7, full), 700, "a fully covered week reports its sum");
});

test("parseCsv and toCsv round-trip a table", () => {
  const csv = toCsv(["date", "@nubjs/nub", "complete"], [["2026-08-10", 38703, true]]);
  assert.equal(csv, "date,@nubjs/nub,complete\n2026-08-10,38703,true\n");
  const [row] = parseCsv(csv);
  assert.equal(row["@nubjs/nub"], "38703", "scoped package names survive as column keys");
  assert.equal(row.complete, "true");
});
