# Install script-failure readout

When a dependency's `preinstall` / `install` / `postinstall` fails during an install, the user needs three facts: which packages failed, why, and what to do next. Today Nub prints a single summary line, naming only the one failure it happens to observe first.

> **Status: proposed.** Every block marked *proposed* below is a mockup of output Nub does not produce yet. Blocks marked *captured* are verbatim runs against a fixture whose `postinstall` exits non-zero, with home-directory and fixture paths abbreviated to `~/…`.

## What Nub prints today

Captured — Nub 0.6.0, one dependency whose `postinstall` exits 3:

```
$ nub install
failing-a: build failed
  × lifecycle script postinstall failed for failing-a@1.0.0: script
  │ `postinstall` exited with code 3
```

Exit code 1. The first line is the script's own stderr, streamed unbuffered and unattributed; the package name appears only because this fixture prints it. Captured, same install with three failing dependencies:

```
$ nub install
failing-c: build failed
failing-a: build failed
  × lifecycle script postinstall failed for failing-c@1.0.0: script
  │ `postinstall` exited with code 7
```

Exit code 1. Three packages failed; one is named. The stray `failing-a: build failed` belongs to a package the summary never mentions, and `failing-b` was aborted mid-flight — the runner uses a `JoinSet`, which cancels every outstanding task when the first error propagates ([`lifecycle.rs`](../../vendor/aube/crates/aube/src/commands/install/lifecycle.rs)).

Three further facts, each measured:

- Linking happens before scripts, so a package whose build failed is still present in `node_modules`.
- A re-run retries the failed build rather than fast-pathing over it.
- The error carries no `ERR_*` code, so it misses `EXIT_TABLE` and falls through to the generic exit 1.

## Prior art

Reproduced against npm 11.17.0 and pnpm 10.15.1 on the same fixtures — packages whose `postinstall` exits 3, 4 and 7, plus one that prints 40 progress lines and a three-line error before exiting 3. Both tools skip dependency scripts by default, so npm needs `--dangerously-allow-all-scripts` and pnpm needs a `pnpm.onlyBuiltDependencies` entry for any of this to run at all.

### One required dependency fails

Captured, npm:

```
$ npm install --dangerously-allow-all-scripts
npm error code 3
npm error path ~/…/fixture/node_modules/failing-a
npm error command failed
npm error command sh -c node ./post.js
npm error failing-a: build failed
npm error A complete log of this run can be found in: ~/.npm/_logs/2026-08-02T03_42_05_777Z-debug-0.log
```

Captured, pnpm:

```
$ pnpm install
Packages: +1
+
Progress: resolved 1, reused 0, downloaded 1, added 1, done
.../node_modules/failing-a postinstall$ node ./post.js
.../node_modules/failing-a postinstall: failing-a: build failed
.../node_modules/failing-a postinstall: Failed
 ELIFECYCLE  Command failed with exit code 3.
```

Both exit 3 — the script's own code. The pnpm line prefix is the load-bearing detail: every line of script output carries the package it came from, which is what keeps concurrent builds legible.

### Several required dependencies fail

Only one is reported by npm, which never mentions the other two — both runs of the fixture named the same package — while its debug log records all three exit codes. Meanwhile pnpm runs all three to completion and reports each, but its summary line races with them. Captured, pnpm, one of three runs:

```
.../node_modules/failing-a postinstall: Failed
.../node_modules/failing-c postinstall: Failed
 ELIFECYCLE  Command failed with exit code 3.
.../node_modules/failing-b postinstall: Failed
```

With the three scripts exiting 3, 4 and 7, three consecutive pnpm runs exited 3, 3 and 7. The exit code under concurrent failures is whichever failure pnpm observes first, so it varies run to run.

### The failure is buried in build noise

Both tools reproduce the whole script stream: npm buffers it and replays all 43 lines prefixed `npm error`, pnpm streams all 43 prefixed with the package path. Neither extracts the cause, and neither elides the progress bars.

### An optional dependency fails

This is where the two diverge sharply, and both exit 0. Captured, npm — the entire output, 21 bytes:

```
$ npm install --dangerously-allow-all-scripts

up to date in 328ms
```

The package is gone from `node_modules`: npm rolls back a failed optional dependency and says nothing about it. Captured, pnpm:

```
.../node_modules/opt-fail postinstall: opt-fail: build failed
.../node_modules/opt-fail postinstall: Failed

optionalDependencies:
+ opt-fail 1.0.0

Done in 386ms using pnpm v10.15.1
```

The package stays in `node_modules`, half-built, and the summary lists it as installed.

### Where Nub is today

Nub makes no required/optional distinction: the same optional dependency fails the whole install with exit 1 — the one place Nub refuses an install both reference tools complete.

## The design

Three rules carry every case below.

