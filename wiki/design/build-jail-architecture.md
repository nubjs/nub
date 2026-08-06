# Build jail architecture — is the shape right?

The three sibling documents record every mechanism tried on one operating system. This one asks the question above them: **the build jail pre-grants each package a set of filesystem paths and executables from a catalog, plus a single per-package network grant, and denies everything else — is that the right architecture, or is the steady stream of defects a symptom of the wrong one?**

It exists because the patch rate invited that question. Five path-spelling defects, a mechanism carried for weeks that measured inert, a preload that cannot reach one of Node's two resolvers, and two harness defects that made runs read green — all inside one subsystem, all in one stretch of work. The shape of the complaint is that Nub keeps reconstructing in userland what the operating system already knows: is this path that path, is this path under that root, did this process do the work.

## The verdict

**The architecture is correct, and the patch stream is mostly the price of being the only one of its kind that pays no privilege.** Every alternative shape surveyed is either unavailable at zero privilege on at least one of the three platforms, or is not a boundary at all — and the two mature implementations closest to this problem, Chromium's Windows sandbox and Microsoft's BuildXL, have the same two-layer structure Nub converged on independently: **a kernel-enforced token or ruleset is the boundary, and a userland compatibility layer sits above it whose job is to keep programs working, not to keep them contained.** Chromium states that split in its own design document. That layer is where the spelling defects were, and a compatibility layer that accretes per-API fixes is what every implementation of this shape looks like — Chromium's Windows sandbox is 56 non-test C++ source files of it.

**But a bounded part of the stream was avoidable, and it has one name: a canonical path is carried as an untyped string and then handed to matchers with different grammars.** One `canonical()` call returning the Windows verbatim `\\?\` form produced four defects at once, because four consumers each read that string under a different set of rules — one of them a glob matcher, where the `?` re-read as a metacharacter and silently dropped a grant. That is a type error wearing the costume of an architecture problem, and the prior art fixes it with a type: BuildXL's canonical path is a distinct immutable class, and its allowlist is a trie of path components rather than a string with a prefix test, so a boundary condition cannot be got wrong because there is no boundary to check.

**The one materially different shape worth taking is narrower than the question implies:** build each package in a Nub-owned scratch tree rather than in place under the consumer's project. It needs a copy rather than a mount, so it works at zero privilege on all three platforms, Nub already does it for git dependencies, and it collapses the read set from "the whole consuming project plus the machine-global store" to "this package and its declared dependencies." It does not fix the Windows blockers.

---

## How to use this document

Each candidate architecture carries a status, what it would buy, the evidence, and what would have to change for the verdict to move. The statuses match the sibling ledgers.

| status | meaning |
| --- | --- |
| **ADOPTED** | in the shipping design |
| **DEAD (mechanism)** | the primitive cannot do it; no privilege or tuning helps |
| **DEAD (privilege)** | works, but needs root, admin, or a setup command — disqualifying for the build jail |
| **REJECTED (design)** | technically available and deliberately not used |
| **WORTH RECONSIDERING** | available, not adopted, and the reason it was not adopted no longer holds or was never established |
| **OPEN** | unresolved, or a candidate whose deciding measurement has not been run |

The constraints every candidate is judged against are the build jail's, not the separate `nub sandbox` product's: **totally unprivileged with no setup command ever, including inside a container**; **packages breaking is the failure mode to avoid, and a residual is acceptable**; and the load-bearing defense is **package identity**, since a package with no catalog entry gets no network at all.

---

# What the patch stream actually is

Before asking whether a different architecture would have avoided the defects, it is worth establishing which axis they were on. The answer decides the rest of the document.



## How the catalog data is actually produced (2026-08-03)

The catalog is not authored. It is COLLATED from measurements, and the measuring system now lives in
its own private repo — `nubjs/build-jail-corpus` — rather than in this tree.

**Why it moved.** The run records are the deliverable and they are large (~50 KB per package-version
with per-cell logs; ~340 MB for a three-platform corpus). They were gitignored here, which meant every
measurement existed in exactly ONE place, on a disposable VM. The corpus also runs arbitrary
third-party install scripts, which belongs in a private repo with its own CI rather than the public
product repo.

**THE QUEUE IS THE SPEC.** `queue.ndjson` carries one row per `(package, version, os)` — 6,750 rows
for 2,250 package-versions across three operating systems. Coverage is then checkable by reading ONE
artifact instead of reconciling shard dispatches against CI history, which is how the previous
approach lost track: 175 packages measured twice on two Linux boxes while 349 sat unmeasured.

| mechanism | why it is that way |
|---|---|
| NDJSON, not a JSON array | a slice rewrites its own rows without touching every byte, so concurrent runs do not conflict on every commit |
| deterministic seeded shuffle | a name- or download-ordered worklist makes early slices structurally similar, so an early failure reads as a platform verdict rather than one unlucky neighbourhood |
| `pending` / `claimed` / `done` | a run that dies mid-slice leaves rows CLAIMED with its run id — attributable and reclaimable, never silently lost or double-run |
| results + queue in ONE commit | a claim and its evidence cannot diverge |
| reclaim BEFORE claiming | else the queue drains to a floor of permanently-stuck rows and reports itself incomplete with nothing pending |
| per-OS concurrency, never `cancel-in-progress` | cancelling an in-flight run would strand its claim |
| ~100-row slices, self-chaining | a failure costs one slice, and the chain continues unattended |
| **each record pushed as it lands** | a slice runs for hours, so committing once at the end meant a runner dying at minute 115 lost 100 measurements and nothing was visible until it finished. Per-record publishing caps the loss at one package and makes progress observable mid-run |
| **never force-push; replay onto origin** | forcing is the obvious way to make every push succeed and is the one mechanism that genuinely destroys data here — a force-push from one platform erases what another just landed. Unnecessary too: record paths are unique per `(platform, package, version)` and add-only, so the only real contention is the queue, which is RE-DERIVED from the records rather than merged |
| **soft reset, never hard** | `reset --hard` discards local commits *and the files they carry*, so a record committed but not yet pushed would vanish from the working tree — and therefore from the end-of-slice commit and the CI artifact too, since both read `records/` on disk. A silent loss of a measured result, inside the mechanism meant to prevent exactly that |

**The gate that had to travel with it.** `verify-corpus.mjs` runs BEFORE each commit and asserts
SUBSTANCE rather than validity: a `MINIMUM` record with a non-empty state must carry a structured
grant; a catalog with packages must carry capabilities; a package measured as needing egress must
still say so after collation; `.store` bookkeeping directories must never appear as package names.

That gate exists because of a measured failure mode: **six defects shipped while the measurement layer
was correct throughout**, each producing output that parsed, collated and reported success — and a
catalog with ZERO capabilities. Nothing caught them because every test asserted the hand-maintained
compiled-in table; nothing compared GENERATOR output against a CONSUMER.

⛔ **A GREEN RUN THAT PRODUCES NOTHING IS THE THING TO BE AFRAID OF.** The first live macOS slice
claimed 100 rows, hit `timeout: command not found` (it is GNU coreutils; macOS does not ship it), had
the refusal swallowed by `|| true`, and committed a slice of zero records while reporting success. The
gate now takes `--expect <n>` and fails when rows were claimed and nothing was produced.


## ✅ RESOLVED — a v2 catalog override never fed the egress table (2026-08-02)

**The defect was in nub, and it was not Windows-specific.**
`catalog_override::package_network_allowed()` consulted only the **v1** catalog. A v2 override
parses and loads fine (`active_v2()`), but v2 carries egress as a per-package CAPABILITY
(`packages[<name>].default.network`) rather than a v1 `packageNetwork.full` table — so the function
returned `None`, `build_jail_net_allowed` fell back to the COMPILED-IN table, and that table cannot
name a package the override exists to test.

**Why it looked like a Windows platform defect.** macOS and Linux deny egress IN-KERNEL (Seatbelt /
Landlock+seccomp) straight off the v2 capability and never reach that function. Windows has no
per-process network filter, so its userland JS net gate is the table's ONLY consumer. A
cross-platform override bug therefore surfaced on exactly one platform.

MEASURED before the fix — Windows accusation rate split by whether the package needs network
(macOS `MINIMUM` state as ground truth):

| | accused / total |
|---|---|
| **needs network** | **15/18 = 83%** |
| no network | 2/57 = 3.5% |

`@apollo/rover@0.29.1` at the WIDEST grant (`write.disk + network`) downloaded on macOS and logged
`blocked network access to rover.apollo.dev` on Windows.

FIXED `e3cdc0e7f9` — the v2 branch derives the egress set from `default.network` plus any band's
`network`, dropping version scoping deliberately (v2 resolves a version to exactly one grant via
`Entry::grant_for`, while this table's matcher is name-scoped, so a name needing egress at ANY
measured version must appear; over-granting is the accepted direction).

PINNED `crates/nub-sandbox/tests/generated_catalog_round_trip.rs` — the first test comparing
GENERATOR output against a CONSUMER. Every prior check asserted the hand-maintained compiled-in
table, which is why the emission defects shipped unnoticed.

### ⛔ TWO WRONG DIAGNOSES ON THE WAY, both specific and plausible

1. **A SCOPED-NAME SKEW.** Raw rates were scoped 41.9% vs unscoped 7.1% — a 6x skew pointing at the
   exact-match lookup in `compiler/package_network.rs:46` missing the store's `@scope+name`
   encoding. Killed by splitting on BOTH variables: scoped packages with no network need are accused
   **0%**. Scope merely correlates with network need (CLI tools download binaries).
2. **"THE HARNESS EMITS A SHAPE NOTHING READS."** Reading `parse_package_network` — which takes
   `networkHosts[].fetchedBy` and `packageNetwork.full[]` only — suggested the harness's v2
   capability was unread, and a v1 table was added to compensate. It worked, and it was a workaround
   over this bug. **TWO PARSERS EXIST:** `catalog_v2::parse` takes the capability shape and
   `catalog_override` tries it FIRST; `catalog::parse` is v1. Asserting a v2 document against the v1
   parser fails with `` `networkHosts` must be an array ``, which reads as a generator defect and is
   not one. Workaround reverted in `2a66aa3bff`.


## Every defect but a handful lands on the read axis

Counted across all three sibling ledgers, the filesystem **read** axis carries the overwhelming majority of both the adopted mechanisms and the open defects. The write axis produced two items, one of which is a package-manager linker bug reproducible with confinement entirely off. The network axis produced capability findings — per-host egress was surveyed on all three platforms and then withdrawn, leaving a per-package boolean — rather than compatibility breaks.

| axis | representative items | count of ledger sections |
| --- | --- | --- |
| **read / exec** | the `/etc` enumeration and its distro-shaped TLS correction, the `EACCES`-versus-`ENOENT` compatibility cost, read-must-render-as-read-plus-execute, resolved-leaf grants, the grant explosion under `ARG_MAX`, the `$HOME` over-grant through a symlink hop, the `posix_spawnp` PATH-search abort, pyenv's Python, the whole Windows bypass-traverse and ancestor-repair and canonicalization apparatus, blockers 1 and 3 | dominant |
| **write** | the metadata-write residual on Linux, node-gyp's sibling-store write, node-gyp's store-entry-root write, a read grant synthesizing a deny that revoked write from everything it enclosed | 4 |
| **network** | the withdrawal of per-host egress, the userland net gate on Windows, the netns bridge readiness race | 3 |
| **neither** | the piped-stdio hang (object namespace), the descriptor sweeps, environment keys containing `=`, `getcwd` refused at an ungranted cwd | 5 |

This is the single most useful fact in the document, because it means the architecture question reduces almost entirely to one narrower question: **is denying reads by default the right posture, and could it have been avoided?** The rest of the ledger below answers no on both platforms where it matters.

**Two later items shift the counts without shifting that conclusion.** The write axis gained a real architectural finding — a backend synthesizing a deny out of an `Allow`, discussed [below](#polarity-is-a-property-of-the-backends-not-only-of-the-ir) — and the network axis collapsed from a host list into a per-package boolean, which removed a capability rather than adding a defect. Neither is a read-axis item, and the read axis is still where the compatibility cost lives.

## The Windows read-repair layer is where the spelling defects live

Node's `realpathSync` walks a path component by component and, on Windows, `lstat`s the volume root first. Bypass-traverse exempts *intermediate* components of a single open; it does not make an ancestor openable as a *target*. So a confined `node` dies before user code runs, and Nub ships a preload that tolerates a refused component when it is a strict ancestor of a granted root.

**That repair is a compatibility layer and its own document says so:** the tolerance rule never grants access, it only asserts that a component the operating system refused to interrogate is a plain directory so the userland walk can continue, and the open that follows is still checked by the kernel against the LowBox token. A tolerance decision wrong in the permissive direction produces a walk that continues to an open that is then denied. Every one of the spelling defects failed in the *restrictive* direction — `EPERM` on a path the jail granted.

**The defect is not Nub's and long predates AppContainer.** Node's ancestor realpath walk has broken under restricted principals since at least the v0.x era, and the same failure was reported against ASP.NET's Node integration for IIS deployments, where the application account holds read on its own directory and below but not on `C:\Users\<name>` ([nodejs/node-v0.x-archive#3977](https://github.com/nodejs/node-v0.x-archive/issues/3977), [aspnet/JavaScriptServices#1101](https://github.com/aspnet/JavaScriptServices/issues/1101)). The second states the mechanism exactly: *"Node tries to walk along the file path to the module, starting from the disk root, reading the attributes of each directory to see whether or not it's a symlink."* Neither was ever fixed upstream.

## One root cause produced four of the spelling defects

The batch that fixed them names it directly: `build_jail.rs`'s `canonical()` returned Windows verbatim paths, and four consumers were wrong — node-gyp parsed `\\?\` as a plain path, a grant's `?` re-read as a glob metacharacter and was **silently dropped**, and two containment checks (`ProbeScope::allows` and the refusal of a grant at or above `$HOME`) could never fire because one side was canonicalized and the other was not.

That is one bug with four blast radii, not four bugs. The fix routed the call through `canonicalize_including_nonexistent`, the single canonicalizer Nub already ships and that the Linux and macOS backends already used — which is also what the Windows canonicalization survey had independently recommended. **The remaining exposure is structural: nothing in the type system stops the next path from taking the same trip**, and the two moves that would close it are in [what the mature implementations do](#what-the-mature-implementations-do-that-nub-does-not).

## Polarity is a property of the backends, not only of the IR

The build jail's central guarantee is that it is a **pure allowlist that emits zero deny rules**, and the compiler enforces that on the IR (`preset::enforce_pure_allowlist`). **That is one layer too high, and a backend broke it from underneath.** The macOS `emit_fs` mapped `(Allow, Read)` to a `(deny file-write* <term>)` rule and emitted it in the write loop, so under Seatbelt's last-match-wins a *read grant* silently revoked write from everything it enclosed — measured on a real profile, where widening one grant wrote 20 files and adding a read grant alone wrote 0.

**The argument that settles the polarity is stronger than the measurement, and it is worth stating in that order.** The synthesized deny had nothing to cap: Seatbelt's write base is already `(deny default)`, and a generous `default_effect` widens only reads, so the only thing that deny could ever cancel was another Nub grant. A rule that can only ever subtract from your own grants is not a boundary, it is a bug with a plausible-looking justification — and it carried one, in a comment, for as long as it survived.

**The generalizable shape: an invariant asserted on an intermediate representation must be re-asserted at every rendering of it.** Four renderings existed and three were already additive — `FsPolicy`'s own contract says the write-set is the `ReadWrite` allows, Landlock unions its rules and has no deny primitive at any ABI, and `windows::derive_grants` accumulates a read set and a write set with no ordering. **Only Seatbelt, the one backend with an ordering rule, could express the mistake, and it did.** This is the same class as the [inert mechanism below](#the-inert-mechanism-was-a-measurement-gap-not-a-design-gap): a property everyone believed held, in a configuration nobody had run the deciding arm on.

**It also cost a capability, and the cost is the right trade rather than a regression.** *"Readable but not writable inside a writable grant"* is now inexpressible on every backend; removing access is a `Deny`, which removes read too. Nothing in the catalog or the docs depended on the demote — but the contributor guidance did recommend the shape that produced it, advising `projectReads` as "the smaller grant", which is how a compiler bug became a documented pattern.

## The inert mechanism was a measurement gap, not a design gap

The ancestor repair was carried while every matrix that varied it either ran with no preload, or stamped the preload only in the repaired arm. The cell that decides the question — repair off *with* the preload, beside repair on with the same preload, one fixture, one run — had never been run. When it was, every de-elevated outcome was identical and the non-Node transcript was byte-identical across all seven cells.

The generalizable rule is not architectural. **A mechanism that carries a disable seam must have the arm where it is disabled in the matrix that claims to measure the shipping configuration**, and Nub already had the seam (`NUB_SANDBOX_WIN_NO_ANCESTOR_REPAIR`). The same round also found why the repair still *looked* load-bearing: the preload's roots and the walked components arrived in different Windows spellings, so its tolerance rule silently never fired — a compensating defect masking an inert mechanism, which is the specific reason it survived as long as it did.

---

# The candidate architectures

## A path allowlist as the primitive — ADOPTED, and the enforcement is not string-based

**The objection.** Every spelling defect was a string comparison that should have been an identity comparison, so a path allowlist looks like the wrong primitive.

**The premise does not hold for the enforcement layer, on any of the three platforms.** Landlock rules are attached through a file descriptor — `rust-landlock` takes an `AsFd`, and the Linux backend deliberately opens each leaf with `O_PATH` so the rule keys on the target inode rather than the name — and the kernel documentation ties a rule to a *file hierarchy* with `LANDLOCK_ACCESS_FS_REFER` governing what happens when a file is reparented. Seatbelt canonicalizes before matching, which is why hardlink, symlink, `cp`, `mv`, `..`, `//` and the `/tmp` alias are all measured closed. A LowBox token is checked against the object's own security descriptor, which is why an NTFS hard link shares one descriptor with its original and why the interpreter is copied rather than linked. **All three are object-identity checks. Nub compiles a policy from paths and hands it to a kernel that resolves them to objects once, at rule-installation time.**

