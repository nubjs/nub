# Judging the build-jail catalog

How to decide whether a grant is right, whether a record is trustworthy, and what is worth chasing. Written because the same misjudgments kept recurring: over-investigating findings that changed no grant, treating a null grant as a measurement, and unioning across platforms that genuinely disagree.

The companion documents are [`build-jail-architecture.md`](build-jail-architecture.md) for the capability model and mechanism, and the corpus repo's own harness README for how a measurement is produced.

## 1. The safety asymmetry is the master rule

**Over-granting fails to confine. Under-granting breaks the install.** These are not symmetric costs. A broken install is a user-visible product defect that makes the jail unshippable; an over-grant is a security shortfall on a package that was already executing arbitrary code on the machine. When the evidence is ambiguous, loosen.

The rule has one important bound: it justifies relaxing about a *narrow* over-grant, never about `write:"disk"`. The terminal rung discards the confinement entirely, so "when in doubt, loosen" stops applying at exactly the point where loosening costs everything.

**`write:{userHome}` is the second bound, and it is easy to miss because the scope reads as narrow.** The capability model is right that the scopes do not formally nest — [`build-jail-architecture.md`](build-jail-architecture.md) gives the container counterexample where the project sits at `/app` with `HOME=/root`. But on the machine most installs actually run on, the project *is* under the home directory, and so are the shell profile, `~/.ssh`, and `~/.npmrc`. The formal relation and the practical one diverge, and a threat model has to follow the practical one: `write:{userHome}` is the **persistence** capability, and granting it defeats the property the jail exists to hold rather than merely widening a blast radius.

So `write:{userHome}` is not a *narrow* over-grant in the sense §1 licenses. It needs a named write, measured — never a guess, never a widening applied because a path could not be resolved. Two grounds for the distinction, both measured rather than argued:

- On `cpu-features@0.0.10` (darwin), **39 of 39** `userHome` writes were Python `__pycache__`/`.pyc` from node-gyp's bundled `gyp/pylib`. The capability was an artifact of where Node happened to be installed on the measuring host — nothing the package needed, and it would have been granted to essentially every package with a native build.
- With that suppressed, the remainder came from a resolver widening to all three write scopes whenever a *relative* write could not be placed. Single-variable, same archive: widening on → `{"write":{deps,project,userHome},"network":true}`; widening off → `{"network":true}`.

Both are the same failure with different faces: **a grant that reflects the measuring apparatus rather than the package.** When a mechanism cannot place a write, the answer is to resolve it against evidence — the script's working directory is the package's own directory, and the post-run artifact manifest can confirm a resolution — and to record by name whatever genuinely cannot be placed. Recording the residual keeps a later breakage a one-line catalog fix instead of a re-measure, which is what makes declining to widen affordable.

## 2. Each rung carries a different burden of proof

| Grant | What justifies it |
| --- | --- |
| nothing | The target state. No justification needed. |
| `write:{deps}`, `write:{project}` | Measurement alone. These are ordinary and cheap. |
| `write:{userHome}` | Measurement, plus a check for whether named `writePaths` would do instead. A directory you can name is better than the scope containing it. |
| `network` | Measurement alone, but note that egress and filesystem confinement are independent axes and can be lost separately. |
| `read:"disk"`, `write:"disk"` | **A named, reproduced mechanism.** "The ascending walk terminated here" is a measurement, not a mechanism, and it is not sufficient. |

The asymmetry in that last row is the whole point. Every full-disk grant in the shipped catalog should trace to a sentence a reader can check, of the form *"this package does X, which no narrower scope can express."* Two worked examples that meet the bar: a package whose lifecycle shells `npx rimraf` against a path outside project, deps and userHome simultaneously; and the Windows family whose scripts cannot start inside a LowBox token at all, where the full-disk rung is not buying disk access but escape from the AppContainer.

## 3. A grant is only as good as the record under it

