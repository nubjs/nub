# Generating the build-jail catalog — how do we learn what a package needs?

The build jail grants each package a measured set of capabilities from a catalog. The sibling documents describe how that catalog is *enforced*. This one describes how it is *produced*, and it exists because the first answer was wrong in a way that took a long time to see.

## The verdict

**The measurement harness and the shipped jail are different systems with opposite constraints, and designing them as one is what made the first harness expensive and blind.**

| | generation harness | shipped build jail |
| --- | --- | --- |
| runs on | our machines and our CI | a stranger's laptop |
| privilege | root and full god-mode observability are expected | unprivileged, always, no exception |
| scripts assumed | well-behaved and non-malicious | hostile |
| job | *interrogate* what a script needs | *enforce* the narrowest grant that works |
| may use | ptrace, eBPF, ETW, dtrace, network namespaces, elevated APIs | only what an unprivileged process can do |
| output | exact paths, hosts and syscalls | a pass or a failure to launch |

Elevated observation produces ground truth; ground truth becomes catalog grants; the catalog feeds the unprivileged jail. Elevation lives entirely on the left of that arrow and never crosses it. Nothing an operator can set — no environment variable, no flag — may influence a grant at enforcement time, which is why the catalog is compiled in and why the override that makes measurement possible is a compile-time feature rather than a runtime one.

## What the first harness was, and why it was shaped that way

The original harness could observe exactly one thing: whether a jailed install passed or failed. From that single bit it had to recover a minimum capability set, so it searched — 55 states ordered by ascending cost, first pass wins, minimum by construction. The design is sound given the constraint, and it produced the corpus we have.

The constraint was self-imposed. Three consequences followed from it, and all three dissolve once the harness may watch:

- **Cost.** A search over 55 states runs the same install dozens of times: 13 minutes for one package, hours for a slice. One traced run answers the same question.
- **Silence about mechanism.** A failing rung says a grant was insufficient. It never says what was missing. Every "why does this package need the whole disk" question had to be re-investigated by hand, one package at a time.
- **A whole class of ambiguity.** A jailed script that exits 0 having written nothing is indistinguishable from success to a blind observer. To a trace it is a refusal with a path attached.

⛔ The corpus record fields do not close this gap and must not be read as though they do. `pathsBlockedWithoutGrant` and `pathsBlockedByPrefix` are both the delta against the **zero-grant** cell, not against the rung that mattered — they describe a *successful* install. That family was misread three separate times in one working session, once nearly landing a fix built on it.

## The v2 pipeline

**Observe → synthesize → verify → fall back.**

1. **Observe.** Run the package unjailed, under OS-level tracing, as an ordinary user. Read off the paths, hosts and syscalls it actually used.
2. **Synthesize.** Map the observed set to the narrowest grant covering it.
3. **Verify.** Run that grant in the real, unprivileged jail. This arm is the check, not the discovery.
4. **Fall back.** If the synthesized grant does not verify, walk the ladder *upward from it* — a handful of states rather than 55.

The ladder is retained deliberately. It is the test suite for the mapping: when the synthesized grant verifies, the mapping was right; when the ladder has to go wider, the observer missed something and the delta names it; when the ladder finds something narrower, the observer over-attributed. **Under-prediction and over-prediction rates are first-class metrics of the harness, not footnotes.**

The standing method that follows: to learn what a package needs, trace it — never infer the need from which jailed rung happens to pass.

## Observation is per-OS, and that is fine

Three adapters, one event contract. `strace` on Linux, ETW on Windows, `fs_usage` on macOS. They share nothing but their output shape:

```jsonc
{ "op": "read" | "write" | "connect" | "exec", "path": "/abs/path",
  "host": "1.2.3.4", "port": 443, "result": "ok" | "denied", "pid": 1234, "ppid": 1200 }
```