1. **Every dependency-script line is prefixed with its package name**, padded to the width of the longest name in the build set. The build set is known before any script starts, so the column is stable for the whole install.
2. **Every script's combined output is also captured to a file** at `node_modules/.nub-logs/<name>@<version>.log`, whether it succeeds or fails.
3. **A failure summary is printed last**, after every other line including the output of builds that were still running. It lists every failed package, sorted by name, each with its hook, exit code, and the tail of its own output.

## Case 1 — one dependency fails in a 200-package install

Proposed:

```
nub 0.6.0
███████████████  200/200 pkgs
✓ resolved 200 · reused 198 · downloaded 2 (3.4 MB) in 2.5s
esbuild   │ node install.js
puppeteer │ Downloading chrome-headless-shell r146 [=========>] 98%
puppeteer │ Error: ERROR: Failed to set up chrome-headless-shell v146.0.7680.153! Set "PUPPETEER_SKIP_DOWNLOAD" env variable to skip download.
sharp     │ node install/check.js

  × 1 of 4 dependency builds failed

    puppeteer@24.40.0 · postinstall · exit 3
      Error: ERROR: Failed to set up chrome-headless-shell v146.0.7680.153! Set
      "PUPPETEER_SKIP_DOWNLOAD" env variable to skip download.
        [cause]: Error: ENOENT: no such file or directory, mkdir
        '~/.cache/puppeteer/chrome-headless-shell'
      + 38 earlier lines in node_modules/.nub-logs/puppeteer@24.40.0.log

  200 packages are linked. puppeteer@24.40.0 is present but unbuilt.
  help: to install without this build, set "allowBuilds": { "puppeteer": false }
```

Exit code 3.

Four of the 200 packages ran a build at all — dependency scripts are default-deny, so the header's denominator is the allowed build set, not the install, which tells a reader whether one build failed out of four or one out of forty.

The state line says the tree is complete and one package in it is unbuilt: the difference between "rerun the install" and "your `node_modules` is half-written". The help line names the neutral `allowBuilds` denial rather than `--ignore-scripts`, because skipping every script to get past one package is the wrong remedy.

## Case 2 — three dependencies fail in one install

Proposed:

```
nub 0.6.0
███████████████  212/212 pkgs
✓ resolved 212 · reused 210 · downloaded 2 (4.1 MB) in 3.0s
better-sqlite3 │ prebuild-install warn install No prebuilt binaries found
node-sass      │ Building: /usr/local/bin/node node-gyp.js rebuild
puppeteer      │ Downloading chrome-headless-shell r146 [====>] 41%
node-sass      │ gyp ERR! stack Error: `make` failed with exit code: 2
better-sqlite3 │ gyp ERR! find Python Python is not set from command line
puppeteer      │ Error: ERROR: Failed to set up chrome-headless-shell v146.0.7680.153! Set "PUPPETEER_SKIP_DOWNLOAD" env variable to skip download.

  × 3 of 5 dependency builds failed

    better-sqlite3@11.5.0 · install · exit 1
      gyp ERR! find Python You need to install the latest version of Python
      gyp ERR! find Python Node-gyp should be able to find and use Python
      + 61 earlier lines in node_modules/.nub-logs/better-sqlite3@11.5.0.log

    node-sass@9.0.0 · postinstall · exit 1
      gyp ERR! stack Error: `make` failed with exit code: 2
      gyp ERR! System Darwin 25.5.0
      + 143 earlier lines in node_modules/.nub-logs/node-sass@9.0.0.log

    puppeteer@24.40.0 · postinstall · exit 3
      Error: ERROR: Failed to set up chrome-headless-shell v146.0.7680.153! Set
      "PUPPETEER_SKIP_DOWNLOAD" env variable to skip download.
      + 39 earlier lines in node_modules/.nub-logs/puppeteer@24.40.0.log
```

Exit code 1.

The streamed region is interleaved and stays interleaved: buffering it to impose an order would cost the live feedback that makes a slow native build bearable. The name prefix is what makes it readable, and greppable after the fact.

The summary is where order is imposed: entries sorted by package name, so two runs of the same broken install produce byte-identical summaries and a CI diff between them means something. Sorting by failure time would be racy for the same reason pnpm's exit code is.

**All three failures appear because none is aborted.** Today the first failure cancels its siblings, so a user fixes one build, re-runs, and discovers the next — three times. The change: stop *starting* new builds after the first failure, and let in-flight ones drain. That preserves the invariant the `JoinSet` exists to protect — no script still executing when the install returns — and strengthens it, since a cancelled build leaves a half-written package directory the drain avoids.

## Case 3 — the cause is 40 lines above the end

The puppeteer shape: a build prints 40 progress lines, then a three-line error with a `[cause]` chain naming the exact directory. The cause is not at the end of the terminal, because other packages kept printing after puppeteer died.

