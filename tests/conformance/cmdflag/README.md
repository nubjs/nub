# Command×flag conformance harness

Exercise nub's **full CLI command × flag surface** against a real project and assert each command behaves: exits cleanly where it should, fails *correctly* where it should, speaks the identity the project earns, and — where parity is claimed — agrees with the reference package manager.

This is a **different axis** from the lockfile harness one level up (`tests/conformance/run.sh`), which verifies lockfile round-trip fidelity. This one verifies that every wired verb + its major flags actually *run* in a real repo. It exists because shallow happy-path probing let `nub audit` ship a real-machine failure (it failed under a normal `~/.npmrc` carrying a custom `registry=`). The fix is a durable, exhaustive surface sweep run on a cadence.

## Loop

For a given nub binary + a real project fixture, the runner:

1. Spins a hermetic sandbox `HOME` / `XDG_*` (the dev box's `~/.npmrc`, caches, and stores never leak in or get clobbered).
2. Optionally seeds `~/.npmrc` from `USER_NPMRC` — the real-world machine state the harness must cover (a custom `registry=` is exactly what broke `audit`).
3. Primes a read-only copy of the fixture with one `nub install` so query verbs have a `node_modules` to read.
4. Drives every cell in [`inventory.tsv`](inventory.tsv): runs `nub <args>` in the right cwd, captures exit + output, classifies PASS / FAIL / RED(expected) / XPASS-STALE, and sweeps the output for the wrong identity's brand.
5. For `mut`/`net` cells (anything that writes the tree or hits the registry), operates on a fresh **throwaway copy** so the fixture is never dirtied.

The brand sweep is scoped by the identity of the project each cell ran in. A pnpm-incumbent fixture — a `pnpm-lock.yaml`, a `pnpm-workspace.yaml`, or a pnpm `packageManager` / `devEngines` pin — must behave as pnpm 12 does, `ERR_PNPM_*` codes and `pnpm.io` links included, so there the sweep hunts `ERR_NUB_*` / `WARN_NUB_*`. Every other fixture speaks nub, so there it hunts pnpm's codes and links. It matches codes and links rather than the brand NAME because nub's own help and agent copy name pnpm on purpose.

## The inventory

[`inventory.tsv`](inventory.tsv) is the canonical command×flag surface, enumerated authoritatively from `crates/nub-cli/src/cli.rs` (the clap `Command` enum + `NodeCommand`) and `crates/nub-cli/src/pm_engine/mod.rs` (`ENGINE_VERBS`, restricted to the WIRED match arms in each `*_family.rs`). Stubbed/unwired verbs are intentionally omitted — they error by design.

Columns (TAB-separated): `id`, `kind`, `parity`, `args`.

| kind | meaning |
| --- | --- |
| `meta` | version / help — no project needed |
| `ro`   | read-only / idempotent — runs in the primed RO copy in place |
| `mut`  | mutates the project — runner copies the fixture first |
| `net`  | needs network / a registry account / a TTY — run only with `NET=1` |

`parity` names the pnpm verb to diff against (or `-`). With `REF=1` the runner also runs `<REFPM> <parity> <args>` on a fresh copy and records exit-code agreement (a coarse but high-signal check; deep output diffing is future work). The reference is **pnpm 12.4.1** and nothing else: `REF=1` exits 2 when `REFPM --version` prints any other version, because a pnpm project must behave as exactly that pnpm.

## Usage

```sh
# build the dev nub first (see the dev-loop skill), then:
tests/conformance/cmdflag/run.sh /path/to/nub /path/to/fixture-checkout

# cover the real-world ~/.npmrc condition that broke audit:
USER_NPMRC=/path/to/custom-registry.npmrc tests/conformance/cmdflag/run.sh nub fixture

# include network cells + parity diffing against pnpm 12.4.1:
npm install --prefix /tmp/pnpm12 pnpm@12.4.1
NET=1 REF=1 REFPM=/tmp/pnpm12/node_modules/.bin/pnpm tests/conformance/cmdflag/run.sh nub fixture

# one cell:
tests/conformance/cmdflag/run.sh nub fixture audit
```

[`expectations.txt`](expectations.txt) is the known-failure red list: a listed cell is expected to fail and stays green overall; the moment it passes the harness flags `XPASS-STALE` so a fix can't land silently. Each entry should reference its tracking thread/issue.

**Expectations are fixture-specific.** A cell that *correctly* exits non-zero on one fixture (e.g. `audit` → 1 because that tree has vulnerabilities) may exit 0 on another. The current `expectations.txt` is tuned to **zod at `c7ec94d309ee8acb25113e3db738b3e03816a424`**, prepared as a pnpm project:

```sh
git init zod && git -C zod fetch --depth 1 https://github.com/colinhacks/zod c7ec94d309ee8acb25113e3db738b3e03816a424
git -C zod checkout FETCH_HEAD && rm -rf zod/.git
node -e 'const f="zod/package.json",fs=require("fs"),m=JSON.parse(fs.readFileSync(f));delete m.packageManager;fs.writeFileSync(f,JSON.stringify(m,null,2)+"\n")'
printf 'strictDepBuilds: false\n' >> zod/pnpm-workspace.yaml
```

Both edits are there to keep the fixture on pnpm 12.4.1 rather than beside it. That commit pins `pnpm@10.12.1`, and a pin naming another pnpm makes nub delegate to it; rewriting the pin to `pnpm@12.4.1` instead leaves the lockfile without the `packageManagerDependencies` pnpm 12 records for a pin, so every frozen install fails `ERR_PNPM_FROZEN_LOCKFILE_WITH_OUTDATED_LOCKFILE` under both tools. `pnpm-lock.yaml` keeps the project pnpm's. pnpm 12 also refuses dependency build scripts it was not told about (`ERR_PNPM_IGNORED_BUILDS`, exit 1 under both), which would turn every install cell into an exit-code match that says nothing; `strictDepBuilds: false` lets them finish. Later zod commits pin `nub@…`, which makes them nub projects that delegate to that nub.

A per-repo run for another fixture needs its own expectations (or the planned `expect` column on `inventory.tsv` encoding `ok` / `nonzero-ok` per cell, so the pass/fail rule is intrinsic rather than fixture-tuned).

## Fixture set (for the L0 fan-out)

The first sweep targets **zod** (single-package TS lib, pnpm project). A broader set spans both identities — pnpm projects (a pin, a lockfile, a workspace file) and nub projects, including repos that still carry an npm, Yarn or Bun lockfile, which nub ignores — × single-pkg vs monorepo × lib vs app.

## CI

This is network-touching and slow, so it belongs on a **scheduled / opt-in CI leg**, not every-PR. The hermetic `meta`/`ro` core (no `NET`, no `REF`) is fast and offline-ish and could gate PRs against a committed fixture; the `net`/`REF` legs run on a cadence.