Everything downstream — normalization, scope classification, grant synthesis — is shared and identical. Three parsers is the right number; what has to be uniform is the mapping they feed, and it is a pure function tested against golden fixtures. Determinism rests on five rules, of which one is easy to violate and expensive: **a path that maps to no declared scope is an error, not a whole-disk grant.** Rounding an unclassifiable path up to `"disk"` is how a catalog silently inflates.

An OS-level boundary is also language-agnostic by construction, which is the property that settled a long detour. Two in-process seams were evaluated as network detectors and each is blind to real cases — an HTTP proxy misses a Node client that passes an explicit `agent`, and a `net.Socket` connect-hook misses every non-Node child, including the PowerShell and `curl` downloaders that are common in this corpus. The full comparison, which stands as a measurement even though its recommendation is superseded, is in [`../research/network-detection-proxy-vs-block.md`](../research/network-detection-proxy-vs-block.md).

### macOS cannot observe a lifecycle script's working directory, and the guard is therefore permanent

npm launches a lifecycle script with its working directory set to the package directory. On Linux that arrives as a `chdir` syscall and the decoder sees it. On macOS it does not: libuv's `posix_spawn` path calls `posix_spawn_file_actions_addchdir_np` (`deps/uv/src/unix/process.c:565`), which changes directory **inside the kernel as part of the spawn**, so no syscall is emitted. The fork path that would emit one (`:382`) is not taken.

A decoder that resolves relative paths against an inherited working directory therefore invents paths that no process touched — and, worse, can write the invented base back as though it had been *observed*, so every later relative path in that process compounds off it. Measured on one real trace: 126 of 128 working-directory changes were relative.

Three repairs were considered and two are dead ends, both eliminated by measurement rather than by argument:

- **Catch the `chdir`** — there is no `chdir` to catch.
- **Read DTrace's `cwd` builtin** — it is `v_name`, a single path COMPONENT, not a path. Apple's own comment in the shipped `/usr/lib/dtrace/darwin.d` names `vn_getpath()` as what they want and says it cannot be called because it takes `namecache_rw_lock`.
- **An `fbt` probe on the kernel routine that performs the change** — SIP is disabled on hosted macOS runners and 93,577 `fbt` probes are visible there, so the obvious blocker does not apply. But measured on a real runner: `fbt::chdir*:entry` **matches no probes at all**. The probe wildcarded rather than guessing a symbol name, which is what makes that zero readable instead of indistinguishable from a wrong guess.

⇒ **The guard is the mechanism, not a placeholder.** When a relative path is resolved against a working directory that was never observed, the record is flagged and the write is billed to the widest scope. An absolute target establishes trust; a relative one preserves whatever trust already existed. ⛔ The root must be seeded as trusted from the fact the driver created it — without that, the flag fires on every record and a reader learns to ignore it.

## What this bought, concretely

`dotnet-2.0.0@1.4.4` sat at `write:"disk"` on Linux. Tracing resolved it to one syscall — a bundled yarn, three processes down, doing `openat("/proc/self/stat")`, being refused, and exiting 1. It is `process.memoryUsage()` reaching libuv's `uv_resident_set_memory`; the package writes nothing unusual and the whole-disk grant had nothing to do with writing. `write:"disk"` "repaired" it only because that rung flips the filesystem default to Allow and incidentally exposes `/proc`.

A positive control confirmed causation rather than correlation: adding `/proc` to the read floor with the grant otherwise unchanged made the install succeed and produced 151 MB of installed artifact. No amount of ladder-walking produces that sentence. One trace did.

## Where the pipeline stops — the generator's output reaches nothing

**The pipeline is observe → records → collate → gate → discard.** `harness/collate.mjs --runs records --out <f>` turns the record corpus into a v2 catalog and runs in CI on every queue slice — but purely as a VALIDITY gate: it writes a temp file, checks nub accepts it (`NUB_BUILD_JAIL_CATALOG=… nub --version`), and throws it away. Nothing promotes the result.