Proposed, the tail of the terminal:

```
sharp     │ sharp: Detected globally-installed libvips v8.15.2
sharp     │ sharp: Building from source via node-gyp
sharp     │ Creating Release/obj.target/sharp-darwin-arm64.node
esbuild   │ node install.js completed

  × 1 of 4 dependency builds failed

    puppeteer@24.40.0 · postinstall · exit 3
      Error: ERROR: Failed to set up chrome-headless-shell v146.0.7680.153! Set
      "PUPPETEER_SKIP_DOWNLOAD" env variable to skip download.
          at /…/puppeteer/install.mjs:120:19
        [cause]: Error: ENOENT: no such file or directory, mkdir
        '~/.cache/puppeteer/chrome-headless-shell'
      + 38 earlier lines in node_modules/.nub-logs/puppeteer@24.40.0.log
```

What is surfaced is the last five lines of **puppeteer's own captured output**, not of the terminal — different streams, and only the first reliably contains the cause. The 38 progress lines above it are elided, counted rather than described so the number says whether anything interesting was dropped.

The full log is one `cat` away. Proposed:

```
$ cat node_modules/.nub-logs/puppeteer@24.40.0.log
$ postinstall: node install.mjs
Downloading chrome-headless-shell r146 [>] 0%
Downloading chrome-headless-shell r146 [=>] 3%
…
Error: ERROR: Failed to set up chrome-headless-shell v146.0.7680.153! Set "PUPPETEER_SKIP_DOWNLOAD" env variable to skip download.
    at /…/puppeteer/install.mjs:120:19
  [cause]: Error: ENOENT: no such file or directory, mkdir '~/.cache/puppeteer/chrome-headless-shell'
exit 3
```

The log's first line records the hook and the command, which the streamed form omits to keep the prefix column narrow. Carriage returns are normalized to newlines on the way in, so a tool that redraws a progress bar in place produces one line per redraw rather than one unreadable line — which is also why the elided count can be large for a build that looked quiet on screen.

A tail of five lines is a heuristic and will sometimes miss. It is chosen over the alternatives — the first stderr line, the last stderr block, a pattern match on `ERR!` — because those fail silently on tools that write everything to stdout. Five lines plus an exact count of what was dropped fails visibly instead.

## Case 4 — an optional dependency's build fails

Proposed:

```
nub 0.6.0
███████████████  201/201 pkgs
✓ resolved 201 · reused 201 · downloaded 0 in 0.9s
fsevents │ node-pre-gyp info install unpacking fse.node
fsevents │ node-pre-gyp ERR! install error

  ⚠ 1 optional dependency was skipped: its build failed

    fsevents@2.3.3 · install · exit 1
      node-pre-gyp ERR! stack Error: EACCES: permission denied, open
      'node_modules/fsevents/lib/binding/Release/node-v127-darwin-arm64/fse.node'
      + 12 earlier lines in node_modules/.nub-logs/fsevents@2.3.3.log

  fsevents@2.3.3 was unlinked from node_modules. 200 packages are linked.

nub 0.6.0 · ✓ installed 200 packages in 1.2s
```

Exit code 0.

**This should not be an error, and today it is.** Both reference tools exit 0 here; Nub exits 1 and fails the install. That is a compatibility defect, not a stricter-by-design choice, and it is the highest-value item in this document.

The recommendation follows npm on the substance and neither tool on the reporting:

| | npm | pnpm | Proposed |
| --- | --- | --- | --- |
| Exit code | 0 | 0 | 0 |
| Package left in `node_modules` | No | Yes | No |
| Named in the output | No | Only as installed | Yes, with the cause |

Removing the package is what makes optionality work, and leaving it linked converts a handled condition into an unhandled one. A consumer guards an optional dependency with a `try`/`catch` around the require, so an absent package degrades gracefully while a present-but-broken one throws from inside the module — past the guard, at some later point, with an error that names neither the install nor the failed build.

Removal here means unlinking the package from the resolution graph, not deleting its log. The virtual-store directory and `node_modules/.nub-logs/fsevents@2.3.3.log` survive, which is what keeps the path in the message valid.

Optionality is a property of the edge, not the package: a package reachable only through `optionalDependencies` or a failed `os`/`cpu` match is optional, and one also reachable through a required edge is required. A failure on the required path is a case 1 failure even when some other importer wanted it optionally.

## Case 5 — the same install without a TTY

Proposed, three failures under CI with no TTY and no color:

```
better-sqlite3 │ prebuild-install warn install No prebuilt binaries found
better-sqlite3 │ gyp ERR! find Python Python is not set from command line
node-sass      │ Building: /usr/local/bin/node node-gyp.js rebuild
node-sass      │ gyp ERR! stack Error: `make` failed with exit code: 2
puppeteer      │ Downloading chrome-headless-shell r146 [====>] 41%
puppeteer      │ Error: ERROR: Failed to set up chrome-headless-shell v146.0.7680.153! Set "PUPPETEER_SKIP_DOWNLOAD" env variable to skip download.

  × 3 of 5 dependency builds failed

    better-sqlite3@11.5.0 · install · exit 1
      gyp ERR! find Python You need to install the latest version of Python
      gyp ERR! find Python Node-gyp should be able to find and use Python
      + 61 earlier lines in node_modules/.nub-logs/better-sqlite3@11.5.0.log

    node-sass@9.0.0 · postinstall · exit 1
      gyp ERR! stack Error: `make` failed with exit code: 2
      gyp ERR! System Darwin 25.5.0
      + 143 earlier lines in node_modules/.nub-logs/node-sass@9.0.0.log

    puppeteer@24.40.0 · postinstall · exit 3
      Error: ERROR: Failed to set up chrome-headless-shell v146.0.7680.153! Set
      "PUPPETEER_SKIP_DOWNLOAD" env variable to skip download.
      + 39 earlier lines in node_modules/.nub-logs/puppeteer@24.40.0.log
```

Exit code 1.

Measured against a successful install under `CI=true --color=never`, three things drop without a TTY: the progress bars, the version header above them, and the `latest x.y.z` hint beside each direct dependency. The trailing summary survives with its check glyph — `nub 0.6.0 · ✓ installed 148 packages in 1.2s` — and on a failed install there is no trailing summary at all, since the failure block is last.

The box-drawing characters stay: Nub already emits `×` and `│` into a redirected stream, so matching that keeps one rendering to reason about instead of two. The prefix column does more work here than interactively — eliding in the summary is safe because the full stream is already in the job log, where `grep '^puppeteer │'` recovers one package's output. The log files are worth archiving as a build artifact too, since they hold the normalized, un-interleaved form.

## Decisions

### The exit code stays non-zero

For a required dependency, the exit code is the failing script's own code — matching npm and pnpm — and 1 when more than one script failed.

Exiting 0 and reporting the failure cleanly is the wrong trade: an install whose build failed produces a `node_modules` that does not work. A package missing its native addon or downloaded binary fails at require time, in a later step, with an error that points nowhere near the install — and CI reads the exit code and nothing else, so a zero exit turns a loud install failure into a quiet, misattributed runtime one.

Propagating the child's code has a trap in it. The engine already defines `ERR_AUBE_SCRIPT_NON_ZERO_EXIT` with a table-assigned exit of 50, and the obvious repair — attaching the missing code to the dependency-lifecycle error — would make Nub exit 50 where npm and pnpm exit 3. The failure carries the code for reporting and bypasses the table for the exit status.

Two edges: a script killed by a signal has no exit code and yields 1, and a script that somehow reports 0 while being treated as failed also yields 1. Multiple failures deliberately do not propagate whichever failure landed first, the way pnpm does, because that value varies between runs of the same install.

### The full log lives beside the tree it belongs to

Each script's combined output is written to `node_modules/.nub-logs/<name>@<version>.log`, with scoped names flattened the way the virtual store already flattens them (`@esbuild+darwin-arm64@0.21.5`). The summary references it by path.

The path is chosen over the alternatives on four properties:

- **Project-local**, so a CI job archives `node_modules/.nub-logs/` without hunting a timestamped file under `$HOME` the way npm's `~/.npm/_logs/<iso-timestamp>-debug-0.log` requires.
- **Deleted with `node_modules`**, so logs cannot accumulate and a stale log cannot be mistaken for a fresh one.
- **Survives the optional-dependency unlink**, so the path printed in case 4 is still valid.
- **Holds for both layouts.** A path inside the virtual store would need a second rule: under `--node-linker hoisted` the store directory is created but holds no per-package entries to write into.

There is no `nub logs <pkg>` command. The package-manager CLI mirrors pnpm's grammar and pnpm has no such verb, a new command would have to answer "from which install?" where a path does not, and a path is already the thing a user pastes into an editor or an issue.

Writing a log per script is not a behavior change for the streamed output — the stream stays live and unbuffered, and the file is a second sink, not a replacement. The cost is bounded by construction: dependency scripts are default-deny, so a large install writes a handful of files.

### Both a message at the failure and a summary at the end

The prefixed stream marks where in a long install the failure happened and keeps the live feedback; the summary is what the user reads, because a finished install leaves the terminal scrolled to the bottom.

The summary is strictly last, after the drained output of every build that was still running. That ordering is easy to lose under concurrency: the captured pnpm runs show its `ELIFECYCLE` line landing above a sibling's failure, which is invisible on a small install and actively misleading on a large one.

## Changelog

- 2026-08-01 — Initial write-up.