The string comparisons are all in the layer above: the policy compiler, and the Windows realpath repair. Both are outside the boundary, and the repair's own errors are compatibility failures that fail closed.

**Where a path-string allowlist genuinely is the boundary, it leaks — and Node's own permission model is the demonstration.** It is a radix tree over resolved path strings, its documented limitation is that *"Symbolic links will be followed even to locations outside of the set of paths that access has been granted to"*, its policy build adds a wildcard only when the directory exists at that moment (`WildcardIfDir`, `src/permission/fs_permission.cc`), it describes itself as a *"seat belt"* that *"Malicious code can bypass"*, and CVE-2026-21715 is precisely a missing check on `fs.realpathSync.native` letting confined code enumerate outside its grants. That is the failure mode of a userland path allowlist as a boundary, and it is not the one Nub is in.

**What would change the verdict.** Nothing available. Handle-based enforcement is what the three kernels already do.

## Pre-granting from a catalog keyed on package identity — ADOPTED

**What it is.** Each package is granted, in advance, the paths and executables it needs, plus network access as one all-or-nothing grant; no catalog entry means no network at all.

**It is the ecosystem's converged answer, and Nub adds enforcement to it rather than replacing it.** The pnpm 10 release flipped the default in January 2025, and pnpm 11 replaced the five older settings with a single `allowBuilds` map, which additionally requires a **trusted package identity** before a name-keyed rule may approve a script — a registry-shaped dependency path — so git, tarball and directory artifacts must be approved by their lockfile path instead. Version 12 of npm flips the same default in July 2026: `allowScripts` off, with `--allow-git=none` and `--allow-remote=none` beside it. LavaMoat's `allow-scripts` is the same shape as a standalone tool, with an `auto` mode that populates the allowlist. None of these confines an allowed package at all — an approved script runs with the user's full privileges.

**So the catalog is not the novel part; the confinement of an approved package is.** That also settles a recurring worry: granting a catalogued package full network is not a weakening relative to the ecosystem baseline, because the baseline is full network *and* full filesystem *and* full environment.

**What would change the verdict.** A registry-side attestation strong enough to make identity itself the boundary. Nothing on that path is close.

## The network axis is governed by identity alone, not by a host list — ADOPTED, and the two lists are decoupled

**What it is.** The catalog carries two things that look like one: `networkHosts`, a set of artifact hostnames, and `packageNetwork`, a per-package grant. **They are decoupled, and mistaking them for a single knob produces the wrong question.**

- **`networkHosts` feeds only Nub's own PREFETCH**, which runs **unconfined, as the user, before the jail exists** (`pm_engine/build_prefetch.rs`). It is where Nub — not the script — derives an artifact URL from the package's manifest, fetches it, and writes it where the installer already looks.
- **`packageNetwork` feeds the jail's egress grant**, which is a coarse per-package boolean on all three platforms and **names no hostname at all** (`compiler/preset.rs:602-671`).

**So the criterion for admitting a host is *"does the PREFETCHER need it?"* — not *"is it safe for a script to reach?"*** That reframing is the finding. It was verified structurally: a 43-entry catalog change promoted **zero** hosts, and `DOWNLOAD_HOSTS` came out **byte-identical** across it (`dde33ef0…` both sides) while the per-package table's digest changed — the second half being the control that distinguishes byte-identity from a stale generated artifact. No prefetch demand was measured for any candidate, so every one went to `packageNetwork` instead.

**A distinct criterion applies to the prefetch list, and it is exfiltration rather than trust.** A host can fail it outright, regardless of prefetch demand, when the *same hostname* also accepts a write or serves arbitrary tenant content — because Nub's unconfined GET is one anonymous read, but the surface is the name:

| host | why it can never be a prefetch host |
| --- | --- |
| `github.com` | serves `git-receive-pack` — an authenticated **write** — on the same name |
| `registry.npmjs.org` | the authenticated **publish** route is the same name, reachable with a project `.npmrc` token; the GitHub shape exactly |
| `raw.github.com` | 301s to `raw.githubusercontent.com`, i.e. arbitrary repo content |
| `storage.googleapis.com`, `www.googleapis.com` | multi-tenant — an attacker rents a bucket and reads back |
| `o30291.ingest.sentry.io` | a telemetry sink **whose POST body is the product** |

**There is a real argument the prefetch side could be broader** — a prefetch entry only lets Nub perform one anonymous GET whose body is written to a file and never executed, which would make `github.com` (covering essentially the whole prebuilt-binary population) cheap there and useless as a script-reachable host. That widening exists, ships inert behind an off-by-default cargo feature, and is a maintainer call rather than an implementer's, because the code path runs unconfined as the user.

**What the boolean costs, stated because it is easy to under-read.** Egress is per-package, so **granting a package restores every host it talks to** — admitting `snyk` also restores its Sentry POST. The catalog's per-package `hosts` arrays are retained purely as provenance: a package that used to fetch from its own CDN and now reaches somewhere else shows up as a reviewable diff.

**What would change the verdict.** An unprivileged per-host mechanism on all three platforms. macOS has one and it was withdrawn precisely because the other two do not — a list that gates one platform and is provenance on two is a compatibility liability rather than a defense.

## Observing instead of pre-granting — REJECTED (design) as a boundary, WORTH RECONSIDERING as a policy generator

**What it is.** BuildXL's model: interpose on filesystem operations, report every access to the engine, and compare the report against the pip's declared manifest. Its `FileAccessPolicy` carries report bits beside allow bits — `ReportAccessIfExistent`, `ReportAccessIfNonexistent`, `ReportDirectoryEnumerationAccess` — and one policy is explicit that enforcement is post-hoc: *"Observe that sandboxing never blocks in this case, denying the access is surfaced as a DFA after the write happened."*

**As a boundary it is a non-starter here, and the reason is that BuildXL is not a security sandbox.** Its blocking exists to keep builds deterministic — the spec says the capability is used *"to block access to disallowed paths, e.g. paths that are known to have been created by other pips that have not been declared as dependencies"* — and Bazel says the same thing about its own more plainly: *"Sandboxing doesn't hide the host environment in any way. Processes can freely access all files on the file system."* Gentoo's `sandbox`, the closest distro analogue, opens its README with *"This is used as a QA measure."* An architecture that reports an exfiltration after it happened does not answer the threat model.

**As a policy generator the idea is ADOPTED, though not in its observational form.** BuildXL ships a `JavaScriptDependencyFixer` analyzer that reads violations out of the execution log and writes the missing entries back into the offending `package.json`. The catalog is now generated by a different mechanism that answers a stricter question.

An observer records what a package *touched*. That over-states the requirement: a script may stat a path it does not need, or write a cache whose absence costs nothing. The catalog needs what a package **cannot install without**, which is a question about counterfactuals, not observations — so the generator runs the package's lifecycle scripts under each candidate capability set and keeps the cheapest one that still reproduces the unconfined result.