What ships instead is `crates/nub-sandbox/data/build-jail-catalog.json`, baked into the crate by `build.rs` as `&'static` Rust at compile time — deliberately, so a malformed catalog fails `cargo build` rather than reaching a user. **It parses the v1 shape only, and there is no compiled-in v2 table at all**: `catalog_v2` reaches nub solely through the dev-only override. The two are ALTERNATIVES rather than layers — an active override REPLACES the curated table wholesale.

⛔ **The gap is one field, and it is not the field the shape difference suggests.** The obvious reading — that the hand-curated v1 entries are precise and a generated v2 catalog would be coarse — is **wrong**, and checking it is what produced this section. v2 carries per-OS `writePaths` and matches v1 on narrowness:

| package | v1 | v2 |
| --- | --- | --- |
| `cypress` | `homePaths: [CYPRESS_CACHE_FOLDER]` | `writePaths: [".cache/Cypress", "Library/Caches/Cypress"]` |
| `puppeteer` | `homePaths: [PUPPETEER_CACHE_DIR]` | `writePaths: [".cache/puppeteer"]` |

**What v2 lacks is the per-package ENV REDIRECT, and that is an active mechanism rather than documentation.** `compiler/curated.rs` builds an env pair out of a `home_paths` entry's `env`, so a v1 entry both GRANTS the path and SETS the variable pointing the package at it. v2's `env` is explicitly "one environment variable set for EVERY jailed script" — a global baseline, today just `PYTHONDONTWRITEBYTECODE=1`.

⇒ **What would be lost is robustness against ambient environment, not precision.** Under v1 a user carrying `CYPRESS_CACHE_FOLDER=/opt/shared` has it overridden into granted space and the install works. Under `writePaths` alone the grant covers `.cache/Cypress`, the package writes to `/opt/shared`, and the install fails — the direction this project treats as unacceptable.

**Observation cannot close this gap on its own.** A trace records where a script DID write; a redirect decides where it SHOULD BE POINTED. That is a designed fix, not an observable fact — which is why the curated entries are not simply a small catalog awaiting replacement by a larger generated one, and why the cheapest route to one model is to give v2 a per-package env field rather than to layer two.

## ⛔ The measuring environment silently picks the STORE LAYOUT, and the three lanes do not agree

`aube_util::env::is_ci()` is, verbatim and in full:

```rust
pub fn is_ci() -> bool {
    std::env::var_os("CI").is_some()
}
```

No value check, no provider list — **bare presence of `CI`**. And nub uses it to choose the dependency layout: with `CI` set the global virtual store is auto-disabled (`install_report.rs`, `Source::Ci => "global virtual store auto-disabled in CI"`) and dependencies land in a **project-local `node_modules/.store`**; without it they land in the **machine-global store** under `${XDG_CACHE_HOME:-$HOME/.cache}/nub/pm/store`. An explicit `enableGlobalVirtualStore` resolves *ahead* of the CI branch, which is the supported way to force either.

⇒ **So which layout a measurement sees depends on whether `CI` reaches the nub process — an accident of each driver's process plumbing rather than a decision anyone made.** Measured, per lane:

| lane | how it launches nub | `CI` visible? | layout measured |
| --- | --- | --- | --- |
| **linux** | `"$NUB" install` directly, ambient env | **yes** | project-local — **not what a user gets** |
| **macos** | `sudo -u <user> -H env "PATH=$PATH"`, whose reset **drops `CI`** | **no** | machine-global — **what a user gets** |
| **windows** | spawns nub with the CI environment intact | **yes** | project-local — **not what a user gets** |

**Evidence.** On a `macos-15` runner the global store **did not exist before the first arm and held 50 entries after it**, while a plain `nub install` in the same job's shell — with `CI` set — left it at 50 and resolved through a project-local `node_modules/.store`. Corroborating from the other side: across 21 landed Linux `driver.out` files, **16 carry a `CLOSURE` line and ZERO carry an `EVICT` line** — the per-arm store eviction never fired, because on that lane there is no machine-global store to evict from.

**Two consequences, and the second is the sharper one.**

