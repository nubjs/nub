---
name: impact-analysis
description: The impact-analysis reviewer lens — a MANDATORY leg of every significant self-review. Given a diff, systematically trace each changed symbol's BLAST RADIUS through the whole codebase (all call sites of a modified function, all readers/writers of a changed field, all impls/match-arms of a changed trait/enum, downstream behavioral/serialized/cross-process effects) so a locally-correct change that breaks a distant caller is caught BEFORE merge. Invoke (via the Skill tool) when dispatching the self-review of a non-trivial code change, or when asked to "trace the impact", "find the blast radius", "what does this change break", "who calls this", "impact analysis". Auto-triggers on a self-review of any behavioral code change.
metadata:
  internal: true
---

# Impact analysis

A self-review answers two different questions, and they need two different lenses. The **correctness** lens asks *"is the changed code itself right?"* The **impact** lens asks *"what ELSE in the codebase did this change just break — silently, at a distance?"* A change can be locally flawless and still break a caller three files away that relied on the old return contract, a serialized format that's now incompatible, or a match arm that no longer compiles. Impact analysis is the lens that catches that, and it is a **MANDATORY leg of every significant self-review** — at least one sub-agent doing impact analysis, alongside the correctness reviewer(s). This is the maintainer directive; see AGENTS.md → "Model tiering for sub-agents."

The deliverable is a CLEAN, EVIDENCE-BACKED impact report: every changed symbol traced to every site it touches, each site classified *traced-and-safe* / *needs-a-corresponding-change* / *couldn't-fully-trace*, with file:line evidence. Same trust bar as an audit — a confident "no impact" that misses a broken caller is worse than no review.

## The one rule: trace, never assume

The single failure mode this lens exists to prevent is **reasoning from memory** — "I'm pretty sure nothing else uses this." You do not know the blast radius until you have *enumerated* it with tools. Every claim of impact (or no-impact) is grounded in `grep` / LSP find-references / reading the actual call site — never in recollection of how the code is shaped. A change is "safe at a site" only after you've READ that site and confirmed the caller's assumptions still hold. If you can't reach a site (dynamic dispatch, reflection, a macro, cross-language FFI, a string-built name), you say *couldn't-fully-trace* — you never paper over the gap with a guess.

(The read-only-reviewer discipline applied to reach: *flag uncertainty explicitly — if you cannot verify a claim, say so rather than guess*; and the load-bearing-claim rule: verify against the actual source, never training data.)

## Method — systematic, evidence-based

**0. Source the diff first; it is the authoritative scope.** Read the diff you were handed (a path on disk, or `git diff --merge-base origin/<base>`). The set of changed symbols comes from the diff, not from a re-derivation of what the task "was about." Enumerate every symbol the diff touches — functions/methods, fields/struct-members/consts/statics, enums/variants, traits/interfaces/impls, public exports, behavioral/semantic changes, serialized or persisted shapes, env/CLI contracts.

**1. For every MODIFIED function/method → find ALL call sites.** `grep` the name across the workspace AND use LSP find-references (a `grep` misses method calls resolved through a trait/receiver; find-references misses string-built/dynamic names — run BOTH). At each call site, READ it and ask whether the caller's assumptions still hold:
- **Arguments** — meaning/order/units/nullability of any arg changed? A param that went from "bytes" to "KiB", or gained an `Option`, breaks every caller silently.
- **Return contract** — type, the meaning of the value, `Ok`/`Err`/`None`/panic behavior, whether it can now return early/empty, ownership/lifetime.
- **Errors & panics** — a function that newly returns `Err` (or newly panics, or stops returning an error a caller matched on) changes every caller's error handling.
- **Ordering & side effects** — did the change alter when/whether a side effect happens (a write, a log, a mutation, an await point, a lock acquisition), or the order relative to other effects a caller depends on?

**2. For every CHANGED variable/field/struct-member/const/static → find ALL readers AND writers.** A field whose type, units, default, or invariant changed ripples to everyone who reads or writes it. Check: does every reader still interpret the value correctly? Does every writer still satisfy the new invariant? Did a const's value change in a way that shifts behavior at a distant use site (a timeout, a batch size, a path, a cache key)?

