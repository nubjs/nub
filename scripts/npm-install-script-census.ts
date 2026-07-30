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
const ECO_FLOOR = num("--eco-floor", THRESHOLD);
const MAX_PAGES = num("--max-pages", 800);
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
const PACE_MIN_STEP = 15;
const PACE_MAX = 400;

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
  for (let attempt = 0; attempt < 8; attempt++) {
    if (attempt > 0) {
      retryCount++;
      await new Promise((r) => setTimeout(r, Math.min(30_000, 400 * 2 ** attempt) + Math.random() * 500));
    }
    try {
      await pace(host);
      fetchCount++;
      const res = await fetch(url, { headers: { "User-Agent": UA, ...headers } });
      if (res.ok) {
        paceSucceeded(host);
        return { ok: true, body: await res.json() };
      }
      if (res.status === 404) return { ok: false, status: 404 };
      if (res.status === 429) paceThrottled(host);
      if (res.status !== 429 && res.status < 500) {
        failures.push({ url, status: res.status });
        return { ok: false, status: res.status };
      }
    } catch (e) {
      if (attempt === 5) {
        failures.push({ url, status: String(e) });
        return { ok: false, status: String(e) };
      }
    }
  }
  failures.push({ url, status: "retries-exhausted" });
  return { ok: false, status: "retries-exhausted" };
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

type RankRow = { name: string; monthly: number; latest: string | null };

const ECO = "https://packages.ecosyste.ms/api/v1/registries/npmjs.org/packages";

// Each ecosyste.ms page is ~1.3 MB gzipped but ~16 MB decompressed — the records
// carry full repo metadata this census never reads. So pages are DISTILLED to
// three fields before caching (6 KB, not 16 MB: caching raw projected to 6.5 GB)
// and fetched in parallel WAVES rather than serially, since the serial loop spent
// its time parsing and rewriting 16 MB per page rather than on the network. The
// wave shape preserves the adaptive stop: fetch `CONCURRENCY` pages at once, then
// halt once a wave crosses the floor.
const fetchRankPage = async (page: number): Promise<RankRow[] | null> => {
  const key = `page-${String(page).padStart(4, "0")}`;
  const cached = readCache("rank", key) as RankRow[] | undefined;
  if (Array.isArray(cached)) return cached;
  const r = await getJSON(`${ECO}?sort=downloads&order=desc&per_page=100&page=${page}`);
  if (!r.ok) return null;
  const list = r.body as Array<Record<string, unknown>>;
  if (!Array.isArray(list)) return null;
  const rows: RankRow[] = list.map((p) => ({
    name: String(p.name),
    monthly: Number(p.downloads ?? 0),
    latest: (p.latest_release_number as string | null) ?? null,
  }));
  writeCache("rank", key, rows);
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
        record(n, s.ok ? Number((s.body as { downloads?: number }).downloads ?? 0) : 0);
      }
    } else {
      unresolved.push(...batch);
    }
    bar(`${label} bulk`, ++done, batches.length);
  });

  done = 0;
  await pmap(scoped, CONCURRENCY, async (n) => {
    const r = await getJSON(POINT + enc(n));
    record(n, r.ok ? Number((r.body as { downloads?: number }).downloads ?? 0) : 0);
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
        failures.push({ url: `weekly batch of ${batch.length}`, status: r.status });
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
  for (const n of seeds) if (!byName.has(n)) byName.set(n, { name: n, monthly: 0, latest: null });

  const weekly = await stageWeekly([...byName.keys()], "weekly");
  if (STAGE === "weekly") return;

  const above = [...byName.keys()].filter((n) => (weekly.get(n) ?? 0) >= THRESHOLD);
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
