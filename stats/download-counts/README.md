# Download count snapshots

Daily snapshots of cumulative download counts, written by the [download-stats workflow](../../.github/workflows/download-stats.yml).

## Files

- `YYYYMMDDTHHMMSSZ.json` — one snapshot per day, committed by the cron job.
- `backfill-YYYYMMDD.json` — one-time recovery file (see [Backfill](#backfill) below).
- `report.mjs` — diff/trend script; run locally with `node stats/download-counts/report.mjs`.
- `README.md` — this file.

## Snapshot format

```json
{
  "timestamp": "2026-06-17T00:15:00.000Z",
  "github_releases": [
    {
      "tag": "v0.0.5",
      "assets": [
        { "name": "nub-darwin-arm64.tar.gz", "download_count": 3 },
        ...
      ]
    }
  ],
  "npm": {
    "downloads": 12,
    "start": "2026-05-18",
    "end": "2026-06-16",
    "package": "@nubjs/nub"
  }
}
```

`github_releases` mirrors the GitHub Releases API response, filtered to tag + per-asset name/count. `npm` is the response from the npm downloads API (`/downloads/point/last-month/@nubjs/nub`).

## Reading the trend

**For anything beyond a quick diff, use `node scripts/download-stats.mjs`.** It reads this same ledger off the `download-stats-data` branch and produces the weekly/monthly series, the npm+GitHub split, and a chart — with the counting rules applied (checksum assets excluded, platform packages kept out of totals, npm reporting gaps kept out of rates). `report.mjs` below remains the minimal snapshot-to-snapshot diff.


GitHub's `download_count` is **cumulative and never resets** — a single snapshot is just a total. The `report.mjs` script diffs consecutive snapshots to compute approximate daily downloads:

```
node stats/download-counts/report.mjs          # all snapshots
node stats/download-counts/report.mjs --last 7 # last 7 days
```

## Known limitation

**GitHub's count is a total-binary-pulls number, not a curl/PowerShell-install count.** Every download of each release asset is counted: curl installs, `nub upgrade` self-updates, CI runners pulling the binary, manual browser downloads, and bots. Use it as a trend and a platform-mix indicator. If you want a count scoped specifically to script-based installs, that requires Phase 1 (a logging redirect or an install-script ping — see `wiki/research/download-analytics.md`).

The npm figure is also an approximation: it's a last-30-days rolling window from the npm downloads API, not a true daily delta.

## Backfill

The daily workflow failed every run from its inception (~2026-06-19) through 2026-06-24 — the write step read the `node -` stdin marker as its timestamp argument instead of the actual timestamp, so it wrote `-.json` while the commit step `git add`ed the real filename and exited 128. No going-forward snapshots were produced in that window.

`backfill-YYYYMMDD.json` recovers what is recoverable:

- **npm: fully recovered.** npm's downloads API keeps complete daily history, so the file carries the full per-day series from the v0.1.0 launch (2026-06-17) forward, fetched from `https://api.npmjs.org/downloads/range/2026-06-17:<today>/@nubjs/nub`.
- **GitHub releases: cumulative only.** GitHub exposes only the current cumulative `download_count` per asset — there is **no historical download endpoint**, so the per-day deltas missed during the outage are **unrecoverable**. The backfill captures today's cumulative per-asset totals as a baseline; going-forward snapshots diff from there.

The backfill schema differs from a daily snapshot: `{ kind: "backfill", npm_daily: <range response>, github_releases_cumulative: <releases> }`. `report.mjs` ignores it (it only reads `YYYYMMDDTHHMMSSZ.json` snapshots).

## Adding a new snapshot manually

Trigger the workflow:

```sh
gh workflow run download-stats.yml
```

Or run it on your own machine (requires `gh` and `node`):

```sh
gh api repos/nubjs/nub/releases --paginate \
  --jq '[.[] | {tag: .tag_name, assets: [.assets[] | {name: .name, download_count: .download_count}]}]' \
  > /tmp/gh.json

npm=$(curl -fsSL "https://api.npmjs.org/downloads/point/last-month/@nubjs/nub")
ts=$(date -u +"%Y%m%dT%H%M%SZ")
node -e "
const fs = require('fs');
const snapshot = {
  timestamp: new Date().toISOString(),
  github_releases: JSON.parse(fs.readFileSync('/tmp/gh.json','utf8')),
  npm: JSON.parse(process.env.NPM),
};
fs.writeFileSync('stats/download-counts/${ts}.json', JSON.stringify(snapshot, null, 2) + '\n');
" NPM="$npm"
```
