#!/usr/bin/env node
// npm-install-script-census — enumerate every npm package above a weekly-download
// threshold and record, PER VERSION, whether it runs install lifecycle scripts.
// The corpus every build-jail compatibility claim rests on.
//
// Runs under BOTH plain Node (type-stripping) and nub:
//   node scripts/npm-install-script-census.ts
//   nub  scripts/npm-install-script-census.ts --threshold 100000 --top-versions 3
//
// ERASABLE TypeScript only (no enums/namespaces/parameter-properties) so plain
// modern `node` runs it with no build step — same constraint as the other
// scripts/*.ts.
//
// WHY AN ABSOLUTE THRESHOLD, NOT A TOP-N RANK. "the top 5,500 packages" is not a
// stable target: it drifts with every publish, and two lanes measuring "the true
// top-5,500" can disagree without either being wrong. ">100,000 weekly downloads
// as reported by npm" is a fixed, reproducible line. Rank is an OUTPUT (where the
// threshold lands), never the cutoff.
//
// WHY PER VERSION, AND WHY THIS IS THE WHOLE BALL GAME. Classifying a package by
// its `latest` manifest systematically UNDERSTATES the corpus, because lockfiles
// pin old versions and installs pile up there. sharp is the proof: `latest`
// (0.35.3) has no install script at all, while 0.34.5 (47,808,857 weekly) runs
// `install: node install/check.js || npm run build`, 0.33.5 (7,841,957 weekly)
// runs `install: node install/check`, and 0.32.6 (2,886,264 weekly) runs the heavy
// `(node install/libvips && node install/dll-copy && prebuild-install) || (node
// install/can-compile && node-gyp rebuild && node install/dll-copy)`. A
// latest-only census scores sharp a clean negative while tens of millions of
// weekly installs execute an install script. So the stored unit is (package,
// version), not package.
//
// WHY TOP-N VERSIONS IS THE INCLUSION RULE, WITH NO SEMVER MODELLING. People pin
// on the last release of a major/minor line, so downloads concentrate there —
// every high-download sharp version is exactly the last of its line (0.34.5,
// 0.35.3, 0.33.5, 0.35.2, 0.32.6). Taking the empirical top-N by download count
// therefore lands on what people actually install without modelling version
// ranges at all. N is `--top-versions`, default 3. A package is install-relevant
// if ANY of its top-N versions has a run script.
//
// N IS A DERIVED VIEW, NOT A FETCH FILTER. Every version of every candidate
// package is cached with its own download count and script status, so changing N
// (or the threshold) re-derives from cache and refetches nothing.
//
// THREE-SOURCE PIPELINE, and why each source is the one used for its job:
//
//   1. RANKING — packages.ecosyste.ms `?sort=downloads&order=desc`. A complete
//      registry enumeration (~5.7M npm packages indexed), NOT search-seeded. The
//      predecessor corpus ranked via registry.npmjs.org/-/v1/search with
//      popularity weighting, which is a RELEVANCE score: it inflated some
//      packages past others with more real downloads, and had unmeasurable recall
//      loss (162 of bun's 367 trusted packages never surfaced from 630 search
//      terms). Search is not a ranking source; do not go back to it.
//
//      ecosyste.ms's `downloads` field is npm's LAST-MONTH count (verified:
//      semver 3382212486 and debug 2773835275 matched api.npmjs.org last-month to
//      the byte). It is used ONLY to bound which packages are worth asking npm
//      about — never as the threshold figure, since its per-package snapshot lags.
//
//   2. DOWNLOAD COUNTS — api.npmjs.org. Package totals from
//      /downloads/point/last-week/<up to 128 names>; per-version counts from
//      /versions/<pkg>/last-week (one request returns every version). Two hard
//      limits on the bulk form, both confirmed by probe: it caps at 128 names
//      ("exceeded max bulk size of 128") and REJECTS scoped names ("scoped
//      packages are not currently supported in bulk lookups"), so scoped
//      packages — over a third of this band — need one request each.
//
//   3. SCRIPTS — the FULL packument, registry.npmjs.org/<pkg>, which carries a
//      `scripts` object per version. The abbreviated ("corgi") packument's
//      per-version `hasInstallScript` boolean was measured to be exactly
//      equivalent: 6,351 versions across 20 packages, zero disagreements, and 38
//      `prepare`-only versions all flagged false (so the field means precisely
//      the three run keys). Corgi is not used anyway, because gzipped it is only
//      20% smaller than the full document (86 KB vs 104 KB mean over a 36-package
//      band sample) while the full document additionally yields the script TEXT —
//      and the text is load-bearing: sharp 0.32.6's libvips+node-gyp path and
//      0.33.5's `node install/check` are materially different demands on a
//      sandbox, and a corpus without the text cannot tell them apart.
//
// ONLY THE THREE RUN KEYS COUNT. npm/pnpm/nub execute preinstall, install and
// postinstall for a package installed AS A DEPENDENCY from the registry.
// prepare/prepublish do NOT run in that case and are recorded separately so they
// can never inflate the headline.
//
// RESUMABILITY. Every network response is distilled and cached under <out>/cache/
// keyed by stage; a re-run reads cache and refetches nothing. `--max-age <days>`
// expires entries so a drift re-run in three months costs only the deltas.

import { mkdirSync, writeFileSync, readFileSync, existsSync, statSync } from "node:fs";
import { join, dirname } from "node:path";

const HELP = `npm-install-script-census — every npm package over N weekly downloads, and whether its most-installed versions run install scripts

Usage:
  node scripts/npm-install-script-census.ts [options]

Options:
  --out <dir>           Output + cache dir (default .fray/npm-census)
  --threshold <n>       Package-level weekly-download membership gate (default 100000)
  --top-versions <n>    Versions per package that count toward install-relevance,
                        ranked by their own download count (default 3)
  --eco-floor <n>       Stop the ecosyste.ms sweep once monthly downloads fall
                        below this (default = --threshold, which is ~4x deeper
                        than the threshold's monthly equivalent, so the band is
                        complete with margin)
  --max-pages <n>       Cap on ranking pages, 100 packages each (default 800)
  --concurrency <n>     In-flight requests per stage (default 10)
  --max-age <days>      Cache entries older than this are refetched (default 30)
  --seed <file>         Newline or JSON-array package names to include regardless
                        of rank — the control set. Repeatable.
  --stage <name>        Run one stage only: rank | weekly | manifest | versions | report
  -h, --help

Outputs (under --out):
  ranking.ndjson          Raw ecosyste.ms sweep (name, monthly downloads, latest)
  census-packages.ndjson  One row per package above the threshold
  census-versions.ndjson  One row per (package, version) for every package with at
                          least one install-script version anywhere in its history
  summary.json            Headline counts, rank bands, definition sensitivity,
                          script-shape spread, unverified residue
`;

