# The build jail

Dependency lifecycle scripts — `preinstall`, `install`, `postinstall` — run arbitrary code from packages a project has never audited, with the full authority of the user who typed `install`. The build jail confines them using the operating system's own enforcement: Landlock plus seccomp on Linux, Seatbelt on macOS, an AppContainer on Windows.

This document is canonical for what the build jail is, what it grants, and how those grants are decided. The per-OS enforcement mechanics live in the platform ledgers; the evaluation of candidate architectures lives in [`build-jail-architecture.md`](build-jail-architecture.md).

> **Status: unshipped.** Everything here exists on a feature branch. No release contains it.

## What it is, and what it is not

The jail is **defense in depth against supply-chain attacks** — specifically against the mechanism that makes something like a self-propagating install-script worm viable. It raises the cost of a broad, opportunistic attack and contains the blast radius of one that lands.

It is **not a security boundary**, and describing it as one would be dishonest. A determined attacker targeting a specific user is out of scope. Residual capability is expected and accepted.

The one design rule that follows from this, and that governs every other decision:

> **The failure mode to avoid is packages breaking.** A residual capability is tolerable. A package that no longer installs is not.

When something breaks, the answer is to loosen the grant. Carve-outs for popular packages are correct rather than a compromise. A large allowlist that keeps the jail on beats a small one that gets it switched off.

### The jail is unprivileged, always

The jail requires **no elevation, no setup command, and no administrative step, on any platform**. That is a hard constraint, not a goal: a confinement users must install something to get is one most users will not have.

This is what separates it from a general-purpose sandbox, which may reasonably ask for one-time elevation and can therefore use mechanisms the jail cannot. The two are different products with different privilege budgets, and a mechanism that fits one may disqualify the other.

| | Build jail | General sandbox |
|---|---|---|
| Privilege | None, ever | May require one-time elevation |
| Linux | Landlock + seccomp | bubblewrap + user/network namespaces |
| macOS | Seatbelt | Seatbelt |
| Windows | AppContainer (LowBox) | Dedicated local account + WFP |
| Shape | Pure allowlist, no deny rules | Generous read, minus secrets |

### Package identity is the defense

The jail grants capability by **package identity**, not by path patterns or heuristics. No catalog entry means no capability beyond the base profile. This is what makes the model auditable: the answer to "why can this script reach the network?" is always a specific, reviewable catalog line.

## The base profile

Every jailed script gets the same starting envelope, without any catalog entry:

- Read and write within its **own package directory**
- Read access to its **declared dependencies**
- A **private, throwaway `$HOME`**
- A **private temporary directory**
- Write access to its **own store entry**
- The **project root node**, so `getcwd` resolves

The base profile is deliberately not read-only. Four of those six are writes, because a build that cannot write its own output is not a build.

## The grant vocabulary

A grant is a **capability over a scope**. It is never a set of paths, because the path space is open — a script may write files whose names it computes at runtime, and an enumeration of what it wrote last time is not a prediction of what it will write next time. Scopes survive varying names; path lists do not.

```ts
type Grant = {
  read?:  { project?: true; userHome?: true } | "disk";
  write?: { deps?: true; project?: true; userHome?: true } | "disk";
  network?: true;
  writePaths?: string[];   // $HOME-relative dirs moved back after the scripts finish
  notes?: string;
  macos?: Overlay; linux?: Overlay; win?: Overlay;
};

// A per-OS override. `null` says the field is not needed on this OS.
type Overlay = Partial<Record<keyof Omit<Grant, "macos" | "linux" | "win">, ... | null>>;

type Entry = { default: Grant; versions?: Record<`<${string}`, Grant> };
```

Four structural rules hold:

1. **Write implies read at the same scope.** A `read` naming a scope its `write` already covers is rejected rather than silently ignored.
2. **Narrow scopes compose, and none nests inside another.** The `deps` scope sits under `project` when a package is materialized into the project and under `userHome` when it is symlinked into a store, so neither contains it.
3. **Disk is the only dominance relation.** It is a separate arm of the type, which makes "disk plus something narrower" unrepresentable — there is nothing narrower left to add.
4. **Per-OS overrides merge onto their grant.** Version bands do not merge; see below.

### Per-OS overrides

A grant's capability fields are its answer on every operating system. A `macos`, `linux` or `win` block overrides that answer **field by field**: it replaces only the fields it names and leaves the rest of the grant standing.

```json
"some-native-addon": {
  "default": { "write": "disk", "network": true, "macos": { "write": { "project": true } } }
}
```

