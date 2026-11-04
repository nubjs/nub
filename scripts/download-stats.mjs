#!/usr/bin/env node
// Download-stats puller for nub's distribution channels. Fetches each channel,
// writes clean CSVs plus a machine-readable summary, and renders a chart.
//
//   node scripts/download-stats.mjs                 # fetch everything, write CSVs + chart
//   node scripts/download-stats.mjs --npm-only      # skip the gh-authenticated GitHub half
//   node scripts/download-stats.mjs --chart-only    # re-render from the CSVs, no network
//   node scripts/download-stats.mjs --no-chart --out <dir> --package <pkg> --repo <owner/repo>
//
// THE CHANNEL MODEL — what may be added to what, and why. Verified against
// npm/nub/package.json, npm/nub/postinstall.js, install.sh and crates/nub-cli/src/cli.rs:
//
//   install path                    @nubjs/nub   @nubjs/nub-<plat>   GitHub asset
//   npm i -g @nubjs/nub                  1              1                 0
//   npm i -g --omit=optional             1              0                 0   (broken install)
//   npm ci from a lockfile               1            1..8                0
//   pnpm w/ supportedArchitectures       1              N                 0
//   curl install.sh                      0              0                 1   (+1 .sha256)
//   nub upgrade                          0              0                 1   (+1 .sha256)
//   brew install nubjs/tap/nub           0              0                 1
//
// So the ONLY unduplicated total is `@nubjs/nub` + GitHub binary assets. The platform
// packages are pulled BY an npm install, never instead of one — adding them to the meta
// package counts the same install twice. They are not even a reliable OS split: a
// multi-arch Docker build, a `supportedArchitectures` pnpm install, or a registry mirror
// caching every optionalDependency fetches several per install, which is why
// platforms_total runs 1.3-6x the meta count. They are written out for reference and
// deliberately excluded from every total.
//
// Nothing installs the binary outside those channels: postinstall.js only chmods and
// re-links shims, and bin/launch.js makes no network call, so an npm install always
// rides a platform package. The rolling `canary` prerelease is excluded — it is recreated
// by every nightly canary build, so its counters reset and would corrupt the snapshot deltas.
//
// These are DOWNLOADS, not users and not installs: an upgrade re-downloads, and npm
// counts CI, mirrors and bots. Needs network; `gh` authenticated (GitHub channel only).

import { execFileSync } from "node:child_process";
import { mkdirSync, writeFileSync, existsSync, readFileSync, rmSync } from "node:fs";
import { join } from "node:path";
import {
  rollupWeekly,
  rollupMonthly,
  projectMonth,
  cumulative,
  snapshots,
  snapshotDeltas,
  addDays,
  allocateDeltasToDays,
  coveredSum,
  weekStart,
  packageColumns,
  parseCsv,
  toCsv,
} from "./lib/download-stats.mjs";
import { renderChart } from "./lib/download-chart.mjs";

const args = process.argv.slice(2);
const flag = (name) => {
  const i = args.indexOf(name);
  return i === -1 ? undefined : args[i + 1];
};
const has = (name) => args.includes(name);

const META = flag("--package") ?? "@nubjs/nub";
const OUT = flag("--out") ?? "tmp/download-stats";
const REPO = flag("--repo") ?? "nubjs/nub";
const DATA_BRANCH = flag("--data-branch") ?? "download-stats-data";
const LEDGER_DIR = "stats/download-counts/";
const today = new Date().toISOString().slice(0, 10);
const chartOnly = has("--chart-only");

mkdirSync(OUT, { recursive: true });

const getJSON = async (url) => {
  const res = await fetch(url);
  if (!res.ok) throw new Error(`${res.status} ${url}`);
  return res.json();
};

// --- npm -------------------------------------------------------------------