const args = process.argv.slice(2);
if (args.includes("-h") || args.includes("--help")) {
  console.log(HELP);
  process.exit(0);
}
const flag = (name: string): string | undefined => {
  const i = args.indexOf(name);
  return i === -1 ? undefined : args[i + 1];
};
const flagAll = (name: string): string[] => {
  const out: string[] = [];
  for (let i = 0; i < args.length; i++) if (args[i] === name && args[i + 1]) out.push(args[i + 1]);
  return out;
};
const num = (name: string, dflt: number): number => {
  const v = flag(name);
  return v === undefined ? dflt : Number(v);
};

const OUT = flag("--out") ?? ".fray/npm-census";
const THRESHOLD = num("--threshold", 100_000);
const TOP_N = num("--top-versions", 3);
// Floor in eco's REPORTED monthly. Set well under the threshold's monthly
// equivalent because eco's reported value can understate the truth by ~100x; the
// staleness gate, not this floor, is what decides which candidates cost an npm
// lookup.
const ECO_FLOOR = num("--eco-floor", 2000);
// Sweep FAR deeper than the threshold's own rank. eco under-reports exactly the
// packages that grew recently, pushing real threshold members down the ranking —
// env-runner reports 484,445 monthly against a real 65,143,376, so it sits ~20,000
// ranks below where it belongs. Pagination is the cheap half of this pipeline
// (~80 pages/min, 6 KB cached per page), and the staleness gate is what keeps the
// expensive npm half from scaling with the depth.
const MAX_PAGES = num("--max-pages", 2000);
const GATE_SAMPLE = num("--gate-sample", 600);
const CONCURRENCY = num("--concurrency", 10);
const MAX_AGE_MS = num("--max-age", 30) * 86_400_000;
const STAGE = flag("--stage");
const RUN_AT = new Date().toISOString();

const CACHE = join(OUT, "cache");
mkdirSync(CACHE, { recursive: true });

// ---------------------------------------------------------------- http + cache

const UA = "nub-install-script-census (https://github.com/nubjs/nub)";

// Two-level shard so a 25k-entry stage dir stays navigable.
const cachePath = (stage: string, key: string): string => {
  const safe = key.replace(/[^A-Za-z0-9._@-]/g, "_");
  return join(CACHE, stage, safe.slice(0, 2).toLowerCase(), `${safe}.json`);
};

const readCache = (stage: string, key: string): unknown | undefined => {
  const p = cachePath(stage, key);
  if (!existsSync(p)) return undefined;
  if (Date.now() - statSync(p).mtimeMs > MAX_AGE_MS) return undefined;
  try {
    return JSON.parse(readFileSync(p, "utf8"));
  } catch {
    return undefined;
  }
};

const writeCache = (stage: string, key: string, value: unknown): void => {
  const p = cachePath(stage, key);
  mkdirSync(dirname(p), { recursive: true });
  writeFileSync(p, JSON.stringify(value));
};

let fetchCount = 0;
let retryCount = 0;
let throttleCount = 0;
const failures: Array<{ url: string; status: number | string }> = [];

// ADAPTIVE PER-HOST PACING. api.npmjs.org 429s hard at concurrency 10 — measured
// directly: three consecutive bulk requests all returned `HTTP/2 429` with
// `retry-after: 0`, which says "slow down" without saying how much. A fixed
// concurrency cap cannot answer that, because the sustainable rate differs per
// host (ecosyste.ms served 83 pages/min happily while npm was refusing) and
// varies over time. So each host carries its own inter-request spacing that
// MULTIPLICATIVELY BACKS OFF on a 429 and decays back down on sustained success —
// AIMD, the same shape as TCP congestion control. The gate is a per-host "next
// allowed time" cursor, so it composes with pmap's concurrency window rather than
// replacing it: N workers stay in flight but their requests are spaced.
const paceDelay = new Map<string, number>();
const paceNext = new Map<string, number>();
const paceOk = new Map<string, number>();
// Calibrated, not guessed. Under pacing the ceiling is 1/delay requests per
// second regardless of concurrency, and api.npmjs.org was measured sustaining
// ~2,400 lookups/min (40/s) — so the useful delay range is TENS of milliseconds.
// A first cut used a 120 ms floor and a 3,000 ms cap, which pinned throughput at
// ~116/min: the cap alone was two orders of magnitude past what the host needed.
// Tuned to what the host actually serves rather than to what makes 429s vanish.
// The burst test showed api.npmjs.org happily serving ~7 requests/s at 2 workers
// WHILE rejecting 37% of them — so a steady one-third rejection rate is the normal
// operating point, not a failure, and the right response to a 429 is a short
// retry rather than a wait that grows. An earlier cut paired a 400 ms pacing
// ceiling with 429 waits that escalated to 1.5 s and stacked on top of it, which
// cost ~1.3 s per twice-rejected request and pinned real throughput at 43/min.
const PACE_MIN_STEP = 10;
const PACE_MAX = 120;
const THROTTLE_RETRY_MS = 150;

// Per-host concurrency ceiling, measured not guessed. A burst test against
// api.npmjs.org's per-package endpoint: 2 workers -> 37% of responses 429, 4
// workers -> 100% 429, 8 workers -> 100% 429. So that host tolerates roughly one
// in-flight request with pacing, and no amount of retrying buys throughput —
// only spacing does. registry.npmjs.org is CDN-fronted and never throttled
// across ~6,400 control requests, so it keeps the full concurrency budget.
const HOST_CONCURRENCY: Record<string, number> = { "api.npmjs.org": 3 };
const hostInFlight = new Map<string, number>();

const acquireHost = async (host: string): Promise<void> => {
  const cap = HOST_CONCURRENCY[host];
  if (cap === undefined) return;
  for (;;) {
    const n = hostInFlight.get(host) ?? 0;
    if (n < cap) {
      hostInFlight.set(host, n + 1);
      return;
    }
    await new Promise((r) => setTimeout(r, 25));
  }
};

const releaseHost = (host: string): void => {
  if (HOST_CONCURRENCY[host] === undefined) return;
  hostInFlight.set(host, Math.max(0, (hostInFlight.get(host) ?? 1) - 1));
};

