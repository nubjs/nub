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

## Two traps that survive the redesign

- **Elevation can silently change the answer.** On Windows an elevated token carries `SeBackupPrivilege`, and libuv sets `FILE_FLAG_BACKUP_SEMANTICS` on every file open, so **every** Node open bypasses the DACL. Measured one-variable: a write into a directory with an explicit Deny ACE succeeded as launched and was refused after the privilege was dropped. Untreated, a package that probes a location, is refused, and falls back elsewhere is observed taking the probe and never the fallback — yielding a grant both wider than needed and missing the real need. Every adapter must run the *target* unprivileged even when the *tracer* is not.
- **A refusal is only a refusal when the kernel says so.** On Linux `grep EACCES` matches the `AT_EACCESS` flag name in every `faccessat2` line; only `= -1 EACCES` is a denial. Raw counts of 26/13/1 were really 11/0/0. On Windows the refusal predicate is exactly four NTSTATUS values, and every other failure is omitted rather than called a denial — one fixture trace held 379 failed-but-not-refused operations against a single real refusal.

## Where the code lives

The harness is not in this repository. It lives in the corpus repository alongside the records it produces, under `harness/v2/`: `README.md` and `MAPPING.md` carry the pipeline contract and the five determinism rules, `measure.sh` is the driver, and `adapters/` holds the three per-OS observers with their known-answer fixtures and validation. The compile-time seam it drives is `build-jail-catalog-override` in `nub-sandbox`.

## Changelog

- 2026-08-06 — Recorded the repeat-observation result: repeating OBSERVE catches variance, but the one real under-prediction in the sample was reproduced identically across runs, so the failure mode is deterministic bias and repeats do not address it. OBSERVE stays at one run; the capability is retained as an on-demand instrument for enumerability questions. Two findings kept: the per-run store eviction now has a positive control proving a non-evicted second run synthesizes a *narrower* grant, and two runs are the mechanical oracle for whether a path family is stable enough to enumerate.
- 2026-08-06 — Recorded where the pipeline stops: `collate.mjs` runs in CI on every queue slice but only as a validity gate, writing a temp file and discarding it, so the generator's output reaches nothing. `build.rs` bakes the v1 shape and there is no compiled-in v2 table at all. Also corrected the intuitive but wrong reading of the two models — v2 is NOT coarser, it carries per-OS `writePaths` and matches v1's narrowness package for package; the single real gap is the per-package ENV REDIRECT, which v1 uses to point a package's cache into granted space and v2 can only express globally. That distinction matters because the loss would be robustness against ambient environment rather than precision, and because a redirect is a designed fix that no amount of observation can discover.
- 2026-08-06 — Initial write-up, recording the generation/enforcement split and the v2 pipeline that follows from it.