That grants `disk` on Linux and Windows, `project` on macOS, and egress everywhere.

Three rules govern the blocks:

- **`null` is how a block says "not needed here"**, and it is the only spelling. `{ "write": "disk", "macos": { "write": null } }` grants no write on macOS. Without it the narrowing direction is inexpressible whenever the outer grant is the union of what the operating systems need — the entry would have to be inverted so the outer grant is the intersection and every block widens, which is a different document for the same policy and one a generator gets wrong in the over-granting direction. `network` accordingly takes `true` or `null`; `false` is refused, so no answer has two spellings.
- **Nesting is exactly one level.** A block may not contain another block. There is no second operating system to refine, so the only thing a nested block could express is a contradiction, and it is a parse error rather than an ignored key.
- **Every rule above is checked on the effective grant, once per operating system** — not on the outer grant and the blocks separately. Write-implies-read is the case that forces it: `{ "write": "disk", "macos": { "read": { "project": true } } }` is redundant on macOS and on no other operating system, and neither half is redundant alone.

**Why the shape is an override and not a filter.** The retired `platforms` field was a filter: a grant either applied on an operating system or it did not, so a package whose needs merely *differ* by operating system had to be written as several mutually exclusive entries, or in practice as one grant carrying the widest answer everywhere. Of 581 package/versions measured on two or more operating systems, 44 (7.6%) diverge in the expensive direction — `write: "disk"` on one and something narrow on another — so the filter over-granted that share of the corpus by construction. Two measured cases: `@ffmpeg-installer/linux-x64@4.1.0` needs nothing on macOS and `disk` on Windows; `@opencode-ai/cli` reads `/proc/cpuinfo` on Linux and shells out to `sysctl` on macOS.

### Why `deps` is its own scope

The `deps` scope means **each declared dependency's own entry, reached by following the package's links** — never by joining a name onto a directory, and never the enclosing `node_modules`, which holds `.bin` plus every sibling's code. A native build legitimately writes into an addon dependency's directory during compilation, so the capability is real; granting the whole enclosing directory to get it would hand over far more.

### Artifacts that outlive a discarded `$HOME`

The jail points `$HOME` at a per-package throwaway directory. A package that caches under `~/.cache/<vendor>` therefore installs cleanly and then loses its cache, which is correct for isolation and wrong for the user.

A grant may declare **`writePaths`**: `$HOME`-relative directories that nub moves back into the real home after the scripts finish. The script never holds a live handle on the real `$HOME` — the move happens outside the jail, on the same device, as a rename.

The scale of the problem, measured: `puppeteer` wrote 359 paths, 355 of them under the throwaway home and none under the real `~/.cache/puppeteer`.

Deriving a `writePaths` entry needs a floor. Collapsing a written path to a fixed depth fails in the worst direction — `cypress` wrote 18,673 paths under `Library/Caches/Cypress/<version>/`, and a two-segment collapse yields `Library/Caches`, the cache root of every application on the machine. **The rule is the longest known shared root that prefixes the path, plus one segment**, so two vendors under one root produce two entries rather than their parent.

## How a grant is decided

Grants are **measured, not designed**. Nothing in the catalog is an argument about what a package ought to need.

### The search

The capability space is enumerated as 54 states and walked in **ascending cost order**. The first state that passes is the true minimum by construction, because every strict subset of it was already tested and failed.

Greedy descent — start wide, drop capabilities until something breaks — was rejected. It finds *a* minimal set, but which one depends on the order capabilities are dropped in, so the same package can yield different answers on different runs.

Cost is a **tiebreak that generates the walk order**, not a statement of preference. It only decides between states that are genuinely incomparable.

### The oracle judges the artifact

A cell passes only if it reproduces the control on **both** the exit code and the digest of the sorted list of paths written.

Exit code alone is blind. A hook installer that cannot see the project writes zero of its seventeen hooks and exits successfully, which is indistinguishable from success unless the artifact is checked.

### The control runs twice, combined by union

Two control runs, and a cell must reproduce every path that **either** produced.

Intersection is wrong and specifically dangerous: it compares against fewer paths, so a cell that failed to write an unstable path still passes, and the search records a minimum that is too narrow. That is precisely the failure the jail exists to avoid, arrived at by a measurement artifact.

Where two controls genuinely differ, the differing scopes are escalated into the grant rather than dropped.

### Every other package is held at full grant

During a search, only the package under test is confined; everything else runs fully granted. Without this, a missing artifact belonging to a *dependency* reads as a failure of the package under test — which made one package appear unfixable when the missing piece belonged to its engine package.