The capability space is 54 states (`read` 5 × `write` 9 × `network` 2, minus the pairs where a write already covers a read at the same scope). They are walked in **ascending cost order and the first pass is the answer** — which is the true minimum by construction, because every cheaper state was already tried and failed. A greedy descent from full grant finds only *a* minimal set, and which one depends on the order capabilities are dropped; that is a heuristic. The cost order's one hard requirement — a strictly wider state costs strictly more — is proven over all 54 pairs rather than reviewed, since a violation would silently return a non-minimal grant.

Two properties of the oracle are load-bearing and were each learned by getting them wrong:

- **A cell is judged by the ARTIFACT SET, not the exit code.** A git-hook installer whose grant is withheld writes none of its hooks and still exits 0 — a silent no-op, not a crash. Judging by exit code inverts the answer for that whole family. The reverse is equally wrong: plenty of lifecycle scripts only print a notice, and for those the exit code is the only signal. Deriving the success condition from an unconfined control run handles both, and collapses to exit-code matching exactly when the package writes nothing.
- **The control runs TWICE and the two runs combine by UNION.** One control cannot distinguish "this cell lacks a capability" from "this package does not write the same paths twice". Combining by intersection is the tempting fix and it is unsound in the dangerous direction: it compares on fewer paths, so a cell that failed to write an unstable path still passes, and the generator records too narrow a grant — the failure this system exists to prevent. Paths that vary between the two controls cannot be required of a cell, so instead the scopes they belong to are **escalated into the grant**. Widening on uncertainty costs breadth; refusing to widen costs a broken package.

Observation is still available unprivileged on two platforms — `strace` on Linux is how the `/etc` set was measured, and a `--import` preload sees every Node-level access on all three — and remains useful for diagnosing a single failure. It is not a boundary and is not presented as one.

**What would change the verdict on the boundary half.** Nothing. The report arrives after the access.

**One route checked and closed.** Seatbelt's `(trace "<path>")` directive would have been the unprivileged macOS observer. Measured on darwin 25.5: a profile of `(version 1) (allow default) (trace "<writable path>")` produces no trace file, while the same profile shape with `(deny file-read* (literal "/private/etc/hosts"))` under `(allow default)` **does** deny the read — so the profile is loaded and its rules take effect, and only `trace` is inert. Use `strace` and the preload instead. (That control also reproduced the canonicalization trap the macOS ledger warns about: the identical deny written against `/etc/hosts` rather than `/private/etc/hosts` matches nothing and the read succeeds.)

## The grant vocabulary is scopes, not paths — ADOPTED, and one scope is not a grant at all

**What it is.** A grant names capabilities over **scopes** — `deps`, `project`, `userHome`, or `disk` — rather than paths. Read and write are independent axes, and the narrow scopes **compose without nesting**, so none can be expressed in terms of another and no combination may be validated away as implied. `disk` is the only dominance relation, which is why it is a separate arm and `disk`-plus-a-narrow-scope is unrepresentable rather than merely rejected.

**The narrow scopes genuinely do not nest, and the counterexample is not exotic.** `project` is not inside `userHome`: containers put the project at `/app` with `HOME=/root`. And `deps` has no fixed home at all — whether a dependency lands inside the project or in the global store depends on whether its dependent was materialized, so `deps` sits under `project` for one package and under `userHome` for another **in the same install**.

**Why `deps` exists as its own scope.** Under the global virtual store a package's declared dependencies are not inside it; they sit beside it as symlinks into separate top-level store entries, and `<pkg>/node_modules` does not exist. Node still resolves them because resolution walks up. A write to a dependency therefore lands in a different top-level store entry — outside the project — so neither project-write nor own-directory-write reaches it. The bound is also the security argument: each dependency is resolved by **following the package's own links, never by joining a name onto a directory**, so the only reachable entries are ones the package can already `require`, and a separator in a name cannot escape because no name is ever joined.

**A grant is over a scope, so varying path NAMES do not defeat it.** This is what makes a package whose output is not byte-stable still catalogable: if two runs write different filenames inside the same dependency entry, one `deps` grant covers both. Nondeterminism only defeats a grant when it crosses a scope boundary.

### ⛔ `disk` is not a scope like the others, and it means something DIFFERENT on each platform

The narrow scopes name a region. `disk` names the **absence of a region**, and how that absence is implemented diverges per backend. Each per-OS document states its own half; they are collected here because reading one platform's meaning onto another has produced real errors more than once — including a classifier that labelled Linux records "AppContainer-incompatible" on a platform that has no AppContainer.

| backend | what `write:"disk"` actually does | what else it costs |
| --- | --- | --- |
| **Linux** (Landlock) | `relax_fs_to_full_disk` **clears the ruleset** and flips the default effect to `Allow` | filesystem confinement only; other axes survive |
| **macOS** (Seatbelt) | same — the ruleset is cleared, the default becomes allow | filesystem confinement only |
| **Windows** (AppContainer/LowBox) | **declines the LowBox token entirely** and runs the child as a plain process, because the AppContainer allowlist has no spelling for "the whole filesystem" | ⛔ **egress with it** — network is an AppContainer *capability*, so the packages in this tail are unconfined on the network axis too, whether or not the catalog granted it ([`../research/windows-unprivileged-egress-block.md`](../research/windows-unprivileged-egress-block.md)) |

Three consequences that follow, and each has cost time when forgotten:

- **On Linux and macOS a `disk` record is not evidence of a write need.** The rung repairs *read* refusals just as readily, by abandoning filesystem confinement wholesale — which is exactly how the `/proc/self/*` residual reached it without any package writing anything unusual ([`../research/linux-procfs-residual.md`](../research/linux-procfs-residual.md)).
- **On Windows a `disk` record may not be a filesystem fact at all.** A package that cannot run inside a LowBox token for unrelated reasons passes only at this rung, so the ladder records `disk` while nothing about paths was ever measured. Asking "which path does it need" has no answer, because no path is the answer.
- **There is no rung between them.** `read:"disk"` front-inserts the enumerated complement of the secret set and stays read-only; `write:"disk"` is total. Nothing expresses "wide but still confined", because `enforce_pure_allowlist` strips every deny on all three platforms — neither Landlock nor AppContainer can express deny-inside-allow, so the preset withholds a path by never granting it rather than by blocking it.