Before believing any grant, check the record that produced it.

- **Read the `verdict`, not just the `grant`.** `grant: null` on a `HARNESS-TIMEOUT` means *no measurement was taken*, not *this package needs nothing*. The two are indistinguishable in the grant field alone and have opposite consequences. This has been misread more than once.
- **Check the binary.** A record measured by a nub build whose relevant defect has since been fixed is stale evidence. Provenance carries the git sha for exactly this; ancestry against the fix commit is the test.
- **A re-measure can destroy a good record.** Re-running a package under a per-package time budget it cannot finish in replaces a valid measurement with a timeout. Any targeted re-measure of a package known to be slow must raise the budget explicitly first. The default is not always enough.
- **Prefer a narrow record over a broad one when both exist for the same identity**, but only after confirming both are real measurements on comparable binaries.

## 4. The unit of judgment is (package, version, platform)

Never the package alone.

- **Across platforms:** unioning is an over-grant wherever the platforms disagree, and they disagree often — measured on the real corpus, 250 of 1,734 cross-platform-comparable specs diverge, and 98 of those would take `write:"disk"` on a platform that measured narrow. Per-OS blocks exist to prevent exactly this.
- **Across versions:** the same. A package that needed a carve-out at 3.x and stopped needing it at 5.x should carry a version bound, not a name-wide grant. This is the ecosystem's direction as source builds give way to prebuilt binaries, so expect it to keep happening.
- **The outer grant stays the widest and the blocks narrow it, never the reverse.** A platform nobody measured inherits the outer grant, and that is what keeps an unmeasured platform on the safe side of §1.

## 5. A cross-platform divergence is usually real — find the mechanism

The instinct to "reconcile" a divergence by widening every platform to match the widest is wrong, and it is how a catalog quietly becomes 4× broader than its evidence.

Platforms differ because their enforcement primitives differ. Windows confines through an AppContainer, which POSIX has no analogue for, and that single fact explains the large majority of the Windows-only full-disk tail. When one platform is broad and the others are narrow, the question is *what does that platform do differently*, not *how do I make them agree*.

## 6. Curated entries are human decisions, and they answer to different rules

A curated grant overrides measurement. That makes it powerful and worth constraining.

- **Per-package, never a class rule.** A rule keyed on "looks like a hook installer" hands the grant to anything that can make itself look like one, which is every dependency — and the jail exists precisely because a lifecycle script is attacker-authored.
- **Version-bound it whenever the package's behavior changed.** A grant covering releases that provably do not use it is pure surface.
- **State the reason inline, with what was measured.** A curated entry whose justification lives only in a commit message is one refactor away from being unexplainable.
- **The set is compiled in and must stay that way.** A trust list an environment variable or flag could extend is not a trust list.

## 7. What is worth investigating, and what is not

The corpus is a coverage campaign, and its characteristic failure is that every unit of coverage surfaces something interesting, and interesting things attract investigation. Measured on this effort: over a stretch of twenty commits following a harness milestone, two added coverage.

**The test for whether a finding deserves work: does it change a grant, or does it change the instrument?** If neither, it gets a row and nothing more.

- **Worth it:** anything that would make a measurement wrong at scale; anything wrong in the under-granting direction; anything blocking coverage on a platform; a case whose failure indicts the harness rather than the package.
- **Not worth it:** explaining an individual package's quirk that lands it on a rung it genuinely needs; reconciling a divergence already explained by a known platform mechanism; a mechanism story for a family when only one member's evidence has been read.

A small harness defect that produces a *small over-grant* is not a reason to discard a corpus that cost tens of hours to produce. Re-run wholesale only for a defect that is wrong at scale, or wrong in the unsafe direction. Everything else is a targeted re-measure.

## 8. Check that the harness runs the configuration you actually ship

The costliest defect found here was not a wrong grant or a bad reader. It was the harness measuring a **different configuration of the product than the one users get**, and every record it produced looked entirely normal.

