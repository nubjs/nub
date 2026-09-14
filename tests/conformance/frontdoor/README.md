# Front-door pm-compat conformance matrix

The anti-resurfacing guard. Every pm-compat gap that reached a user — `npm_config_reporter` not honored, `--env-file` grammar, regex script selection, the config-read gate, the `npm_config_*` bridge — was rediscovered ad-hoc because only the **engine** and the **lockfile** were ever tested, never the **front door**: the CLI surface a user actually drives (config read/write, env knobs, run/exec flags, another tool's lockfile), *per project identity*.

This harness turns the per-identity behavior map below into a CI matrix. A gap or regression on a covered cell **fails CI** instead of waiting for a user report.

It is a **third axis**, distinct from its two siblings:

- `tests/conformance/run.sh` — lockfile round-trip fidelity (does nub read/write each PM's lockfile?).
- `tests/conformance/cmdflag/run.sh` — does every wired verb × flag *run* on one real repo?
- **this** — does each front-door SURFACE behave correctly *under each project identity*? It is the only suite parameterized by identity.

## The matrix

Two dimensions. **Identity** (the one nub detects) × **surface** (the front-door behavior class). Each filled cell is one or more `assertions.tsv` rows.

There are exactly two identities, decided by `crates/nub-core/src/pm/identity.rs`. A **pnpm project** carries a pnpm pin (`packageManager` or `devEngines.packageManager`), a `pnpm-lock.yaml` or a `pnpm-workspace.yaml`, and must behave exactly like **pnpm 12.4.1**. Every other project is a **nub project** — including one holding an npm, Yarn or Bun lockfile, which no longer confers an identity of its own. The `npm`, `yarn` and `bun` fixtures exist to prove that: they are nub projects carrying another tool's files.

| surface ↓ \ identity → | nub project | pnpm project (= pnpm 12.4.1) |
|---|---|---|
| **config READ** | project `.npmrc`; never `~/.config/pnpm/config.yaml`, `.yarnrc.yml` or `bunfig.toml` | project `.npmrc`, `pnpm-workspace.yaml` over it, global `config.yaml` |
| **config WRITE home** | project `.npmrc` | global `config.yaml`; `--location project` → `pnpm-workspace.yaml` |
| **env: `npm_config_*`** | honored (install and `config get`) | ignored (install and `config get`) |
| **env: branded** | `pnpm_config_*` and `BUN_CONFIG_*` ignored | `pnpm_config_*` honored |
| **foreign lockfile** | unread, untouched, `nub.lock` written, one `nub pm migrate` line | unread, untouched, `pnpm-lock.yaml` written, silent |
| **run/exec flags** | reporter / regex / env-file (identity-invariant) | |

Every pnpm-project expectation was derived by running pnpm 12.4.1 on the same fixture, and no other pnpm version is a reference: a pnpm 10 or 11 answer describes a different config model (pnpm 12 writes `config set` to its global `config.yaml`, not `.npmrc`, and does not honor `npm_config_registry`).

The run/exec-flag surface is **identity-invariant** (the run echo, `--env-file`, reporter, regex selection are nub's own CLI), so it is asserted once rather than per identity.

Lockfile round-trip is **covered by the sibling harnesses**; this harness only asserts that another tool's lockfile is left alone. See "Deferred / covered elsewhere."

### Scope decisions

- **Documented behavior, hermetic by default.** Every `doc` cell runs offline with no reference tool installed, so the suite can gate every PR. The expectations it asserts are measured against pnpm 12.4.1 once, recorded in the row comments, and re-derived when the reference moves.
- **`ref` cells need the network.** The two `npm_config_registry` install probes run only under `REF=1`.
- **Known divergences are listed, not hidden.** `expectations.txt` names each cell that is red because the product disagrees with the reference; the cell reports XFAIL, and XPASS-STALE fails the run once the product is fixed.

## Cells

`assertions.tsv` is the canonical cell list. Columns (TAB-separated):

| column | meaning |
|---|---|
| `id` | unique slug (`<fixture>-<surface>-<case>`) |
| `fixture` | `fixtures/<name>` the cell runs in, or `-` (identity-invariant, uses `fixtures/nub`) |
| `surface` | `config-read` `config-write` `env-bridge` `env-gate` `run-flag` `lockfile` |
| `mode` | `doc` (hermetic, offline) · `ref` (needs the network, runs only under `REF=1`) |
| `assert` | the assertion verb (see below) + its args |

Assertion verbs (run by `run.sh`):

| verb | meaning |
|---|---|
| `echo-shown` / `echo-hidden` | run nub with the given flags; assert the `$ <cmd>` run-echo is present / suppressed (a suppressed echo only counts if the script ran) |
| `echo-hidden-env` / `echo-shown-env` | the same, with one `VAR=val` in the environment |
| `runs-scripts <a,b,…>` | assert exactly these scripts ran (by their stdout markers) |
| `env-injected <VAR>=<val>` | assert the child process saw the env var (script prints it) |
| `exits-nonzero` | assert the command fails |
| `config-reads <key>=<val>` | seed the project `.npmrc`, assert `config get` returns it |
| `config-file <file> <val> <control\|-> honored\|ignored` | seed `registry` in `<file>` (`~/…` is the sandbox HOME), optionally with a different `.npmrc` control; assert the file wins, or that the control (or the default) does |
| `config-writes-to <target> <key> <val> [flags…]` | run `config set [flags…] <key> <val>`; assert the value landed in `<target>` and in no other config home |
| `env-gate <key> <VAR> <val> honored\|ignored` | assert `config get <key>` does / does not follow `VAR` |
| `env-bridge-resolver <url> honored\|ignored` | `REF=1`: install with `npm_config_registry=<url>`; assert the resolver tries that host, or ignores it and succeeds |
| `foreign-lockfile <file> hint\|quiet` | `install --offline` with another tool's lockfile; assert it is untouched and nub's own lockfile (hint) or pnpm's (quiet) is written, with or without the `nub pm migrate` line |

`expectations.txt` lists cells that are red because of a known product divergence, one line each with the reference output that proves it. A listed cell reports XFAIL; once it passes it reports XPASS-STALE and fails the run, so the entry is deleted with the fix.

## Usage

```sh
# build the dev nub first (see the dev-loop skill), then:
tests/conformance/frontdoor/run.sh /path/to/nub                 # hermetic doc-mode core (every-PR leg)
REF=1 tests/conformance/frontdoor/run.sh /path/to/nub           # also the network cells
tests/conformance/frontdoor/run.sh /path/to/nub run-flag        # one surface
KEEP=1 tests/conformance/frontdoor/run.sh /path/to/nub          # keep sandbox for forensics

# re-derive the pnpm-project cells against the reference: pnpm accepts every command they run
npm install --prefix /tmp/pnpm12 pnpm@12.4.1
REF=1 tests/conformance/frontdoor/run.sh /tmp/pnpm12/node_modules/.bin/pnpm $(grep -oE '^pnpm-[a-z-]+' tests/conformance/frontdoor/assertions.tsv)
```

The runner spins a **hermetic sandbox** with its own `HOME`/`XDG_*` per cell, and unsets every inherited `npm_config_*`, `NPM_CONFIG_*`, `pnpm_config_*` and `PNPM_*` variable before any cell runs (the dev box's `~/.npmrc` carries a dead proxy that breaks fetches, and a developer's `PNPM_HOME` would move a pnpm cell's store out of the sandbox). Each cell runs in a fresh copy of its fixture. No network in `doc` mode.

## Fixtures

One minimal fixture per shape under `fixtures/<name>/`. Fixtures are hand-built and tiny by design — the suite tests *identity + surface behavior*, not real installs, so none needs `node_modules`.

| fixture | identity | marker |
|---|---|---|
| `nub` | nub | nothing; also carries the run-flag marker scripts |
| `pnpm` | pnpm | `pnpm-workspace.yaml` |
| `npm` | nub | `package-lock.json` |
| `yarn` | nub | `packageManager: "yarn@4.5.0"` + berry `yarn.lock` |
| `bun` | nub | `bun.lock` + `bunfig.toml` |
| `envbridge` | nub | one real dependency, so the `REF=1` resolver probe actually fetches |
| `envbridge-pnpm` | pnpm | the same dependency + `pnpm-workspace.yaml` |

The pnpm fixtures declare pnpm with `pnpm-workspace.yaml` rather than a `packageManager` pin on purpose: under a `pnpm@12.4.1` pin the engine resolves pnpm itself as a config dependency from the registry, so an offline install fails with `ERR_PNPM_BAD_CONFIG_DEP` and a config command reaches for the network — pnpm 12.4.1 does the same.

### Front-door behaviors this slice pinned

Each is a guarded cell, and each pnpm-project row agrees with pnpm 12.4.1 run on the same fixture:

- **`--reporter=silent` before the subcommand** suppresses the run-echo under pnpm 12.4.1 (`pnpm --reporter=silent run x`). nub still prints it — the one listed divergence. `--silent run x` and `run --reporter=silent x` suppress it under both.
- **Space-separated `run a b` runs only `a`** and forwards `b` as an arg (NOT a multi-script feature — matches pnpm).
- **Regex selection `run /^build:/` runs all matching scripts** (`build:app`+`build:lib`).
- **`config set` has a different home per identity.** A nub project writes the project `.npmrc`. pnpm 12.4.1 writes its global `config.yaml`, and `--location project` writes `pnpm-workspace.yaml` as `storeDir`. Cells grep the distinctive VALUE, not the key.
- **`registry` in `pnpm-workspace.yaml` outranks the project `.npmrc`** in a pnpm project.
- **`npm_config_*` is nub's and not pnpm 12's.** A nub project honors `npm_config_registry` at install time and in `config get`; pnpm 12.4.1 honors it in neither. The install cells point it at an unreachable host: the nub row requires a resolve attempt against that host, the pnpm row requires a successful install that never names it.
- **The engine's `pnpm_config_*` env reader follows the identity** — a pnpm project honors it, as pnpm 12 does; a nub project ignores it, and no `NUB_*` variable takes its place. The gate cells target `store-dir`/`pnpm_config_store_dir` because that is a scalar the reader covers and `config get` surfaces.
- **Another tool's lockfile is never read or rewritten.** A nub project writes `nub.lock` and prints one line pointing at `nub pm migrate`; a pnpm project writes `pnpm-lock.yaml` and prints nothing, as pnpm 12.4.1 does.

### Anti-vacuousness discipline (the guard guarding itself)

This suite exists to catch false greens, so its OWN cells must not be vacuous. Every identity-scoped cell was broken on purpose (its fixture flipped to the other identity, or its expectation removed) and watched go red. Rules a new cell must satisfy:

- **Negative cells need a positive control.** "nub did NOT read the forbidden thing" is only meaningful if the reader is demonstrably live. A `config-file ... ignored` row seeds a distinct `.npmrc` control and must return it; where no control is possible (a HOME file a project `.npmrc` would outrank), the row has an `honored` twin in the other identity.
- **Every `ignored` / absent assertion has an `honored` / present twin** differing only in the fixture: `pnpm_config_store_dir` (nub ignores, pnpm honors), `npm_config_registry` (nub honors, pnpm ignores), the global `config.yaml`, the migrate hint. `BUN_CONFIG_REGISTRY` has no reader in either identity, so its twin is the `npm_config_registry` row that keeps the same `config get registry` env path live.
- **An absence must come from a command that ran.** `echo-hidden` requires the script's marker, and `foreign-lockfile quiet` requires `pnpm-lock.yaml`, so a crash cannot pass as a suppressed echo or a silent install.

## Deferred / covered elsewhere

- **Lockfile round-trip** — fully covered by `tests/conformance/run.sh` and `tests/lockfile-conformance/`. Not duplicated here.
- **nub.jsonc and the neutral `package.json` fields** in a nub project — no cell yet.
- **A pin naming another pnpm version** provisions that pnpm and delegates to it; that needs the network and a provisioned pnpm, so no hermetic cell observes it.

## Adding a cell when a new gap is found

This is the whole point. When a pm-compat gap surfaces:

1. Add a fixture under `fixtures/<name>/` if its shape isn't represented yet.
2. Add an `assertions.tsv` row for the surface + case. For a pnpm project, run the row against pnpm 12.4.1 first (see Usage) — that output is the expectation.
3. Run `run.sh` — red until the gap is fixed; it then becomes the permanent regression guard. Break the fixture once and watch it go red before trusting it.

A gap that lives as a row here can never silently resurface.
