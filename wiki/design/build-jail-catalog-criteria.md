# Judging the build-jail catalog

How to decide whether a grant is right, whether a record is trustworthy, and what is worth chasing. Written because the same misjudgments kept recurring: over-investigating findings that changed no grant, treating a null grant as a measurement, and unioning across platforms that genuinely disagree.

The companion documents are [`build-jail-architecture.md`](build-jail-architecture.md) for the capability model and mechanism, and the corpus repo's own harness README for how a measurement is produced.

## 1. The safety asymmetry is the master rule

**Over-granting fails to confine. Under-granting breaks the install.** These are not symmetric costs. A broken install is a user-visible product defect that makes the jail unshippable; an over-grant is a security shortfall on a package that was already executing arbitrary code on the machine. When the evidence is ambiguous, loosen.

The rule has one important bound: it justifies relaxing about a *narrow* over-grant, never about `write:"disk"`. The terminal rung discards the confinement entirely, so "when in doubt, loosen" stops applying at exactly the point where loosening costs everything.

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

## Changelog

- 2026-08-05 — Initial write-up, distilled from the corpus effort's recurring misjudgments.