const pace = async (host: string): Promise<void> => {
  const delay = paceDelay.get(host) ?? 0;
  if (delay === 0) return;
  const now = Date.now();
  const at = Math.max(now, paceNext.get(host) ?? 0);
  paceNext.set(host, at + delay);
  if (at > now) await new Promise((r) => setTimeout(r, at - now));
};

const paceThrottled = (host: string): void => {
  throttleCount++;
  paceOk.set(host, 0);
  paceDelay.set(host, Math.min(PACE_MAX, Math.max(PACE_MIN_STEP, (paceDelay.get(host) ?? 0) * 2)));
};

const paceSucceeded = (host: string): void => {
  const delay = paceDelay.get(host) ?? 0;
  if (delay === 0) return;
  const ok = (paceOk.get(host) ?? 0) + 1;
  // Decay only after a run of clean responses, so one lucky reply cannot undo a
  // backoff the host actually asked for.
  if (ok < 20) {
    paceOk.set(host, ok);
    return;
  }
  paceOk.set(host, 0);
  paceDelay.set(host, delay <= PACE_MIN_STEP ? 0 : delay * 0.75);
};

// Retry only transient shapes (429, 5xx, network). A 404 is DATA — an unpublished
// or renamed package — so it returns immediately rather than burning six retries.
const getJSON = async (
  url: string,
  headers: Record<string, string> = {},
): Promise<{ ok: true; body: unknown } | { ok: false; status: number | string }> => {
  const host = new URL(url).host;
  let lastWas429 = false;
  for (let attempt = 0; attempt < 8; attempt++) {
    if (attempt > 0) {
      retryCount++;
      // A 429 is answered by SPACING, not by a long sleep. The burst test showed
      // throughput is set purely by inter-request spacing, so a ladder climbing to
      // 30 s just parks a worker while the pacing gate already carries the fix —
      // and on a run of throttles the ladder is what collapses throughput. Cap the
      // per-attempt wait near the pacing ceiling and let the gate do the work.
      const wait = lastWas429 ? THROTTLE_RETRY_MS : Math.min(20_000, 400 * 2 ** attempt);
      await new Promise((r) => setTimeout(r, wait + Math.random() * 150));
    }
    lastWas429 = false;
    try {
      await pace(host);
      await acquireHost(host);
      fetchCount++;
      let res: Response;
      try {
        res = await fetch(url, { headers: { "User-Agent": UA, ...headers } });
      } finally {
        releaseHost(host);
      }
      if (res.ok) {
        paceSucceeded(host);
        return { ok: true, body: await res.json() };
      }
      if (res.status === 404) return { ok: false, status: 404 };
      if (res.status === 429) {
        paceThrottled(host);
        lastWas429 = true;
      } else if (res.status < 500) {
        failures.push({ url, status: res.status });
        return { ok: false, status: res.status };
      }
    } catch (e) {
      if (attempt === 7) {
        failures.push({ url, status: String(e) });
        return { ok: false, status: String(e) };
      }
    }
  }
  failures.push({ url, status: "throttled-out" });
  return { ok: false, status: "throttled-out" };
};

// Fixed in-flight window rather than batching, so one slow response cannot stall
// a whole batch behind it.
const pmap = async <T, R>(items: T[], limit: number, fn: (item: T) => Promise<R>): Promise<R[]> => {
  const out: R[] = new Array(items.length);
  let next = 0;
  await Promise.all(
    new Array(Math.min(limit, items.length)).fill(0).map(async () => {
      for (;;) {
        const i = next++;
        if (i >= items.length) return;
        out[i] = await fn(items[i]);
      }
    }),
  );
  return out;
};

const bar = (label: string, done: number, total: number): void => {
  if (done % 50 === 0 || done === total) {
    process.stderr.write(`\r  ${label}: ${done}/${total}      `);
    if (done === total) process.stderr.write("\n");
  }
};

const enc = (name: string): string => name.replace("/", "%2F");

// ------------------------------------------------------------- stage 1: ranking

type RankRow = { name: string; monthly: number; latest: string | null; synced: string | null };

// ECOSYSTE.MS VALUES ARE STALE, AND `last_synced_at` BOUNDS HOW STALE.
// Its downloads figure is npm's last-month, but only for entries it synced
// recently; for the rest it reports whatever the package was doing when it was
// last crawled. Measured against npm's own last-month over 849 paired unscoped
// samples spread across ranks 100-200,000:
//
//   sync age   n    median err   MAX err
//   <1d      240        1.017      3.1x
//   1-3d     127        1.02       2.2x
//   3-7d      63        1.025      3.6x
//   7-14d     75        1.023      5.4x
//   14-30d   135        1.069     24.7x
//   30-90d   113        1.061    134.5x   (env-runner: eco 484,445 vs real 65,143,376)
//
// Two things follow, and the second is the one that is easy to get wrong:
//
//   1. The error TAIL grows monotonically with sync age, so age BOUNDS the error.
//      That is what makes eco usable at all: a deep-ranked entry can only be a
//      real threshold member if its age admits a large enough correction.
//
//   2. Freshness is NOT byte-exactness. Even under one day, 9 of 299 entries were
//      off by more than 10% (worst 3.09x) and only 11.7% matched to the byte. A
//      spot-check of top packages suggests otherwise only because the very top of
//      the ranking is re-synced constantly — minimatch matching exactly is luck,
//      not a property. So the age signal is used ONLY as a bound on how far a
//      value can be wrong, never as licence to trust a value.
//
// The band maxima below are multiplied by ECO_ERROR_MARGIN because a max over 849
// samples is an estimate of a tail, not a proven ceiling — treating an observed
// maximum as a hard bound is exactly how a sweep silently loses its completeness
// claim. Packages the gate excludes are still counted and reported, so the
// completeness statement rests on a measured quantity rather than an assumption.
const ECO_ERROR_BANDS: Array<[number, number]> = [
  [1, 3.1],
  [3, 2.2],
  [7, 3.6],
  [14, 5.4],
  [30, 24.7],
  [90, 134.5],
  [Infinity, 134.5],
];
const ECO_ERROR_MARGIN = 3;