async function fetchNpm() {
  const doc = await getJSON(`https://registry.npmjs.org/${META}`);
  const created = (flag("--start") ?? doc.time.created).slice(0, 10);
  // The platform packages are exactly the meta package's optionalDependencies;
  // @nubjs/nub-types is a separate devDep and correctly absent from this list.
  const platforms = Object.keys(
    doc.versions[doc["dist-tags"].latest].optionalDependencies ?? {},
  ).sort();
  const pkgs = [META, ...platforms];

  // The range API rejects windows over ~18 months; stitch 500-day chunks.
  const chunks = [];
  for (let d = new Date(created); ; ) {
    const start = d.toISOString().slice(0, 10);
    d = new Date(d.getTime() + 500 * 86400e3);
    const end = d <= new Date() ? d.toISOString().slice(0, 10) : today;
    chunks.push([start, end]);
    if (end === today) break;
    d = new Date(d.getTime() + 86400e3);
  }

  const perPkg = {};
  for (const pkg of pkgs) {
    perPkg[pkg] = new Map();
    for (const [start, end] of chunks) {
      const r = await getJSON(`https://api.npmjs.org/downloads/range/${start}:${end}/${pkg}`);
      for (const { day, downloads } of r.downloads ?? []) perPkg[pkg].set(day, downloads);
    }
  }

  // Trim leading zeros (pre-first-publish padding) and trailing zeros (npm's ~2-day
  // reporting lag) — interior zero days are real and kept.
  let days = [...perPkg[META].keys()].sort();
  const total = (day) => pkgs.reduce((s, p) => s + (perPkg[p].get(day) ?? 0), 0);
  while (days.length && total(days[0]) === 0) days.shift();
  while (days.length && total(days[days.length - 1]) === 0) days.pop();

  return {
    pkgs,
    platforms,
    rows: days.map((day) => ({
      day,
      counts: Object.fromEntries(pkgs.map((p) => [p, perPkg[p].get(day) ?? 0])),
    })),
  };
}

function writeNpm({ pkgs, platforms, rows }) {
  const platTotal = (c) => platforms.reduce((s, p) => s + c[p], 0);

  let cum = 0;
  const daily = rows.map(({ day, counts }) => [
    day,
    ...pkgs.map((p) => counts[p]),
    platTotal(counts),
    (cum += counts[META]),
  ]);
  writeFileSync(
    join(OUT, "npm-daily.csv"),
    toCsv(["date", ...pkgs, "platforms_total", `${META}_cumulative`], daily),
  );

  const weekly = rollupWeekly(rows);
  const cums = cumulative(weekly, META);
  writeFileSync(
    join(OUT, "npm-weekly.csv"),
    toCsv(
      ["week_start", "week_end", "days", "gaps", "complete", META, "platforms_total", `${META}_cumulative`],
      weekly.map((w, i) => [
        w.week_start,
        w.week_end,
        w.days,
        w.gaps,
        w.complete,
        w.counts[META],
        platTotal(w.counts),
        cums[i],
      ]),
    ),
  );
  const monthly = rollupMonthly(rows);
  const mcums = cumulative(monthly, META);
  // Only the FINAL month can be in progress; every earlier short month is history.
  const projection = projectMonth(monthly.at(-1), rows, META);
  writeFileSync(
    join(OUT, "npm-monthly.csv"),
    toCsv(
      ["month_start", "month_end", "days", "expected_days", "gaps", "complete", META, "platforms_total", `${META}_cumulative`, "projected_remainder", "projected_total"],
      monthly.map((mo, i) => [
        mo.month_start,
        mo.month_end,
        mo.days,
        mo.expected_days,
        mo.gaps,
        mo.complete,
        mo.counts[META],
        platTotal(mo.counts),
        mcums[i],
        i === monthly.length - 1 && projection ? projection.projected : "",
        i === monthly.length - 1 && projection ? projection.total : "",
      ]),
    ),
  );

  return { weekly, monthly, projection, allTime: cum, first: rows[0].day, last: rows.at(-1).day };
}

// --- GitHub release assets -------------------------------------------------

