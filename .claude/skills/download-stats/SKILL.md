---
name: download-stats
description: Generate all-time download-stats CSVs for nub across its distribution channels (npm + GitHub release assets, which subsume Homebrew). Invoke (via the Skill tool) when asked to check, chart, or export download numbers, install counts, or adoption stats. Encodes the channel map, each channel's real granularity and lag, and the checksum-asset inflation gotcha.
metadata:
  internal: true
---

# download-stats — pull nub's download numbers into CSVs

One command generates the CSVs:

```sh
node scripts/download-stats.mjs            # writes tmp/download-stats/*.csv
node scripts/download-stats.mjs --npm-only # skip the gh-authenticated GitHub half
node scripts/download-stats.mjs --out <dir> --package @nubjs/nub --repo nubjs/nub
```

Outputs:

- `npm-daily.csv` — one row per day since first publish: the meta package, each `@nubjs/nub-<platform>` package (auto-discovered from the meta package's `optionalDependencies`), `platforms_total`, and a running cumulative for the meta package.
- `github-releases.csv` — per-release, per-asset cumulative download counters, stamped with `snapshot_date`. Re-running **appends** (replacing any same-day rows), so repeated runs accumulate the time series GitHub itself never provides.

## Channel map — where a nub install actually lands

| Install path | Counted in |
|---|---|
| `npm install -g @nubjs/nub` (and CI installs via lockfile) | npm registry counts |
| curl `install.sh` | GitHub release assets |
| `nub upgrade` | GitHub release assets |
| **Homebrew tap** (`nubjs/homebrew-tap`) | GitHub release assets — the formula's `url` is the release `nub-<platform>.tar.gz`; taps get **no** formulae.brew.sh analytics (that API covers homebrew-core only) |

The two channels are fully independent; summing them does not double-count. Within GitHub assets, brew / curl / `nub upgrade` are indistinguishable — the per-asset platform split is the only lens.

## Granularity and gotchas (the reasons this skill exists)

- **npm: daily is the finest granularity.** `api.npmjs.org/downloads/range/<start>:<end>/<pkg>` returns UTC-day buckets; there is no hourly API. A single request caps at ~18 months — the script stitches windows, so all-time works regardless of age.
- **npm lags ~2 days.** The trailing day(s) read 0 until npm's batch job runs; the script trims trailing zero-days so charts don't show a fake cliff. Don't quote "yesterday" from npm.
- **Per-version npm splits have no history** — `api.npmjs.org/versions/<pkg>/last-week` is a trailing-7-day snapshot only.
- **GitHub asset counters are cumulative-only.** No history endpoint exists; a time series only exists if you snapshot repeatedly (hence the append behavior). Deltas between snapshots = downloads in that interval.
- **Exclude `.sha256` assets when quoting GitHub numbers.** Every release ships a checksum file per tarball and installers fetch them; they inflate the raw sum ~15%. Filter `asset !~ /sha256/` for the real binary count.
- **The meta npm package is the canonical "installs" number.** Platform packages roughly mirror it (each install pulls exactly one) but diverge on lockfile-driven CI, which can fetch platform tarballs at different rates — use them as an OS split, not a total.
- npm counts are registry downloads (CI, mirrors, bots included), not unique users or machines.
