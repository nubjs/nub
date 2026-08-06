# Setting `CI` for lifecycle scripts

**Status:** v1, 2026-08-06. Two separate questions, measured against real published tarballs in a clean Linux container. The answers diverge: the runner should not set `CI`, and the measurement harness should not set it either — but for opposite reasons.

## TL;DR

- **Q1 — should Nub set `CI` when it runs a package's install scripts? No.** Setting it changes what dependency code does, which is a modification of existing semantics rather than an addition to them. MEASURED: with `CI` set, core-js skips a `$TMPDIR` write it otherwise performs, and husky 4 skips writing 19 git hooks entirely. Neither npm, pnpm, Yarn nor Bun sets `CI` for lifecycle scripts, so a project that installs identically everywhere else would install differently under Nub.
- **Q2 — should the corpus harness set `CI` when it observes a package? No, and it must actively scrub it.** The failure mode is real and confirmed: a package that skips work under `CI` is measured as needing a smaller capability grant than a developer on a laptop will hit. This is the one direction the project forbids. The harness already scrubs `CI` and 14 vendor variables; that decision is correct and is now backed by a reproduction.
- **The two answers agree in outcome and disagree in reasoning.** Q1 is a compatibility argument — do not perturb dependency behavior. Q2 is a measurement-safety argument — never observe the narrower code path. A change to one does not license a change to the other.
- **The canonical example in the folklore is stale.** Husky has not read `CI` since version 5 (November 2020). MEASURED: husky 4.3.8 skips under `CI`; versions 5.0.0, 5.2.0, 6.0.0, 7.0.4, 8.0.3 and 9.1.7 contain no `CI` reference at all, and 9.1.7 ships no lifecycle scripts whatsoever. The under-grant risk is real, but husky is no longer the package that demonstrates it — core-js is.
- **The native-build path is entirely `CI`-blind.** MEASURED by source inspection: `prebuild-install` 7.1.3, `node-gyp` 13.0.0, `node-gyp-build` and `node-pre-gyp` 2.0.4-pre.0 contain zero `CI` reads. The packages whose grants are hardest to measure — the compilers and prebuilt-binary downloaders — are the ones least affected either way.
- **Nub's own engine reads `CI`.** Setting it for lifecycle scripts would change the behavior of a nested `nub install`, and of nested `yarn` and `pnpm` invocations, which is the sharpest form of the compatibility objection.

## The two questions are not the same question

| | Q1 — the product | Q2 — the harness |
| --- | --- | --- |
| Who sets `CI` | Nub, for every user, on every install | The corpus probe, when observing a package |
| What goes wrong | Dependency code behaves differently under Nub than under npm | A grant is recorded from a code path that skipped work |
| Direction of the error | Behavior change, in either direction | Under-grant, always |
| Governing rule | Augmentation is additive; it never modifies existing semantics | Over-granting is safe, under-granting breaks installs |
| Answer | Do not set it | Do not set it, and scrub it |

The reasoning does not transfer. If the compatibility argument were somehow settled — if the ecosystem converged on package managers setting `CI` — Q2 would still be answered "scrub it", because the harness's job is to observe the widest code path a user can reach, not the most convenient one.

## What `CI` actually is