The probe pinned the binary by content-addressing it into a cache directory — and copied the executable alone, leaving behind a sidecar the binary resolves relative to itself. The resulting lookup failure was caught and downgraded to a log line nobody sees at default level, so the run silently fell back to a different shell. Every record on that platform was measured under a shell the product does not ship, and the ladder was discovering a property of that substitute rather than of the package.

What makes this class expensive is that nothing looks wrong: the runs complete, the verdicts are well-formed, the grants are plausible, and the records carry correct provenance for the binary. The only tell was that a whole platform's tail had a shape the others didn't.

- **Pin the product, not the executable.** Anything the binary locates relative to itself — a bundled shell, a sidecar, a resource directory — is part of what you are measuring. A content-addressed copy that omits it is a different program.
- **A silent fallback in the thing under test is a measurement hazard, not just a UX one.** Any place the product degrades quietly rather than failing is a place the harness can be running something other than what you think. Grep for the fallbacks and assert against them.
- **When one platform's results have a distinctive shape, suspect the harness on that platform before theorising about the OS.** A platform-shaped anomaly is evidence about the *setup* at least as often as about the operating system, and the OS story is the more satisfying one to construct — which is exactly why it gets constructed first.
- **Re-verify the environment, not only the binary.** Provenance that pins the binary's hash still says nothing about what was on `PATH`, which sidecars were present, or which shell resolved. Record those too, or a future reader cannot tell a good record from this one.

## 9. Never trust your own reader

Most wrong conclusions here came from a broken instrument rather than bad data, and they arrive looking like clean, confident results.

- **Run every reader against a case whose answer you already know, in the same invocation.** A suspiciously clean zero is a broken parser until proven otherwise. Four separate false zeros on this effort traced to schema drift, shell word-splitting, and reading the wrong file shape.
- **Print the value you are about to claim, not the container holding it.** Printing a version band's key and asserting about its contents produced a false alarm that cost real time.
- **A predicate over the new state alone cannot measure a change.** To show a re-measure narrowed something, compare those specific specs' prior state to their new one; a predicate matching "is now narrow" also matches everything that was already narrow.
- **Attribution fields in a record cannot tell you why a rung was needed.** The fields that look like they can are computed against the zero-grant floor and are dominated by downstream consequence. Only a cell log or a syscall trace answers that question.
- **A tracer that counts file creations is blind to deletions, and a tracer that runs unjailed never observes a denial.** Both bound what any trace-derived claim can mean.

## 10. An under-grant can fail SILENTLY, and that shapes what the catalog owes

The obvious failure mode for a too-narrow grant is a loud one: the script hits a denied path, errors, and the install fails with something to read. That is the *easy* case. The one that matters is the other:

**A jailed script whose write is denied may exit 0 and report success.** Measured on husky 4.3.8 (macOS, no catalog entry): `nub install` and `nub approve-builds --all` both exit 0, and zero of the nineteen git hooks are written. Nothing in the output says so. The same shape appears in the corpus — `iedriver@4.0.0` has a Linux cell that exits 0 while producing no artifact.

**The cause is the package, not nub, and that distinction decides what can be done about it.** Landlock, Seatbelt and AppContainer all deny at the kernel boundary: the script's own syscall returns `EPERM`/`EACCES`, and a script that ignores its return value swallows it there. nub is not in that path and never observes the denial — Landlock ABI v7 exposes no audit channel at all. So "warn when a jailed script is denied" is not a feature waiting to be written; on all three platforms there is nothing to warn from.

Three consequences:

- **Never treat "the install exited 0" as evidence a grant is sufficient.** A measurement must compare produced ARTIFACTS against a jail-off control, which is what the cell predicate does and why it does it. An exit-code oracle would have scored husky's zero-hook run as a pass.
- **Coverage is the mitigation, and it is the only one.** A catalogued package never reaches the failing path. That is the argument for breadth in the catalog, distinct from the argument for narrowness in each entry.
- **Say so where users can see it.** Anyone can hit this with a package the catalog does not cover, and the honest statement is that an uncatalogued install script may fail without a message. Do not write copy implying nub reports jail denials — it cannot.

