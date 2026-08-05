---
name: impact-analysis
description: The impact-analysis reviewer lens. Given a diff, systematically trace each changed symbol's BLAST RADIUS through the whole codebase (all call sites of a modified function, all readers/writers of a changed field, all impls/match-arms of a changed trait/enum, downstream behavioral/serialized/cross-process effects) so a locally-correct change that breaks a distant caller is caught BEFORE merge. Invoke (via the Skill tool) when a change earns a dedicated fresh-context reviewer — wide blast radius, security posture, serialized format, public surface — or when asked to "trace the impact", "find the blast radius", "what does this change break", "who calls this", "impact analysis".
metadata:
  internal: true
---

# Impact analysis

A self-review answers two different questions. The **correctness** lens asks *"is the changed code itself right?"* The **impact** lens asks *"what ELSE did this change just break — silently, at a distance?"* A change can be locally flawless and still break a caller three files away that relied on the old return contract, a serialized format that's now incompatible, or a match arm that no longer compiles.

**When to buy it:** AGENTS.md governs escalation — a dedicated fresh-context reviewer is an escalation for a change that earns it (wide blast radius, security posture, serialized format, memory/UB-adjacent), not a default leg on every diff, and it never substitutes for running the thing (`ad-hoc-test`). When you do escalate, this is the highest-value lens to buy.

The deliverable is a CLEAN, EVIDENCE-BACKED report: every changed symbol traced to every site it touches, each site classified *traced-and-safe* / *needs-a-corresponding-change* / *couldn't-fully-trace*, with file:line evidence. A confident "no impact" that misses a broken caller is worse than no review.

## The one rule: trace, never assume

The failure mode this lens exists to prevent is **reasoning from memory** — "I'm pretty sure nothing else uses this." Every claim of impact (or no-impact) is grounded in `grep` / LSP find-references / reading the actual call site. A site is "safe" only after you've READ it and confirmed the caller's assumptions hold. If you can't reach a site (dynamic dispatch, reflection, a macro, cross-language FFI, a string-built name), say *couldn't-fully-trace* — never paper over the gap.

## Method

**0. Source the diff first; it is the authoritative scope.** Read the diff you were handed (a path on disk, or `git diff --merge-base origin/<base>`). The set of changed symbols comes from the diff, not from a re-derivation of what the task "was about." Enumerate every symbol it touches — functions/methods, fields/consts/statics, enums/variants, traits/impls, public exports, behavioral changes, serialized shapes, env/CLI contracts.

**1. For every MODIFIED function/method → find ALL call sites.** `grep` the name across the workspace AND use LSP find-references — grep misses method calls resolved through a trait/receiver; find-references misses string-built/dynamic names. Run both. At each site, read it and check:

- **Arguments** — meaning/order/units/nullability changed? A param that went from bytes to KiB, or gained an `Option`, breaks every caller silently.
- **Return contract** — type, meaning, `Ok`/`Err`/`None`/panic behavior, whether it can now return early/empty, ownership/lifetime.
- **Errors & panics** — newly returning `Err`, newly panicking, or no longer returning an error a caller matched on changes every caller's error handling.
- **Ordering & side effects** — did when/whether a side effect happens change (a write, a log, a mutation, an await point, a lock acquisition), or its order relative to effects a caller depends on?

**2. For every CHANGED field/const/static → find ALL readers AND writers.** Does every reader still interpret the value correctly? Does every writer still satisfy the new invariant? Did a const's value shift behavior at a distant use site (a timeout, a batch size, a path, a cache key)?

**3. For every CHANGED enum/trait/interface → find ALL match arms / impls.** Rust's exhaustiveness catches *missing* arms, but a catch-all `_` SILENTLY swallows a new variant — flag those. A changed trait signature breaks every impl; removing a method an external crate implements is a breaking change.

**4. For BEHAVIORAL/SEMANTIC changes → trace downstream effects.** The highest-value category, because the compiler won't catch it:

- **Persisted / serialized formats** — lockfiles, cache files, on-disk state, JSON/wire formats. A changed field name or shape that round-trips through disk breaks compatibility and every consumer.
- **Cache keys** — did the input feeding a key change meaning? A stale-key bug is a silent correctness bug.
- **Public API / ABI** — anything a user imports, calls by a documented name, or links against (in nub, anything crossing the brand/public-surface boundary).
- **Cross-process / env contracts** — env vars set for a child shim, exit codes, stdout/stderr another process parses, the `node`-hijack contract. Both ends must move together.
- **Concurrency / ordering invariants** — reordered effects, work moved across an await/thread boundary, altered lock scope.

**5. Tests — coverage delta.** Which tests exercise the changed path? Did the change move behavior out from under an existing test (still passing, no longer covering)? Does a changed call site have a test baking in the OLD assumption that will pass misleadingly? Name the gap.

## Output contract

Per changed symbol:

- **Symbol** — what changed (file:line in the diff) and the nature (signature / type / value / behavior / format).
- **Blast radius** — the sites found, each `file:line` with a one-line note of what it relies on. Say how you found it (grep / find-references / both) so a gap is visible.
- **Per-site classification:**
  - **traced-and-safe** — read it, the assumption still holds, with the reason.
  - **needs-a-corresponding-change** — this site breaks or silently misbehaves; state the required update.
  - **couldn't-fully-trace** — dynamic/reflective/macro/FFI/string-built reach you could not resolve; name it as residual risk.
- **Latent breaks** — anything that compiles but is wrong at runtime: a `_` arm swallowing a new variant, a serialized-format skew, a caller relying on old ordering, a now-uncovered test path.
- **Confidence** — with an explicit escalation marker if low.

Lead with the findings; the traced-and-safe list is evidence the trace was thorough, not the headline.

**Exempt diffs:** genuinely trivial ones with no behavioral surface — comment/doc/whitespace-only, a mechanical rename whose only effect is import-path updates, a lockfile regeneration.

## Tier + role

Opus at high+ effort — judgment-dense reach analysis, not a mechanical harvest. (A cheap tier may harvest raw call-site lists, but every "safe"/"breaks" verdict is Opus-decided.) The reviewer is read-only: it traces and reports, it does not land fixes. The implementer treats each finding as a hypothesis, verifies it against the code, applies the change, and re-reviews until the report is clean.

Impact analysis runs alongside the correctness lens; its unique job is *reach* — the distance a change travels — which a diff-focused correctness lens structurally under-weights.