**3. For every CHANGED enum/trait/interface → find ALL match arms / impls / implementors.** Adding/removing/renaming an enum variant breaks every `match` (Rust's exhaustiveness catches *missing* arms at compile time, but a catch-all `_` SILENTLY swallows a new variant — flag those). Changing a trait/interface signature breaks every `impl`/implementor; check each one. Removing a method that an external crate or the public API implements is a breaking change.

**4. For BEHAVIORAL/SEMANTIC changes → trace downstream effects.** The hardest and highest-value category — the compiler won't catch these. When the *behavior* of a code path changed (not its signature), trace where that behavior is observed:
- **Persisted / serialized formats** — lockfiles, cache files, on-disk state, JSON/wire formats. A changed field name or shape that round-trips through disk breaks forward/backward compatibility and every consumer of the old format.
- **Cache keys** — did the input that feeds a cache key change meaning? A stale-key bug is a silent correctness bug.
- **Public API / ABI** — anything a user imports, calls by a documented name, or links against. (In nub: anything crossing the brand/public-surface boundary — see AGENTS.md.)
- **Cross-process / env contracts** — env vars set for a child shim, exit codes, stdout/stderr format another process parses, the `node`-hijack contract. A child reads what the parent writes; both ends must move together.
- **Concurrency / ordering invariants** — a change that reorders effects, moves work across an await/thread boundary, or alters lock scope.

**5. Tests — coverage delta.** Which tests exercise the changed path? Did the change move behavior OUT from under an existing test (the test still passes but no longer covers the new path)? Does a now-changed call site have a test that bakes in the OLD assumption and will pass misleadingly? Name the coverage gap; a green suite over a stubbed/uncovered path is the failure the testing philosophy warns about.

## Output contract — a structured impact report

Per changed symbol, report:

- **Symbol** — what changed (file:line in the diff), and the nature of the change (signature / type / value / behavior / format).
- **Blast radius** — the sites found, each as `file:line` with a one-line note of what it relies on. Distinguish how you found it (grep / find-references / both) so a gap is visible.
- **Per-site classification** — each site is one of:
  - **traced-and-safe** — read it, the caller's assumption still holds, with the reason.
  - **needs-a-corresponding-change** — this site breaks (or silently misbehaves) under the change; state the required update. This is a finding.
  - **couldn't-fully-trace** — dynamic/reflective/macro/FFI/string-built reach you could not resolve statically; name it as a residual risk, don't bury it.
- **Latent breaks** — anything that compiles but is wrong at runtime (the high-value catches: a `_` arm swallowing a new variant, a serialized-format skew, a caller relying on old ordering, a now-uncovered test path).
- **Confidence** — and an explicit escalation marker if low, so the orchestrator re-routes or widens the trace.

Lead with the findings (needs-a-change + latent breaks + couldn't-trace); the traced-and-safe list is the evidence that the trace was thorough, not the headline. A useful finding shape: an `Affected sites` list of `file:line` and a `Required outcome` per real finding.

## When it triggers

- **Every significant self-review** dispatches at least one impact-analysis pass — this is mandatory, not a judgment call (AGENTS.md). For large / cross-cutting / public-surface / format-touching changes, give it its own dedicated reviewer split from the correctness lens; for a smaller-but-non-trivial change, it's still a required lens of the review.
- **Exempt:** genuinely trivial diffs with no behavioral surface — comment/doc/whitespace-only, a mechanical rename whose ONLY effect is import-path updates (read the *shape*, not the line count), a lockfile regeneration. The moment a diff changes a signature, a value, a behavior, or a format, impact analysis is required.

## Tier + role

Opus (or Fable) at high+ effort — this is judgment-dense reach analysis, not a mechanical harvest; never economize it to Sonnet/Haiku for the deciding. (A cheap tier MAY harvest raw call-site lists, but every "safe"/"breaks" verdict is Opus-decided.) The reviewer is read-only over the diff + the codebase: it traces and reports, it does not land fixes. The owning implementer (or orchestrator) treats each finding as a hypothesis, verifies it against the code, and applies the corresponding change — then re-reviews until the impact report is clean.

## Relationship to the other review lenses

Impact analysis is one lens of a multi-lens self-review, not the whole review. It runs ALONGSIDE the correctness lens (is the changed code right?) and, where the blast radius warrants, the safety / portability / docs-honesty lenses (see `implementation-thread`). Its unique job is *reach* — the distance a change travels — which the correctness lens, focused on the diff itself, structurally under-weights.
