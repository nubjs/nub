---
name: download-stats
description: Generate download-stats CSVs and a chart for nub across its distribution channels (npm + GitHub release assets, which subsume Homebrew). Invoke (via the Skill tool) when asked to check, chart, or export download numbers, install counts, or adoption stats. Encodes the channel map, which numbers may be added together, each channel's real granularity and lag, and the checksum-asset inflation gotcha.
metadata:
  internal: true
---

# download-stats — pull nub's download numbers into CSVs and a chart

One command does everything:

```sh
node scripts/download-stats.mjs             # npm from the API, GitHub from the ledger branch, then CSVs + chart
node scripts/download-stats.mjs --npm-only  # skip the gh-authenticated GitHub half
node scripts/download-stats.mjs --chart-only # re-render from the CSVs on disk, no network
node scripts/download-stats.mjs --no-chart --out <dir> --package @nubjs/nub --repo nubjs/nub
```

Output lands in `tmp/download-stats/` (gitignored — derived data, regenerated on demand):

| File | What it holds |
|---|---|
| `downloads-weekly.csv` | **The clean series.** `week_start,week_end,days,complete,npm,github,total` — one row per UTC Monday week. `github` fills only when snapshot intervals cover all seven days; `total` also needs the npm week complete. |
| `npm-weekly.csv` | The npm half by UTC Monday week, with a `gaps` column and `platforms_total` for reference. |
| `npm-monthly.csv` | Calendar months, with `expected_days`, `gaps`, and the current month's `projected_remainder`/`projected_total`. |
| `npm-daily.csv` | One row per day since first publish: the meta package, each platform package, `platforms_total`, running cumulative. |
| `github-releases.csv` | Per-release, per-asset cumulative counters, one set per snapshot date — a flattened export of the ledger, regenerated each run. |
| `github-intervals.csv` | Written once two or more snapshots exist — the deltas between them, with a `per_day` rate. Intervals, NOT weeks: a snapshot delta ignores week boundaries. |
| `summary.json` | Machine-readable totals, the platform split, and the caveats as data. |
| `downloads.html` | Self-contained page — no deps, no CDN, opens from disk. Hero total, an all-time source split, and one stacked chart (npm below, GitHub above) toggling weekly/monthly. |

Pure aggregation logic lives in `scripts/lib/download-stats.mjs` and is tested in `scripts/download-stats.test.mjs`; the renderer is `scripts/lib/download-chart.mjs`.

The page draws COMPLETE PERIODS ONLY. A part-period bar is not a small period, it is an unfinished one, and drawing it puts a fake cliff at the right edge; the month in progress is the single exception, shown at its full projected height with the forecast hatched so the bar still spans a real month. Bars stack by SOURCE — the only split that can be stacked, since the npm platform packages are pulled BY an npm install and stacking them would draw the same install twice.

Two renderer traps, both of which shipped a silently-wrong chart before they were caught by magnifying a screenshot rather than by reading the code:

- **A CSS custom property does not resolve inside `<pattern>` content** — the tile is a paint resource, not part of the rendered tree, so `var(--x)` there paints nothing. Emit literal hex per mode and select with a CSS `fill:url(#id)` rule on the referencing element.
- **A paint server inside a `display:none` subtree is dead.** The weekly and monthly charts are both in the document with one hidden, so a `<defs>` emitted per chart gives duplicate ids where `url()` takes the first — which is whichever chart the toggle just hid. Define patterns ONCE in a zero-sized but never-hidden `<svg>`.

## What may be added to what — the double-counting rule

**The only unduplicated total is npm `@nubjs/nub` + GitHub binary assets.** Verified against `npm/nub/package.json`, `npm/nub/postinstall.js`, `install.sh` and `crates/nub-cli/src/cli.rs`:

| Install path | `@nubjs/nub` | `@nubjs/nub-<plat>` | GitHub asset |
|---|---|---|---|
| `npm i -g @nubjs/nub` | 1 | 1 | 0 |
| `npm i -g --omit=optional` | 1 | 0 | 0 (a broken install, still counted) |
| `npm ci` from a lockfile | 1 | 1..8 | 0 |
| pnpm with `supportedArchitectures` | 1 | N | 0 |
| curl `install.sh` | 0 | 0 | 1 (+1 `.sha256`) |
| `nub upgrade` | 0 | 0 | 1 (+1 `.sha256`) |
| Homebrew tap | 0 | 0 | 1 |

