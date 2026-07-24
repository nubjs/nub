#!/usr/bin/env node
/**
 * stats/download-counts/report.mjs
 *
 * Reads the daily download-count snapshots in this directory and prints
 * a trend: approximate downloads-per-day per platform asset and via npm.
 *
 * Usage:
 *   node stats/download-counts/report.mjs          # all snapshots, newest last
 *   node stats/download-counts/report.mjs --last 7 # only the last 7 snapshots
 *
 * Each snapshot is a JSON file written by the download-stats GitHub Actions
 * workflow. The GitHub counts are cumulative totals — this script diffs
 * consecutive snapshots to compute the period delta.
 *
 * Limitation: GitHub's download_count includes every binary pull (curl installs,
 * `nub upgrade` self-updates, CI runners, manual browser downloads, bots). It is
 * a total-binary-pulls number, not a pure curl/PowerShell-install count. Use it
 * as a trend and a platform-mix indicator, not as an isolated install figure.
 * npm counts (last_30_days from the npm downloads API) are a separate signal for
 * the npm-install path.
 */

import fs from "fs";
import path from "path";
import { fileURLToPath } from "url";

const dir = path.dirname(fileURLToPath(import.meta.url));

// Parse --last N flag
const lastIdx = process.argv.indexOf("--last");
const limitN = lastIdx !== -1 ? parseInt(process.argv[lastIdx + 1], 10) : Infinity;

// Load all snapshot files, sorted chronologically
const files = fs
  .readdirSync(dir)
  .filter((f) => f.match(/^\d{8}T\d{6}Z\.json$/))
  .sort();

if (files.length === 0) {
  console.log("No snapshots found. The cron workflow will produce the first one on its next scheduled run.");
  console.log("You can also trigger it manually via: gh workflow run download-stats.yml");
  process.exit(0);
}

const recent = isFinite(limitN) ? files.slice(-limitN) : files;
const snapshots = recent.map((f) => ({
  file: f,
  data: JSON.parse(fs.readFileSync(path.join(dir, f), "utf8")),
}));

console.log(`\nDownload trend — ${snapshots.length} snapshot(s), ${files.length} total on disk\n`);
console.log(`Oldest shown : ${snapshots[0].data.timestamp}`);
console.log(`Newest shown : ${snapshots[snapshots.length - 1].data.timestamp}`);
console.log();

if (snapshots.length < 2) {
  console.log("Need at least 2 snapshots to compute a delta. Check back tomorrow.");
  printLatestTotals(snapshots[0]);
  process.exit(0);
}

// Build a flat map of assetName → cumulative count for each snapshot
function assetMap(snapshot) {
  const map = {};
  for (const release of snapshot.data.github_releases ?? []) {
    // Skip the rolling canary release (belt-and-suspenders — the snapshot
    // workflow already filters it): its per-asset counters are destroyed by
    // every canary build's --clobber re-upload, so a snapshot that caught one
    // would read as a reset-to-zero (negative delta) in the series.
    if (release.tag === "canary") continue;
    for (const asset of release.assets ?? []) {
      // Skip checksum sidecars — they're downloaded alongside the archive
      // and inflate the count without adding signal
      if (asset.name.endsWith(".sha256")) continue;
      map[`${release.tag}/${asset.name}`] = asset.download_count;
    }
  }
  return map;
}

// Print period deltas between each consecutive pair of snapshots
for (let i = 1; i < snapshots.length; i++) {
  const prev = snapshots[i - 1];
  const curr = snapshots[i];
  const prevMap = assetMap(prev);
  const currMap = assetMap(curr);

  const t1 = new Date(prev.data.timestamp);
  const t2 = new Date(curr.data.timestamp);
  const hoursDiff = (t2 - t1) / 1000 / 3600;
  const daysDiff = hoursDiff / 24;

  console.log(`── Period: ${prev.data.timestamp} → ${curr.data.timestamp} (${hoursDiff.toFixed(1)}h)`);

  // Compute deltas — all assets that appear in curr
  const deltas = [];
  for (const [key, count] of Object.entries(currMap)) {
    const prevCount = prevMap[key] ?? 0;
    const delta = count - prevCount;
    if (delta !== 0 || count > 0) deltas.push({ key, count, delta });
  }

  // Group by tag
  const byTag = {};
  for (const d of deltas) {
    const [tag, name] = d.key.split("/");
    (byTag[tag] ??= []).push({ name, ...d });
  }

  let totalDelta = 0;
  for (const [tag, assets] of Object.entries(byTag)) {
    console.log(`   ${tag}`);
    for (const a of assets.sort((x, y) => x.name.localeCompare(y.name))) {
      const rate = daysDiff > 0 ? (a.delta / daysDiff).toFixed(1) : "—";
      const sign = a.delta >= 0 ? "+" : "";
      console.log(`     ${a.name.padEnd(40)} total=${a.count}  Δ=${sign}${a.delta}  (~${rate}/day)`);
      totalDelta += a.delta;
    }
  }
  console.log(`   GitHub total delta: +${totalDelta}  (~${daysDiff > 0 ? (totalDelta / daysDiff).toFixed(1) : "—"}/day)`);

  // npm delta
  const npmPrev = prev.data.npm?.downloads ?? 0;
  const npmCurr = curr.data.npm?.downloads ?? 0;
  if (npmCurr > 0 || npmPrev > 0) {
    const npmDelta = npmCurr - npmPrev;
    console.log(`   npm last-30-days:   ${npmCurr}  Δ=${npmDelta >= 0 ? "+" : ""}${npmDelta}`);
  }
  console.log();
}

// Always show the latest snapshot's totals
printLatestTotals(snapshots[snapshots.length - 1]);

function printLatestTotals(snap) {
  console.log(`── Current totals (${snap.data.timestamp})`);
  const map = assetMap(snap);
  const byTag = {};
  for (const [key, count] of Object.entries(map)) {
    const [tag, name] = key.split("/");
    (byTag[tag] ??= []).push({ name, count });
  }
  let grand = 0;
  for (const [tag, assets] of Object.entries(byTag)) {
    console.log(`   ${tag}`);
    for (const a of assets.sort((x, y) => x.name.localeCompare(y.name))) {
      console.log(`     ${a.name.padEnd(40)} ${a.count}`);
      grand += a.count;
    }
  }
  console.log(`   Grand total (GitHub): ${grand}`);
  const npm = snap.data.npm;
  if (npm?.downloads) console.log(`   npm last-30-days:    ${npm.downloads}`);
  console.log();
}
