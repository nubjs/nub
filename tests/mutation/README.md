# Lockfile mutation differential (`tests/mutation/`)

The write-path counterpart to the static round trips in [`tests/conformance/`](../conformance/) and [`tests/lockfile-conformance/`](../lockfile-conformance/). A static install can pass while `nub add`, `nub remove` or `nub update` churns the untouched part of a lockfile, de-dups a shared transitive differently from pnpm, or over- or under-prunes on `remove`. This harness runs the same mutation through nub and through real pnpm and compares the results.

## The legs

Real pnpm is the judge, fetched through `npx` at the version the engine tracks (`PNPM_PIN` in `run-mutations.sh`).

- **pnpm** — a pnpm project (`packageManager: pnpm@<pin>`). Real pnpm installs in two copies; nub mutates one and real pnpm the other. Real pnpm must frozen-install nub's mutated `pnpm-lock.yaml` without rewriting it, and the two mutated lockfiles must describe the same graph.
- **nub** — a nub project. nub installs and mutates, writing `nub.lock`, while the reference copy is a pnpm project that real pnpm installs and mutates. A frozen nub install must leave `nub.lock` unchanged. `nub.lock` is pnpm's lockfile format, so real pnpm must also frozen-accept it renamed to `pnpm-lock.yaml` in a copy that declares pnpm, and its graph must equal real pnpm's.

```
1. stage the fixture into two copies:   nub/  and  ref/
2. baseline install in both             (pnpm leg: real pnpm in both; nub leg: nub in nub/, real pnpm in ref/)
3. mutate:                              nub/ -> nub <verb> <args>     ref/ -> pnpm <verb> <args>
4. (a) frozen accept                    the lockfile is byte-identical before and after
5. (b) semantic equivalence             the two mutated lockfiles describe the same graph
```

## The semantic differential — why not `cmp`

Check (a) uses byte-`cmp`, because a frozen install must not rewrite a well-formed lockfile. Check (b) cannot: `add` ordering and incidental key order legitimately differ from run to run, so byte identity would fail a correct mutation. Instead each lockfile is reduced to a normalized, order-insensitive graph and the two graphs are compared.

### `extract-graph.mjs <project-dir>`

Reads the project's `pnpm-lock.yaml`, or its `nub.lock` when there is none, and emits:

```json
{
  "format": "pnpm",
  "direct":   { "<name>": "<declared-spec>", ... },
  "resolved": { "<name>@<version>": <count>, ... }
}
```

- **`resolved`** — the multiset of every `name@version` under `packages:`, with the peer suffix `(...)` stripped. It captures what a mutation changes:
  - **add** (M.1) — the new dependency and its transitives appear.
  - **dedup** (M.3) — a shared transitive kept at one version or at two shows up as one key or two.
  - **prune** (M.5) — removed and kept transitives are absent or present.
- **`direct`** — the root importer's declared specifiers (name → range). `add pkg@^1` must write `^1` verbatim, and `remove` must drop the entry.

### `compare-graphs.mjs <a.json> <b.json>`

Diffs two extracted graphs (exit 0 = equal, 1 = divergence with a readable diff, 2 = error). It checks the `direct` map and the `resolved` multiset exactly.

## Running

```sh
cargo build -p nub-cli
tests/mutation/run-mutations.sh target/debug/nub
tests/mutation/run-mutations.sh target/debug/nub m3-add-dedup   # one fixture
LEGS=nub tests/mutation/run-mutations.sh target/debug/nub       # one leg
KEEP=1 tests/mutation/run-mutations.sh target/debug/nub         # keep the sandbox
```

The harness needs network access and `node`/`npx`. It runs in a hermetic `HOME`/`XDG` sandbox, so neither the machine's `~/.npmrc` nor its stores leak in. CI runs it nightly from `.github/workflows/lockfile-roundtrip.yml`.

## Known-red mutation bugs

`expected-failures.txt` (`<fixture> <leg> <reason>`) lists write-path divergences the differential has caught. The list must shrink: a listed scenario that starts passing fails the run (XPASS-STALE), so the fix and the entry's deletion land in one commit.

## Adding a case

1. `mkdir fixtures/<id>` with a `package.json` — the pre-mutation state, marked `"private": true`.
2. Add a `mutation` file with one line: `add: <args>`, `remove: <args>` or `update: <args>`. The args go to both `nub <verb>` and `pnpm <verb>`; `#` lines are ignored.
3. Add `<id>` to `ALL_FIXTURES` in `run-mutations.sh`.
4. Run it. When nub diverges from real pnpm, record the scenario in `expected-failures.txt` with a precise reason rather than letting it fail silently.

Pick packages with small, stable dependency trees so the resolved set is deterministic. The `is-odd` / `is-even` / `is-number` / `kind-of` cluster suits this: `is-even@1.0.0` depends on `is-odd@^0.1.2`, distinct from `is-odd@3.0.1`, which is the multiple-version case M.3 needs.

## Cases not yet covered

The fixtures cover M.1 (an add with no overlap), M.3 (an add that keeps a second version) and M.5 (a remove that prunes orphans). Each case below is a fixture and a `mutation` line away:

- **M.2** `add <pkg>@<range>` with an explicit caret, tilde or exact spec — declared-spec preservation.
- **M.4** an add that introduces a nested peer or version conflict.
- **M.6** a remove whose transitive is still needed by another dependency — the inverse of M.5.
- **M.7–M.9** `--save-optional`, `--save-dev` and peer adds — bucket correctness.
- **M.10/M.11** `update <pkg>` and a bare `update` — bumps within range.
- **M.12** an add in a workspace member, which must touch only that importer.
- **M.13** an npm-alias dependency. **M.14** forcing two versions where there was one.