### When the control fails, the jail-off cell is asked FIRST — and it short-circuits

If the control fails even at the widest possible grant, the next question is not "what does npm do?" but **"does it still fail with confinement off?"** That cell is run before either reference package manager, and when it fails the verdict is settled without them:

- **Fails with the jail off too** → `BROKEN-WITHOUT-JAIL-TOO`. Confinement is not implicated. **The oracles are skipped**, because they compare nub against other package managers and so cannot separate "nub's package manager is wrong" from "nub's jail is wrong" — while this one cell can. Running them would cost two full installs to learn nothing that changes the answer.
- **Succeeds with the jail off** → the jail *is* implicated, and only then do the reference arms run, to rule out a package that no tool can install here.

⛔ **`BROKEN-WITHOUT-JAIL-TOO` IS NOT A NUB DEFECT COUNT.** Its own definition is "a nub PM/linker or packaging problem, **or a package that cannot run on this host at all**", and in practice the second dominates. Measured on a 100-package Linux slice: 19 landed in this bucket and at least 15 were environmental — old C++ against a modern V8, dead download CDNs, a Windows-only package on Linux, `primordials is not defined` on too-new Node, a package whose own `postinstall` invokes a binary it never depends on (npm exits 127 on that one too). Quoting the bucket size as a failure rate overstates it several-fold; triage requires reading the per-cell log per package.

This ordering is also the common path, not an edge case: 35 of 35 re-measured defect verdicts across Linux and macOS resolved here, which is why the short-circuit exists at all.

### The jail-off cell must PROVE it ran unjailed

The verdict above means "the jail is not implicated", and it is inferred from a cell *agreeing* with the control. An off-switch that silently stops working therefore produces no error — it produces unanimous agreement, which reads as a confident exoneration.

That is not hypothetical. The fixture set a per-package opt-out key for months after the code that read it was deleted, so every "jail off" cell ran jailed and every failing package was filed as not-the-jail. Re-measuring with a working switch flipped 2 of the first 5 to `BROKEN-EVEN-WITH-EVERYTHING` — real jail defects the broken control had buried.

So the cell now requires **positive evidence**: nub announces an unconfined dependency script, and without that announcement the record is `HARNESS-ERROR` — an instrument failure, never a verdict about the jail. A warning would not do; in a 2,250-package sweep it would scroll past, which is precisely how the original went unnoticed.

### npm and pnpm are the reference oracles, keyed on SUCCESS

When the oracles do run, the test is **whether npm succeeded**, not whether failure signatures match. Signature equality was tried and rejected: two tools failing for the same underlying reason routinely word it differently, and four packages were written off as environmental while their oracle had in fact run the script and succeeded.

- **npm also fails** → `BROKEN-IN-ENVIRONMENT`. The package cannot be installed here by anything. Grant nothing.
- **npm succeeds where nub fails** → `BROKEN-EVEN-WITH-EVERYTHING`, the most consequential thing this harness emits and never a grant gap.

## Version bands

A package's entry has a `default` and an optional map of version bands:

```json
"esbuild": {
  "default":  { "notes": "passes ungranted" },
  "versions": { "<0.28.1": { "network": true } }
}
```

Four rules make this cover the whole version space:

- **The default is generated from `latest`**, so it covers today's release and every future one.
- **Every band key is a `<` bound**, so bands reach downward without limit. A band written for one old version also covers every older version, including those too unpopular to ever probe.
- **Bands nest by construction, so the narrowest bound wins.** Resolution never depends on key order.
- **Nothing merges across versions.** A version resolves to exactly one grant, complete in itself. The default is not a base that bands extend.

That last rule is deliberately unlike the per-OS overrides, which do merge. A package is exactly one version, so bands are alternatives; an override refines a grant that still applies.

A band is written **only where an older version needs more than latest**. A version needing less gets no band at all and falls through to the default, harmlessly over-granted.

### The known gap

A band's grant is the union of the versions actually **measured** below its bound, so a version in an unmeasured gap that needs more is not covered.

Measured: `bcrypt@5.0.0` fails under a band derived from `5.1.1`, because it writes into a dependency's store entry that the band does not grant. The same install succeeds with the jail off.

Banding makes coverage total for versions that behave like the ones probed. It does not make coverage total for all versions.

## Placement — why a script must see the project

Under a global virtual store, a package is symlinked into the project from a shared location. A lifecycle script that walks up from its working directory therefore lands in the store and never finds the project root. Hook installers write nothing and exit successfully.