1. A lane whose `CI` leaks through measures a layout **no non-CI user ever receives**. Whether a grant transfers between layouts is a real question, because the layout decides *where a dependency physically lives* and a grant is a claim about *which scope a write falls in*: under the global store a sibling-dependency write lands outside the project, under the project-local `.store` the same write lands inside it.
2. **The lanes are not measuring the same thing as each other**, so any cross-platform divergence in the corpus is confounded by store layout unless something rules it out.

### ✅ ANSWERED on Linux: the grants TRANSFER, and the enforcement is symmetric BY CONSTRUCTION

A two-arm differential settles it. Four packages chosen to discriminate, one variable (the layout), same runner image: **every synthesized grant, every verified grant, and every per-capability descent verdict is byte-identical across the two layouts.**

The control is what makes the null mean anything, and it is asserted per-arm out of the log that decided each verdict rather than from a pre-flight — `linker isolated (global virtual store auto-disabled in CI)` on all nine arms of one side against `linker global-virtual-store (npm_config_enable_global_virtual_store)` on all nine of the other. A second, independent instrument agrees: the per-arm store eviction removed **611 entries** on the global-store side and could remove **none** on the CI side, because there was no global store to evict from. Eviction can only delete what exists.

**And the null is EXPECTED, not lucky** — the enforcement handles both roots symmetrically, in code. `resolve_declared_dep` accepts a resolved dependency under the machine-global store **or** under the project-local virtual store, and `store_entry_write_root` recognises the same pair for the package's own entry. So `write.deps` compiles to real rules under either layout.

⛔ **That symmetry is a FIX for exactly this bug, not a happy accident** — which is the strongest evidence in the account, because it means the divergence class was once real and was *observed*. Before it landed, the resolver clamped every resolution to inside the project, so under the global store `deps` compiled to nothing and **`wordpos`'s measured minimum sat at `write.userHome` (cost 7) instead of `write.deps` (cost 3)**. The fix is an ancestor of both the probe binary and the branch tip.

✅ **AND THE LADDER-FALLBACK PATH IS COVERED — the one place a layout could plausibly have changed which rung passes first.** The four packages above all verified at their FIRST synthesized grant, so none of them exercised the fallback walk. A fifth run on `playwright-chromium@0.17.0` — whose record is `verifiedBy: ladder` — drives it deliberately, and the two layouts agree at every step:

| | ci-default (isolated) | global-store |
| --- | --- | --- |
| synthesized grant | `{"network":true}` → `rc=1`, insufficient | **identical** |
| fallback rung reached | `fb2126003217` → `rc=0` | **identical rung id** |
| `=> MINIMUM` | `{"write":{"deps":true,"project":true,"userHome":true},"network":true}` | **identical** |
| artifact / tree counts | `artifacts=6/6 missing=0`, tree `1583`→`1585` / `521` | **identical** |

The agreement extends past the verdict into the intermediate counts, which is the stronger form: the walk did not merely arrive at the same answer, it traversed the same path to get there — same rung, same failure at the same rung below it.

⇒ **The confound named in consequence 2 is measured-absent on Linux, on BOTH the direct and the fallback path.** Two residuals remain, stated rather than papered over: **Windows is untested** and needs its own arm; and the scope **lattice** does genuinely differ even though no recorded answer does — under the global store a sibling-dep write also falls inside `userHome`, under the CI layout inside `project` — so a future rule reasoning about scope *containment* would be layout-dependent even though today's minimum is not.

**⇒ The rule this yields, and it survives the null: a measurement harness must PIN the environment axes that change what is being measured, not inherit them.** `CI` is invisible, is set by every runner, and silently reroutes the filesystem the whole measurement is about — the same shape as the elevation trap below, one layer further out. Here the axis turned out not to move the answer; that was established by experiment, and could not have been assumed.

## Repeating the observation catches variance, and our failure mode is bias — so OBSERVE stays at one run

A natural hardening is to observe each package twice and take the union, on the theory that a flaky first run would otherwise produce a grant that is too narrow. It was built and measured, and the answer is that it does not buy what it appears to buy. The capability is kept as an on-demand instrument rather than turned on for the corpus.

