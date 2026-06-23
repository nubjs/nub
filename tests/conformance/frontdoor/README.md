# Front-door pm-compat conformance matrix

The anti-resurfacing guard. Every pm-compat gap that reached a user — `npm_config_reporter` not honored, `--env-file` grammar, regex script selection, the config-read gate, the `npm_config_*` bridge — was rediscovered ad-hoc because only the **engine** and the **lockfile** were ever tested, never the **front door**: the CLI surface a user actually drives (config read/write, env knobs, run/exec flags, lockfile round-trip), *per incumbent package manager*.

This harness turns the per-incumbent behavior map (`wiki/research/nub-incumbent-behavior.md`) into a CI matrix. A gap or regression on a covered cell **fails CI** instead of waiting for a user report.

It is a **third axis**, distinct from its two siblings:

- `tests/conformance/run.sh` — lockfile round-trip fidelity (does nub read/write each PM's lockfile?).
- `tests/conformance/cmdflag/run.sh` — does every wired verb × flag *run* on one real repo?
- **this** — does each front-door SURFACE behave correctly *under each incumbent identity*? It is the only suite parameterized by incumbent.

## The matrix

Two dimensions. **Incumbent** (the project identity nub detects) × **surface** (the front-door behavior class). Each filled cell is one or more `assertions.tsv` rows.

| surface ↓ \ incumbent → | nub-identity | npm | pnpm 9/10 | pnpm 11 | yarn-classic | yarn-berry | bun |
|---|---|---|---|---|---|---|---|
| **config READ honored** | neutral only | npmrc | npmrc + pnpm.* | npmrc + yaml | yarnrc | yarnrc.yml | bunfig |
| **config WRITE home** | npmrc | npmrc | npmrc | pnpm-workspace.yaml | npmrc | npmrc | npmrc |
| **env: `npm_config_*` bridge** | honored | honored | honored | honored | honored | honored | honored |
| **env: branded gating** | no AUBE_*/pnpm_* | no pnpm_* | pnpm_* honored | pnpm_* honored | no pnpm_* | no pnpm_* | BUN_CONFIG_* honored |
| **run/exec flags** | reporter / regex / env-file / filter (incumbent-invariant) |
| **lockfile round-trip** | lock.yaml | package-lock | pnpm-lock v9 | pnpm-lock v9 | yarn.lock v1 | yarn.lock v2+ | bun.lock |

The run/exec-flag surface is **incumbent-invariant** (the run echo, `--env-file`, reporter, regex selection are nub's own CLI, not gated on identity), so it is asserted once rather than per-incumbent — the matrix tracks it as a single column to avoid bloat.

Lockfile round-trip is **already covered comprehensively** by the two sibling harnesses; this harness does not duplicate it. It is listed in the matrix for completeness and links out — see "Deferred / covered elsewhere."

### Scope decisions (recorded here, the thread's open questions)

- **Hybrid assertion model.** Most cells assert **documented behavior** — hermetic, offline, no real PM needed (cheap, gates every PR). The high-traffic front-door cells that historically churned (run reporter/regex/env-file) ALSO get a **real-PM differential** on an opt-in leg (`REF=1`), diffing nub against the actual PM. This is the hybrid the matrix thread recommended: assert-documented for breadth, real-PM-diff for the surfaces that bite.
- **First slice = the surfaces that just churned.** The run reporter/regex/env-file flags, config read+write home per incumbent, and the `npm_config_*` bridge. Everything else in the grid is scaffolded (a fixture exists, the assertion is a TODO row) so a new finding *adds a row* instead of resurfacing.
- **CI cadence.** The documented-behavior core is hermetic + offline (no network, no real PM) → it can gate **every PR**. The `REF=1` real-PM-diff leg needs the PMs installed and (for a couple of cells) the network → **scheduled / opt-in leg**, same posture as the sibling harnesses.

## Cells

`assertions.tsv` is the canonical cell list. Columns (TAB-separated):

| column | meaning |
|---|---|
| `id` | unique slug (`<incumbent>-<surface>-<case>`) |
| `incumbent` | fixture identity: `nub` `npm` `pnpm9` `pnpm10` `pnpm11` `yarn1` `yarnberry` `bun` `-` (incumbent-invariant) |
| `surface` | `config-read` `config-write` `env-bridge` `env-gate` `run-flag` |
| `mode` | `doc` (assert documented behavior, hermetic) · `ref` (also diff vs real PM under `REF=1`) |
| `assert` | the assertion verb (see below) + its args |

Assertion verbs (run by `run.sh`):

| verb | meaning |
|---|---|
| `echo-shown` / `echo-hidden` | run nub with the given env/flags; assert the `$ <cmd>` run-echo is present / suppressed |
| `runs-scripts <a,b,…>` | assert exactly these scripts ran (by their stdout markers) |
| `env-injected <VAR>=<val>` | assert the child process saw the env var (script prints it) |
| `config-reads <key>=<val>` | seed the incumbent's config file with `<key>`, assert nub honors it (`config get` / behavior) |
| `config-ignores <file>` | seed a *foreign-branded* config file, assert nub does NOT read it |
| `config-writes-to <relpath>` | run `nub config set`, assert the value landed in `<relpath>` and nowhere else |

## Usage

```sh
# build the dev nub first (see the nub-dev skill), then:
tests/conformance/frontdoor/run.sh /path/to/nub                 # hermetic doc-mode core (every-PR leg)
REF=1 tests/conformance/frontdoor/run.sh /path/to/nub           # also real-PM diffs (opt-in leg; needs PMs)
tests/conformance/frontdoor/run.sh /path/to/nub run-flag        # one surface
KEEP=1 tests/conformance/frontdoor/run.sh /path/to/nub          # keep sandbox for forensics
```

The runner spins a **hermetic sandbox** `HOME`/`XDG_*` (the dev box's `~/.npmrc` — which carries a dead proxy that breaks fetches — never leaks in; isolation is mandatory). Each cell runs in a fresh copy of its incumbent's fixture. No network in `doc` mode.

## Fixtures

One minimal fixture per incumbent identity under `fixtures/<incumbent>/`. Each is the **smallest** project that makes nub detect that identity (a `packageManager` field or the marker lockfile/config file) plus a `package.json` with the marker scripts the run-flag assertions need. Fixtures are hand-built and tiny by design — the suite tests *identity detection + surface behavior*, not real installs, so most fixtures need no `node_modules`.

| fixture | makes nub detect | marker |
|---|---|---|
| `nub` | nub-identity | no lockfile, no declaration, no pnpm-named file |
| `npm` | npm | `package-lock.json` |
| `pnpm10` | pnpm 9/10 | `packageManager: "pnpm@10.x"` |
| `pnpm11` | pnpm 11 | `packageManager: "pnpm@11.x"` + `pnpm-workspace.yaml` |
| `yarn1` | yarn-classic | `yarn.lock` (v1) + classic `.yarnrc` |
| `yarnberry` | yarn-berry | `.yarnrc.yml` + berry `yarn.lock` |
| `bun` | bun | `bun.lock` + `bunfig.toml` |
| `envbridge` | nub-identity (with a dep) | one real dependency, so the `REF=1` resolver probe actually fetches |

The run-flag fixture is identity-agnostic (the run surface isn't gated on identity) — it reuses `fixtures/nub`.

### Front-door behaviors this slice pinned (corrected against the real binary)

Building the slice surfaced/confirmed several exact behaviors — each is now a guarded cell:

- **`--reporter=silent` suppresses the run-echo only PRE-subcommand** (`nub --reporter=silent run x`), not post-script (`nub run x --reporter=silent`, where it forwards to the script). Same three-position rule as every other nub flag.
- **Space-separated `run a b` runs only `a`** and forwards `b` as an arg (NOT a multi-script feature — matches pnpm).
- **Regex selection `run /^build:/` runs all matching scripts** (`build:app`+`build:lib`).
- **`config set` normalizes kebab→camel in the pnpm-11 yaml home** (`store-dir` → `storeDir:`), but keeps kebab in `.npmrc`. Cells grep the distinctive VALUE, not the key.
- **The `npm_config_*` bridge is a RESOLVER knob, not a config-display value** — `config get registry` does NOT reflect `npm_config_registry` (it reads config FILES). The bridge is observed at install time (`REF=1`) by pointing it at an unreachable host and asserting the resolver ATTEMPTS that host (the host string must co-occur with a fetch/resolve/DNS-failure token — keying on a startup log mention would be a false green), with a hermetic negative cell pinning that `config get` stays file-only.
- **`AUBE_*` env vars are suppressed under the nub embedder profile** — the gate cell targets `store-dir`/`AUBE_STORE_DIR` specifically: aube reads that env in standalone mode AND surfaces it via `config get store-dir`, so the cell can actually OBSERVE the gate (it verifies `config get store-dir` surfaces a value first, then that the `AUBE_STORE_DIR` env does not change it). A setting with no `AUBE_*` env source — e.g. `registry` — would make the cell vacuous (it'd pass even if the gate were broken), so it must not be used here.

### Anti-vacuousness discipline (the guard guarding itself)

This suite exists to catch false greens, so its OWN cells must not be vacuous. Two rules a new cell must satisfy:

- **Negative cells need a positive control.** "nub did NOT read the forbidden thing" is only meaningful if nub demonstrably reads the RIGHT thing on the same probe. The pnpm-named-file ignore cell seeds both a leak (pnpm yaml) and a control (`.npmrc`) and asserts nub returns the control — proving the reader is live, not merely returning a default because it read nothing.
- **Brand-gate cells must target an observable gate.** The setting must be one the unbranded engine actually reads from the branded env AND surfaces through the assertion's read path — otherwise the assertion passes regardless of the gate.

## Deferred / covered elsewhere

- **Lockfile round-trip** — fully covered by `tests/conformance/run.sh` (both directions, all PMs, pnpm-11 leg) and `tests/aube-conformance/`. Not duplicated here.
- **Config-write per-field incumbent-aware shared-ness** (the `pm-config-field-level-audit` known gap) — a TODO row per affected scalar once that audit lands; this harness is its natural regression home.
- **Detection-chain tail** (installed-PM `--version` / lockfile-version-signal refinement, gap G9) — deliberately unwired; no cell until the posture changes.
- **The `REF=1` yarn-berry leg** — host `yarn` is v1; berry round-trip fidelity lives in `aube-lockfile` unit tests (see the sibling README). The berry *config-read* cell here is doc-mode only.

## Adding a cell when a new gap is found

This is the whole point. When a pm-compat gap surfaces:

1. Add a fixture under `fixtures/<incumbent>/` if its identity isn't represented yet.
2. Add an `assertions.tsv` row for the surface + case, `mode=doc` (and `mode=ref` if it's a high-traffic surface worth a real-PM diff).
3. Run `run.sh` — red until the gap is fixed; it then becomes the permanent regression guard.

A gap that lives as a row here can never silently resurface.