const ecoErrorBound = (syncedAt: string | null, now: number): number => {
  if (!syncedAt) return ECO_ERROR_BANDS[ECO_ERROR_BANDS.length - 1][1] * ECO_ERROR_MARGIN;
  const ageDays = (now - Date.parse(syncedAt)) / 86_400_000;
  for (const [maxAge, err] of ECO_ERROR_BANDS) if (ageDays < maxAge) return err * ECO_ERROR_MARGIN;
  return ECO_ERROR_BANDS[ECO_ERROR_BANDS.length - 1][1] * ECO_ERROR_MARGIN;
};

const ECO = "https://packages.ecosyste.ms/api/v1/registries/npmjs.org/packages";

// Each ecosyste.ms page is ~1.3 MB gzipped but ~16 MB decompressed — the records
// carry full repo metadata this census never reads. So pages are DISTILLED to
// three fields before caching (6 KB, not 16 MB: caching raw projected to 6.5 GB)
// and fetched in parallel WAVES rather than serially, since the serial loop spent
// its time parsing and rewriting 16 MB per page rather than on the network. The
// wave shape preserves the adaptive stop: fetch `CONCURRENCY` pages at once, then
// halt once a wave crosses the floor.
// Cache stage is "rank2": the v1 distillation dropped last_synced_at, without
// which the staleness gate cannot run, so those entries must not be reused.
const fetchRankPage = async (page: number): Promise<RankRow[] | null> => {
  const key = `page-${String(page).padStart(5, "0")}`;
  const cached = readCache("rank2", key) as RankRow[] | undefined;
  if (Array.isArray(cached)) return cached;
  const r = await getJSON(`${ECO}?sort=downloads&order=desc&per_page=100&page=${page}`);
  if (!r.ok) return null;
  const list = r.body as Array<Record<string, unknown>>;
  if (!Array.isArray(list)) return null;
  const rows: RankRow[] = list.map((p) => ({
    name: String(p.name),
    monthly: Number(p.downloads ?? 0),
    latest: (p.latest_release_number as string | null) ?? null,
    synced: (p.last_synced_at as string | null) ?? null,
  }));
  writeCache("rank2", key, rows);
  return rows;
};

const stageRank = async (): Promise<RankRow[]> => {
  console.error(`[rank] ecosyste.ms downloads-desc sweep; stop below ${ECO_FLOOR.toLocaleString()} monthly`);
  const rows: RankRow[] = [];
  let prev = Infinity;
  let sortViolations = 0;
  let lastFetched = 0;
  let failed = 0;
  for (let base = 1; base <= MAX_PAGES; base += CONCURRENCY) {
    const wave = [];
    for (let p = base; p < base + CONCURRENCY && p <= MAX_PAGES; p++) wave.push(p);
    const pages = await pmap(wave, CONCURRENCY, fetchRankPage);
    let stop = false;
    for (let i = 0; i < pages.length; i++) {
      const list = pages[i];
      if (list === null) {
        console.error(`\n[rank] page ${wave[i]} failed; stopping sweep`);
        failed++;
        stop = true;
        break;
      }
      if (list.length === 0) {
        stop = true;
        break;
      }
      lastFetched = wave[i];
      for (const p of list) {
        // Monotonicity check on the source's own sort — the one control that
        // catches a paginating API silently reordering underneath us.
        if (p.monthly > prev) sortViolations++;
        prev = p.monthly;
        rows.push(p);
      }
      if (prev < ECO_FLOOR) stop = true;
    }
    bar("rank pages", lastFetched, MAX_PAGES);
    if (stop) break;
  }
  process.stderr.write("\n");
  if (failed > 0) failures.push({ url: "ecosyste.ms rank sweep", status: `${failed} page(s) failed` });
  console.error(`[rank] ${rows.length} packages over ${lastFetched} pages; sort violations: ${sortViolations}`);
  writeFileSync(join(OUT, "ranking.ndjson"), rows.map((r) => JSON.stringify(r)).join("\n") + "\n");
  writeCache("meta", "rank-stats", { packages: rows.length, pages: lastFetched, sortViolations, floorMonthly: prev });
  return rows;
};

const loadRank = (): RankRow[] => {
  const p = join(OUT, "ranking.ndjson");
  if (!existsSync(p)) throw new Error("no ranking.ndjson — run --stage rank first");
  return readFileSync(p, "utf8")
    .split("\n")
    .filter(Boolean)
    .map((l) => JSON.parse(l) as RankRow);
};

// ------------------------------------------- stage 2: package-level weekly total

const POINT = "https://api.npmjs.org/downloads/point/last-week/";

const stageWeekly = async (names: string[], label: string): Promise<Map<string, number>> => {
  const weekly = new Map<string, number>();
  const todo: string[] = [];
  for (const n of names) {
    const c = readCache("weekly", n) as { downloads?: number } | undefined;
    if (c && typeof c.downloads === "number") weekly.set(n, c.downloads);
    else todo.push(n);
  }
  console.error(`[${label}] ${weekly.size} cached, ${todo.length} to fetch`);
  if (todo.length === 0) return weekly;

  const unresolved: string[] = [];
  const record = (n: string, d: number): void => {
    weekly.set(n, d);
    writeCache("weekly", n, { downloads: d });
  };

  const plain = todo.filter((n) => !n.startsWith("@"));
  const scoped = todo.filter((n) => n.startsWith("@"));
  const batches: string[][] = [];
  for (let i = 0; i < plain.length; i += 128) batches.push(plain.slice(i, i + 128));

  let done = 0;
  await pmap(batches, CONCURRENCY, async (batch) => {
    const r = await getJSON(POINT + batch.join(","));
    if (r.ok) {
      const body = r.body as Record<string, { downloads?: number } | null>;
      for (const n of batch) {
        const e = body[n];
        record(n, e && typeof e.downloads === "number" ? e.downloads : 0);
      }
    } else if (r.status === 404 || (typeof r.status === "number" && r.status < 500)) {
      // Split to singles ONLY for a real per-name data problem (a 404 or a 4xx
      // that is not throttling), so one bad name cannot void 128 rows. Never on a
      // 429: exploding a throttled batch into 128 individual requests amplifies
      // load 128x at exactly the moment the host asked for less, which is what
      // drove sustained throughput from 2,400/min down to 116/min on the first
      // attempt. A throttled batch is left uncached and picked up by the next run.
      for (const n of batch) {
        const s = await getJSON(POINT + enc(n));
        if (s.ok) record(n, Number((s.body as { downloads?: number }).downloads ?? 0));
        else unresolved.push(n);
      }
    } else {
      unresolved.push(...batch);
    }
    bar(`${label} bulk`, ++done, batches.length);
  });

  // A FAILED LOOKUP MUST NOT BE CACHED AS ZERO. Recording 0 for a throttled
  // request makes the package read as below-threshold forever, and because the
  // zero is cached, a re-run will not correct it — the census silently loses real
  // members. This bug put 431 zeros in the cache and dropped 219 packages from a
  // band whose LOWEST member has 54.6M monthly downloads. Leave a failure
  // uncached so a later run retries it, and count it as unresolved.
  done = 0;
  await pmap(scoped, CONCURRENCY, async (n) => {
    const r = await getJSON(POINT + enc(n));
    if (r.ok) record(n, Number((r.body as { downloads?: number }).downloads ?? 0));
    else unresolved.push(n);
    bar(`${label} scoped`, ++done, scoped.length);
  });

  // One retry sweep over batches the host throttled, now that pacing has settled
  // to a rate it accepts. Anything still unresolved stays uncached, so a later run
  // picks it up rather than silently recording a zero.
  if (unresolved.length > 0) {
    console.error(`[${label}] retrying ${unresolved.length} throttled names at settled pacing`);
    let retried = 0;
    const rebatch: string[][] = [];
    for (let i = 0; i < unresolved.length; i += 128) rebatch.push(unresolved.slice(i, i + 128));
    await pmap(rebatch, Math.max(2, Math.floor(CONCURRENCY / 2)), async (batch) => {
      const r = await getJSON(POINT + batch.join(","));
      if (r.ok) {
        const body = r.body as Record<string, { downloads?: number } | null>;
        for (const n of batch) {
          const e = body[n];
          record(n, e && typeof e.downloads === "number" ? e.downloads : 0);
        }
      } else {
        for (const n of batch) failures.push({ url: `weekly ${n}`, status: r.status });
      }
      bar(`${label} retry`, ++retried, rebatch.length);
    });
  }

  return weekly;
};