// The GitHub half is NOT fetched live. `.github/workflows/download-stats.yml` has taken a
// daily snapshot since 2026-06-24 and commits it to the `download-stats-data` branch —
// GitHub's counters are cumulative with no history API, so that ledger is the ONLY record
// of what happened on any past day, and it cannot be rebuilt if it is lost or ignored.
// Reading it gives real per-day deltas; taking a fresh snapshot here would give exactly
// one data point and silently start a second, poorer series alongside it.
function readLedger() {
  try {
    execFileSync("git", ["fetch", "origin", DATA_BRANCH], { stdio: "ignore" });
  } catch {
    // Offline, or no such remote — fall through to whatever ref is already local.
  }
  let files;
  try {
    files = execFileSync(
      "git",
      ["ls-tree", "-r", "--name-only", `origin/${DATA_BRANCH}`, "--", LEDGER_DIR],
      { encoding: "utf8" },
    );
  } catch {
    throw new Error(
      `cannot read origin/${DATA_BRANCH}; fetch it or pass --npm-only to skip the GitHub half`,
    );
  }
  const snapshotFiles = files
    .trim()
    .split("\n")
    .filter((f) => /\d{8}T\d{6}Z\.json$/.test(f)) // skip README, report.mjs, backfill-*
    .sort();

  const rows = [];
  for (const f of snapshotFiles) {
    const doc = JSON.parse(
      execFileSync("git", ["show", `origin/${DATA_BRANCH}:${f}`], {
        encoding: "utf8",
        maxBuffer: 64 * 1024 * 1024,
      }),
    );
    const date = doc.timestamp.slice(0, 10);
    for (const rel of doc.github_releases ?? [])
      for (const a of rel.assets ?? [])
        rows.push({
          snapshot_date: date,
          tag: rel.tag,
          asset: a.name,
          download_count: a.download_count,
        });
  }
  return rows;
}

function writeGithub(rows) {
  if (!rows) return { snaps: [], deltas: [], latest: undefined };
  writeFileSync(
    join(OUT, "github-releases.csv"),
    toCsv(
      ["snapshot_date", "tag", "asset", "download_count"],
      rows.map((r) => [r.snapshot_date, r.tag, r.asset, r.download_count]),
    ),
  );

  const snaps = snapshots(rows);
  const deltas = snapshotDeltas(snaps);
  // Both superseded: this script no longer keeps its own snapshot file, and the old
  // `github-weekly.csv` mis-stated week boundaries. Remove rather than leave stale,
  // wrong-shaped files beside the correct ones.
  rmSync(join(OUT, "github-weekly.csv"), { force: true });
  if (deltas.length) {
    writeFileSync(
      join(OUT, "github-intervals.csv"),
      toCsv(
        ["from", "to", "days", "binary_downloads", "per_day"],
        deltas.map((d) => [d.from, d.to, d.days, d.binary, (d.binary / d.days).toFixed(1)]),
      ),
    );
  }
  return { snaps, deltas, latest: snaps.at(-1) };
}

// --- the combined, unduplicated weekly series ------------------------------

function writeCombined(npm, gh) {
  // A week gets a GitHub figure only when snapshot intervals cover ALL SEVEN of its days,
  // and a combined total only when the npm week is complete too. Anything less compares
  // two different windows: the first measured interval here ran 6 days across a week
  // boundary, which would otherwise have been summed against a 2-day npm figure and
  // presented as that week's total. An EMPTY cell means unmeasured, never a measured zero.
  const perDay = allocateDeltasToDays(gh?.deltas ?? []);
  const rows = npm.weekly.map((w) => {
    const n = w.counts[META];
    const g = coveredSum(w.week_start, 7, perDay);
    const combinable = g !== null && w.complete;
    return [w.week_start, w.week_end, w.days, w.complete, n, g ?? "", combinable ? n + g : ""];
  });
  writeFileSync(
    join(OUT, "downloads-weekly.csv"),
    toCsv(["week_start", "week_end", "days", "complete", "npm", "github", "total"], rows),
  );
}

