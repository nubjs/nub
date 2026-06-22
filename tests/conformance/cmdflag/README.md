# Command×flag conformance harness

Exercise nub's **full CLI command × flag surface** against a real project and
assert each command behaves: exits cleanly where it should, fails *correctly*
where it should, never leaks the `aube` brand, and — where parity is claimed —
agrees with the reference package manager.

This is a **different axis** from the lockfile harness one level up
(`tests/conformance/run.sh`), which verifies lockfile round-trip fidelity. This
one verifies that every wired verb + its major flags actually *run* in a real
repo. It exists because shallow happy-path probing let `nub audit` ship a
real-machine failure (it failed under a normal `~/.npmrc` carrying a custom
`registry=`). The fix is a durable, exhaustive surface sweep run on a cadence.

## Loop

For a given nub binary + a real project fixture, the runner:

1. Spins a hermetic sandbox `HOME` / `XDG_*` (the dev box's `~/.npmrc`, caches,
   and stores never leak in or get clobbered).
2. Optionally seeds `~/.npmrc` from `USER_NPMRC` — the real-world machine state
   the harness must cover (a custom `registry=` is exactly what broke `audit`).
3. Primes a read-only copy of the fixture with one `nub install` so query verbs
   have a `node_modules` to read.
4. Drives every cell in [`inventory.tsv`](inventory.tsv): runs `nub <args>` in
   the right cwd, captures exit + output, classifies PASS / FAIL / RED(expected)
   / XPASS-STALE, and sweeps the output for the `aube` brand.
5. For `mut`/`net` cells (anything that writes the tree or hits the registry),
   operates on a fresh **throwaway copy** so the fixture is never dirtied.

## The inventory

[`inventory.tsv`](inventory.tsv) is the canonical command×flag surface,
enumerated authoritatively from `crates/nub-cli/src/cli.rs` (the clap `Command`
enum + `NodeCommand`) and `crates/nub-cli/src/pm_engine/mod.rs` (`ENGINE_VERBS`,
restricted to the WIRED match arms in each `*_family.rs`). Stubbed/unwired verbs
are intentionally omitted — they error by design.

Columns (TAB-separated): `id`, `kind`, `parity`, `args`.

| kind | meaning |
| --- | --- |
| `meta` | version / help — no project needed |
| `ro`   | read-only / idempotent — runs in the primed RO copy in place |
| `mut`  | mutates the project — runner copies the fixture first |
| `net`  | needs network / a registry account / a TTY — run only with `NET=1` |

`parity` names the reference-PM verb to diff against (or `-`). With `REF=1` the
runner also runs `<REFPM> <parity> <args>` on a fresh copy and records exit-code
agreement (a coarse but high-signal check; deep output diffing is future work).

## Usage

```sh
# build the dev nub first (see the nub-dev skill), then:
tests/conformance/cmdflag/run.sh /path/to/nub /path/to/fixture-checkout

# cover the real-world ~/.npmrc condition that broke audit:
USER_NPMRC=/path/to/custom-registry.npmrc tests/conformance/cmdflag/run.sh nub fixture

# include network cells + reference-PM parity diffing:
NET=1 REF=1 REFPM=pnpm tests/conformance/cmdflag/run.sh nub fixture

# one cell:
tests/conformance/cmdflag/run.sh nub fixture audit
```

[`expectations.txt`](expectations.txt) is the known-failure red list: a listed
cell is expected to fail and stays green overall; the moment it passes the
harness flags `XPASS-STALE` so a fix can't land silently. Each entry should
reference its tracking thread/issue.

**Expectations are fixture-specific.** A cell that *correctly* exits non-zero on
one fixture (e.g. `audit` → 1 because that tree has vulnerabilities; `add` → 1
because a monorepo root refuses a bare add) may exit 0 on another. The current
`expectations.txt` is tuned to **zod**. The fan-out's per-repo runs will each
need their own expectations (or — the planned refinement — an `expect` column on
`inventory.tsv` encoding `ok` / `nonzero-ok` / `run` semantics per cell, so the
pass/fail rule is intrinsic and not fixture-tuned). For phase 1 the red list is
the mechanism; the `expect`-column refinement is the follow-up.

## Fixture set (for the L0 fan-out)

The first sweep targets **zod** (single-package TS lib, pnpm). The diverse set
for the broader fan-out spans three axes — PM/lockfile, project shape, and
ecosystem — so the surface is exercised under every incumbent the PM mirrors.
Candidate repos at pinned SHAs are listed in the parent thread; pick a few that
together cover npm / pnpm / yarn-classic / yarn-berry / bun × single-pkg vs
monorepo × lib vs app.

## CI

This is network-touching and slow, so it belongs on a **scheduled / opt-in CI
leg**, not every-PR. The hermetic `meta`/`ro` core (no `NET`, no `REF`) is fast
and offline-ish and could gate PRs against a committed fixture; the `net`/`REF`
legs run on a cadence.