// ------------------------------------------------- stage 3: per-version scripts

// The only three lifecycle keys npm/pnpm/nub run for a registry DEPENDENCY.
const RUN_KEYS = ["preinstall", "install", "postinstall"];
// Recorded but never counted as install-relevant: these do not run for a
// dependency install (verified — `npm install uuid`, whose only script is
// `prepare`, does not run it).
const INERT_KEYS = ["prepare", "prepublish", "prepublishOnly", "prepack", "postpack"];

type VersionInfo = {
  v: string;
  run: Record<string, string>;
  inert: string[];
  optdeps: number;
  os: string[] | null;
  cpu: string[] | null;
};
type Manifest = {
  name: string;
  status: "ok" | "not-found" | "error";
  latest: string | null;
  deprecated_latest: boolean;
  versions: VersionInfo[];
};

const distill = (name: string, doc: Record<string, unknown>): Manifest => {
  const versions = (doc.versions ?? {}) as Record<string, Record<string, unknown>>;
  const tags = (doc["dist-tags"] ?? {}) as Record<string, string>;
  const out: VersionInfo[] = [];
  for (const [v, e] of Object.entries(versions)) {
    const scripts = (e.scripts ?? {}) as Record<string, string>;
    const run: Record<string, string> = {};
    for (const k of RUN_KEYS) {
      if (typeof scripts[k] === "string" && scripts[k].trim() !== "") run[k] = scripts[k];
    }
    out.push({
      v,
      run,
      inert: INERT_KEYS.filter((k) => typeof scripts[k] === "string" && scripts[k].trim() !== ""),
      optdeps: Object.keys((e.optionalDependencies ?? {}) as object).length,
      os: (e.os as string[] | undefined) ?? null,
      cpu: (e.cpu as string[] | undefined) ?? null,
    });
  }
  const latest = tags.latest ?? null;
  return {
    name,
    status: "ok",
    latest,
    deprecated_latest: Boolean(latest && typeof versions[latest]?.deprecated === "string"),
    versions: out,
  };
};

const stageManifests = async (names: string[]): Promise<Map<string, Manifest>> => {
  const have = new Map<string, Manifest>();
  const todo: string[] = [];
  for (const n of names) {
    const c = readCache("manifest", n) as Manifest | undefined;
    if (c) have.set(n, c);
    else todo.push(n);
  }
  console.error(`[manifest] ${have.size} cached, ${todo.length} to fetch (full packuments)`);
  let done = 0;
  await pmap(todo, CONCURRENCY, async (n) => {
    const r = await getJSON(`https://registry.npmjs.org/${enc(n)}`);
    const m: Manifest = r.ok
      ? distill(n, r.body as Record<string, unknown>)
      : {
          name: n,
          status: r.status === 404 ? "not-found" : "error",
          latest: null,
          deprecated_latest: false,
          versions: [],
        };
    writeCache("manifest", n, m);
    have.set(n, m);
    bar("manifest", ++done, todo.length);
  });
  return have;
};

// ---------------------------------------- stage 4: per-version download counts

// Fetched ONLY for packages with at least one install-script version anywhere in
// their history. A package where no version ever had a run script is a true
// negative under every N, so its per-version split cannot change any verdict —
// that is what keeps this to one request per candidate rather than per
// threshold-member.
const stageVersionDownloads = async (names: string[]): Promise<Map<string, Record<string, number>>> => {
  const have = new Map<string, Record<string, number>>();
  const todo: string[] = [];
  for (const n of names) {
    const c = readCache("verdl", n) as Record<string, number> | undefined;
    if (c) have.set(n, c);
    else todo.push(n);
  }
  console.error(`[versions] ${have.size} cached, ${todo.length} to fetch (per-version weekly)`);
  let done = 0;
  await pmap(todo, CONCURRENCY, async (n) => {
    const r = await getJSON(`https://api.npmjs.org/versions/${enc(n)}/last-week`);
    const dl = r.ok ? ((r.body as { downloads?: Record<string, number> }).downloads ?? {}) : {};
    writeCache("verdl", n, dl);
    have.set(n, dl);
    bar("versions", ++done, todo.length);
  });
  return have;
};

// -------------------------------------------------------------------- reporting

