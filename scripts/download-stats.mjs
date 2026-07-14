#!/usr/bin/env node
// Download-stats puller for nub's distribution channels. Writes CSVs suitable
// for charting or appending into a long-run series:
//
//   npm-daily.csv       date,<meta>,<each platform pkg>,platforms_total,<meta>_cumulative
//                       Full daily history from the package's registry creation date
//                       (npm's range API caps a window at ~18 months, so requests are
//                       stitched). npm reports UTC days and lags ~2 days; trailing
//                       zero-days for today/yesterday are trimmed, interior zeros kept.
//
//   github-releases.csv snapshot_date,tag,published_at,asset,download_count
//                       GitHub asset counters are CUMULATIVE-ONLY (no history API), so
//                       each run is a snapshot stamped with snapshot_date — append runs
//                       over time to build a time series. This channel covers the curl
//                       installer, `nub upgrade`, AND Homebrew (the tap formula fetches
//                       these same release assets; taps get no formulae.brew.sh analytics).
//
// Usage: node scripts/download-stats.mjs [--out <dir>] [--package <name>]
//          [--start YYYY-MM-DD] [--npm-only | --github-only]
// Needs: network; `gh` authenticated (github channel only).

import { execFileSync } from "node:child_process";
import { mkdirSync, writeFileSync, existsSync, readFileSync, appendFileSync } from "node:fs";
import { join } from "node:path";

const args = process.argv.slice(2);
const flag = (name) => {
  const i = args.indexOf(name);
  return i === -1 ? undefined : args[i + 1];
};
const has = (name) => args.includes(name);

const META = flag("--package") ?? "@nubjs/nub";
const OUT = flag("--out") ?? "tmp/download-stats";
const REPO = flag("--repo") ?? "nubjs/nub";
const today = new Date().toISOString().slice(0, 10);

mkdirSync(OUT, { recursive: true });

const getJSON = async (url) => {
  const res = await fetch(url);
  if (!res.ok) throw new Error(`${res.status} ${url}`);
  return res.json();
};

async function npmDaily() {
  const doc = await getJSON(`https://registry.npmjs.org/${META}`);
  const created = (flag("--start") ?? doc.time.created).slice(0, 10);
  // Platform packages are the meta package's optionalDependencies (the
  // @nubjs/nub-<platform> binary packages).
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
      const r = await getJSON(
        `https://api.npmjs.org/downloads/range/${start}:${end}/${pkg}`,
      );
      for (const { day, downloads } of r.downloads ?? []) perPkg[pkg].set(day, downloads);
    }
  }

  let days = [...perPkg[META].keys()].sort();
  // Trim leading zeros (pre-first-publish padding) and trailing zeros (npm's
  // ~2-day reporting lag) — interior zero days are real and kept.
  const total = (day) => pkgs.reduce((s, p) => s + (perPkg[p].get(day) ?? 0), 0);
  while (days.length && total(days[0]) === 0) days.shift();
  while (days.length && total(days[days.length - 1]) === 0) days.pop();

  const short = (p) => p.replace(/^@[^/]+\//, "");
  const header = [
    "date",
    ...pkgs.map(short),
    "platforms_total",
    `${short(META)}_cumulative`,
  ];
  let cum = 0;
  const rows = days.map((day) => {
    const vals = pkgs.map((p) => perPkg[p].get(day) ?? 0);
    cum += vals[0];
    const platTotal = vals.slice(1).reduce((a, b) => a + b, 0);
    return [day, ...vals, platTotal, cum];
  });
  const file = join(OUT, "npm-daily.csv");
  writeFileSync(file, [header, ...rows].map((r) => r.join(",")).join("\n") + "\n");

  const sum = (n) => rows.slice(-n).reduce((s, r) => s + r[1], 0);
  console.log(`npm-daily.csv  ${rows.length} days (${days[0]} → ${days.at(-1)})`);
  console.log(`  ${META}: all-time ${cum}, last 7d ${sum(7)}, last 30d ${sum(30)}`);
  return file;
}

function githubReleases() {
  const raw = execFileSync(
    "gh",
    ["api", `repos/${REPO}/releases`, "--paginate"],
    { encoding: "utf8", maxBuffer: 64 * 1024 * 1024 },
  );
  const releases = JSON.parse(raw.replace(/\]\s*\[/g, ",")); // join paginated arrays
  const header = "snapshot_date,tag,published_at,asset,download_count";
  const rows = releases.flatMap((rel) =>
    rel.assets.map(
      (a) => `${today},${rel.tag_name},${rel.published_at},${a.name},${a.download_count}`,
    ),
  );
  const file = join(OUT, "github-releases.csv");
  // Append across runs (skipping the header) so repeated snapshots accumulate
  // into a time series; a fresh file gets the header first.
  if (existsSync(file) && readFileSync(file, "utf8").startsWith(header)) {
    const prior = readFileSync(file, "utf8");
    const withoutToday = prior
      .split("\n")
      .filter((l) => l && !l.startsWith(`${today},`) && !l.startsWith("snapshot_date"));
    writeFileSync(file, [header, ...withoutToday, ...rows].join("\n") + "\n");
  } else {
    writeFileSync(file, [header, ...rows].join("\n") + "\n");
  }
  const total = releases.reduce(
    (s, r) => s + r.assets.reduce((x, a) => x + a.download_count, 0),
    0,
  );
  console.log(`github-releases.csv  snapshot ${today}: ${total} cumulative asset downloads across ${releases.length} releases`);
  return file;
}

if (!has("--github-only")) await npmDaily();
if (!has("--npm-only")) githubReleases();
console.log(`→ ${OUT}/`);