## 11. A grant at a wide rung is not evidence the package needs that rung

The ladder reports where a package's install STOPPED failing. That is not the same as what it needs, and the gap between them is where the worst misreadings live. A package sitting at `write:"disk"` looks like it demands unrestricted write. It may demand nothing of the sort.

**The case that established this, and it accounts for every Linux `write:"disk"` survivor in the corpus:** the `read:"disk"` rung is INERT on Linux. `compile_mount_plan` drops a whole-root read allow, and the Landlock backend derives its rule set from that same plan, so no rule is ever emitted. A package needing broad READ therefore fails every read-scoped rung — those rungs grant it nothing — and terminates at `write:"disk"`, the only rung that reaches broad read at all, because it bypasses the plan entirely. Eight packages read as "needs the whole disk writable". None of them needs write.

**So when a grant looks too wide, ask which rung BELOW it was actually capable of passing on that platform.** Two checks, and the second is the one that convinces:

- **In the record:** find the cells naming the rung you expect to have sufficed. If the package failed *every* one of them, and that rung is known-inert, the failures are predicted rather than informative — the cells were testing a grant the backend never applied.
- **Across platforms:** run the same package on an OS where that rung DOES work. If it stops there instead of falling through, the mechanism is confirmed by a control rather than argued from a story. Measured here: `codeceptjs` and `postman-code-generators` land at `read.disk` on macOS and at `write.disk` on Linux — same package, same version, same binary.

**The consequence for the catalog is a priority rule.** A wide grant caused by a dead rung is not a package to re-measure or a mechanism to document; it is a BACKEND to fix, and fixing it moves every member of the family at once. Before writing "irreducible" beside a `write:"disk"` entry, confirm that the narrower rungs were live on the platform that produced it.

⛔ **And the fix order can matter more than the fix.** Making a dead rung live is not automatically an improvement: if the wider rung it replaces carries a protection the narrower one lacks — or if the narrower one has an unclosed hole on the platform where it already works — enabling it faithfully reproduces that hole somewhere new. Check what the working platform's version of the rung actually grants before making another platform match it.

## 12. An INVALID catalog is a silent downgrade, not an error — so one bad entry disables every good one

§10 covers an under-grant failing silently. This is the adjacent hazard and it is strictly worse, because its blast radius is the whole artifact rather than one package: **the parser rejects the catalog, nub warns, and then runs the compiled-in table instead.** The install succeeds, nothing surfaces as a failure, and every other grant in the file silently stops applying.

**The concrete case, and it was seconds from reaching the corpus at scale.** A package that needs nothing is the MODAL answer — roughly half of all records. Under v1 those carry `grant: null`, so collation skipped them and no entry was ever emitted. A **v2** record instead carries a **verified `{}`** — and the parser rejects an entry whose `default` widens nothing (*"`default` widens nothing and there are no version bands … drop it"*). The generator emitted one anyway, and `git-validate@2.2.4` took a whole platform's catalog gate down alongside two otherwise-sound records.

⇒ **The trap arms exactly when the generation harness changes shape**, which is the moment nobody is looking at the parser. It could not have fired under v1 at all.

**The rule this yields, and it generalises past the catalog:** in this system an invalid artifact is a **fallback**, not a stop. So every path that produces one owes a **positive check that it engaged** — never an inference from rc=0.