// Coarse triage of what a script DEMANDS of a sandbox, ordered most-specific
// first (first match wins). Its job is to expose the SPREAD across a package's
// top versions — sharp 0.32.6's libvips+node-gyp path and 0.33.5's `node
// install/check` are different demands, and a corpus that collapses sharp to one
// row hides that. It is triage, not a substitute for reading the script when
// writing a grant.
const SHAPES: Array<[string, RegExp]> = [
  ["native-compile", /node-gyp|cmake-js|node-pre-gyp\s+(?:install|rebuild)|\bgyp\b|\bmake\b|\bgcc\b|clang\+\+/i],
  ["prebuilt-download", /prebuild-install|node-pre-gyp|prebuildify|napi-postinstall|install-artifact|binary-install/i],
  ["network-fetch", /\bcurl\b|\bwget\b|https?:\/\//i],
  ["package-manager", /\bnpm\s+(?:install|i|ci|run)\b|\bpnpm\b|\byarn\b|\bnpx\b/i],
  ["patch-tooling", /patch-package|husky|lefthook|simple-git-hooks|yorkie/i],
  ["noop-guard", /^\s*(?:true|exit\s+0|echo\b)/i],
  ["node-script", /(?:^|\|\||&&|;)\s*node\s/i],
  ["shell", /^\s*(?:sh|bash|zsh|\.\/)|\|\||&&|;/],
];

const shapeOf = (cmd: string): string => {
  for (const [name, re] of SHAPES) if (re.test(cmd)) return name;
  return "other";
};

const main = async (): Promise<void> => {
  mkdirSync(OUT, { recursive: true });

  const rank = STAGE === undefined || STAGE === "rank" ? await stageRank() : loadRank();
  if (STAGE === "rank") return;

  // PREWARM. The two bottlenecks sit on different hosts — npm's download API
  // throttles to ~76 lookups/min while the registry's packuments come off a CDN
  // that never throttled across ~6,400 control requests — so the manifest fetch
  // can run concurrently with, rather than after, the download sweep. This stage
  // populates the manifest cache for the top `--prewarm-top` ranked names
  // independently of the threshold, taking the registry work off the critical path.
  if (STAGE === "prewarm") {
    const names = rank.slice(0, num("--prewarm-top", 10_000)).map((r) => r.name);
    await stageManifests(names);
    return;
  }

  // The seed control set: names that must appear in the census even if the
  // ranking sweep never surfaced them, so a pipeline hole shows up as an explicit
  // below-threshold row rather than a silent absence.
  const seeds = new Set<string>();
  for (const f of flagAll("--seed")) {
    const raw = readFileSync(f, "utf8").trim();
    const names: string[] = raw.startsWith("[")
      ? (JSON.parse(raw) as string[])
      : raw
          .split("\n")
          .map((l) => l.trim())
          .filter((l) => l !== "" && !l.startsWith("#"));
    for (const n of names) seeds.add(n);
  }

  const byName = new Map<string, RankRow>(rank.map((r) => [r.name, r]));
  // Names the ranking sweep surfaced, captured BEFORE seeds are merged in — the
  // difference is a direct measurement of the ranking source's recall.
  const fromSweep = new Set(byName.keys());
  for (const n of seeds) if (!byName.has(n)) byName.set(n, { name: n, monthly: 0, latest: null, synced: null });

  // WHERE THE EXPENSIVE BUDGET GOES. npm's per-package endpoint tolerates ~400
  // lookups/min and scoped names cannot be batched, so asking it about every
  // candidate in a deep sweep is hours of throttled traffic. Instead, ask only
  // where eco's own staleness admits the package could clear the bar: a candidate
  // is plausible if eco_monthly x errorBound(sync age) reaches the threshold's
  // monthly equivalent. A freshly-synced entry deep in the ranking is genuinely
  // small and can be excluded cheaply; a months-stale entry gets checked even at a
  // low reported value, which is exactly the recently-grown case that matters.
  const MONTHLY_PER_WEEKLY = 4;
  const now = Date.now();
  const thresholdMonthly = THRESHOLD * MONTHLY_PER_WEEKLY;
  const plausible: string[] = [];
  const excluded: RankRow[] = [];
  for (const r of byName.values()) {
    if (seeds.has(r.name) || r.monthly * ecoErrorBound(r.synced, now) >= thresholdMonthly) plausible.push(r.name);
    else excluded.push(r);
  }
  console.error(
    `[gate] ${plausible.length} plausible candidates to query npm; ${excluded.length} excluded by the staleness-scaled bound`,
  );

  const weekly = await stageWeekly(plausible, "weekly");
  if (STAGE === "weekly") return;

  const above = plausible.filter((n) => (weekly.get(n) ?? 0) >= THRESHOLD);
  const seedBelow = [...seeds].filter((n) => (weekly.get(n) ?? 0) < THRESHOLD);
  console.error(`[gate] ${above.length} packages at or above ${THRESHOLD.toLocaleString()} weekly downloads`);

  // Seeds below threshold are still classified — that is how "did we drop a known
  // corpus member, and why" becomes answerable rather than a guess.
  const manifests = await stageManifests([...above, ...seedBelow]);
  if (STAGE === "manifest") return;

  const everHadRun = (m: Manifest | undefined): boolean =>
    Boolean(m && m.versions.some((v) => Object.keys(v.run).length > 0));
  const candidates = [...above, ...seedBelow].filter((n) => everHadRun(manifests.get(n)));
  console.error(`[candidates] ${candidates.length} packages have >=1 install-script version anywhere in history`);

  const verdl = await stageVersionDownloads(candidates);
  if (STAGE === "versions") return;

  // ---- per-(package,version) rows: the stored unit, so any N re-derives ------

  type VRow = {
    package: string;
    version: string;
    package_weekly_downloads: number;
    version_weekly_downloads: number | null;
    version_download_rank: number;
    in_top_n: boolean;
    is_latest: boolean;
    has_install_script: boolean;
    install_script_keys: string[];
    install_scripts: Record<string, string>;
    install_script_shapes: string[];
    inert_script_keys: string[];
    optional_deps_count: number;
    os: string[] | null;
    cpu: string[] | null;
  };

  const vrows: VRow[] = [];
  for (const name of candidates) {
    const m = manifests.get(name);
    if (!m) continue;
    const dl = verdl.get(name) ?? {};
    const ranked = [...m.versions].sort((a, b) => (dl[b.v] ?? 0) - (dl[a.v] ?? 0));
    const topSet = new Set(ranked.slice(0, TOP_N).map((v) => v.v));
    ranked.forEach((v, i) => {
      const keys = Object.keys(v.run);
      vrows.push({
        package: name,
        version: v.v,
        package_weekly_downloads: weekly.get(name) ?? 0,
        version_weekly_downloads: dl[v.v] ?? null,
        version_download_rank: i + 1,
        in_top_n: topSet.has(v.v),
        is_latest: v.v === m.latest,
        has_install_script: keys.length > 0,
        install_script_keys: keys,
        install_scripts: v.run,
        install_script_shapes: [...new Set(keys.map((k) => shapeOf(v.run[k])))],
        inert_script_keys: v.inert,
        optional_deps_count: v.optdeps,
        os: v.os,
        cpu: v.cpu,
      });
    });
  }
  vrows.sort(
    (a, b) =>
      b.package_weekly_downloads - a.package_weekly_downloads ||
      (b.version_weekly_downloads ?? -1) - (a.version_weekly_downloads ?? -1),
  );
  writeFileSync(join(OUT, "census-versions.ndjson"), vrows.map((r) => JSON.stringify(r)).join("\n") + "\n");

  // ---- package rollup -------------------------------------------------------

  const vrowsByPkg = new Map<string, VRow[]>();
  for (const r of vrows) {
    const list = vrowsByPkg.get(r.package);
    if (list) list.push(r);
    else vrowsByPkg.set(r.package, [r]);
  }

  const relevantUnder = (name: string, n: number): boolean =>
    (vrowsByPkg.get(name) ?? []).some((r) => r.version_download_rank <= n && r.has_install_script);

  const pkgRows = [...above, ...seedBelow].map((name) => {
    const m = manifests.get(name);
    const rows = vrowsByPkg.get(name) ?? [];
    const top = rows.filter((r) => r.in_top_n);
    const relevantVersions = top.filter((r) => r.has_install_script);
    const shapes = [...new Set(relevantVersions.flatMap((r) => r.install_script_shapes))].sort();
    return {
      name,
      package_weekly_downloads: weekly.get(name) ?? 0,
      monthly_downloads_ecosystems: byName.get(name)?.monthly ?? 0,
      above_threshold: (weekly.get(name) ?? 0) >= THRESHOLD,
      latest_version: m?.latest ?? null,
      versions_total: m?.versions.length ?? 0,
      top_versions: top.map((r) => ({
        version: r.version,
        weekly: r.version_weekly_downloads,
        install_script_keys: r.install_script_keys,
        shapes: r.install_script_shapes,
      })),
      has_install_script_top_n: relevantVersions.length > 0,
      has_install_script_latest_only: rows.some((r) => r.is_latest && r.has_install_script),
      has_install_script_any_version: rows.some((r) => r.has_install_script),
      install_relevant_top_versions: relevantVersions.length,
      script_shapes_in_top_n: shapes,
      shape_spread_in_top_n: shapes.length,
      deprecated_latest: m?.deprecated_latest ?? false,
      manifest_status: m?.status ?? "error",
      seeded: seeds.has(name),
    };
  });
  pkgRows.sort((a, b) => b.package_weekly_downloads - a.package_weekly_downloads);
  const inBand = pkgRows.filter((r) => r.above_threshold);
  inBand.forEach((r, i) => Object.assign(r, { weekly_rank: i + 1 }));
  writeFileSync(join(OUT, "census-packages.ndjson"), pkgRows.map((r) => JSON.stringify(r)).join("\n") + "\n");

  // ---- summary --------------------------------------------------------------

  const relevant = inBand.filter((r) => r.has_install_script_top_n);
  const bandAt = (n: number, pick: (r: (typeof inBand)[number]) => boolean): number =>
    inBand.slice(0, n).filter(pick).length;

  const shapeHistogram: Record<string, number> = {};
  for (const r of relevant) for (const s of r.script_shapes_in_top_n) shapeHistogram[s] = (shapeHistogram[s] ?? 0) + 1;

  const keyHistogram: Record<string, number> = {};
  for (const r of relevant) {
    for (const k of new Set(r.top_versions.flatMap((t) => t.install_script_keys))) {
      keyHistogram[k] = (keyHistogram[k] ?? 0) + 1;
    }
  }

  // GATE CONTROL. The staleness bound rests on band maxima estimated from 849
  // samples, so it is validated rather than trusted: draw a sample from the
  // EXCLUDED set — biased toward the ones closest to passing, where a failure
  // would surface first — and ask npm directly. Any excluded package found at or
  // above the threshold is a real completeness breach, reported by name.
  const gateSample = excluded
    .map((r) => ({ name: r.name, headroom: r.monthly * ecoErrorBound(r.synced, now) }))
    .sort((a, b) => b.headroom - a.headroom)
    .slice(0, GATE_SAMPLE)
    .map((x) => x.name);
  const gateWeekly = gateSample.length > 0 ? await stageWeekly(gateSample, "gate-control") : new Map<string, number>();
  const gateBreaches = gateSample
    .filter((n) => (gateWeekly.get(n) ?? 0) >= THRESHOLD)
    .map((n) => ({ name: n, weekly: gateWeekly.get(n) ?? 0, eco_monthly: byName.get(n)?.monthly ?? 0 }));

  const summary = {
    meta: {
      generated: RUN_AT,
      threshold_weekly_downloads: THRESHOLD,
      top_versions_per_package: TOP_N,
      download_window: "last-week (api.npmjs.org/downloads/point/last-week and /versions/<pkg>/last-week)",
      ranking_source: `${ECO}?sort=downloads&order=desc — complete registry enumeration; its downloads field is npm last-month`,
      script_source: "registry.npmjs.org/<pkg> full packument, scripts object per version",
      install_script_definition: `any of ${RUN_KEYS.join("/")} — the only keys run for a registry dependency. ${INERT_KEYS.join("/")} recorded separately, never counted.`,
      inclusion_rule: `install-relevant = any of a package's top ${TOP_N} versions by weekly downloads has a run script`,
      eco_floor_monthly: ECO_FLOOR,
      rank_sweep: readCache("meta", "rank-stats") ?? {},
      requests: fetchCount,
      retries: retryCount,
      throttle_429s: throttleCount,
      final_host_pacing_ms: Object.fromEntries(paceDelay),
      fetch_failures: failures.length,
      fetch_failure_sample: failures.slice(0, 40),
    },
    headline: {
      packages_above_threshold: inBand.length,
      install_relevant_top_n: relevant.length,
      share_install_relevant: inBand.length ? `${((relevant.length / inBand.length) * 100).toFixed(2)}%` : null,
      threshold_lands_at_rank: inBand.length,
      lowest_weekly_in_band: inBand.length ? inBand[inBand.length - 1].package_weekly_downloads : null,
    },
    // The direct measurement of what latest-only classification costs — the
    // reconciliation lever for the predecessor corpus's rank-band figures.
    definition_sensitivity: {
      install_relevant_latest_only: inBand.filter((r) => r.has_install_script_latest_only).length,
      install_relevant_top_1: inBand.filter((r) => relevantUnder(r.name, 1)).length,
      install_relevant_top_3: inBand.filter((r) => relevantUnder(r.name, 3)).length,
      install_relevant_top_5: inBand.filter((r) => relevantUnder(r.name, 5)).length,
      install_relevant_top_10: inBand.filter((r) => relevantUnder(r.name, 10)).length,
      install_relevant_any_version_ever: inBand.filter((r) => r.has_install_script_any_version).length,
      latest_only_false_negatives: inBand.filter((r) => r.has_install_script_top_n && !r.has_install_script_latest_only)
        .length,
      latest_only_false_negative_examples: inBand
        .filter((r) => r.has_install_script_top_n && !r.has_install_script_latest_only)
        .slice(0, 40)
        .map((r) => ({
          name: r.name,
          weekly: r.package_weekly_downloads,
          latest: r.latest_version,
          top: r.top_versions,
        })),
    },
    rank_bands: {
      note: "rank is by npm weekly downloads within this census; an output, never a cutoff",
      top_5500: {
        top_n: bandAt(5500, (r) => r.has_install_script_top_n),
        latest_only: bandAt(5500, (r) => r.has_install_script_latest_only),
        any_version: bandAt(5500, (r) => r.has_install_script_any_version),
      },
      top_10000: {
        top_n: bandAt(10000, (r) => r.has_install_script_top_n),
        latest_only: bandAt(10000, (r) => r.has_install_script_latest_only),
        any_version: bandAt(10000, (r) => r.has_install_script_any_version),
      },
      top_1000: { top_n: bandAt(1000, (r) => r.has_install_script_top_n) },
      top_15000: { top_n: bandAt(15000, (r) => r.has_install_script_top_n) },
      top_20000: { top_n: bandAt(20000, (r) => r.has_install_script_top_n) },
    },
    script_shape: {
      note: "coarse triage of what the script demands of a sandbox; a package counts once per distinct shape across its top-N",
      histogram: Object.fromEntries(Object.entries(shapeHistogram).sort((a, b) => b[1] - a[1])),
      lifecycle_key_histogram: Object.fromEntries(Object.entries(keyHistogram).sort((a, b) => b[1] - a[1])),
      packages_with_mixed_shapes_across_top_n: relevant.filter((r) => r.shape_spread_in_top_n > 1).length,
      mixed_shape_examples: relevant
        .filter((r) => r.shape_spread_in_top_n > 1)
        .slice(0, 30)
        .map((r) => ({
          name: r.name,
          weekly: r.package_weekly_downloads,
          shapes: r.script_shapes_in_top_n,
          top: r.top_versions,
        })),
    },
    seeds: {
      seeded: seeds.size,
      seeded_above_threshold: [...seeds].filter((n) => (weekly.get(n) ?? 0) >= THRESHOLD).length,
      seeded_below_threshold: seedBelow.length,
      seeded_below_threshold_but_install_relevant: seedBelow.filter((n) => relevantUnder(n, TOP_N)).length,
      seeded_below_threshold_detail: seedBelow
        .map((n) => ({ name: n, weekly: weekly.get(n) ?? 0, install_relevant: relevantUnder(n, TOP_N) }))
        .sort((a, b) => b.weekly - a.weekly),
    },
    // Recall of the ranking source, measured rather than assumed: a seeded name
    // that clears the threshold but never appeared in the sweep is a package the
    // sweep alone would have missed. Non-zero here means the census is
    // sweep-plus-seeds, not sweep-complete, and the gap must be reported.
    ranking_recall: {
      above_threshold_from_sweep: inBand.filter((r) => fromSweep.has(r.name)).length,
      above_threshold_seed_only: inBand.filter((r) => !fromSweep.has(r.name)).length,
      above_threshold_seed_only_names: inBand
        .filter((r) => !fromSweep.has(r.name))
        .map((r) => ({ name: r.name, weekly: r.package_weekly_downloads })),
    },
    // The completeness statement, resting on measured quantities: how many
    // candidates the staleness gate excluded, and whether a sample of the ones
    // closest to passing actually cleared the threshold.
    staleness_gate: {
      swept_candidates: byName.size,
      queried_npm: plausible.length,
      excluded: excluded.length,
      error_margin_over_observed_band_max: ECO_ERROR_MARGIN,
      observed_band_maxima: Object.fromEntries(ECO_ERROR_BANDS.map(([a, e]) => [`<${a}d`, e])),
      control_sample_size: gateSample.length,
      control_breaches: gateBreaches.length,
      control_breach_names: gateBreaches,
      verdict:
        gateSample.length === 0
          ? "not run"
          : gateBreaches.length === 0
            ? "no excluded package in the sample reached the threshold"
            : "BREACH — the gate dropped real threshold members; widen the margin and re-run",
    },
    unverified: {
      manifest_status_counts: pkgRows.reduce<Record<string, number>>((acc, r) => {
        acc[r.manifest_status] = (acc[r.manifest_status] ?? 0) + 1;
        return acc;
      }, {}),
      candidates_missing_version_downloads: candidates.filter((n) => Object.keys(verdl.get(n) ?? {}).length === 0)
        .length,
      note: "ecosyste.ms snapshots lag by days, so a package that crossed the threshold within the last week can be missed by the ranking sweep even though the npm weekly figure gating membership is current. The sweep floor sits ~4x below the threshold's monthly equivalent to absorb that.",
    },
  };
  writeFileSync(join(OUT, "summary.json"), JSON.stringify(summary, null, 2) + "\n");

  console.error("");
  console.error(`packages >= ${THRESHOLD.toLocaleString()} weekly downloads   : ${inBand.length}`);
  console.error(`  install-relevant (top-${TOP_N} versions)        : ${relevant.length}`);
  console.error(
    `  same, classified from latest only        : ${summary.definition_sensitivity.install_relevant_latest_only}`,
  );
  console.error(
    `  latest-only false negatives             : ${summary.definition_sensitivity.latest_only_false_negatives}`,
  );
  console.error(`threshold lands at rank                   : ${inBand.length}`);
  console.error(`wrote ${OUT}/{census-packages,census-versions}.ndjson and summary.json`);
};

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