- ⛔ **`read:"disk"` recovers the SUBTREE secrets only — the `.env*` family stays readable, and "the enumerated complement of the secret set" above should not be read as "all secrets".** REPRODUCED on macOS arm64, two arms differing only in the grant, with three controls and the fixtures kept out of `/tmp` (a fixture there sits inside the jail's own private-temp redirect and cannot test a denial at all):

  | probe | no catalog entry | under `read:"disk"` |
  | --- | --- | --- |
  | `.env` at an absolute path outside the project | DENIED | ⛔ **READ_OK** |
  | a plain file beside it | DENIED | READ_OK *(expected — that is what the rung grants)* |
  | `~/Library/Keychains/…` (a subtree secret) | DENIED | **DENIED** |

  **Why the asymmetry is structural rather than an oversight.** The complement is only enumerable for the `$HOME`-anchored **subtree** secrets — `.ssh`, `.aws`, `.gnupg` and the rest are finite prefixes, so their complement can be front-inserted. `**/.env*` is a **depth-independent basename** match with no finite allow-complement, so it cannot be recovered the same way. Closing it needs a per-backend rendering step, **not** a deny in the shared IR: `deny_shadows_grant` fail-closes any policy carrying a deny whose `literal_prefix` is `""`, `**/.env*` normalises to exactly that, and six floor globs trip it — so putting the deny back immediately re-breaks Windows. ⇒ The naive fix is known-harmful; see [`build-jail-windows.md`](build-jail-windows.md) and [`build-jail-macos.md`](build-jail-macos.md).

  **Not live in the shipped catalog today, and the reason is worth stating** so nobody "confirms" it is safe for the wrong reason: the shipped v1 model has **no read-only-disk tier at all** (`full_disk` is read *and* write), so no shipped package can reach this rung. It becomes reachable the moment a v2-derived catalog ships — **21 darwin records and 1 linux record** sit on `read:"disk"` — which makes closing it a prerequisite of that promotion rather than a live exposure.

⇒ **Never read a `write:"disk"` count as a filesystem-capability number**, and never compare that count across platforms without saying which mechanism produced it.

### ⛔ TEMP IS THE SECOND SCOPE THAT MEANS SOMETHING DIFFERENT PER PLATFORM — and on Windows it is not even a separate place

Two of the three platforms give a jailed script a **private, per-run temp directory**, grant it read+write, point the child's temp env at it, and hide the shared system temp. Windows does **not**, and the asymmetry is structural rather than an oversight:

| | private temp? | where scratch lands | consequence for a `temp` scope |
| --- | --- | --- | --- |
| **linux** | yes | `/tmp/nub-tmp-<random>`, granted; `TMPDIR` repointed | temp is its OWN place, currently classified `outside` |
| **macos** | yes | same shape | same |
| **windows** | **no** | the OS virtualizes an AppContainer's TEMP into `…\Packages\<profile>\AC\Temp`, under the user profile | **temp carves out of `userHome`, not `outside`** |

**Verified from source**, not from a report: `compiler/preset.rs` inserts `fs["$tmp"] = "rw"` under `#[cfg(not(windows))]`, so on Windows the policy never asks for a private temp; `backend/mod.rs`'s `make_private_tmp` returns `None` unless `TmpMode::Private`, and the env-setting helper is therefore never called. Confinement on Windows still holds — it comes from the AppContainer profile rather than from a nub-created directory.

**The consequence that bites, and it is measured on both POSIX platforms.** Because the jail repoints the temp environment variable, **how a package spells its temp path decides whether it needs a grant at all**:

| what the script does | unjailed | jailed at the empty grant |
| --- | --- | --- |
| `os.tmpdir()/…` — honours the repointed variable | ALLOW | **ALLOW — needs no grant** |
| a hardcoded `/tmp/…` | ALLOW | **EACCES** |

Reproduced on Linux CI with `$HOME` and `/var/tmp` as negative controls refused in the same arm, on two independent runners. ⛔ **On Linux the build jail is Landlock-ONLY** — there is no mount namespace, so nothing rebinds `/tmp`; the grant sits on the host path `nub-tmp-<random>` and bare `/tmp` stays ungranted. A source read of the bubblewrap path predicts the opposite and is wrong, because that path is unreachable for a build-jail policy.

⇒ **Two rules follow.** First, a `/tmp` path appearing in an observation record proves NOTHING about which spelling the script used — OBSERVE runs unjailed with the variable unset, where `os.tmpdir()` *is* `/tmp`; only reading the constructing line settles it. Second, any future `temp` scope must be defined per platform, because on Windows it is a subset of `userHome` while on POSIX it is disjoint from every existing scope.

## Artifacts that outlive a discarded HOME — the declared-writes mechanism

The jail gives each package a **private, throwaway `$HOME`**, which is what makes the large scratch-directory population work: a corpus of lifecycle scripts put an unwritable home behind most filesystem failures, and each wanted a home-anchored scratch directory rather than the user's actual home. It is per-package rather than shared, because a shared home is a config root two dependencies both write — one package could drop a `$HOME/.npmrc` naming a `script-shell` or `node-gyp` under its control, a second package's build fallback would honour it, and the attacker would then be running inside that second package's jail with write access to a native addon the user later loads **unconfined**.

**That redirect creates a failure the artifact oracle structurally cannot see.** A package that caches a large download under `~/.cache/<vendor>` installs perfectly, reproduces the control exactly, and has its artifact **discarded** — because the control does the same thing. Measured on a browser-downloading package: 355 of 359 written paths landed in the throwaway, and zero in the real cache.

So a grant may declare `$HOME`-relative **directories** that Nub moves into the real home once the scripts finish. It carries the same authority as a grant, which is why it is named as one — but the script never holds a live handle on the user's `$HOME`, so an **undeclared** home write still lands harmlessly in the throwaway instead of being denied, which is what keeps the scratch-directory population working. Same device, so the move is a rename rather than a copy.

## Generous read minus secrets — DEAD (mechanism) at zero privilege

**What it is.** The shape every comparable tool uses: allow reads broadly, deny a named set of secrets inside the allowed region, and confine writes and egress. Trail of Bits' `build-wrap`, the closest sibling in another ecosystem — it re-links Cargo build scripts to run under a sandbox — defaults to exactly five bubblewrap flags: `--ro-bind / /`, `--dev-bind /dev /dev`, a write bind on `OUT_DIR`, a write bind on `/tmp`, and `--unshare-net`. Anthropic's `sandbox-runtime` emits `(allow file-read*)` as its read base and carves denies out of it (`src/sandbox/macos-sandbox-utils.ts`, annotated *"default: allow everything"*).

**It is enormously attractive, because it would delete most of the read axis** — and the read axis is where nearly every defect in this subsystem lives. No `/etc` enumeration, no distro-shaped TLS correction, no grant explosion, no PATH-search abort, no pyenv problem, and no ancestor problem on Linux or macOS.

**It is not expressible at zero privilege on two of the three platforms, and both are measured.** Landlock rules union and there is no deny primitive at any ABI through 10 — `allowed_access = 0` returns `ENOMSG`, and execute-only and write-only rules are accepted with zero restricting effect. On Windows a per-file deny ACE naming the per-run AppContainer SID is **inert against that AppContainer's own child**, measured 9 of 9 cells including LPAC, because access is checked at handle-open and the granted mask is cached in the handle. Neither is a tuning problem.

**And the privilege cost is visible in the prior art rather than inferred.** Trail of Bits' tool supports Linux and macOS only, and its own installation instructions require `sudo` to install an AppArmor profile on Ubuntu 24.04. The Windows backend of `sandbox-runtime` requires `npx … windows-install` once per machine with admin, creating a local group and installing Windows Filtering Platform filters, and its README states the result *"is not a security boundary against a deliberately adversarial sandboxed process"*, lists Task Scheduler and parent-process re-parenting as unfixable same-user escapes, and records that `filesystem.allowWrite` and `filesystem.allowRead` are **not supported on Windows at all**. Its stated fix is a separate sandbox user account — which is the `nub sandbox` route, and is dead on privilege for the build jail.

**On Windows the pure allowlist is not even a choice.** A LowBox token reaches an object only where that object's ACL names its AppContainer SID, a held capability, or `ALL APPLICATION PACKAGES`. Granting broad read means writing ACEs on roots no unprivileged user holds `WRITE_DAC` on, and the six recorded attempts to reach `C:\` and `C:\Users` are all dead. Allow-polarity is the primitive.

**What would change the verdict.** A Landlock ABI with a deny or precedence primitive, and a Windows mechanism that denies inside an AppContainer grant. Upstream Landlock's union semantics are deliberate and there is no sign of either.

## Hermetic isolation, the Nix and Guix model — DEAD (privilege), and its content-addressed half is already ADOPTED

**What it is.** Do not allowlist at all: give the build a fresh filesystem view containing only its declared inputs, and the question of what it may read does not arise. Nix builds in a chroot with private mount, PID, IPC and network namespaces, and adds `CLONE_NEWNET` whenever `derivationType.isSandboxed()` (`src/libstore/linux/build/linux-derivation-builder.cc`). Guix does the same.

**It needs a mount namespace, which needs the user namespace the build jail cannot rely on.** That is the same wall bubblewrap hits, for the same reasons: denied by default on Ubuntu 23.10 through 25.04 with `apparmor_restrict_unprivileged_userns=1` and no shipped exemption profile on 24.04, and impossible inside Docker because `cap_sys_admin` is absent so even root cannot create the namespace. Nix's unprivileged daemon mode still uses user namespaces; its `--disable-chroot` escape produces builds that *"will not be isolated from one another or from the rest of the system."* There is no Windows analogue at zero privilege.

**Its network model is the part that transferred, and it already has.** Nix forbids network in a build outright and provides one escape hatch: a fixed-output derivation, which may reach the network precisely because its output hash is declared in advance. Nub's prefetch is that shape — pre-place the artifact so the lifecycle script never opens a socket — and the Linux ledger already treats it as structural rather than a convenience, because it is what lets a package that would need network run with network off. Of 230 corpus packages read at source, 85 need no network at all and five hosts cover 92% of the rest.

**Portage's escape from the same corner is also worth recording**, because it is the design Nub's Linux egress bridge independently arrived at: with `network-sandbox` on, `network-sandbox-proxy` spawns a SOCKSv5 proxy on a UNIX socket and exports its address into the sandbox, so a build that genuinely needs the host's network crosses the namespace wall through one audited channel. Nub's bridge is the same shape and is unavailable for the same reason the namespace is.

**What would change the verdict.** Universal unprivileged user namespaces — every distribution shipping an exemption profile and containers granting `cap_sys_admin`. Neither is coming.

## Building in a Nub-owned scratch tree — WORTH RECONSIDERING

**What it is.** Copy the package into a Nub-owned scratch directory, run its lifecycle script there against a grant set consisting of that tree plus its declared dependencies, and move the results back. It is hermetic isolation's *shape* obtained with a copy instead of a mount, which is what makes it available at zero privilege on all three platforms.

**Nub already does this on one path.** Git dependencies are copied into a temporary directory and built there (`prepare_scratch_copy`, `install/git_prepare.rs`). Registry dependencies are not, and the difference has never been deliberate.

**What it buys.** The grant set today gives a dependency's install script read on the **whole consuming project** and on the machine-global package store, so a script can read every package any project on that machine installed. A scratch tree collapses both. On Windows it has a second effect: every directory in the grant tree becomes Nub-owned, which is the one condition under which a leaf grant reliably installs — the ledger already records that a Nub-owned program's leaf grant always installs while a System32 binary's never does de-elevated.

**What it costs.** A copy per built package, on top of the measured grant-then-populate ordering (24 ms to grant an empty directory against 426 ms to re-grant a populated tree). A behavior change for any package that writes into the consumer's tree and expects it to persist. And an unmeasured compatibility risk: the read-ladder study that narrowed the macOS read set found that dropping the project read outright fails 27 of 33 packages, so the scratch tree must carry the dependency closure the script resolves through, not merely the package.

**What it does not buy.** It does not fix the Windows blockers. The scratch tree still has ancestors, and `C:\` is always one of them.

**What would change the verdict.** A measurement: the corpus run with registry packages built in a scratch copy, against the same corpus built in place.

## Userland interposition — ADOPTED as a compatibility layer, never as a boundary

**The question.** Nub's preloads already hit a wall Node put there: the ESM resolver destructures `realpathSync` out of `fs` when `internal/modules/esm/resolve.js` is first required, which happens before any `--import` preload evaluates, so the shim is never seen on that path. Is userland interposition structurally doomed?

**As a boundary, yes, and every implementation says so in its own words.** Chromium's Windows sandbox is the sharpest: *"The interception + IPC mechanism does not provide security; it is designed to provide compatibility when code inside the sandbox cannot be modified to cope with sandbox restrictions."* Gentoo's `sandbox` is a QA measure whose README records that statically linked programs run unmonitored and that setuid binaries have to be handled by `ptrace` instead. BuildXL is migrating its Linux sandbox off `LD_PRELOAD` to eBPF for exactly this reason: interposed accesses *"can only be detected when executables are dynamically linked and the corresponding libc wrapper is used"*, and `io_uring` is named as the API that broke it.

**As a compatibility layer it is correct, and Nub's is used exactly that way.** The token or ruleset is the boundary; the preload repairs programs that cannot cope with it. That is Chromium's architecture, and Chromium has been paying the per-API cost of it for eighteen years — `sandbox/win/src` is 56 non-test C++ source files, with a policy, an interception and a dispatcher module for each intercepted API family. **The patch stream is not evidence of a wrong architecture; in this shape it is the architecture.**

**The honest cost, stated plainly.** A compatibility shim delivered through a language runtime's extension surface reaches only what that surface reaches, and the ESM destructure is the demonstration. Node's source varies across the support band — `const { realpathSync } = require('fs')` on v18.19, v20.19, v22.15, v22.23, v23.11 and v25.9, and `const fs = require('fs')` on v24.17 and current `main` — so it flip-flopped and a version check is not a fix. The `--require` channel would reach it and cannot be used: `--require` takes a specifier the CJS resolver must resolve, and that resolution realpaths, under a jail where realpath is the thing that is broken.

**What would change the verdict.** Node reading `fs.realpathSync` through the namespace on every supported line, which is already true on `main`; or a decided design for the `module.registerHooks` fallback, which lands in v22.15 and therefore leaves the compat tier uncovered.

## Kernel-side interposition — DEAD (privilege) on Linux, DEAD (posture) on Windows

**What it is.** Interpose below the language runtime, where the ESM destructure and a static binary are both invisible. BuildXL's two answers are Detours on Windows and, replacing `LD_PRELOAD`, eBPF on Linux.

**On Linux, eBPF needs privilege.** BuildXL's own architecture loads its programs once per build through a daemon. That is a setup command by another name.

**On Windows, Detours is unprivileged and out of scope for a different reason.** Nub is an augmenter whose mechanism is restricted to Node's own extension surfaces; a DLL-injection interception layer is not one of them. BuildXL's short-name handling is the concrete thing being given up — it detours the `FindFirstFile` family and zeroes the alternate name outright, with the rationale stated in its source: *"We want to hide short file names, since they are not deterministic, not always present, and we don't canonicalize them for enforcement."* Nub cannot intercept discovery, but the design position transfers and is [adopted below](#the-confined-process-never-sees-the-ambient-temp-directory): decide one spelling and stop the others entering the child.

**What would change the verdict.** Nothing on Linux. On Windows, a change to the augmenter posture, which is a project-level architecture decision and not a jail one.

## Brokering every operation — REJECTED (design)

**What it is.** Chromium's answer to the same allow-polarity Windows problem: the confined process gets essentially no filesystem access, and *"almost all resources that the renderer process uses have been acquired by the Browser and their handles duplicated into the renderer process."*

**It works because Chromium controls the confined code.** A renderer is written to ask its broker. A dependency's install script is arbitrary third-party code running arbitrary tools, and the only seam Nub could broker through is the same Node extension surface whose reach is bounded above.

**The build jail brokers nothing, and the one broker Nub ships belongs to the other product.** The loopback egress proxy — a Seatbelt profile permitting exactly the proxy's port, so every packet must traverse it and a raw socket cannot bypass it — is `nub sandbox`'s mechanism. The jail withdrew per-host egress and therefore starts no proxy on any platform; its net axis is the per-package boolean above.

**What would change the verdict.** Nothing. The premise — that the confined code cooperates — is false here by construction.

## Not running the script at all — the baseline, and now the ecosystem default

**What it is.** Refuse dependency lifecycle scripts unless the project names the package. This is what pnpm 10 did in January 2025 and what npm v12 ships in July 2026.

**It is the correct floor and Nub already sits on it.** A package with no catalog entry runs no script. The build jail's whole contribution is what happens to the packages that *are* approved: under npm and pnpm they run with the user's complete access, and under Nub they run confined. Framing the jail as a *reduction* from that baseline is not a rhetorical device — it is why granting more never requires elevation, and why a generous carve-out for a popular package is the right move rather than a compromise.

**What would change the verdict.** Nothing; it is not a competing architecture but the layer beneath this one.

## A Windows silo with per-silo bind links — DEAD (privilege), already probed

**What it is.** Promote a job object with `JobObjectCreateSilo` and attach per-silo bind-filter mappings, giving the child a private filesystem view under a normal token.

**Measured on Windows Server 2022 across four primary tokens, and it works.** Unflagged `node <deep file>` reaches user code inside the silo, `realpathSync('C:\')` resolves, and piped `spawnSync` returns in 80 ms rather than hanging — so it closes blockers 1 and 2 outright. Bind-linked paths realpath to the virtual path, and `\\?\` and `\\?\GLOBALROOT` device paths are redirected rather than escaping the mapping.

**And it is disqualified.** Silo creation needs no privilege, but the bind mapping needs Administrators membership; `SeTcbPrivilege` is neither necessary nor sufficient. An elevated helper can create the silo and duplicate the job handle to an unprivileged process, making the privileged step one-time rather than per-install — which is precisely the currency `nub sandbox` spends and the build jail cannot.

**What would change the verdict.** An unprivileged bind-filter mapping. It is the single most valuable Windows capability that does not exist.

## Mapping the scratch tree to a drive letter — OPEN, unverified

**What it would buy.** The Windows blockers are both `lstat 'C:\'` — an ancestor opened as a target, above every grant, on a root no unprivileged user can re-ACE. A build directory reached through its own drive letter has one ancestor instead of four, and that ancestor is an object-manager symbolic link to a directory Nub owns and has already granted.

**Why it is worth a probe rather than a paragraph.** The `DefineDosDevice` call creates the link in the calling user's own DOS-device directory, which a normal low-privileged account can write, so the mechanism itself needs no privilege. If the link resolves for the confined child, blockers 1 and 3 both close with no preload at all, which would also retire the tolerance rule and the entire spelling class with it.

**Why it may not work, stated so the probe is designed against the risks.** A LowBox token has its own object namespace and it is not established that a device link created by the launcher is visible inside it. Nub's own record already shows `\\.\pipe\LOCAL\…` resolving where `\\.\pipe\…` is refused, which is the same namespace split in a different subsystem. And Windows resolves a `subst` drive back through to the underlying volume for canonicalization, so the walk may meet `C:\` anyway.

**The probe that settles it.** One arm of the existing de-elevated jail workflow: define the device, launch the AppContainer with the entry point on the mapped letter, and report whether `lstat` of the drive root succeeds and whether an unflagged `node` reaches user code — beside the current arm as the control. Windows AppContainer work cannot run over SSH, so this goes through the branch-scoped workflow like every other Windows measurement here.

---

# What the mature implementations do that Nub does not

Four transferable practices, each from a system that solved the same problem at larger scale, and each cheap.

## A canonical path is a distinct type

BuildXL's in-sandbox enforcement path canonicalizes with `GetFullPathNameW` and nothing else, and wraps the result in a class whose header states the contract: *"Immutable, typed, and canonical path string. The represented path is absolute, free of .. and . traversals, redundant path separators, etc."* (`Public/Src/Sandbox/Windows/DetoursServices/CanonicalizedPath.cpp` in `microsoft/BuildXL`). Bazel makes normalization a property of the interned path object rather than a property of each comparison.

**Nub carries canonical paths as `PathBuf` and `String` and relies on every consumer knowing which spelling it received.** That is what let one `canonical()` return reach a glob matcher. A newtype around the output of `canonicalize_including_nonexistent`, with the compiler's matchers accepting only that type, would have made all four of those defects compile errors. It is the highest-value change in this document per unit of work.

## The allowlist is a component trie, not a string prefix test

BuildXL's policy lives in a trie of path components searched one component at a time (`PolicySearch.cpp`), and its subtree test walks both paths element by element, tolerating duplicate separators and either separator flavor (`IsPathWithinTree`, `StringOperations.cpp`). **The boundary condition then cannot be got wrong, because there is no boundary to check** — the comparison never sees a partial component.

Nub's Windows tolerance predicate is a string prefix test with an explicit boundary check, and it was measured correct against an adversarial table including sibling prefixes, verbatim and UNC roots, case, and dot segments. The recommendation is to keep it — it is cheaper than splitting on every probe — with the boundary check documented as the thing that makes it equivalent, so nobody removes it as redundant. The trie is the right answer if the rule ever moves into a per-operation layer.

## The confined process never sees the ambient temp directory

BuildXL lists `TEMP` and `TMP` on a `DisallowedTempVariables` set annotated *"these environment variables should not be read from config, since they refer to temporary directories that we reserve the right to redirect"*, and overrides both to a build-owned directory on top of a nine-name inherited allowlist (`Public/Src/Engine/ProcessPipExecutor/PipEnvironment.cs`).

**Nub's Windows environment floor passes the ambient `TEMP` and `TMP` through verbatim, and on a hosted runner that value is 8.3-short.** That is where the short spelling entered the child, and it reached the policy a second way through `std::env::temp_dir()`. Owning both removes the only spelling Nub does not choose — a smaller and more durable surface than reconciling spellings at the comparison, and the general form of the same discipline that already overwrites `NODE_OPTIONS` unconditionally.

## Outputs are declared, so nothing has to be reconstructed

Neither BuildXL nor Bazel infers whether a process did its work. A pip declares its outputs; the engine hashes them and stores them against a two-phase fingerprint. Bazel moves *"the known output artifacts out of the sandbox into the execroot"* — known, because they were declared.

**The npm ecosystem declares nothing, so reconstruction is forced**, and that is the honest reason the corpus harness diffs a path set and digests content: there is no manifest to compare against. The harness has already converged on the only sound substitute — a validity gate requiring the same package to reach its class effect with the jail off, so a package whose inputs were missing cannot score a pass by exiting 0. The two harness defects were mechanical rather than modelling errors: a `\r` from CRLF landing on a field so a class lookup missed, and backslash paths defeating attribution so 338 packages read as not-installed while installing fine. **The durable fix is provenance, not a different model** — every result artifact stamping the commit, the arm, and whether the curated grants were compiled in, which is already recorded as a defect because a run's configuration had to be inferred from file mtimes after the fact.

---

# Which patches were unavoidable, and which were not

| defect | axis | avoidable? | why |
| --- | --- | --- | --- |
| Windows verbatim `\\?\` reaching four consumers, one of them a glob matcher | read | **yes** | a typed canonical path makes it a compile error; the fix landed by routing to the one canonicalizer already shipped |
| Realpath-shim roots and walked components in different 8.3 spellings | read | **partly** | unavoidable given an ambient short `TEMP`; avoidable by owning `TEMP` and `TMP`, which is the recorded root cause |
| Roots that do not exist yet keeping only their as-built spelling | read | **partly** | same class as above, and the fix was again to use the existing canonicalizer |
| Ancestor repair carried while inert | — | **yes** | the deciding arm was never run, and the disable seam already existed |
| Preload cannot reach Node's ESM resolver | read | **no** | Node binds the function before any `--import` preload evaluates, and the one channel that would reach it needs the resolution the jail breaks |
| Node's realpath walk opening ancestors as targets | read | **no** | a Node defect since the v0.x era, reproduced against IIS long before AppContainer existed |
| The `EACCES` versus `ENOENT` compatibility cost | read | **no** | Landlock has no stat right to withhold, so it cannot present `ENOENT` |
| Distro-shaped TLS layout under an enumerated `/etc` | read | **no** | the corpus that measured the set ran on one distribution family; the correction is the cost of enumerating rather than granting wholesale |
| The `posix_spawnp` PATH-search abort on an ungranted symlinked entry | read | **no** | libuv treats `EPERM` as fatal to the whole search; the fix is to canonicalize the child's PATH |
| Piped `child_process` stdio hanging | — | **no** | global NPFS is closed to a LowBox token and libuv spells only that namespace; no filesystem rule reaches it |
| CRLF and backslash defects in the corpus harness | — | **yes** | ordinary harness bugs; the structural fix is stamping provenance on every artifact |
| A backend synthesizing a deny out of an `Allow` | write | **yes** | the pure-allowlist invariant was asserted on the IR and not at each rendering; the argument that no such deny could ever cap anything was available before the measurement |
| A confined process unable to resolve its own cwd | — | **no** | Seatbelt gates `getcwd` on the cwd's own directory node, and nothing in a path-granting model predicts that the *current directory* is itself a read subject |
| node-gyp writing to its store-entry root | write | **no** | gyp's path arithmetic makes `build/` absorb one `..`; npm and pnpm compute the same escaping path and only confinement turns it into a failure |

**Read as a whole: five of fourteen were avoidable, three of those five by one change (a typed canonical path plus owning the temp directory), and the unavoidable ones are Node, gyp and kernel behaviors that no architecture on offer removes.**

**The two new unavoidable rows both argue the same way, and it is the strongest available defense of the shape.** In each case the correct path is one npm and pnpm compute identically, and only confinement turns into a failure — which means the defect is the price of being the only tool in the class that enforces anything, not evidence that the enforcement is modelled wrong.

---

## What could not be verified

- **The drive-letter candidate is unverified in both directions.** Whether a device link created by the launcher resolves inside a LowBox token's object namespace was not measured, and Windows AppContainer launches cannot run over SSH.
- **The scratch-tree recommendation has no compatibility measurement.** The read-ladder study established that packages need the consumer's `node_modules`, not that a scratch copy carrying the resolved dependency closure would satisfy them.
- **Ringfence**, reported as a package-manager-native wrapper that shims npm, pnpm, yarn and bun into a bubblewrap sandbox with the home directory replaced by a tmpfs, could not be located as a primary source. The same architecture is verified in `build-wrap` and `sandbox-runtime`, which is what the generous-read section rests on.
- **BuildXL's short-name and temp handling were read from source, not run.** Nothing here was reproduced against a BuildXL build.
- **Landlock's ABI ceiling moved during this survey.** The sibling Linux ledger records ABI 7 as current; the kernel documentation now describes ABI 10 with `LANDLOCK_ADD_RULE_QUIET`. No deny primitive appears at any of them, so no verdict changes, but the ceiling in that document is stale.

## The first catalog measured from the corpus — 133 packages

Collated from 2,443 records across macOS, Linux and Windows. It is the first end-to-end proof that the
pipeline described above produces a usable artifact rather than a well-formed empty one.

| | count |
| --- | --- |
| packages | 133 |
| carrying at least one capability | 132 |
| needing egress | 105 |
| needing `write: "disk"` | 2 |
| version-banded | 30 |
| carrying `writePaths` | 43 |
| carrying an OS overlay | 0 |

Two numbers are worth reading carefully rather than skimming.

**Egress dominates — 105 of 133.** That is not a finding about npm so much as about which packages have
install scripts at all: the population is overwhelmingly native addons and binary downloaders, and
fetching a prebuilt is what their scripts DO. It also means the network axis, not the filesystem axis,
is where this catalog earns its keep.

**Zero OS overlays, and that is a measurement artifact, not a result.** An overlay is only written when
platforms genuinely disagree, and the Windows corpus is still small and entirely pre-fix. Do not read
"packages behave identically across operating systems" out of this table; the honest reading is that
the cross-platform comparison has barely begun.

**Thirty version bands, all of the same shape: latest needs LESS than an older release.** `@sentry/cli`,
`bcrypt` and `better-sqlite3` each need egress only below their current version, because their modern
releases resolve a prebuilt without reaching the network. This is exactly what the band rule is built
to produce — a band exists only when an older version needs MORE than latest — and it is what makes a
`default` generated from `latest` safe rather than lossy.

## A gate is only as good as its own failure controls

The corpus gate exists to catch output that parses and carries nothing. Running it against the real
2,443-record corpus — rather than against a fixture — found three defects **in the gate itself**, and
the third is the one worth generalizing.

1. **An unknown flag was silently ignored.** `verify-corpus.mjs --catalog <file>` printed
   `no records yet` and exited 0. A misspelled `--record` would leave the records path at its default,
   so the gate would verify a directory the caller never meant and pass. Unknown flags now exit 2.
2. **A capability living only in a version band was invisible.** A catalog entry is
   `{default, versions}` where `versions` is a MAP of bands; the predicate walked the entry's values,
   so `versions.network` was `undefined`. Nine packages whose egress is band-only were reported as
   having lost their egress — and band-only is, per the section above, the single commonest shape in
   the catalog.
3. **A missing grant was treated as a defect even when it was recoverable.** Every pre-fix record
   lacks a serialized `grant` but carries a state label, and collation reconstructs those exactly
   (261 of 261 on the macOS corpus, none lost). The gate failed on data that was completely fine.

All three were false ALARMS rather than false passes, which is the safe direction. But the reason to
fix them promptly is that **a gate which cries wolf gets ignored, and an ignored gate is indistinguishable
from an absent one.** The rule this leaves behind: a verification tool must be exercised against real
corpus data and against deliberately broken data, because a gate tested only on healthy fixtures
demonstrates that it can say yes.

## ⛔ "Zero packages broken by the jail" is not yet an earned claim

The corpus currently holds 2,523 records and **zero** carrying `BROKEN-EVEN-WITH-EVERYTHING`, the
verdict that means the jail is implicated. That number is not evidence yet, and the reason is worth
stating precisely, because it is the single easiest way for this project to fool itself.

`BROKEN-EVEN-WITH-EVERYTHING` is reached only by RULING OUT the jail-off cell — a package must fail
jailed, succeed unjailed, and succeed under the reference package managers. The jail-off cell was
**inert** until the harness fix that made it write `install.buildJail: false` instead of a
`dependenciesMeta` key nub had already deleted. An inert off-switch does not error; it makes the cell
run *with the jail on*, which makes it AGREE with the control, which routes the package to
`BROKEN-WITHOUT-JAIL-TOO` instead.

**So a zero here is the exact signature of the bug, and indistinguishable from a working jail** unless
you check which harness measured each record.

Splitting the corpus on the recorded harness revision:

| | records | are the jail-off verdicts trustworthy? |
| --- | --- | --- |
| measured by the current harness | 80 — all `MINIMUM`, no failures of any kind | **yes**, the jail-off self-check proves the cell ran unjailed or records `HARNESS-ERROR` |
| measured by an older harness | 2,443, including 123 `BROKEN-WITHOUT-JAIL-TOO` | **no** |

The consequence is concrete: **each of those 123 may be a misclassified real jail defect.** They are
not evidence of the jail's innocence; they are the bucket a jail defect would have been swept into.
They need re-measuring under the current harness before any breakage claim rests on them, which is
what verdict-scoped invalidation exists for — the failure verdicts are dropped and re-run while the
measured minima, which no jail-off change can affect, are kept.

What can honestly be said today: **in 80 packages measured with a provably-real jail-off control, the
jail broke nothing.** That is a good early signal and it is not the release number.

## An identifier is only as stable as the thing it is computed from

Every corpus record carries `provenance.harnessSha256`, a hash of the harness source, so a later
reader can tell which methodology produced it. That identifier turned out not to identify the
harness. It identified the harness **as checked out** — and Git rewrites line endings on Windows.

Identical committed source hashed `0de2d34e463a748a` on the Linux and macOS runners and
`6fa2b6be501b4b5d` on Windows. Re-hashing the committed bytes with `\n` → `\r\n` reproduces the
Windows value exactly, which is what makes this a measurement rather than a theory; `git log` confirms
the two hashed files have never been modified.

Two consequences, both silent:

- **Staleness purging inverts.** The tool that discards records "measured under an older harness"
  would, run on Windows, discard every Linux and macOS record — and vice versa. The whole corpus,
  every time, reported as routine cleanup.
- **Cross-platform grouping breaks.** The determination of which records have a trustworthy jail-off
  control is keyed on one sha. It would have excluded every Windows record as measured by an unknown
  harness, which is exactly backwards: those records were fine.

The fix is `.gitattributes` with `eol=lf`, so the working-tree bytes are identical everywhere and the
hash means what it claims. Verified against a local clone with `core.autocrlf=true` — the Windows
default — which preserves LF and reproduces the shared value, so the fix is confirmed by mechanism
rather than by waiting for a runner to agree.

**The general rule this leaves: before an identifier is used to group or discard measurements, check
that everything it is computed from is stable across the environments that will compute it.** A hash
over source is only stable if the source bytes are; a hash over a built binary is never comparable
across platforms at all, which is why "was this measured before fix X?" needs a commit, not a
checksum.

## The measurement layer was never the problem

Worth stating plainly, because the debugging effort has consistently pointed the wrong way. Across
every defect found while bringing the corpus up, the part that decides a package's minimum
capability — the ascending state walk, the double control, the oracle comparison — has been correct
throughout. What broke, repeatedly, was the plumbing around it:

| defect | layer |
| --- | --- |
| a grant computed but never serialised | record writing |
| `.store` bookkeeping dirs read as package names | tree discovery |
| a v2 override never reaching the egress table | catalog loading |
| the jail-off control writing a deleted config key | fixture construction |
| `timeout` assumed present on every host | process supervision |
| records written where the runner never read them | output paths |

Every one produced output that parsed, validated and reported success. None was caught by a test,
because the tests asserted the hand-maintained compiled-in table rather than the pipeline's own
output. **The lesson is not "measure more carefully" — it is that a pipeline needs a gate asserting
its own artifact carries what it measured, at every seam where one stage hands off to the next.**

The last row is the sharpest case. `search.mjs` defaults its output to its own directory; the CI
runner collects and commits from `records/` at the repo root; nothing connected the two. The measure
step reported `attempted 3, recorded 3, FAILED 0` while the collector reported `collected 0 verdict(s)
from 0 record file(s)`. Both were telling the truth about different directories.

It also demonstrates why one fix is rarely the fix. This sat directly behind a missing `timeout`
binary, which produced the identical symptom — a green slice that measured nothing. Repairing the
first changed nothing observable, and only an end-to-end probe that ran every stage in sequence and
checked the artifact between them separated the two.

## What the corpus actually costs to run

Measured over a 39-minute window with the fleet in steady state, from the QUEUE DELTA rather than
wall-clock timing — the measuring host is contended and absolute times there mean nothing:

| platform | concurrent chains | rate | rows left | time to drain |
|---|---|---|---|---|
| macOS | 3 | 3.54 pkg/min | 1,719 | ~8 h |
| Linux | 8 | 3.03 pkg/min | 1,496 | ~8 h |
| **Windows** | 10 | 1.10 pkg/min | 2,000 | **~30 h** |

**The whole 6,750-row corpus drains in about thirty hours, and Windows is what bounds it.** Per chain
Windows now costs 9.1 min/package against a previously measured 13.2, so the walk-depth reduction
described below does reach wall clock — just not by as much as the cell count alone would suggest.

Two things had to be true before any of this was possible, and both were invisible defects rather than
tuning:

- **A claim has to be published before measuring starts.** While it lived only in the claiming
  runner's working tree, a second runner on the same OS read the same pending rows and measured the
  same packages, so parallelism bought nothing at all. Measured on a local rig: three concurrent
  runners claiming four rows each from a ten-row queue took the IDENTICAL four.
- **Publishing must never stage a deletion.** `git add <dir>` is `git add -A <dir>`, so a runner that
  keeps its own `records/` rather than resetting to origin stages every record another runner pushed
  since its checkout as a DELETE. That was dormant while runs were serialised — there was no other
  runner's record to delete — and became live data loss the moment they were not.

## Half of all install-script packages need no catalog entry at all

The jail's rule is that no catalog entry means no capability, so the compatibility risk for any
package the corpus has not reached is exactly: how often does an install script need MORE than the
base profile? Counted across every `MINIMUM` record measured so far:

| platform | records | needed nothing beyond base | needed egress | needed a `disk` grant |
|---|---|---|---|---|
| linux-x64 | 194 | 104 (**54%**) | 88 (45%) | 0 |
| darwin-arm64 | 78 | 35 (45%) | 39 (50%) | 1 |
| win32-x64 | 34 | 14 (41%) | 19 | 9 |
| **all** | **306** | **153 (50%)** | | |

**Half of them run correctly with no entry whatsoever.** That is the base profile doing its job: a
materialized package's own directory is already writable, so a script that unpacks or generates files
beside itself needs nothing granted. It also means an unmeasured package is not automatically a broken
one — the corpus buys coverage of the other half.

**Egress is the dominant need, and disk is nearly absent.** On Linux 88 packages need the network and
ZERO need a disk grant. That shapes what the catalog is mainly for: it is a record of which packages
may legitimately reach the network, which is also the capability that matters most for a supply-chain
attack. The disk column is where the platforms diverge, and Windows' 9 are an artifact of the harness
defect described in the next section rather than a property of those packages.

⛔ **Read the denominator before quoting these.** Every package here HAS an install script and was
selected as most-downloaded — that is the population the jail has to get right, not npm at large. And
306 measured versions is a partial corpus, with Windows barely sampled at 34. The shape is stable
across two platforms with very different totals, which is why it is worth recording now, but the
figures will move.

## Why Windows measured 13x slower per package — it was walking 4.7x deeper

The ascending-cost walk stops at the first passing state, so the number of cells a record carries is
how much work that package actually cost: each cell is a full install. Counted across every `MINIMUM`
record in the corpus at the time (failures excluded, since a failure always walks the whole ladder):

| platform | records | mean cells walked | grants containing `disk` |
|---|---|---|---|
| linux-x64 | 194 | 4.00 | 0 (0%) |
| darwin-arm64 | 78 | 5.67 | 1 (1%) |
| **win32-x64** | **34** | **18.79** | **9 (26%)** |

The histogram is where it shows: Linux effectively never exceeds 6 cells, while Windows had **8 of 34
records at the full 55-cell walk**. Windows I/O is a few times slower than Linux on its own, but 4.7x
the number of installs is what turns that into the measured 13.2 min/package against Linux's ~1.0-1.8.

**One mechanism produced both the over-granting and the slowness, and it was in the HARNESS.** The
path tokeniser hardcoded the POSIX cache root, so on Windows it emitted zero `$home/` and `$store/`
tokens. `writePaths` are derived from paths that land in the throwaway `$HOME`, so with no such tokens
none could ever be derived — and with no narrow write available, nothing below `write.disk` could
satisfy the package. The walk then had to climb the entire ladder to get there.

The signature is visible per record. `dprint@0.25.1` on Windows: `pathsLandingInThrowawayHome: 0`,
`writePaths: []`, 53 cells walked, final grant `{"write": "disk"}`.

**MEASURED AFTER THE FIX, and the correction matters more than the confirmation.** Splitting Windows
records by harness version:

| metric | pre-fix (n=40) | post-fix (n=17) |
|---|---|---|
| mean cells walked | 16.48 | **9.18** |
| grants containing `disk` | 23% | **12%** |
| records with a derived `writePaths` | 0 | **0** |

Walk depth nearly halved and the `disk` rate halved, so the fix is real. But the MECHANISM originally
claimed for it is refuted: the expectation was that correct tokenisation would reveal writes landing
in the throwaway `$HOME` and let narrow `writePaths` be derived. It did not, because there were no
such writes to reveal. The raw blocked path is `home/AppData/Local/nub/pm/primer/.auto_prune` —
nub's own primer cache, which is neither the store nor the jail home, so no tokeniser rule matches it
and `pathsLandingInThrowawayHome: 0` is the correct answer.

What the fix actually did was tokenise the STORE correctly on Windows, where the cache lives under
`%LOCALAPPDATA%` rather than `~/.cache`. That alone accounts for the improvement.

**The primer-cache denial is uniform, not a Windows defect.** Across platforms it appears in roughly
3% of records — 12 of 303 on macOS, 13 of 470 on Linux, 5 of 77 on Windows. A dependency script being
denied write access to nub's own cache directory is correct: writing there would be a persistence
vector.

⛔ **Windows remains above Linux (9.18 against 4.00), and this fix does not explain the remainder.**
That residual needs its own hypothesis rather than an extension of this one.

⛔ **The 34 records in the table above predate the fix, so that table describes the defect, not
current behaviour.** The prediction it makes is sharp and cheap to falsify: post-fix Windows records
should show nonzero `pathsLandingInThrowawayHome`, at least some derived `writePaths`, a mean cell
count far below 18.79, and a correspondingly lower `disk` rate. If the cell count stays high once the
tokens are present, the tokeniser was not the whole cause and the throughput question needs a fresh
hypothesis. (An earlier note put the Windows `disk` rate at 41%; this sample of 34 gives 26%. Different
samples, and the 34 are the ones with complete records — neither figure is a measurement of the
post-fix world.)

## The two Windows home axes behave differently, and the asymmetry is load-bearing

`%APPDATA%` is redirected into the jail's private home. `%LOCALAPPDATA%` is deliberately not. That
looks like an oversight and is not, so it is written down here — reading it as a bug cost one wrong
root-cause diagnosis.

| | redirected into the private home? | why |
|---|---|---|
| `APPDATA` | **yes** | npm on Windows resolves its cache to `%APPDATA%\npm-cache`, not `$HOME/.npm`. Redirecting only `HOME`/`USERPROFILE` fixes the POSIX spelling and leaves the Windows one addressing the real user profile, outside the writable set — so every script shelling out to npm or `prebuild-install` took `EPERM: mkdir …\AppData\Roaming\npm-cache`. That was 14 of 63 Windows corpus breaks, the largest single mechanism. Nothing in the launch path reads it, so repointing it is safe. |
| `LOCALAPPDATA` | **no** | the Windows LowBox launch resolves its AppContainer profile directory from it. Pointing it elsewhere breaks PROCESS CREATION rather than a cache path. |

**The consequence people get backwards.** Not redirecting `LOCALAPPDATA` does not leave it pointing at
the real user profile from the child's point of view. Measured on windows-latest: the OS itself
redirects that known folder for a LowBox token, so the child's `%LOCALAPPDATA%` is the per-container
`…\AppData\Local\Packages\<profile>\AC` and the real one is denied outright. `unique_profile_name`
keys that profile on pid plus a nonce and `ProfileGuard::drop` deletes it, so the redirect target is
fresh, empty, and destroyed per LAUNCH.

⛔ **So a `%LOCALAPPDATA%` write SUCCEEDS, and this cannot explain a wide grant.** Windows tooling that
caches there never hard-fails; it gets zero reuse, ever. The gap is a compat gap of a different shape —
no cache reuse — not a reachability one. A write that succeeds cannot drive the ascending-cost walk to
escalate, so any theory that blames `write.disk` on `%LOCALAPPDATA%` is answering the wrong question.
The `USERPROFILE` jail home IS persistent, which is why the two axes do not behave alike.

## The measuring harness manufactured the Windows result it then measured

The section above is correct about PRODUCTION and was read as covering the harness too. It does not,
and the gap accounts for most of the Windows grants the corpus produced.

Both statements are true at once, which is why this took so long to see:

| where | `%LOCALAPPDATA%` | what happens |
| --- | --- | --- |
| a user's machine | the real `C:\Users\<user>\AppData\Local` | Windows redirects the known folder for the LowBox token into a per-launch container profile. The write SUCCEEDS. |
| the measuring harness | redirected into the throwaway `$HOME` | the child resolves its profile somewhere the parent never created it. Every rung EPERMs. |

`OS_ESSENTIAL_ENV` carries `LOCALAPPDATA` because the enforcing path resolves the per-container
profile dir (`%LOCALAPPDATA%\Packages\…`) **from the environment** — without it `CreateProcessW`
fails 203. So whatever the harness puts there is what the CHILD uses to find its own profile. But
`CreateAppContainerProfile` is called by nub, the *unsandboxed parent*, which lands the real
correctly-ACLed profile in the parent's own known-folder location. Redirect the variable and those
two disagree:

```
EPERM: operation not permitted, mkdir '<throwaway>\home\AppData\Local\Packages'
```

Nothing below a grant that hands the container write access to the throwaway home can pass, so the
walk escalates for a reason that has nothing to do with the package.

**The tell was a record shape that no other theory explained.** `impit@0.14.0` and
`postcss-rtlcss@5.1.1` both grant `write.userHome` while reporting **zero** blocked paths — cells
0–29 all fail at 6,485 files, cell 30 passes at 7,055. Nothing is *blocked* because no file
*differs*; the run cannot START until the container can create its profile directory, and
`write.userHome` is simply the narrowest rung that allows it. A blocked-path count of zero beside a
non-null grant is the signature of a launch failure being scored as a capability need.

Measured across the retained artifacts, the `AppData\Local\Packages` signature appears in 110 of one
artifact's cell logs and 54 of another's — broader than the `C:\npm\prefix` cause, and in one
artifact the only one present.

### The obvious fix was tried on a real runner and REFUTED

Stop redirecting `LOCALAPPDATA`, keep `USERPROFILE` moving, and redirect nub's and npm's caches with
their own documented knobs (`NUB_CACHE_DIR`, `npm_config_cache`) instead. It is clean, it uses
supported surfaces, and it does not work.

**`NUB_CACHE_DIR` does not relocate the store.** It is the PM cache knob, not the store /
virtual-store root, and nub resolves the store through `%LOCALAPPDATA%` on Windows. The fixture
canary caught it on the first real run and refused the batch:

```
puppeteer@25.4.0 control: 113 files (expected >5000)
EXISTS  C:\Users\runneradmin\AppData\Local\nub  (3924 files, depth<=6)
EXISTS  C:\Users\runneradmin\.cache\nub         (3 files, depth<=6)
```

3,924 files in the runner's real profile against 3 in the knob's directory. The store left the
fixture — so cells would have shared state and every package would have measured as needing nothing,
which is the regression the redirect exists to prevent. Nothing was measured and no bad data reached
the corpus.

⛔ **The instrument caught the change to the instrument.** The canary asserts a POSITIVE — a known
package must produce >5,000 files — rather than checking for an error, which is why it fired on a
change that produced no error at all. An isolation mechanism cannot be validated by "did anything
break"; it has to be validated by "did the bytes land where I think they land".

**Two ways forward that do not disturb the store**, neither yet tested: pre-create
`<throwaway>/home/AppData/Local/Packages` in the fixture so the parent's `CreateAppContainerProfile`
populates it with correct ACLs instead of the child hitting a `Packages` it cannot create; or fix it
in nub, letting the container always write its own per-launch profile directory regardless of the
catalog rung. The second is the better shape — the profile is the container's own ephemeral scratch,
deleted on drop, so granting it is not a meaningful widening.

⛔ **The general lesson, and it is the expensive one.** An isolation mechanism is part of the
instrument, and an instrument that perturbs the thing it measures produces confident, precise,
wrong numbers. The corpus reported those grants as `MINIMUM` — measured, reproducible, controlled —
and every one of them was minimal *for a condition that cannot occur on a user's machine*. The
control that catches this is not a better walk; it is asking, for each environment variable the
harness overrides, whether the SUT resolves anything through it.

## What the catalog actually grants today — 20% of it is whole-disk

The first measurement of the DELIVERABLE rather than the instrument, collated from ~2,500 corpus
records:

| | count |
| --- | --- |
| packages | 147 |
| version bands | 33 |
| network only | 52 |
| narrow filesystem | 84 (61 carrying `writePaths`) |
| `read: "disk"` | 1 |
| **`write: "disk"`** | **30** |

⛔ **Thirty of 147 packages — 20% — carry a whole-disk write.** For those the jail blocks essentially
nothing, and on Windows a `write:"disk"` grant declines the LowBox token outright, so they run with no
filesystem and no network confinement at all. This is the number that decides whether "blocks
Shai-Hulud-style attacks" is a true statement about the shipped default, and it is the metric to track.

Attributing the tail by which platform actually MEASURED disk:

| bucket | n | meaning |
| --- | --- | --- |
| disk only on **Windows** | **17** (57%) | recoverable — the AppContainer profile defect below |
| disk on a **POSIX** platform too | 13 | investigated individually; mostly genuine |
| no record carries disk | **0** | the version/platform union does not invent disk packages |

That last row matters for prioritisation: the band union widens which VERSIONS inherit a grant, never
the package SET, so it cannot be blamed for the size of the tail.

**The POSIX 13 were individually checked and mostly hold up.** `@tensorflow/tfjs-backend-wasm` runs a
full `yarn install` from its postinstall and still fails at `write.userHome + network` (4,895 files
against a 40,972-file control); `sharp` fetches prebuilds; `azure-streamanalytics-cicd` writes 398
files into its own `bin/`. Three hypotheses that would have narrowed them — a scan gap, a missing
`.git/hooks` scope, and unfired `writePaths` promotion — were each refuted by measurement. `writePaths`
in particular is ALREADY populated for `detox`, `sharp` and `tfjs`, and they need disk anyway.

⛔ **So the honest read is that ~13/147 (9%) is close to the floor for the current capability
vocabulary**, and closing the remaining gap needs a MODEL change — a scope between `userHome` and
`disk` — rather than more measurement. Two packages remain unexplained and are worth a log read before
any such design: `husky` (needs disk where four sibling hook installers pass at `write.project`) and
`detox` (identical file count to its control at the no-grant floor, differing only in digest).

## Windows: the child cannot create its own AppContainer profile directory

The mechanism behind the 17 Windows-only whole-disk grants, confirmed from a cell log rather than
inferred.

`OS_ESSENTIAL_ENV` carries `LOCALAPPDATA` because the enforcing path resolves the per-container
profile dir from the CHILD's environment. But `CreateAppContainerProfile` runs in nub — unsandboxed —
and Windows places the real profile via the PARENT's known-folder location. Where those two disagree,
the child looks somewhere nothing was created and has no ACE to create it:

```
npm error syscall mkdir
npm error path ...\home\AppData\Local\Packages\nub_sbx_4412_18c8788d58963f90_0
```

⛔ **The path ends in the PROFILE NAME, not `Packages`** — and that name is `unique_profile_name()`,
generated per launch. Pre-creating `Packages` externally therefore cannot help, which was confirmed
the expensive way: a corpus run with exactly that change produced grants byte-identical to baseline.
Only nub knows the name, and only nub knows the value it handed the child, so the fix has to live at
the launch site: ensure `<child %LOCALAPPDATA%>\Packages\<name>` exists and carries the container's
ACE before spawning. One per-launch directory, removed on drop.

**This is a product defect, not only a measurement artifact.** Any environment where the child's
`%LOCALAPPDATA%` differs from the parent's known folder — redirected folders, enterprise profiles,
anything that sets the variable explicitly — reproduces it. The harness redirects it on every run,
which is why it showed up as a systematic 17-package over-grant rather than a rare bug report.

⛔ **Three fixes were attempted; the first two were wrong, and both were proposed without reading the
failing cell's log.** Removing the redirect put 3,924 store files in the runner's real profile (the
fixture canary caught it and refused the batch). Pre-creating `Packages` was one path segment short.
The log named the exact path in a single line. **Read the failing cell's log before proposing a fix**
— the CI artifact carries per-cell logs whenever the batch actually ran.

## The residual is ONE SHAPE on both Windows and Linux — packages that pass only at `write:"disk"`

Measured 2026-08-04 across every Windows record on a binary carrying the day's fixes. The tail is not a
collection of missing paths, and **no amount of path-granting addresses it** — which retires an entire
class of hypotheses.

**Each of the six confirmed residuals fails at the WIDEST state the capability model can express and
passes at exactly one state: `write:"disk"`.**

| package | non-disk cells | widest tried (`write.deps + write.project + write.userHome + read.disk`) | passes at |
| --- | --- | --- | --- |
| `bs-platform@9.0.2` | 51, all failed | FAIL | `write.disk` |
| `cz-customizable@2.6.0` | 51, all failed | FAIL | `write.disk` |
| `@sap/hana-client@2.29.23` | 51, all failed | FAIL | `write.disk` |
| `@larksuite/cli@1.0.9` | — | FAIL (+ network) | `write.disk + network` |
| `jpegtran-bin@5.0.2` | — | FAIL (+ network) | `write.disk + network` |
| `@posthog/cli@0.7.34` | — | FAIL (+ network) | `write.disk + network` |

⛔ **THE PATTERN IS MEASURED; ITS CAUSE IS NOT THE APPCONTAINER TOKEN.** This section first attributed
the tail to Windows declining the LowBox token at `write:"disk"`, which is true of the backend but is
**refuted as the explanation** by the same query run on Linux: **8 of 8 Linux residuals show the
identical signature** — ~51 non-disk cells, every one failing, the widest expressible state failing,
passing only at `write:"disk"`. Linux's `write:"disk"` declines no token at all; it only widens Landlock
to one `/` rule, leaving the seccomp socket ceiling, `setsid`, the descriptor sweep and the capability
drop untouched (`linux.rs:234`). A cause present on both platforms cannot be a Windows-only mechanism,
so whatever these packages need, **the token is not it**.

Two further hypotheses were tested and both fail. The write-scope vocabulary is **not** missing a
region: across all 8 Linux residuals, **266 deciding paths sit inside `deps`/`project`/`home` and zero
sit outside**. And the grants are **not** inert — 43 packages win at `write.userHome`, 5 of them on a
path under `$home/`, so the scope demonstrably works where it is needed.

⛔⛔ **AND THE DISPOSITION IS WRONG FOR MOST OF THE TAIL.** Taking the whole Windows population at once — every `write:"disk"` record on a post-fix binary — and asking what each package needs on the OTHER platforms settles it:

```
win32 at write:"disk" on a fixed binary: 25
  NARROW on another platform : 20   ← WINDOWS-SPECIFIC ESCALATION
  disk on every platform     :  2   ← genuinely needs disk
  no cross-platform record   :  3
```

**11 of the 20 need NOTHING AT ALL on macOS and Linux** — `bs-platform`, `cz-customizable`, `@larksuite/cli`, `@posthog/cli`, `jpegtran-bin`, `electron-chromedriver`, `purescript`, `dprint`, and all five `@ffprobe-installer/*`. A package that runs with zero grants on two platforms and demands the entire disk on the third is not a package that needs the disk. A second group escalates from a narrow grant rather than from nothing — `husky`, `lefthook` ×3, `shared-git-hooks` and `mathlive` all sit at `write.project` on POSIX, and `lefthook@1.13.6` carries the *same nub commit* on win32 and linux, so the binary is controlled. Only `@opencode-ai/cli` and `dotnet-2.0.0` genuinely need disk everywhere.

So "over-grant, not a fix" holds for **2** packages, not for the tail. For the other 20 the Windows backend is escalating a grant that suffices elsewhere, and that is a defect to find. Mechanism currently **UNKNOWN** and deliberately not guessed here — four mechanisms proposed for this tail have already been refuted, and the cheap next step is reading the widest failing cell's log, which every local harness run writes by default.

**The positive control matters more than the finding, because without it the table is an artifact.** If
every Windows package were disk-or-nothing, "these six are special" would only mean the ladder cannot
find intermediate states on Windows. It can. Across 570 Windows `MINIMUM` records at the time:

```
288  null (needs nothing)   — runs inside the AppContainer with ZERO grants
159  network only
 84  write:disk             — the residual, most of it stale pre-fix records
 25  write:userHome    ┐
  8  write:deps        │  intermediate filesystem grants WORK
  4  write:project     │
  2  read:project      ┘
```

486 of 570 (85%) run correctly inside the AppContainer and 39 more win at an intermediate grant. The
machinery works; the six are genuinely exceptional.

**RE-COUNTED 2026-08-06 on a corpus 2.6× larger. ⛔ The Windows ratio did NOT merely hold — it IMPROVED, and an earlier revision of this paragraph got that wrong by comparing two different categories.** The `486 of 570 (85%)` above counts everything EXCEPT `write:"disk"` — null, network-only AND the intermediate filesystem grants. The comparable figure today is **1,370 of 1,466 (93.5%)**. Counting only null + network-only, the same two snapshots are 447/570 (78.4%) → 1,245/1,466 (84.9%). Either category shows a rise, not a plateau.

**Sampled across the corpus's own history, the three platforms behave DIFFERENTLY, and that is the finding:**

| snapshot | win32 | linux | darwin |
| --- | --- | --- | --- |
| n = 167 / 652 / 418 | 70.7% | 95.2% | 93.1% |
| n = 313 / 1347 / 940 | 69.0% | 95.2% | 92.1% |
| n = 731 / 1767 / 1316 | 79.9% | 95.4% | 93.0% |
| n = 1466 / 1767 / 1672 (today) | **84.9%** | **95.4%** | **92.5%** |

(null + network-only, as a share of that platform's `MINIMUM` records.)

⇒ **Linux and macOS are genuinely stable** — 95.2→95.4% across a 2.7× growth, 93.1→92.5% across 4×. A proportion that survives that is a property of the package population rather than of which packages were measured first, so those two distributions can be quoted with confidence.

⇒ **Windows is NOT stable; it climbs monotonically from 70.7% to 84.9%.** The most likely reading is that early win32 records were depressed by product and harness defects that were subsequently fixed — the window-station ACE, the busybox script-shell refusal, the command tokeniser — each of which turned failing cells into passing ones. ⛔ **That is INFERRED from the timing, not measured**, and the alternative (later slices happening to contain easier packages) is not excluded here. What IS measured is that the win32 number is still moving while the POSIX ones have settled, which means **the win32 distribution is the least settled of the three and should carry the loosest confidence.**

**All three platforms, counted the same way** (`verdict == "MINIMUM"` only — `grant: null` on a
`HARNESS-TIMEOUT` means *nothing was measured*, not *nothing is needed*, and conflating them is the
misread §3 of the criteria document warns about):

| | darwin-arm64 | linux-x64 | win32-x64 |
| --- | --- | --- | --- |
| `MINIMUM` records | 1,672 | 1,767 | 1,466 |
| **needs NOTHING** | **824** | **917** | **741** |
| `network` only | 722 | 768 | 504 |
| needs nothing **or** only network | **92.5%** | **95.4%** | **84.9%** |
| `read:"disk"` | **21** | 1 | 0 |
| `write:"disk"` | **0** | 8 | 96 |

⇒ **Needing nothing at all is the MODAL outcome on every platform.** That is the fact a default-on
build jail rests on, and it needs no discriminator, no inference and no cohort filter — it is read
straight off the records.

⛔ **macOS does not simply win, it TRADES.** Zero packages need whole-disk write; twenty-one need
whole-disk read. That is a far weaker grant — `read:"disk"` front-inserts the enumerated complement
of the secret set and stays read-only, so credentials remain withheld — but "macOS is clean" is an
over-simplification and the trade is the honest statement.

⛔ **Windows' 166 `HARNESS-TIMEOUT` records (7.4%, against 0.4% on darwin and 0.7% on linux) are a
CONFOUND, not a footnote.** An incomplete run is exactly what walks a blind ladder to its last rung,
so the win32 `write:"disk"` excess is partly the AppContainer token declining and partly a harsher
measurement environment. Nothing in these counts separates the two, so do not attribute the whole
win32/POSIX gap to the token.

**One member is independently confirmed at the mechanism level.** `cz-customizable` fails
`EPERM: operation not permitted, symlink` in 51 of 54 cell logs, and the three EPERM-free logs are
*exactly* the three passing cells — present in 100% of failing rungs, absent in 100% of passing ones.
Windows symlink creation needs `SeCreateSymbolicLinkPrivilege`, which a LowBox token structurally cannot
hold; `write:"disk"` succeeds only because the token is declined and the process inherits the caller's.

**Supporting, and it rules out the packages themselves:** all six are `narrow` or `null` on BOTH POSIX
platforms — same package, same install script, same version. `bs-platform` and `cz-customizable` need
*nothing* on POSIX and lose all filesystem confinement on Windows.

⛔ **STRONGLY SUPPORTED, NOT PROVEN.** "Fails at the widest non-disk state" is also consistent with "needs
a path outside every expressible scope", and records alone cannot separate those. What lifts it is the one
confirmed member sharing the identical ladder signature. A local reproduction on a real Windows box closes
it.

⛔ **Four mechanisms were proposed for this tail and REFUTED — do not re-try them.** A sidecar DLL breaking
the spawn (libuv maps `ERROR_MOD_NOT_FOUND` to `ENOENT`, so a DLL failure can never print `UNKNOWN`); a
silently-skipped read-grant ACE (the fail-closed arm produced **zero** ACE aborts across 61 and 56 logs
while the errors persisted); a missing `writePaths` entry (it is *derived* from the home writes, so the
disconfirming population is empty and the hypothesis was untestable as posed); and a "temp-staging" source
classification that held for one of four packages. Each was inferred from STRUCTURE and reported before it
was tested. **Read the generator — the cell logs, libuv's errno table, the harness's own field definitions
— not the shape of the code.**

## Changelog

- 2026-08-04 — Recorded that the entire Windows residual is ONE boundary: all six confirmed residuals fail at the widest expressible grant and pass only at `write:"disk"`, the rung that declines the AppContainer token. Included the positive control (486 of 570 Windows packages run fine inside the container, 39 at intermediate grants) because without it the finding is indistinguishable from a ladder that cannot find intermediate states. One member (`cz-customizable`) is confirmed at the mechanism level via `SeCreateSymbolicLinkPrivilege`. Also recorded the four refuted mechanisms so they are not re-tried.
- 2026-08-04 — **REVERSAL:** the entry above attributed that residual to Windows declining the AppContainer token. The MEASUREMENT stands and is unchanged; the CAUSAL ATTRIBUTION is withdrawn. Running the same query on Linux found 8 of 8 Linux residuals with the identical signature, and Linux's `write:"disk"` declines no token — it only widens Landlock to `/`. Two follow-up hypotheses were also falsified: the write-scope vocabulary has no gap (266 deciding paths inside `deps`/`project`/`home`, zero outside), and the scopes are not inert (43 packages win at `write.userHome`, 5 on a `$home/` path). The disposition — over-grant, not a fix — never depended on the cause and is unaffected. `cz-customizable`'s `SeCreateSymbolicLinkPrivilege` mechanism and the positive control are also unaffected.
- 2026-08-04 — Recorded the first measurement of the deliverable: 30 of 147 catalog packages (20%) carry a whole-disk write, 17 of them Windows-only and 13 POSIX. Confirmed the Windows mechanism from a cell log — the child cannot create its own per-launch AppContainer profile directory when its `%LOCALAPPDATA%` disagrees with the parent's known folder — and noted that the first two fixes for it were wrong because neither was grounded in that log.
- 2026-08-04 — Tried the obvious fix for the above (stop redirecting `LOCALAPPDATA`, isolate the caches with `NUB_CACHE_DIR`/`npm_config_cache`) on a real Windows runner and REFUTED it: `NUB_CACHE_DIR` is the PM cache knob, not the store root, so 3,924 store files landed in the runner's real profile against 3 in the knob's directory. The fixture canary refused the batch, so nothing was measured. Reverted. The diagnosis is unchanged; the remedy is not — the two untested options are pre-creating the `Packages` dir in the fixture, or letting the container write its own per-launch profile dir in nub.
- 2026-08-04 — Recorded that the harness's own `LOCALAPPDATA` redirect manufactured the Windows failure it then measured: the child resolved its AppContainer profile inside the throwaway `$HOME` while nub created the real one elsewhere, so every rung EPERMed until a grant opened the throwaway home. Explains the zero-blocked-paths-with-a-grant records that nothing else did, and refines the section above from 2026-08-03 — which is right about production and was wrongly read as covering the harness.
- 2026-08-03 — Recorded what the corpus costs to run now that same-OS chains are parallel: ~30 h to drain all 6,750 rows, bounded by Windows at 1.10 pkg/min across 10 chains, against ~23 days for Windows alone when runs were serialised. Measured from the queue delta, not wall clock.
- 2026-08-03 — Measured the tokeniser fix on real post-fix Windows records: mean cells 16.48 -> 9.18 and disk grants 23% -> 12%, but ZERO derived `writePaths`, refuting the stated mechanism. The blocked paths are nub's own primer cache, not throwaway-`$HOME` writes; the gain came from tokenising the STORE correctly under `%LOCALAPPDATA%`. Also recorded that the primer denial is uniform across platforms (~3%) and correct, not a Windows defect.
- 2026-08-03 — Measured the compat risk the no-entry-no-capability rule actually carries: 153 of 306 measured versions (50%) need nothing beyond the base profile, so an unmeasured package is not automatically a broken one. Egress is the dominant need (88 of 194 on Linux) and disk is nearly absent (0 on Linux), which says what the catalog is mainly a record OF.
- 2026-08-03 — Quantified why Windows measured ~13x slower per package: it walked 4.7x more cells (18.79 vs Linux's 4.00), with 8 of 34 records hitting the full 55-state ladder. Same root cause as the Windows over-granting — the tokeniser emitted no `$home/` tokens, so no `writePaths` could be derived and nothing below `write.disk` could pass. Confirmed per record on `dprint@0.25.1`.
- 2026-08-03 — Wrote down why `APPDATA` is redirected into the jail home and `LOCALAPPDATA` is not. The asymmetry reads as an oversight, and a starred root-cause task had been opened against it; the code records both the reason (the LowBox launch resolves its AppContainer profile dir from `LOCALAPPDATA`, so repointing it breaks process creation) and the measurement that kills the hypothesis (the OS already redirects that folder for a LowBox token, so the write succeeds into a per-launch container dir and never EPERMs).
- 2026-08-03 — Corpus now publishes each record as it is measured rather than once per slice, and the mechanism table records why it replays onto origin instead of merging or forcing. The soft-vs-hard reset distinction is included because a hard reset silently deletes a record stranded by a failed push — found by testing the publisher against a local bare origin rather than in production.
- 2026-08-03 — Added "An identifier is only as stable as the thing it is computed from": the record
  provenance hash identified the CHECKOUT rather than the harness, because Git rewrites line endings
  on Windows, which silently inverted staleness purging and cross-platform grouping.
- 2026-08-03 — Added "The measurement layer was never the problem": six defects, all in plumbing
  rather than in the capability search, and the records-path mismatch that let a slice report success
  while committing nothing.
- 2026-08-03 — Added the first measured catalog (133 packages from 2,443 records) and the gate-defect
  section. Recorded that the zero OS-overlay count is a measurement artifact of a small pre-fix Windows
  corpus rather than a finding about cross-platform behavior, and that band-only capabilities are the
  commonest shape rather than an edge case — which is what made the gate's blindness to them consequential.
- 2026-07-31 — Scrubbed the residual host-allowlist framing. The opening question, the read/write/network axis table and the pre-granting section each still described the jail as granting a package a set of network HOSTS; all three now say what it grants, which is one per-package boolean. The brokering section claimed the jail ships the macOS loopback egress proxy — it ships nothing of the kind, and that broker belongs to `nub sandbox`, so the section now states that the jail brokers nothing.
- 2026-07-31 — Added the network-axis governance section: `networkHosts` and `packageNetwork` are decoupled, the former gating only Nub's unconfined prefetch, so the criterion for admitting a host is whether the PREFETCHER needs it rather than whether a script may safely reach it. Verified structurally — a 43-entry catalog change promoted zero hosts and left `DOWNLOAD_HOSTS` byte-identical, with a changed per-package digest as the control proving the codegen re-ran. Recorded the separate exfiltration criterion that disqualifies a host outright. Added the polarity finding: the pure-allowlist invariant was asserted on the IR and one backend synthesized a deny out of an `Allow` underneath it, which is the same measurement-gap class as the inert ancestor repair. Three rows added to the avoidable/unavoidable table.
- 2026-07-30 — Initial write-up. Surveyed BuildXL, Bazel, Chromium's Windows sandbox, Nix, Guix, Portage, Gentoo's `sandbox`, `build-wrap`, `sandbox-runtime`, LavaMoat, Node's own permission model, and the npm/pnpm install-script defaults, against the question of whether the build jail's pre-granted per-package allowlist is the right architecture. Verdict: it is, the two-layer split it converged on is Chromium's, and the avoidable share of the patch stream reduces to an untyped canonical path plus an ambient temp directory. Measured that Seatbelt's `(trace …)` directive is inert on darwin 25.5 with a positive control on the same profile shape.