// --- entry point -----------------------------------------------------------

function loadNpmFromDisk() {
  const file = join(OUT, "npm-daily.csv");
  if (!existsSync(file)) throw new Error(`--chart-only needs ${file}; run without it first`);
  const rows = parseCsv(readFileSync(file, "utf8"));
  const pkgs = packageColumns(Object.keys(rows[0]));
  const platforms = pkgs.filter((p) => p !== META);
  return {
    pkgs,
    platforms,
    rows: rows.map((r) => ({
      day: r.date,
      counts: Object.fromEntries(pkgs.map((p) => [p, Number(r[p])])),
    })),
  };
}

const npmRaw = chartOnly ? loadNpmFromDisk() : has("--github-only") ? null : await fetchNpm();
const npm = npmRaw && writeNpm(npmRaw);
const gh = has("--npm-only") ? writeGithub(null) : writeGithub(readLedger());
if (npm) writeCombined(npm, gh);

const lastComplete = npm?.weekly.filter((w) => w.complete).at(-1);
const summary = {
  generated: today,
  note: "Downloads, not users. npm counts CI/mirrors/bots; an upgrade re-downloads.",
  npm: npm && {
    package: META,
    first_day: npm.first,
    last_day: npm.last,
    lag_note: "npm reports UTC days ~2 days behind; the trailing week is always partial.",
    all_time: npm.allTime,
    last_complete_week: lastComplete && {
      week_start: lastComplete.week_start,
      downloads: lastComplete.counts[META],
    },
    reporting_gaps: npm.weekly.reduce((s, w) => s + w.gaps, 0),
    gap_note:
      "Days npm reported as zero for EVERY package at once — a batch gap, not a zero-download day. Left at zero in totals, excluded from the forecast rate.",
    current_month_projection: npm.projection && {
      month: npm.monthly.at(-1).month_start,
      measured: npm.projection.measured,
      projected_remainder: npm.projection.projected,
      projected_total: npm.projection.total,
      days_observed: npm.projection.daysObserved,
      days_remaining: npm.projection.daysRemaining,
      daily_rate: npm.projection.rate,
      method: `Remaining days at the mean of the last ${npm.projection.window} days (${npm.projection.windowReported} reported). Assumes the current rate holds.`,
    },
    platform_packages_excluded_from_totals: npmRaw
      ? Object.fromEntries(
          npmRaw.platforms.map((p) => [
            p,
            npmRaw.rows.reduce((s, r) => s + r.counts[p], 0),
          ]),
        )
      : undefined,
  },
  github: gh.latest && {
    repo: REPO,
    snapshot_date: gh.latest.date,
    snapshots: gh.snaps.length,
    releases: gh.latest.releases,
    binary_downloads: gh.latest.binary,
    excluded: { checksum: gh.latest.checksum, launcher: gh.latest.launcher },
    by_platform: gh.latest.byPlatform,
    source: `${DATA_BRANCH} branch, written daily by .github/workflows/download-stats.yml`,
    history_note:
      gh.snaps.length < 2
        ? "Fewer than two snapshots — no interval can be measured yet."
        : `${gh.snaps.length} daily snapshots since ${gh.snaps[0].date}; ${gh.deltas.length} intervals.`,
  },
  total_downloads: (npm?.allTime ?? 0) + (gh.latest?.binary ?? 0),
};
writeFileSync(join(OUT, "summary.json"), JSON.stringify(summary, null, 2) + "\n");

const utcLabel = (d, opts) =>
  new Date(`${d}T00:00:00Z`).toLocaleDateString("en-US", { ...opts, timeZone: "UTC" });

/**
 * Chart rows, COMPLETE PERIODS ONLY. A part-period bar is not a small period, it is an
 * unfinished one, and drawing it puts a fake cliff at the right edge. The month in
 * progress is the one exception: it carries its forecast so the bar still spans a real
 * month rather than a stump.
 */
