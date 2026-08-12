# npm ecosystem data sources for install-script corpora

A survey of every practical way to answer three questions across the whole npm registry: which packages run install lifecycle scripts, at which versions, and how widely each version is installed.

Every endpoint below was called on 2026-08-01; the status codes, sizes and throughputs are measured, not quoted from documentation.

The registry itself is the only complete source, a metadata-only crawl is enough (no tarballs), and no bulk dataset anywhere provides per-version download counts.

## Summary

Eight candidate sources, scored on the two fields that matter — install-script presence and per-version downloads — plus the status each returned on 2026-08-01.

| Source | Carries install-script presence | Carries per-version downloads | Status 2026-08-01 |
| --- | --- | --- | --- |
| [Abbreviated packument](#abbreviated-packuments-corgi) | Yes, per version | No | Current, unthrottled |
| [Full packument](#full-packuments) | Yes, full `scripts` object | No | Current, unthrottled |
| [Replication feed](#replication-replicatenpmjscom) | No — names and revisions only | No | Current, `include_docs` removed |
| [Downloads API](#downloads-apinpmjsorg) | No | Yes, one package per request | Current |
| [ecosyste.ms](#ecosystems) | No | No — package-level monthly only | Current |
| [deps.dev](#depsdev) | No | No | Current |
| ClickHouse ClickPy | No | No | PyPI only; no npm equivalent |
| BigQuery public datasets | No | No | No npm downloads dataset exists |

## The registry is the only complete source

Four registry surfaces were tested: abbreviated packuments, full packuments, the replication feed, and the downloads API.

### Abbreviated packuments (corgi)

Requesting a packument with `accept: application/vnd.npm.install-v1+json` returns npm's abbreviated form, and **it carries a per-version `hasInstallScript` boolean**.

That makes a whole-registry survey cheap: install-script presence is a metadata read, not a tarball download.

```
# 481 versions of esbuild, 478 of them flagged
curl -H 'accept: application/vnd.npm.install-v1+json' https://registry.npmjs.org/esbuild
# HTTP 200, 887,361 bytes uncompressed
```

The abbreviated form omits the `scripts` object entirely, so it answers *whether* a version runs an install script but never *which* keys or *what* they run. Version records carry only `name`, `version`, `dist`, `bin`, `engines`, dependency maps and `hasInstallScript`.

The flag is trustworthy. Comparing it against the full packument's `scripts` object across 4,825 version pairs on six packages chosen to span the range — including two controls whose answers were known in advance — produced zero disagreements:

| Package | Versions compared | Flagged in abbreviated | `scripts` has preinstall/install/postinstall | Mismatches |
| --- | --- | --- | --- | --- |
| esbuild | 481 | 478 | 478 | 0 |
| typescript | 3,783 | 1 | 1 | 0 |
| lodash | 117 | 0 | 0 | 0 |
| husky | 215 | 165 | 165 | 0 |
| sharp | 189 | 171 | 171 | 0 |
| node-gyp-build | 40 | 0 | 0 | 0 |

So `hasInstallScript` is exactly `scripts ∩ {preinstall, install, postinstall} ≠ ∅`. The `prepare` key and the `pre*`/`post*` publish hooks do not set it, matching npm's own behavior — those keys never run for a registry dependency.

### The field is omitted when false, and it is not a recent addition

Two properties of the field are easy to get wrong in the unsafe direction.

**Absent means false.** The key is not always present: `node-sass` omits it on 2 of 148 versions, `canvas` on 1 of 159. Each of those versions has no install script in the full packument, so the registry omits the key rather than emitting `false`. Treating an absent key as false is correct; treating it as *unknown* and skipping the version would silently drop real data. Encode the distinction deliberately rather than relying on a truthiness check.

**It is backfilled across the whole history, not applied going forward.** It is present and true on `canvas@0.0.1`, `bcrypt@0.1.2`, `fsevents@0.1.1` and `node-sass@0.2.0` — versions published years before the abbreviated form existed. A consumer holding a cached packument from any era will therefore still see the field.

### Full packuments

Requesting with `accept: application/json` returns the full document, which carries the complete per-version `scripts` object and the `time` map of publish dates. It does **not** carry `hasInstallScript` — the two forms are complementary, not nested.

The accept header has to name `application/json` alone. Content negotiation prefers the abbreviated form whenever it is named or a `*/*` wildcard appears, regardless of q-values, so a permissive header silently yields a document with no `time` map. The same trap is documented in this repo's vendored engine at [`vendor/aube/scripts/generate-primer.mjs`](../../vendor/aube/scripts/generate-primer.mjs).

### Replication (`replicate.npmjs.com`)

The replication service is alive and reports the registry's true size:

```
$ curl https://replicate.npmjs.com/
{"db_name":"registry","engine":"npm-replicate","doc_count":4250224,"update_seq":122786079}
```

**Document retrieval through replication is gone.** npm's [replication API migration](https://github.com/orgs/community/discussions/152515) removed `include_docs` in the March–May 2025 transition, and the removal is real rather than merely documented:

```
$ curl -o /dev/null -w '%{http_code}\n' 'https://replicate.npmjs.com/registry/_changes?limit=1&include_docs=true'
400
$ curl -o /dev/null -w '%{http_code}\n' 'https://replicate.npmjs.com/registry/_all_docs?limit=2&include_docs=true'
400
$ curl -o /dev/null -w '%{http_code}\n' https://replicate.npmjs.com/esbuild
404
```

Supported parameters are now `doc_ids`, `descending`, `last-event-id`, `limit` and `since` on `_changes`, and `descending`, `startkey`/`endkey`, `inclusive_end`, `key`, `keys` and `limit` on `_all_docs`. The streaming feed modes (`feed=continuous`, `longpoll`, `eventsource`) went with them, so a consumer paginates manually. The `REPLICATE-API.md` file in the `npm/registry` repository still describes the pre-migration shape and should not be trusted.

What replication still does well is enumeration and change detection:

- **Full name list** — `_all_docs?limit=10000` returns 10,000 rows in about 0.4 s and roughly 1 MB. A `limit` of 20,000 returns 400, so the whole 4.25M-name list is 425 requests, a few minutes.
- **Change feed** — `update_seq` advanced 42 over 45 s (≈0.9 seq/s, ≈80,000 seq/day), and a sampled span carried 0.254 changes per seq unit, so roughly 20,000 packages change per day. Re-fetching that many abbreviated packuments takes under two minutes.

### Downloads (`api.npmjs.org`)

Package-level counts are bulk-friendly. Per-version counts are not, and there is exactly one endpoint for them anywhere.

**Per-version.** The only source is `https://api.npmjs.org/versions/<pkg>/last-week`, which returns a `{version: count}` map for every published version. It works for scoped names when the slash is percent-encoded. No other period exists:

```
$ for p in last-day last-month last-year; do curl -o /dev/null -w "$p %{http_code}\n" \
    "https://api.npmjs.org/versions/esbuild/$p"; done
last-day 404
last-month 404
last-year 404
```

This is a one-package-per-request endpoint with no bulk form. Twenty requests fired in parallel all returned 200, so it tolerates more concurrency than a conservative crawler assumes. Sustained high-volume use is a different regime and was not measured here; the harvester in [`vendor/aube/scripts/fetch-download-weights.mjs`](../../vendor/aube/scripts/fetch-download-weights.mjs) documents Cloudflare burst limiting on this endpoint and paces itself at 1.5 s between requests.

**Package-level.** Both `downloads/point/<period>/<names>` and `downloads/range/<from>:<to>/<names>` accept comma-separated names, with two hard limits that return explicit errors:

```
$ curl 'https://api.npmjs.org/downloads/point/last-week/lodash,@swc%2Fcore,react'
{"error":"scoped packages are not currently supported in bulk lookups"}   # HTTP 400

$ curl 'https://api.npmjs.org/downloads/point/last-week/<129 names>'
{"error":"exceeded max bulk size of 128"}                                  # HTTP 400
```

One scoped name poisons the whole request, so scoped packages must be fetched individually.

The range endpoint has a subtler trap: it **silently truncates an over-long window instead of erroring**. A 396-day request returned all 396 days, but a 943-day request returned 547 days with the start date moved forward and no indication that anything was dropped. Any caller assuming it got the window it asked for will quietly under-count.

## Third-party datasets

No source outside the registry carries install-script presence or per-version download counts. Only ecosyste.ms adds anything usable, in the form of a download-ranked package list.

### ecosyste.ms

The [ecosyste.ms packages API](https://packages.ecosyste.ms/) is current, free and unauthenticated, and it is the only source found here that offers **registry enumeration already ranked by downloads**.

The `registries/npmjs.org/packages?sort=downloads&order=desc` endpoint paginates in popularity order, which npm's own endpoints cannot do. Its `downloads` field is npm's last-month package total. Sampling 18 pages spread across ranks 30,000–200,000 returned monthly totals from 139,785 down to 2,238, consistent with a correctly sorted feed over that span.

Per-version records carry publish time, licenses, integrity, tarball URL and a `metadata` blob mirroring the registry's dist and `_npmUser` fields. They do not carry `scripts`, `hasInstallScript`, or per-version download counts.

### deps.dev

Google's [Open Source Insights](https://docs.deps.dev/bigquery/v1/) offers both a REST API and a free BigQuery public dataset.

Tables cover package versions, dependency graphs, advisories and publish timestamps. It carries **neither download counts nor any script information**, so it cannot answer either question this survey is about.

### Everything else

Five further sources, all discarded: ClickPy and BigQuery are PyPI-only, libraries.io is superseded by ecosyste.ms, Socket requires authentication, and the registry search endpoint carries neither field.

- **ClickHouse ClickPy** is PyPI-only. There is no npm counterpart.
- **BigQuery** hosts PyPI's per-download table (`bigquery-public-data.pypi.file_downloads`). No npm equivalent exists — the asymmetry is easy to assume away, because PyPI publishes raw download events and npm publishes only aggregates.
- **libraries.io** responds 200 and its API is still up, but ecosyste.ms is its successor and carries fresher, broader data.
- **Socket** requires authentication (`api.socket.dev` returns 401 unauthenticated).
- **The registry search endpoint** (`registry.npmjs.org/-/v1/search`) returns no script or download fields and cannot filter on them.

## Sizing the install-script population

Uniform random samples drawn from `_all_docs` and checked against abbreviated packuments, against a stratified sample taken from the ecosyste.ms popularity ranking:

| Band | Sample | Any version has an install script | Latest has one |
| --- | --- | --- | --- |
| Uniform random over the whole registry | 500 | 2.00% | 1.40% |
| Uniform random over the whole registry | 1,500 | 1.13% | 1.00% |
| Popularity ranks ≈30,000–200,000 (2,238–139,785 monthly downloads) | 1,773 | 5.30% | 2.31% |

Two conclusions follow, and they pull in opposite directions.

**By package count the population is large.** At 1–2% of 4,250,224 packages, somewhere between 48,000 and 85,000 packages have run an install script at some version. The wide interval is honest: these are cluster samples (each draw takes consecutive `_all_docs` rows from a random key prefix), so alphabetically adjacent names can be correlated and the true confidence interval is wider than a simple random sample would give.

**By install volume almost all of it is noise.** The 17 install-script packages found in the 1,500-package uniform sample had a **median of 14 weekly downloads**, a maximum of 2,630, and a combined total of 3,170 — none reached 100,000. The install-script rate is also *higher* in the mid-popularity band than in the deep tail, which means popularity ranking is a good filter rather than a biased one.

The practical reading: a corpus built for correctness-under-real-installs should be ranked by downloads and truncated, not enumerated exhaustively. A corpus built to characterize the ecosystem as a whole needs the full crawl, which is affordable anyway.

## What a whole-registry crawl costs

Measured against 800 randomly drawn packages fetching abbreviated packuments with gzip:

| Concurrency | Throughput | Non-200 responses | Extrapolated to 4.25M packages |
| --- | --- | --- | --- |
| 32 | 120 req/s | 0 | ≈9.9 h |
| 64 | 227 req/s | 0 | ≈5.2 h |

Compressed response sizes were heavily skewed — mean 3,036 bytes, median 881 bytes, maximum 530 KB — putting a full pass at roughly 13 GB transferred. No throttling appeared at either concurrency.

That makes the natural shape a two-stage crawl: abbreviated packuments for every package to identify the population, then full packuments for the small surviving fraction. On a deliberately heavy sample of 24 native-build packages the full form averaged 541 KB gzip against 452 KB for the abbreviated form, so stage two is dominated by how many packages survive stage one rather than by any per-request premium.

## Prior art in this repository

Two pieces of machinery in the vendored engine look relevant, and only one is.

**The OSV bloom filter is not reusable for this.** [`vendor/aube/crates/aube-registry/src/osv_bloom_client.rs`](../../vendor/aube/crates/aube-registry/src/osv_bloom_client.rs) fetches a roughly 380 KB `filter.bin` that `endevco/osv-bloom` regenerates every ten minutes and publishes to GitHub Pages, then probes `(name, semver-major-bucket)` pairs against it and escalates hits to the live OSV API. It encodes malicious-advisory membership and nothing else — no version existence, no release dates, no script presence — and a bloom filter is membership-only and lossy by construction, so it cannot enumerate and admits false positives by design. The name invites the assumption that it carries release data; the code does not.

**The primer pipeline is directly reusable.** Two scripts already solve most of the corpus problem:

- [`vendor/aube/scripts/generate-primer.mjs`](../../vendor/aube/scripts/generate-primer.mjs) fetches full packuments and already extracts `hasInstallScript` into its compact schema, along with the accept-header gotcha documented in a comment.
- [`vendor/aube/scripts/fetch-download-weights.mjs`](../../vendor/aube/scripts/fetch-download-weights.mjs) is the two-signal download harvester this survey converges on independently: bulk `downloads/range` for package-level ranking, sequential per-version `last-week` for version-level weight, with the 128-name cap and the scoped-name exclusion handled.

The name list those scripts consume comes from `jdx/aube-primer-packages`, whose GitHub Actions cron publishes a 100,000-name popularity ranking plus a transitive-dependency list on the first of each month — a maintained, ready-made ranking.

## Recommended pipeline

Ranked by popularity, two stages, incremental thereafter:

1. **Enumerate** — page `_all_docs?limit=10000` for the complete name list (425 requests), or start from a popularity ranking when the corpus is download-weighted.
2. **Detect** — fetch abbreviated packuments at concurrency 32–64 and keep the per-version `hasInstallScript` flags. Full registry in about five hours; a top-200,000 slice in about fifteen minutes.
3. **Detail** — fetch full packuments only for packages that survived step 2, for the `scripts` keys, bodies and publish times.
4. **Weight** — bulk `downloads/point` for package-level ranking (128 unscoped names per request, scoped names individually), then one `versions/<pkg>/last-week` request per surviving package for version-level weight.
5. **Refresh** — poll `_changes?since=<seq>` and re-run steps 2–4 for changed names only. At roughly 20,000 changed packages per day this is a few minutes of work.

Resist exhaustive coverage for its own sake: the deep tail is real, but its median member is installed fourteen times a week, so extending a download-ranked corpus downward buys package count rather than relevance.

## What none of this can give you

Four hard limits: no historical per-version downloads, no script bodies without the full packument, no way to tell what a script does, and nothing about private registries.

- **Historical per-version downloads.** The per-version endpoint serves `last-week` and nothing else. There is no archive, no bulk export, and no third-party mirror. Any longitudinal view has to be accumulated by snapshotting weekly from now on. The signal is partly self-correcting because it is time-integrated — a version pinned in millions of lockfiles keeps downloading for years — but a version's history before the first snapshot is unrecoverable.
- **Script bodies without the full packument.** The abbreviated form's boolean is the ceiling; the keys and their commands require the larger document.
- **What a script actually does.** Every source here is declarative metadata. A `postinstall` that shells out to a downloaded binary is indistinguishable from one that prints a message, and no registry field will ever close that gap.
- **Anything about unpublished or private registries.** All of the above is `registry.npmjs.org` only.

## Changelog

Every revision to this document, with the date and what changed.

- 2026-08-01 — Initial write-up.