The decisive case is `playwright-chromium@0.17.0`, whose record shows OBSERVE under-predicting: it synthesized `{"network":true}`, the jailed arm failed, and the bounded ladder repaired it. Two repeat observations of that package reproduced the same insufficient grant **byte for byte, with zero path-level difference between runs**. The observation was not noisy, it was consistently wrong — and a repeat run answers consistency, never correctness. What catches that class is a known-answer fixture, which is why each adapter carries one.

The path-level churn that repeats *do* surface turned out to be entirely outside the part of the observation that becomes a grant. Across four packages, up to half of the attributed write set differed between runs, and **all of it** was in scopes no grant is derived from: compiler `mkstemp` scratch names, and per-run random download directories. Grant-level disagreement was 0 of 4.

Two findings from the experiment are worth keeping even though the feature is not:

- **The per-run store eviction is load-bearing, and there is now a positive control proving it.** Without it, run 2 of a native build finds the previous build's artifacts and headers already in place, the lifecycle script barely executes, and the run synthesizes a *narrower* grant than run 1 — 9,773 trace lines against 223,405, one whole-tree write against 7,857. A degraded observation that fails in the under-granting direction is the worst available outcome, so the harness now asserts eviction inline rather than assuming it.
- **Two runs are the honest oracle for whether a path family can be enumerated at all.** A grant that names specific directories is only sound if those directories have the same names next time. One downloader writes into a freshly-generated 32-hex directory on every single run; a second observation settles that mechanically, where a single run leaves it to a human's judgement about whether a path *looks* stable.

Measured on linux/arm64 under Docker at n=4, which bounds what it can say: it distinguishes "essentially never" from "routinely", and cannot distinguish a true rate of 0% from one around 25%.

## A measurement is worthless until the instrument has been seen to FAIL

The harness accumulated dozens of checks — an artifact gate, a replay detector, an exit-code check, a permissions assertion, a marker parser — and every one of them was a check that PASSES. None had ever been shown capable of going red for the right reason on a real package. That is not a gap in coverage; it is a gap in the meaning of every green result the harness has produced.

The repair is a **falsification control**: take a package whose true minimum is independently verified, feed the jail a grant strictly NARROWER than that minimum, and require the harness to report failure. Two cases on independent capability axes, each run cold AND warm so a cached side effect cannot mask the denial:

| package | grant fed | detector that fired | evidence in the arm log |
| --- | --- | --- | --- |
| `@apollo/rover@0.4.8` | `{"network":true}` — `write.deps` removed | install exit code ALONE | `EACCES … mkdir '<store>/binary-install@0.1.1-…/bin'` |
| `hugo-extended@0.141.0` | `{}` — `network` removed | exit code AND artifact gate | `EAI_AGAIN`/`ENETUNREACH` on the release host |

⭐ **The two detectors are not interchangeable, which is why one case would not have sufficed.** rover's artifact lands in a SIBLING package that the gate deliberately does not walk, so the gate passes it 6/6 and only `rc` catches it. A single-case validation would have "proved" whichever mechanism happened to fire.

The control is itself falsified: a one-line-sabotaged driver (`local rc=$?; rc=0` — the real `|| (exit 0)` shape seen in the wild) makes both mechanisms go red. And it runs as a **precondition**, not a report: the batch runner executes it before the first package and refuses to start on FAIL *or* INCONCLUSIVE, at a cost of ~55s against a ~13-minute per-package budget.

### Three ways an arm passes while proving nothing

Each was found on a real package, and none is detectable from the verdict alone:

1. **Warm state from a previous arm.** A package whose install downloads a prebuilt binary into the real `~/.npm` will find it there on every later arm, so a `no-network` arm passes and the record claims network is unnecessary. Fixed by a private per-arm `HOME`; the general rule is that eviction must reach everything the package can cache, not just the package manager's own store.
2. **The package ships its own build output.** `ttf2woff2@1.2.3` publishes a working `build/Release/addon.node` and a 43-entry `build/` tree, so the artifact gate's "artifacts produced" set is just the tarball contents. No script needs to run for the gate to pass. Mechanically detectable: compare the published tarball's file list against the gate's OBSERVE manifest, and if the manifest is a subset, the gate cannot distinguish a working arm from a broken one.
3. **The script swallows its own exit code.** A `scripts.install` ending `|| (exit 0)`, `|| true`, or `; exit 0` makes `rc` uninformative. Also mechanically detectable from the script text — but the pattern must be written carefully: `|| (exit 0)` parenthesises the whole construct, and a first attempt that only allowed a paren after `exit 0` missed the very package it was written for.

⛔ The correct response to all three is to **FLAG, not fail**. These packages remain measurable; what they lose is the evidence value of a green arm. A record that says "MINIMAL, and here is why that is weak" is worth more than a refused record, and far more than a confident one.

## Measure as a real user, or measure the fallback path

The traced script must run with the permissions an ordinary developer has — not root, and not a reduced service account — and the harness must ASSERT it rather than assume it.

The failure this prevents is specific and invisible: a script that tries its primary path, is refused, and falls back gets measured on the FALLBACK. A real user with the permission takes the primary path and needs a capability that was never observed. That is an under-grant, and nothing in the record hints at it.

The converse error is safe — a more-privileged observer measures a path a real user cannot take, which over-grants — so the rule is "an ordinary user" with erring toward more privilege as the tolerable side.

⛔ Do not confuse this with the TRACER's privilege. The tracer may need root; it is measuring apparatus. The traced PROCESS is the environment under test. Two consequences that were each measured rather than reasoned:

- The assertion must test what it means. A `[ -w ]` check passes on a `rw,noexec` mount, where the script can write but cannot execute — so the real probe is an execution, not a write.
- The CI-detection environment must be SCRUBBED, never forced to a value. A sweep on a hosted runner measures every CI-branching package as needing less than a developer hits: `core-js@3.50.0` skips its `$TMPDIR` banner write entirely when `CI` is set. And there is no value that means "not CI" to everyone — `ci-info` reads `CI=0` as CI-ON while `core-js` reads it as CI-OFF. Only ABSENCE is unambiguous.

## A record must not depend on the machine that produced it

A grant measured on one box and a grant measured on another must be the same grant, or the corpus is a record of its own infrastructure. Three requirements follow, each of which failed in practice before it was stated:

- **Every root the classifier keys on is DECLARED in the capture header**, and the classifier reads roots only from there. A root it needs but the capture does not declare is a hard error, never a fallback — a fallback produces a plausible answer on the machine that happens to match and a wrong one everywhere else. Measured instance: a decoder that resolved a relative path against a fallback base wrote that fabricated base back as an OBSERVED working directory, and with 126 of 128 working-directory changes in a real trace being relative, one lost process edge fabricates a whole subtree.
- **Decoding is a property of the ARCHIVE, not of the decoding host.** A decoder that consulted the live filesystem to expand short path names produced a different view depending on where it ran, which silently breaks any comparison between two archives. The repair is to resolve once on the capture host and record the map.
- **Normalisation that is recorded is a covered axis; normalisation that is invisible is a silent bet.** Every environment variable the harness sets, unsets or redirects is named in the record.

⛔ And the constraint that governs all of it: **the harness may normalise its own apparatus, never the environment under test.** The jail runs in CI and on developer machines both, so an override that hides a CI-only behaviour produces a catalog that under-grants every CI user. Where an environment axis genuinely changes what a package needs, measure both states and take the UNION — over-granting is safe, and a capability that only appears under CI is not an edge case but one a real user hits on every push.

## Leave-one-out does not measure the joint drop

The descent drops each capability in turn and keeps the ones whose removal breaks the install. It is tempting to conclude that if dropping A passes and dropping B passes, dropping both passes. **That does not follow**, and a rule that narrows on it under-grants every record with two or more droppable capabilities.

So narrowing from a leave-one-out descent is sound only at N=1. For N≥2 the joint case needs its own arm, or the wide grant stands.

