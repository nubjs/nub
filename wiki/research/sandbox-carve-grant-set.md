# Resolving allow/deny globs to a minimal Landlock grant set — is there a crate?

## The question

nub's Linux **Landlock** backend must turn an ordered allow/deny glob ruleset
(last-match-wins, with negations) into a concrete set of **positive, whole-subtree
grants** that admits every allowed path and excludes every denied one. Landlock is
**allow-only** — there is no deny primitive, and every rule (`PathBeneath`) covers a
**whole subtree** — so "allow `/proj`, deny `/proj/.env`" cannot be expressed as one
grant. The backend must compute a *minimal grant set*: grant clean subtrees whole,
and where a later rule reaches inside an allowed subtree, descend and grant the
allowed children individually so the denied leaf is never granted.

nub hand-rolls this as a **carve walk** in
[`crates/nub-sandbox/src/backend/linux_grants.rs`](../../crates/nub-sandbox/src/backend/linux_grants.rs)
(`walk_read` / `walk_write`, driven by an order-aware `View`). It grants a subtree
whole where no later rule can flip its verdict inside; otherwise it emits a `ReadDir`
on the directory (keeps it listable) and recurses per-child, skipping the denied
leaf — **enumerating the filesystem at carve points**, bounded by `MAX_GRANTS` /
`MAX_VISITS` with a `read_partial` fail-safe.

Two sub-questions:

1. **Is there an off-the-shelf crate (or a clean named algorithm) that does this?**
2. **Can it be done WITHOUT filesystem enumeration**, or is bounded enumeration at
   carve points inherent to Landlock's subtree model?

## What the operation actually requires

From the hand-roll, the operation is:

- Input: an ordered `Vec<FsRule>` of `(CanonGlob, Effect::{Allow,Deny}, FsAccess)` +
  a `default_effect`. Semantics are **last-match-wins** (a later rule overrides an
  earlier one for any path both match), which is exactly gitignore/`.dockerignore`
  ordering.
- Output: a minimal `Vec<Grant>` of `(existing path, GrantKind)` where `GrantKind ∈
  {ReadSubtree, ReadDir, ReadFile, WriteSubtree}` — positive, additive, subtree-or-leaf.
- Constraints the grant set must honor: **order-aware** (a subtree is grantable whole
  only if no rule *after* the one that decided it can match inside it); **symlink-
  dropping** (a link is never followed — its target is granted on its own merits);
  **`/proc`,`/sys` never granted** (env-read boundary); **carved dirs stay listable**
  (`ReadDir` without blanket-granting file children); **budget-bounded** with a
  fail-safe partial flag. The grants are consumed in
  [`linux.rs`](../../crates/nub-sandbox/src/backend/linux.rs) as
  `PathBeneath::new(PathFd(path), bits)` — the raw allow-only Landlock primitive.

The genuinely non-trivial pieces are (a) the **order-aware "can a later rule flip
this subtree's verdict inside it" test** (`later_reaches` + `glob_reaches_under`
prefix-overlap analysis) and (b) the **grant-whole-else-descend** decomposition. The
plain "does this path match?" part is ordinary last-match glob matching.

## Survey — crates and prior art

Every crate claim below is grounded in the crate's actual docs/source, not its name.