**No grant can fix this.** It is a layout problem wearing the costume of a permission problem, and it silently produced a wrong answer: of 59 measured records with an install script, 48 could not see the project axis at all.

Placement is now seeded from the manifest — a package that **declares** a lifecycle script is materialized into the project rather than symlinked. The signal is read from the package's own manifest at plan time, so it needs no list.

The prior art is unanimous, and it is why this is stated as settled rather than proposed. Bun ships this exact rule, with ineligibility propagating to dependents. Yarn's Plug'n'Play unplugs packages with build scripts. Pnpm's default layout places a real directory inside the project. Every package manager that put script-runners in a shared store walked it back, and none of them used a list of package names to do it.

## No package lists in source

**A list of package names governing build-jail behavior is never embedded in source code.** Not in Rust, not in the harness, not as a performance hint. Package-keyed data lives in the catalog or a catalog override; source code holds mechanism.

Most such lists should be **derived rather than relocated**. Whether a package performs a native build needs no list: a package whose install script invokes `node-gyp`, `prebuild-install`, or `node-gyp-build` is one, and that is readable from the manifest the generator already parses. A list is what remains when the rule has not been worked out.

This does not govern security policy whose value depends on *not* being overridable. A resolver trust-exemption list is correctly compiled in, because anything that can add a name to it bypasses the supply-chain check for that package. The test is whether dynamism makes the system more **honest** — a grant derived from measurement — or more **attackable** — an exemption an attacker can grant themselves.

## Changing the catalog shape — everything that must move together

The catalog shape is written in one place and read in four, and a change that misses one fails in a way that looks like something else. A shape change that reached the parser but not the harness produced a hundred-package sweep in which every package reported that the override had not engaged — which reads as a broken binary, not a shape mismatch.

| # | What | Where |
|---|---|---|
| 1 | The Rust types, parser, validation, and resolution | `crates/nub-sandbox/src/catalog_v2.rs` |
| 2 | The startup grant count and lookup used by the compiler | `crates/nub-sandbox/src/catalog_override.rs` |
| 3 | The collator, which **writes** the catalog | `tests/build-jail-search/collate.mjs` |
| 4 | The synthesized per-cell catalogs, which the search **writes on every run** | `catalogFor` in `tests/build-jail-search/search.mjs` |
| 5 | Hand-written overrides, and the schema they are validated against | `tests/build-jail-search/overrides/` |

Two of these are easy to forget because they are not the catalog anyone edits: the search synthesizes a fresh catalog for every cell it measures, and the selftest asserts against that synthesized shape.

**The pre-flight check must take its probe catalog from the emitting function, never from a literal.** A hand-written probe drifts silently from what the harness actually emits. A catalog with an empty package map is the worst possible probe, because it parses under every shape there has ever been and therefore proves nothing at all.

## Overrides

A hand-written override may replace a measured entry. Each one is a place the catalog stops being derived and starts being an assertion, and assertions rot silently while measurements re-run. Every override carries a mandatory rationale naming its investigator, evidence, and date; one that matches what measurement already produces is reported as dead weight.

The override directory is currently empty. The one entry it held was retired when measurement reproduced its capabilities exactly.

## Failure semantics

The jail **fails closed for scripts**. On a backend error, a missing backend, or a platform below the enforcement floor, the lifecycle script is skipped and the install completes, with a warning naming what was skipped and how to approve it.

This degrades to the behavior several package managers already ship by default — script skipped, install fine — and never to running an unconfined script from an unaudited package. It is what keeps the claim "runs jailed or does not run" honest.

## Cross-references

- [`build-jail-architecture.md`](build-jail-architecture.md) — the candidate architectures, each with a verdict and the evidence behind it
- [`build-jail-linux.md`](build-jail-linux.md), [`build-jail-macos.md`](build-jail-macos.md), [`build-jail-windows.md`](build-jail-windows.md) — per-OS enforcement mechanics and measured platform behavior

## Changelog

- 2026-08-03 — Corrected "How a grant is decided". The document said npm is consulted whenever the control fails; the classifier actually asks the JAIL-OFF cell first and short-circuits, skipping the oracles entirely when it fails. Recorded that `BROKEN-WITHOUT-JAIL-TOO` is not a nub defect count — measured 19 in a 100-package Linux slice, at least 15 environmental — that the jail-off cell must PROVE it ran unjailed, and that the oracle keys on whether npm SUCCEEDED rather than on signature equality.
- 2026-08-01 — Initial write-up. Supersedes an earlier front-end design note whose posture predated the measured catalog, and which recorded the `$HOME` redirect as rejected — the mechanism the jail now uses.