The companion rule is about evidence rather than logic: **the absence of a "this arm could have failed" flag means the check never ran, not that the arms were falsifiable.** Records predating the falsification work carry no flag, and treating them as falsifiable would retroactively narrow a corpus on a test never performed. The flag must be positively emitted, and it cannot be backfilled onto old logs — whether the arms COULD have failed is a property of the run, not of the trace, so no amount of re-parsing recovers it.

## Two traps that survive the redesign

- **Elevation can silently change the answer.** On Windows an elevated token carries `SeBackupPrivilege`, and libuv sets `FILE_FLAG_BACKUP_SEMANTICS` on every file open, so **every** Node open bypasses the DACL. Measured one-variable: a write into a directory with an explicit Deny ACE succeeded as launched and was refused after the privilege was dropped. Untreated, a package that probes a location, is refused, and falls back elsewhere is observed taking the probe and never the fallback — yielding a grant both wider than needed and missing the real need. Every adapter must run the *target* unprivileged even when the *tracer* is not.
- **A refusal is only a refusal when the kernel says so.** On Linux `grep EACCES` matches the `AT_EACCESS` flag name in every `faccessat2` line; only `= -1 EACCES` is a denial. Raw counts of 26/13/1 were really 11/0/0. On Windows the refusal predicate is exactly four NTSTATUS values, and every other failure is omitted rather than called a denial — one fixture trace held 379 failed-but-not-refused operations against a single real refusal.

## Where the code lives

The harness is not in this repository. It lives in the corpus repository alongside the records it produces, under `harness/v2/`: `README.md` and `MAPPING.md` carry the pipeline contract and the five determinism rules, `measure.sh` is the driver, and `adapters/` holds the three per-OS observers with their known-answer fixtures and validation. The compile-time seam it drives is `build-jail-catalog-override` in `nub-sandbox`.

## Changelog

- 2026-08-06 — Recorded why a lifecycle script's working directory is unobservable on macOS (libuv spawns with `addchdir_np`, so no `chdir` syscall fires) and that all three alternative instruments are measured dead ends, including an `fbt` probe on a runner with SIP disabled where `fbt::chdir*:entry` matches nothing. The trust-propagation guard is therefore the mechanism rather than a stopgap.
- 2026-08-06 — Recorded the falsification control and the three vacuity classes it exposed. The harness had dozens of checks and none had been shown able to fail; a deliberately-narrowed grant now demonstrates it detects an under-grant on two independent axes, runs as a batch precondition, and is itself falsified by a sabotaged driver. Also recorded: measure as a real user (with the `rw,noexec` and CI-scrub consequences), the venue-independence requirements, and why leave-one-out cannot narrow at N≥2.
- 2026-08-06 — Recorded the repeat-observation result: repeating OBSERVE catches variance, but the one real under-prediction in the sample was reproduced identically across runs, so the failure mode is deterministic bias and repeats do not address it. OBSERVE stays at one run; the capability is retained as an on-demand instrument for enumerability questions. Two findings kept: the per-run store eviction now has a positive control proving a non-evicted second run synthesizes a *narrower* grant, and two runs are the mechanical oracle for whether a path family is stable enough to enumerate.
- 2026-08-06 — Recorded where the pipeline stops: `collate.mjs` runs in CI on every queue slice but only as a validity gate, writing a temp file and discarding it, so the generator's output reaches nothing. `build.rs` bakes the v1 shape and there is no compiled-in v2 table at all. Also corrected the intuitive but wrong reading of the two models — v2 is NOT coarser, it carries per-OS `writePaths` and matches v1's narrowness package for package; the single real gap is the per-package ENV REDIRECT, which v1 uses to point a package's cache into granted space and v2 can only express globally. That distinction matters because the loss would be robustness against ambient environment rather than precision, and because a redirect is a designed fix that no amount of observation can discover.
- 2026-08-06 — Initial write-up, recording the generation/enforcement split and the v2 pipeline that follows from it.