- **Never add the platform packages to the meta package.** They are pulled BY an npm install, never instead of one, so summing counts the same install twice. Measured all-time they run ~1.7x the meta count (and up to 6x in a given week), which is the tell that they are not one-per-install.
- **They are not a usable OS split either.** A multi-arch Docker build, a `supportedArchitectures` pnpm install, and a registry mirror caching every optionalDependency each pull several per install. For an OS split use the GitHub per-asset counts, where one download really is one binary.
- **Nothing installs the binary outside these channels.** `postinstall.js` only chmods and re-links shims, and `bin/launch.js` makes no network call — so an npm install always rides a platform package, and the curl/`upgrade`/brew paths never touch npm. The two channels are independent and add cleanly.
- **These are downloads, not users and not installs.** An upgrade re-downloads; npm counts CI, mirrors and bots.

## Granularity and gotchas (the reasons this skill exists)

- **npm: daily is the finest granularity.** `api.npmjs.org/downloads/range/<start>:<end>/<pkg>` returns UTC-day buckets; there is no hourly API. A single request caps at ~18 months — the script stitches windows, so all-time works regardless of age.
- **npm lags ~2 days**, so the trailing week is ALWAYS short. The script trims trailing zero-days and flags every week with a `complete` column; quote the last COMPLETE week, never the trailing partial one, or a reporting artifact reads as a cliff.
- **Per-version npm splits have no history** — `api.npmjs.org/versions/<pkg>/last-week` is a trailing-7-day snapshot only.
- **A snapshot interval is not a week, and summing the two channels across mismatched windows is the trap.** The first real interval here ran 6 days across a week boundary; attributing it to either week would have compared 6 days of GitHub against 2 days of npm and called the sum a weekly total. `allocateDeltasToDays()` spreads an interval evenly over the days it covers, and `weeklyGithub()` returns null unless every day of the week is covered — an empty cell, never a partial one dressed as a whole.
- **GitHub asset counters are cumulative-only, and the daily ledger is the ONLY record of the past.** No history endpoint exists. [`.github/workflows/download-stats.yml`](../../../.github/workflows/download-stats.yml) has taken a daily snapshot since 2026-06-24 and commits it to the **`download-stats-data`** branch (never main — main's ruleset blocks the Actions bot). `readLedger()` reads that branch; deltas between consecutive snapshots are the downloads in each interval, clamped at 0 since a deleted release can walk a counter backwards.
  - **Never start a second snapshot series.** A fresh live fetch gives exactly one data point and quietly competes with a ledger that already has months of depth. This skill previously described the script taking its own snapshots; that was a duplicate built without finding the existing one, and it is why this bullet now leads with the ledger.
  - Periods before 2026-06-25 (the first delta) have **no** GitHub figure and print `—`. That is missing data, not zero, and it is not recoverable.
  - Bucketing a release's downloads by its publish date to fake earlier history **does not work** — tested against two snapshots: 0% of the interval's gain went to releases published inside it, and a release from two months earlier still gained 566. Old releases keep accruing.
- **Exclude `.sha256` assets** — every release ships a checksum file per tarball and installers fetch them, inflating the raw sum by roughly half (measured: 17.5K checksum fetches against 34.1K real binaries). `nub-launcher-*` templates are fetched by `nub compile` to cross-compile, not by anyone installing nub. `assetKind()` does this classification; do not re-derive it with a grep.
- **The month in progress is projected, and the method is deliberate.** `projectMonth()` extrapolates the remaining days at the mean of the trailing 7 days. Two traps it exists to avoid: (1) remaining days are counted forward from the LAST OBSERVED DAY, never as `expected_days - observed_days` — a package's first month is short because publishing started mid-month, and the naive form would "project" days that already happened; (2) the rate is a trailing 7-day window rather than a month-to-date mean, because that window holds one of each weekday and npm traffic is heavily CI-driven, so weekdays run far above weekends.
- **npm sometimes reports a day as zero for EVERY package at once** — a batch-job gap, not a zero-download day (measured: 2026-07-12 and 2026-08-14). `isGapDay()` flags them. They are left at zero in totals, because imputing would invent downloads, but they are excluded from the forecast rate, where one false zero cost 6%. A day where only the meta package reads zero while platform packages report is left alone as the ambiguous case.
- **The rolling `canary` prerelease is excluded** — it is recreated by every nightly canary build, so its counters reset and would corrupt the snapshot deltas. `nub upgrade --canary` downloads are therefore uncounted.