| Candidate | What it does | Fits? |
|---|---|---|
| [`landlock`](https://docs.rs/landlock) `0.4` (what nub uses) | Thin safe wrapper over the Landlock syscalls: `Ruleset`, `RulesetCreated`, `PathBeneath`, `PathFd`, `AccessFs`, `NetPort`, `Scope`. The one "helper," `path_beneath_rules()`, just maps a **list of paths** to `PathBeneath` rules. | **No.** No glob support, no deny/exclude, no grant-set computation. `PathBeneath` is allow-only, whole-subtree — it *is* the primitive the carve has to feed, not a solver for it. All ABI versions (v1–v5) lack a deny primitive. |
| [`globset`](https://docs.rs/globset) `0.4` (what nub uses) | Compile globs → `GlobMatcher`/`GlobSet`; match a path. | **Partial (already used).** A matcher only. nub's `View::decide` layers last-match ordering + canonicalization on top. Does not decompose to prefixes or a grant set. |
| [`ignore`](https://docs.rs/ignore) (`gitignore::Gitignore`) | gitignore-style matcher: last-match-wins + `!` negation, returns `Match::{None,Ignore,Whitelist}`; drives `WalkBuilder` to *filter a filesystem walk*. | **No (matcher, not solver).** This is the closest thing to nub's last-match semantics and could in principle replace `View::decide`'s ordering logic — but it produces per-path verdicts, not a minimal covering set of directories, and it still enumerates via the walker. It solves the easy half, not the carve. |
| [`birdcage`](https://docs.rs/birdcage) (Phylum) | Cross-platform embeddable sandbox — Landlock on Linux, `sandbox-exec` on macOS, one API. **The closest analog to nub's own use case.** | **No — and tellingly so.** Its model is a pure additive allowlist (`Exception::ExecuteAndRead`, `Exception::WriteAndRead` on paths). It offers **no nested exclusion at all** — you cannot allow `/proj` and deny `/proj/.env`. The most prominent cross-platform Rust sandbox simply **doesn't attempt** the operation nub needs. |
| [`extrasafe`](https://docs.rs/extrasafe) | seccomp + Landlock wrapper; `SystemIO` ruleset with `allow_read_path` / `allow_write_file` / `allow_create_in_dir`. | **No.** Additive allowlist over `landlock`; no deny, no glob, no grant-set derivation. |
| `radix_trie` / `qp-trie` / `sequence_trie` / `path-tree` | Prefix/radix tries and path routers — store and look up keys by prefix. | **No.** They can *represent* a path-prefix set, but the hard part isn't representing the set — it's **enumerating the concrete grant paths**, which needs the filesystem. A trie of the rules doesn't tell you which children of a carved dir exist. |
| interval / segment-tree crates | 1-D range containment. | **No.** Paths form a *tree/prefix* space, not a 1-D interval line; the deny is a subtree hole, not a range gap. Wrong shape. |

### How other sandboxes solve "allow-only with a nested deny"

This is the load-bearing context: nub's operation is unusual **because Landlock's
model is unusual**. Every mature sandbox that supports nested exclusion does it with
a *different mechanism* that sidesteps enumeration — none of them are a reusable
library for the Landlock case:

- **OpenBSD `unveil(2)`** — the primitive nub *wishes* it had. `unveil("/home","rx")`
  then `unveil("/home/user/.ssh","")` makes the deeper path inaccessible: the **kernel
  natively resolves a per-subpath permission override**. No enumeration, no carve —
  because the primitive itself supports subtracting a nested path. Landlock has no
  equivalent; its rules only *union*.
- **systemd** (`ReadOnlyPaths=`, `InaccessiblePaths=`, `ReadWritePaths=`) — implemented
  with **bind mounts in a mount namespace**. A nested deny is an *overmount* of an
  empty inaccessible dir over the one path — O(1), no enumeration. (systemd can layer
  Landlock on top, but the *path* restrictions are mount-based.)
- **bubblewrap** (`--ro-bind` / `--bind` / `--tmpfs`) and **firejail**
  (`blacklist`/`whitelist` + `noblacklist`) — same story: **bind/tmpfs overmounts** in
  a mount namespace. Nested deny = mount a tmpfs over the denied subpath. No walk.
- **Landlock kernel sample `sandboxer.c`** — takes `LL_FS_RO` / `LL_FS_RW` path lists
  and grants each. **No deny, no glob** — upstream's own reference tool punts on the
  nested-deny problem entirely, exactly as `birdcage`/`extrasafe` do.
- **Chromium Linux sandbox** — namespaces + seccomp-bpf, not a path allowlist. N/A.

The mount-namespace approach (systemd/bwrap/firejail) is unavailable to nub: nub is a
**Rust CLI augmenter**, not a root/namespace manager — it can't overmount the user's
filesystem, and doing so would violate its additivity posture. Landlock is precisely
the tool for an unprivileged process, and its price is that nested deny is not a
primitive — it must be *synthesized* by granting around the hole.

## The no-enumeration analysis (honest)

**Eliminating filesystem enumeration is impossible within Landlock's model; only
minimizing it is achievable — and nub already does the minimization.**

The proof is short. A Landlock grant is **positive, whole-subtree, and union-only**:
once you grant read on `/proj`, no later rule can subtract `/proj/.env` — Landlock
never intersects or removes. So to admit everything under an allowed parent `P`
*except* a nested denied path `X`, the only expressible grant set is: for every
directory `D` on the path `P → parent(X)`, grant each **child of `D` that is not on
the path toward `X`** (plus a listable `ReadDir` on `D` itself). The names of those
siblings are **not derivable from the globs** — a glob like `/proj/**` says nothing
about which entries exist in `/proj`. Learning them **requires reading the
directory**. Hence enumeration at each carve point is *inherent*, not an
implementation shortcut.

What a pure glob→prefix computation *can* do without touching the disk:

- Decide, per glob, its literal absolute prefix (`literal_prefix`) and whether a rule
  can reach inside a given subtree (`glob_reaches_under`) — nub does this, and it's
  what lets it **skip enumeration entirely for clean subtrees** (grant whole, walk
  stops).
- Therefore the *only* directories ever read are the ones a later opposite-effect rule
  actually reaches into. A policy with no nested denies enumerates **nothing**; a
  policy with a nested `.env` deny enumerates only the ancestor chain of each `.env`.

So the correct framing is **"minimize enumeration," not "eliminate" it**, and nub's
carve already realizes the minimum for Landlock: it walks *iff* an order-later rule of
the opposite effect can flip the verdict inside the subtree, and grants whole
otherwise. The `SYSTEM_TOPLEVELS` clean-grant of `/usr`,`/etc`,… under a generous `**`
read (skipping the built-in `.env*` carve there) is an additional, deliberate
enumeration-avoidance optimization on the hot path.

The only way to get to *zero* enumeration is to **change the mechanism** — a native-
subpath-override kernel primitive (`unveil`) or mount-namespace overmounts (systemd/
bwrap) — neither of which is available to an unprivileged augmenter on Linux. Landlock
is the right tool; the carve is the price of its allow-only model.

## Verdict

**No standard crate exists for this operation, and none is likely to — keep nub's
hand-roll.** The operation is a poor fit for a library because it is specific to
Landlock's unusual **allow-only, union-only, whole-subtree** model: the two mature
solutions to nested deny (kernel subpath-override à la `unveil`, and mount-namespace
overmounts) don't need a grant-set solver at all, and the prominent Landlock wrappers
(`birdcage`, `extrasafe`, the kernel `sandboxer`) **omit nested deny entirely** rather
than solve it. nub is doing something none of the ecosystem crates do — synthesizing a
nested deny out of positive subtree grants — so there is nothing to adopt.

The enumeration nub does at carve points is **inherent to Landlock**, not a design
smell; it cannot be removed without abandoning Landlock. nub already minimizes it
correctly (grant clean subtrees whole via prefix analysis; descend only where an
order-later opposite-effect rule reaches inside; budget-bounded with a fail-safe
partial flag). The algorithm is best described as computing a **minimal antichain of
subtree covers of the "allow" region on the filesystem's path tree, with per-leaf
carve-outs** — a sound, standard-shaped decomposition; there is no cleaner known
algorithm it is failing to use.

Optional, low-value refinements (none change the verdict — do NOT prioritize):

- The last-match ordering in `View::decide` overlaps `ignore::gitignore`'s semantics,
  but nub's version is tightly coupled to its canonicalization
  (`canonicalize_including_nonexistent` + `normalize_slashes`) and the read/write
  effect projection; swapping in `ignore` would add a dependency to replace ~15 lines
  and lose the coupling. Not worth it.
- `glob_reaches_under` is conservative (may over-carve); that's correct (children are
  re-decided) and cheap. No change needed.
- If enumeration cost is ever shown to matter on a real policy (it isn't today — the
  budget + clean-subtree fast paths bound it), the lever is *tightening which rules
  count as "reaching inside,"* not adopting a crate.

**Recommendation: keep the hand-roll as-is; record here that the crate survey was
done and came back empty so a future agent doesn't re-litigate it.** No maintainer
sign-off needed — this is a "don't-change-anything" finding.

## Changelog

- 2026-07-09 — Initial write-up. Surveyed `landlock`, `globset`, `ignore`, `birdcage`,
  `extrasafe`, trie/interval crates, and the unveil/systemd/bwrap/firejail/Landlock-
  sample prior art. Verdict: no crate does this; enumeration at carve points is
  inherent to Landlock's allow-only model; keep and don't re-litigate the hand-roll.