function chartRows(npm, gh) {
  const perDay = allocateDeltasToDays(gh?.deltas ?? []);
  const row = (label, n, g, forecast, forecastNote) => ({
    label,
    npm: n,
    github: g,
    forecast,
    forecastNote,
    total: n + (g ?? 0) + (forecast ?? 0),
  });

  // GitHub covers exactly the window npm covers — `days`, not the calendar length. For a
  // finished period the two are identical; for the month in progress, keying off the
  // calendar length would demand days that have not happened, return null, and print an
  // em-dash for GitHub beside a real npm figure for the same elapsed days.
  const ghFor = (start, days) => coveredSum(start, days, perDay);

  // Trailing GitHub rate over the same 7-day window the npm forecast uses, so the month
  // in progress is projected on BOTH channels. Forecasting only npm would draw a
  // "full month" bar whose GitHub share silently stops partway through.
  const ghRate = (endDay, window = 7) => {
    let sum = 0, seen = 0;
    for (let i = 0; i < window; i++) {
      const v = perDay.get(addDays(endDay, -i));
      if (v !== undefined) { sum += v; seen++; }
    }
    return seen ? sum / seen : 0;
  };

  const weeks = npm.weekly
    .filter((w) => w.complete)
    .map((w) =>
      row(utcLabel(w.week_start, { month: "short", day: "numeric" }), w.counts[META], ghFor(w.week_start, w.days), 0, ""),
    );

  const lastIdx = npm.monthly.length - 1;
  const months = npm.monthly
    .map((mo, i) => {
      const isCurrent = i === lastIdx && npm.projection;
      if (!mo.complete && !isCurrent) return null; // a past short month is just partial
      const p = isCurrent ? npm.projection : null;
      const gh = ghFor(mo.month_start, mo.days);
      const ghAhead = p && gh !== null ? Math.round(ghRate(mo.month_end) * p.daysRemaining) : 0;
      return row(
        utcLabel(mo.month_start, { month: "short", year: "numeric" }),
        mo.counts[META],
        gh,
        (p?.projected ?? 0) + ghAhead,
        p ? `${p.daysRemaining}d at ${(p.rate + Math.round(ghRate(mo.month_end))).toLocaleString()}/day across both` : "",
      );
    })
    .filter(Boolean);

  return { weeks, months };
}

if (!has("--no-chart") && npm) {
  writeFileSync(join(OUT, "downloads.html"), renderChart({ summary, ...chartRows(npm, gh), meta: META }));
}

if (npm) {
  console.log(`npm      ${npm.weekly.length} weeks (${npm.first} → ${npm.last})`);
  console.log(`         ${META}: all-time ${npm.allTime.toLocaleString()}`);
  if (lastComplete)
    console.log(
      `         last complete week (${lastComplete.week_start}): ${lastComplete.counts[META].toLocaleString()}`,
    );
  if (npm.projection) {
    const { measured, projected, total, daysObserved, daysRemaining, rate } = npm.projection;
    console.log(
      `         ${npm.monthly.at(-1).month_start.slice(0, 7)} projected ${total.toLocaleString()}` +
        ` (${measured.toLocaleString()} over ${daysObserved}d + ${projected.toLocaleString()} for ${daysRemaining}d at ${rate.toLocaleString()}/day)`,
    );
  }
}
if (gh.latest) {
  console.log(
    `github   ${gh.latest.binary.toLocaleString()} binary downloads across ${gh.latest.releases} releases` +
      ` (excluded ${gh.latest.checksum.toLocaleString()} checksum fetches)`,
  );
  console.log(`         ${gh.snaps.length} snapshot(s); ${summary.github.history_note}`);
}
const channels = [npm && `npm ${META}`, gh.latest && "GitHub binaries"].filter(Boolean);
console.log(`TOTAL    ${summary.total_downloads.toLocaleString()} downloads (${channels.join(" + ")})`);
console.log(`→ ${OUT}/`);