- A harness arm applying `NUB_BUILD_JAIL_CATALOG` must assert on `catalog OVERRIDDEN` (engaged) **and** `REJECTED` (malformed) in the log. A malformed override warns and falls back; the arm still exits 0 and still installs, so it measures the COMPILED-IN grants under the override's name.
- A generator must round-trip its own output through the real parser before publishing. **Verified the right way when this was fixed:** the corrected generator reproduced the v1 catalog **byte-identically across all 6,648 v1 records** (`cmp` exit 0), with an A/B control confirming the bad entry *does* appear without the fix — so the test could fail for the right reason.
- The empty-grant sentinel (`__v2_empty_grant_sentinel__`) exists for this same reason: it is how a fixture expresses "this package needs nothing" without emitting a rejected entry.

⛔ **The tempting shortcut is to relax the parser** so an empty `default` is accepted. Do not: the rejection is what stops a catalog silently carrying entries that grant nothing, and the right fix is for the generator to OMIT the package — the override replaces the table wholesale, so an omitted package already resolves to the base profile.

## 13. …and the INVERSE of §10: a FAILURE at grant X is not evidence that X is insufficient

§10 says a pass does not prove sufficiency. This is the other half, and it is the one that manufactures false findings rather than hiding real ones: **an arm that fails at grant X tells you the arm failed, not that the package needed more than X.** Under-granting is the direction that matters, so a false under-grant is the most expensive wrong answer this corpus can produce — it is exactly the shape that gets escalated.

**⇒ THE RULE: never record or report an INSUFFICIENT without re-running at a deliberately WIDER grant.** If it fails there too, the grant was never the variable.

**MEASURED, and the ratio is the argument.** A 24-package sample's raw output showed **four** apparent under-grants. Under the wider-grant control, **four of four dissolved** — none was a capability gap:

| package | raw | what the control showed |
| --- | --- | --- |
| `tree-sitter-ruby@0.20.1` | INSUFFICIENT | `rc=0`, install SUCCEEDS; the verdict came only from the artifact gate flagging node-gyp metadata as undersized. At a wider grant the shortfalls are **byte-identical** — `8978B < 13366B`, `1036B < 1293B`, `1043B < 1300B` |
| `tree-sitter-typescript@0.20.5` | INSUFFICIENT | same shape, same invariance |
| `@pulumi/datadog@0.18.9` | INSUFFICIENT | **zero** `= -1 EACCES`/`EPERM` in either arm — a harness self-collision, not a refusal |
| `appium-uiautomator2-driver@0.3.4` | INSUFFICIENT | `getaddrinfo EAI_AGAIN`, and the grant already carried `network:true` — the widest that axis goes |

⇒ **A shortfall that is invariant under widening is not a capability gap**, and that single test separates a genuine under-grant from every one of the four impostors above. Two further discriminators worth running before believing a failure: **zero real refusals in the arm logs** means the cause is not the jail at all (⛔ and `grep EACCES` matches the flag name `AT_EACCESS` in ordinary *successful* calls — only `= -1 EACCES` is a refusal); and **a failure at the widest value of the relevant axis** cannot be a grant problem by construction.

**Why this rule earns its cost.** Reporting those four would have looked like a serious correctness finding, sent someone to widen four catalog entries, and quietly degraded the catalog in the direction §1 exists to protect. The control takes one re-run per candidate.

## Changelog

- 2026-08-06 — Added §13 (a failure is not evidence of insufficiency) after a 24-package sample where 4 of 4 apparent under-grants dissolved under a wider-grant control.
- 2026-08-06 — Added §12 (an invalid catalog is a silent downgrade) after a v2-only defect where one needs-nothing package would have voided the entire catalog.
- 2026-08-05 — Initial write-up, distilled from the corpus effort's recurring misjudgments.
- 2026-08-05 — Added §8 (harness must run the shipped configuration) after the busybox finding, §9 (never trust your own reader), §10 (silent under-grant), and §11 (a wide rung is not evidence of need) after the Linux `read:"disk"` inertness was proven to account for all 8 Linux `write:"disk"` survivors. Moved the changelog back to the end, where §10 had been appended past it.