There is no specification. The convention is that a CI service sets `CI` in its own environment, and packages detect it. The de-facto detector is [`ci-info`](https://www.npmjs.com/package/ci-info), which at version 4.4.0 enumerates 53 vendors and treats the bare `CI` variable as one signal among many, with `CI=false` as an explicit global bypass.

Consumers disagree about what the *value* means. MEASURED against `ci-info` 4.4.0 and core-js 3.50.0's own truthiness helper on Node 22.23.2:

| Value | `ci-info` `isCI` | core-js `is()` | `@sentry/cli` check |
| --- | --- | --- | --- |
| unset | `false` | `false` | `false` |
| `CI=` (empty) | `false` | `false` | `false` |
| `CI=0` | **`true`** | **`false`** | `false` |
| `CI=false` | `false` | `false` | `false` |
| `CI=1` | `true` | `true` | `true` |
| `CI=true` | `true` | `true` | `true` |

The `CI=0` row is the interesting one: the two most widely used detectors disagree outright. A package manager choosing to set `CI` would also have to choose a value, and no value is correct for every consumer. That is a symptom of a convention rather than a contract.

Source: the bypass and the value handling are at [`ci-info` `index.js:18,37-48`](https://github.com/watson/ci-info/blob/master/index.js); core-js's helper is at `postinstall.js:33` in the published 3.50.0 tarball; the Sentry check is at `scripts/install.js:49`.

## No package manager sets `CI` for lifecycle scripts

MEASURED by reading each script runner's environment construction. None of the four injects `CI`:

| Package manager | Script runner | What it adds to the child env |
| --- | --- | --- |
| npm 12.0.0-pre.2 | `@npmcli/run-script` `lib/run-script-pkg.js:57-68`, `lib/package-envs.js` | `npm_package_*`, `npm_config_*`, `PATH` |
| pnpm 11 | `exec/lifecycle/src/runLifecycleHook.ts:119-127` | `INIT_CWD`, `PNPM_SCRIPT_SRC_DIR`, `npm_config_user_agent` |
| Yarn 4 (berry) | `yarnpkg-core/sources/scriptUtils.ts:108-196` | `BERRY_BIN_FOLDER`, `INIT_CWD`, `PROJECT_CWD`, `npm_*`, `PATH` |
| Bun 1.4.0 | `src/install/lifecycle_script_runner.rs` | no `CI` reference in the file |

All four do the opposite: they **read** `CI` to change their own behavior, and none propagates a synthesized value downward. That asymmetry looks deliberate — reading an ambient signal about the environment is different from manufacturing one.

I found no rejected RFC or reverted pull request proposing that a package manager set `CI`. INFERRED, from the absence rather than from a recorded decision: the idea does not appear to have been seriously pursued in any of the four. That is weaker evidence than a documented rejection would be, and it is the main gap in this survey.

## Which packages branch on `CI` in a lifecycle script

Method: 48 packages known to carry install scripts were downloaded as published tarballs and their lifecycle entry points traced transitively through local `require` edges, matching reads of `CI`, `CONTINUOUS_INTEGRATION`, `is-ci` and `ci-info`. Five hits, out of 34 packages that declare a `preinstall`/`install`/`postinstall` hook at all.

| Package | Hook | What `CI` changes | Source |
| --- | --- | --- | --- |
| core-js 3.50.0 | `postinstall` | Suppresses the funding banner **and the `$TMPDIR/core-js-banners` write** | `postinstall.js:17-23,37-54` |
| core-js-pure 3.50.0 | `postinstall` | Same, same file | `postinstall.js:17-23` |
| `@sentry/cli` 3.6.2 | `postinstall` | Suppresses the download progress bar only; the binary downloads either way | `scripts/install.js:45-52` |
| cypress 15.20.0 | `postinstall` | Forces color output on; also exposes an `isCi()` helper | `dist/xvfb-CzsAKhkL.js:338-348` |
| nx 23.1.1 | `postinstall` | Reachable `isCI()` helper; no observed effect on the install | `dist/src/utils/is-ci.js:4-8` |
| husky 4.3.8 | `install` | **Skips git-hook installation entirely** | `lib/installer/index.js:26-28`, default at `lib/getConf.js:8` |
| `opencollective-postinstall` | via husky 4's `postinstall` | Suppresses its funding banner | observed in the differential |

The `CI` reads above are MEASURED by source inspection of the published tarball. Whether each one changes the installed tree is a separate question, answered only for the four packages carried into the differential below — core-js-pure and cypress were traced but not run, so their rows are INFERRED as to observable effect.

Everything else in the corpus was clean, including the whole native-build family: `node-gyp` 13.0.0, `node-gyp-build`, `node-pre-gyp` 2.0.4-pre.0 and `prebuild-install` 7.1.3 contain no `CI` reads, so `bcrypt`, `canvas`, `sqlite3`, `keytar`, `argon2`, `re2`, `ffi-napi`, `zeromq`, `msgpackr-extract`, `node-hid` and `@parcel/watcher` are unaffected through their build helpers. Also clean: esbuild, puppeteer, playwright, electron, prisma, `@swc/core`, `@biomejs/biome`, chromedriver, geckodriver, phantomjs-prebuilt, es5-ext, unrs-resolver, deasync, dtrace-provider, and the imagemin binaries (gifsicle, mozjpeg, optipng-bin, pngquant-bin, jpegtran-bin, cwebp-bin).

### Husky is not the example any more

MEASURED against published tarballs:

| Version | Released | Lifecycle scripts | Reads `CI` |
| --- | --- | --- | --- |
| 4.3.8 | 2021-01-15 | `install`, `postinstall` | Yes — `isCI && conf.skipCI`, and `skipCI` defaults to true |
| 5.0.0 | 2020-11-16 | none published | No |
| 5.2.0 | 2021-03-21 | none published | No |
| 6.0.0 | 2021-03-29 | none | No |
| 7.0.4 | 2021-10-21 | none published | No |
| 8.0.3 | 2023-01-03 | none published | No |
| 9.1.7 | 2024-11-18 | none | No — the only env gate is `HUSKY=0` |

Husky 9.1.7's entire guard is `if (process.env.HUSKY === '0') return 'HUSKY=0 skip install'` at `index.js:9`. Version 4.3.8 is a v4 patch published two months *after* v5 shipped, which is part of why the folklore persists.

Two things follow. The widely repeated claim that husky skips in CI describes software superseded more than five years ago. And from husky 5 onward the hook installation moved to `prepare` — which runs for the root project rather than for a dependency, and is stripped from the published tarball besides — so a corpus harness observing husky as a dependency would never have run it regardless.

## The differential

Both arms ran in `node:22-slim` (Node v22.23.2, npm 10.9.8, linux/arm64) with git installed, one package per run, cold npm cache per arm, and an isolated `HOME`, `TMPDIR` and cache directory. Each arm was snapshotted as the sorted set of every file under the project, temp and home directories with size and content hash.

**Exactly one variable differs between arms: whether `CI=1` is exported.** An earlier version of this fixture also varied the working-directory path between arms, which produced a spurious nx difference — nx derives its native cache directory from `sha256(workspaceRoot + version + username)` at `dist/src/native/native-file-cache-location.js:14-21`, so a different path meant a different directory name. Both arms were re-run at identical absolute paths and the nx difference disappeared. The result below is from the corrected run.

| Package | Artifacts differ | What differed |
| --- | --- | --- |
| **husky 4.3.8** *(positive control)* | **Yes** | 19 git hook files written without `CI`, zero with it |
| **core-js 3.50.0** | **Yes** | `$TMPDIR/core-js-banners` (676 bytes) written without `CI`, absent with it |
| `@sentry/cli` 3.6.2 | No | Binary identical (20,987,864 bytes, same hash) both ways |
| es5-ext 0.10.64 *(control)* | No | — |
| esbuild 0.28.1 *(control)* | No | — |
| nx 23.1.1 | No | — |

Every arm, including both controls, also differed by one file: npm's own `_update-notifier-last-checked` stamp, which npm writes into its cache when `CI` is unset and skips when it is set (`lib/cli/update-notifier.js:109`). That is the package manager's behavior rather than the package's, and its appearance in all six pairs is what confirms it is a constant baseline rather than a signal.

With `--foreground-scripts`, the mechanism is visible directly:

```
husky@4.3.8 install  [CI unset]      husky@4.3.8 install  [CI=1]
> node husky install                 > node husky install
husky > Setting up git hooks         husky > Setting up git hooks
husky > Done                         CI detected, skipping Git hooks installation.
  → 19 git hooks written             husky > Done
                                       → 0 git hooks written

core-js@3.50.0 postinstall [unset]   core-js@3.50.0 postinstall [CI=1]
Thank you for using core-js …        (no output)
  → $TMPDIR/core-js-banners written    → $TMPDIR untouched
```

### Reproducing it

One arm, parameterised by package and by whether `CI` is exported. Run it twice per package inside `node:22-slim` with git installed, then diff the two snapshots.

```bash
# both arms MUST use these identical absolute paths — nx and others hash their
# cache directory from the workspace path, so a differing path is a second variable
rm -rf /run/proj /run/tmp /run/home /run/cache
mkdir -p /run/proj /run/tmp /run/home /run/cache
cd /run/proj && git init -q . && echo '{"name":"probe","private":true}' > package.json

export HOME=/run/home TMPDIR=/run/tmp npm_config_cache=/run/cache
unset CI CONTINUOUS_INTEGRATION GITHUB_ACTIONS BUILD_NUMBER RUN_ID DRONE
[ "$ARM" = ci ] && export CI=1          # the only variable

# npm's own stdout goes to a SEPARATE file — folding it into the snapshot makes the
# snapshot differ on "added 2 packages in 1s" vs "…in 2s", which is wall-clock noise
npm install "$PKG" --no-audit --no-fund --foreground-scripts > /run/install.log 2>&1

# snapshot: path + size + content hash for every file, minus the two known noise sources.
# git's own internals are excluded, but .git/hooks is snapshotted EXPLICITLY — that is where
# the husky positive control lands, and dropping the whole .git directory silently hides the
# one signal that proves the fixture can detect anything at all.
{ find /run/proj /run/tmp /run/home -type f | grep -vE '/\.git/|node-compile-cache/|/_logs/'
  find /run/proj/.git/hooks -type f 2>/dev/null | grep -v '\.sample$'
} | sort | xargs -I{} sh -c 'printf "%s %s %s\n" {} $(stat -c %s {}) $(sha256sum {} | cut -c1-16)'
```

Diff the two snapshots on the path column first. Content hashes are a useful second pass but carry embedded timestamps in some packages, so a hash-level difference is a lead rather than a result — confirm it with a same-arm run before believing it.

### The controls are what make this readable

The husky arm is a positive control: it proves the fixture can detect a skipped lifecycle script, so "no difference" elsewhere means the packages did not diverge rather than that the harness was not looking. The es5-ext and esbuild arms are negative controls: they produce byte-identical trees, so a difference elsewhere is attributable to `CI` and not to install nondeterminism.

Some noise had to be excluded, and each exclusion earned its place by appearing in **both** arms of **every** pair rather than by being inconvenient: Node's V8 compile cache under `$TMPDIR/node-compile-cache/`, whose blobs are not byte-reproducible; npm's `_logs/` timestamped debug logs; and npm's own install stdout, which reports elapsed seconds.

A **same-arm control** is what establishes that an exclusion is noise rather than signal. Running esbuild twice with `CI` unset in both arms reproduces a two-line difference identical to the one seen across arms — npm reporting `added 2 packages in 1s` against `…in 2s`. A difference that survives with the variable held constant is nondeterminism by definition. Any residual difference this document calls noise was confirmed that way, not assumed.

The same control turned up one more trap worth stating, because it would otherwise look like a finding. Husky 4.3.8 stamps a creation time into every hook it writes:

```
# Created by Husky v4.3.8 (https://github.com/typicode/husky#readme)
#   At: 8/6/2026, 10:56:11 PM
```

Two same-arm runs a second apart therefore produce 19 files at identical sizes with 19 different content hashes. Compare the **path set** first and treat content hashes as a second pass — the husky result reported above is presence against absence (19 files with `CI` unset, none with it set, and no file present in only the `CI` arm), which the timestamp cannot affect.

## What breaks, and what does not

**Breaks — a package skips work it would otherwise do:**

- **core-js and core-js-pure stop writing `$TMPDIR/core-js-banners`.** MEASURED. This is the live under-grant hazard for Q2: a grant measured under `CI` would not include the temp-directory write, and a developer without `CI` set would hit it. core-js is among the most-installed packages in the ecosystem, so this is not an exotic corner.
- **Husky 4.3.8 writes no git hooks.** MEASURED. Historical, since husky 4 is five years old, but it is the cleanest demonstration of the failure shape and the reason it is retained as the positive control.

**Does not break — behavior changes but the installed tree does not:**

- **Downloads still happen.** MEASURED for `@sentry/cli`: with `CI` set, only the progress bar disappears; the 20 MB binary is downloaded and lands byte-identical. No package in the corpus was observed to skip a download because of `CI`.
- **Native builds are unaffected.** The compile-and-fetch helpers read `CI` nowhere, so the capability-heavy packages behave identically either way.
- **Progress-bar suppression is mostly already in effect.** The `@sentry/cli` guard is `silentFlag || silentConfig || silentEnv || ciEnv || notTTY`, and a jailed or piped install is already not a TTY. INFERRED for the general case, MEASURED for `@sentry/cli`: the two arms produced identical output, so `CI` contributed nothing the missing TTY had not already contributed. Most of the "deterministic, no spinners" benefit that motivates Q1 is therefore already available without setting anything.

**The sharpest objection — nested package managers.** A lifecycle script that invokes another package manager inherits the environment, and all four change behavior on `CI`:

| Tool | Setting | Effect under `CI` | Source |
| --- | --- | --- | --- |
| Yarn 4 | `enableImmutableInstalls` | Defaults to **true** — a nested `yarn install` hard-fails if the lockfile would change | `plugin-essentials/sources/index.ts:153-156` |
| pnpm 11 | `enableGlobalVirtualStore` | Forced off when not explicitly set | `config/reader/src/index.ts:705-710` |
| pnpm 11 | `minimumReleaseAgeStrict` | Refuses to prompt; throws instead of asking | `installing/commands/src/policyHandlers.ts:166,187-190` |
| npm 12 | update notifier | Suppressed | `lib/cli/update-notifier.js:109` |
| Nub / aube | global virtual store | Disabled when `CI` is present | `vendor/aube/crates/aube/src/commands/install/gvs.rs:15,59` |

The Yarn row is the concrete break: a package whose install script runs a nested `yarn install` would fail under Nub and succeed under npm, on the same inputs. That is a behavior modification, not an addition, and it is caused entirely by Nub having manufactured a signal the user did not set. The last row is self-inflicted — Nub would be changing its own nested behavior.

## Recommendation for Q1: do not set `CI`

Nub should not set `CI` when running lifecycle scripts.

The governing rule is that augmentation is additive and never modifies existing Node or npm semantics. Setting `CI` fails that test directly: it is a signal the user did not set, it changes what dependency code does, and the change is observable in the installed tree. The nested-Yarn case turns it from a theoretical concern into a reproducible install failure. No other package manager does this, so a project would install differently under Nub than under the tool it was developed against — which is the specific outcome the compatibility position exists to prevent.

The benefit is also smaller than it first appears. The stated goal is deterministic, non-interactive behavior with no prompts, spinners or telemetry dialogs. MEASURED, most of that is already delivered by the absence of a TTY, which every jailed lifecycle script already lacks. What remains is the funding-banner family, and suppressing banners is not worth a semantics change.

**Strongest counter-argument.** Install scripts genuinely are a non-interactive, automated context — arguably more so than a CI runner, since they are confined in a build jail with no controlling terminal. A package that prompts or spins there is misbehaving, and `CI` is the one signal the ecosystem has for saying so. There is force in this: Nub *is* running these scripts in a machine context, and the honest description of that context is closer to "CI" than to "developer laptop". The reason it does not carry the day is that `CI` is overloaded. It does not mean "non-interactive" — it means "a continuous-integration service", and packages act on that broader meaning by skipping work, changing lockfile strictness, and altering what they write to disk. If Nub wants to say "non-interactive", the accurate mechanisms are the ones already in play: no TTY, and `npm_config_loglevel`, both of which several of these packages already honor. Manufacturing a claim about the *kind of machine* in order to get a *terminal-interactivity* effect is the wrong lever, and the Yarn immutable-install failure is what that mismatch costs.

## Recommendation for Q2: do not set `CI`, and scrub it

The corpus harness should continue to scrub `CI` along with the vendor-specific variables, rather than setting it or passing the ambient value through.

The under-grant failure mode is confirmed rather than hypothetical. MEASURED: with `CI` set, core-js does not write `$TMPDIR/core-js-banners`. A grant measured that way would omit a temp-directory write that an ordinary developer install performs, and would break for that developer. Over-granting is safe; this is the forbidden direction.

Scrubbing is also strictly better than forcing `CI=0` or `CI=false`. The value semantics are inconsistent — MEASURED, `ci-info` reads `CI=0` as CI-on while core-js reads it as CI-off — so any explicit value picks a side that some consumer disagrees with. An absent variable is the only state every detector in the survey agrees on, and it is the state that selects the code path that runs the most code.

The harness already does this, at `tests/build-jail-search/search.mjs:368-372`, scrubbing `CI` plus 14 vendor variables. That decision is correct and this document is the reproduction behind it. **One correction is warranted:** the comment above that scrub cites "husky, cypress, puppeteer, telemetry installers" as packages that skip work under `CI`. MEASURED, none of those three named packages does so today — husky has not since version 5, cypress only forces color on, and puppeteer never read `CI` at all. The scrub is right; its stated examples are stale and should name core-js, which is the package that actually demonstrates the hazard.

**Strongest counter-argument.** Scrubbing `CI` is itself a deviation from the developer environment when the sweep runs on a CI runner, and the catalog is regenerated on exactly such machines. A user who really does install on GitHub Actions has `CI` set, and the grant measured without it may be *wider* than that user needs. This is correct, and it is the right trade: a wider grant costs a user nothing but a slightly larger capability set, while a narrower one breaks their install. The asymmetry is the whole reason the rule is stated as "over-granting is safe". The residual risk is that scrubbing makes the grant wide enough to be uninformative — that has not been observed, and the measured delta is one temp-file write.

## Do the two questions have the same answer?

Yes in outcome, no in reasoning, and the distinction is load-bearing.

Q1 is decided by compatibility: Nub must not perturb what dependency code does, and setting `CI` demonstrably does. Q2 is decided by measurement safety: the harness must observe the widest reachable code path, and `CI` narrows it. If the compatibility argument were overturned — say the ecosystem converged on package managers setting `CI` — Q2 would not follow. The harness would still scrub, because it would still need to measure the grant that the *unset* path requires in order to stay on the safe side of the over-grant rule.

They also fail differently. A wrong answer on Q1 produces a visible, debuggable install failure, such as the nested Yarn immutable error. A wrong answer on Q2 produces a silent under-grant that ships in a catalog and breaks a user weeks later, with nothing in the record indicating why. The second is the more expensive mistake, which is why the harness scrubs rather than merely declining to set.

## What this does not settle

- **No documented rejection was found.** The claim that no package manager has seriously proposed setting `CI` rests on the absence of an RFC or reverted pull request in the four projects surveyed, not on a maintainer statement. A recorded rejection would be stronger evidence and may exist somewhere not reached here.
- **Corpus coverage is 48 packages, not the ecosystem.** The list was assembled from packages known to carry install scripts, which biases toward native builds and downloaders. A sweep over the full corpus would put a real bound on how many CI-branching packages exist; this one establishes that they exist and what they do, not how many there are.
- **The tracer follows local `require` edges only.** It does not follow into a dependency's `bin`, so a hook such as `install: prebuild-install` was resolved by reading that helper's source separately. A package whose install script shells out to something unexamined could branch on `CI` without being caught.
- **Linux and arm64 only.** The differential ran in one container on one architecture. Nothing observed looks platform-dependent, but Windows in particular was not tested.

## Changelog

- 2026-08-06 — Initial write-up. Answers both questions "do not set `CI`", with different reasoning: compatibility for the product, measurement safety for the harness. Differential fixture in `node:22-slim` (Node 22.23.2, npm 10.9.8) across six packages with one positive and two negative controls; husky 4.3.8 skips 19 git hooks and core-js 3.50.0 skips a `$TMPDIR` write under `CI`, while `@sentry/cli`, es5-ext, esbuild and nx are unaffected. Establishes that husky has not read `CI` since version 5 (verified across 5.0.0, 5.2.0, 6.0.0, 7.0.4, 8.0.3 and 9.1.7) and that the native-build helper family reads it nowhere; that none of npm, pnpm, Yarn or Bun sets `CI` for lifecycle scripts while all four read it; and that Yarn's `enableImmutableInstalls` defaulting to `isCI` makes a nested `yarn install` fail under a manufactured `CI`. Corrects the stale package names in the existing harness scrub comment at `tests/build-jail-search/search.mjs:353-358`. The reproduction snippet in this document was extracted and run verbatim to confirm it fires on husky (19 paths), fires on core-js (one path), and is silent on both negative controls; a same-arm control identified npm's elapsed-seconds stdout and husky's per-hook creation timestamp as nondeterminism rather than signal.
