# `setup-nub` — field-for-field `actions/setup-node` compatibility design

> **Status: PROPOSAL (2026-06-15). DESIGN ONLY — not API-locked.** The action's input/output contract is a NEW PUBLIC SURFACE. Every "hard question" decision below is a recommendation that needs maintainer sign-off (see [Decisions needing sign-off](#decisions-needing-sign-off)). The v1 action (`setup/action.yml`, `d9ff98e`) stays the working shipped version; the [proposed action.yml](#proposed-actionyml) here is a draft.

## What `setup-nub` is *for*

`actions/setup-node` does three jobs: (1) **provision a Node toolchain** onto the runner, (2) **cache the package-manager's store** keyed on the lockfile, (3) **write a project `.npmrc`** for authed install/publish. The adoption pitch for `setup-nub` is "swap `setup-node` for `setup-nub` and your existing workflow keeps working" — so for each of those jobs we decide MIRROR / ADAPT / N/A.

The central fact that reshapes everything: **nub already does jobs (1) and (3) itself, at runtime.** nub provisions the project's pinned Node from `.node-version` / `.nvmrc` / `package.json` the first time it runs a file or installs, and nub's PM reads a standard `.npmrc` for registry+auth with no action involvement. So `setup-nub`'s real job is narrower than `setup-node`'s: **put `nub` on PATH, optionally pre-warm the toolchain + store cache, and stay out of nub's way.** The mental model we sell: *`setup-nub` installs the tool; nub handles Node and auth.*

## Ground truth (cited)

| Fact | Source |
|---|---|
| nub cache root: `$XDG_CACHE_HOME/nub` else `~/.cache/nub` (hardcoded `.cache`, even on macOS) | `crates/nub-core/src/node/discovery.rs:809` |
| Provisioned Node toolchains: `<cache>/node/<version>/` (dir name IS the version) | `discovery.rs:817`; `wiki/commands/node-versions.md:33` |
| PM engine cache (packuments, git-clone, node-gyp): `$XDG_CACHE_HOME/nub/pm` | `pm_engine/identity.rs:60-65` (`cache_namespace="nub/pm"`) |
| Global CAS store: `$XDG_CACHE_HOME/nub/store/v11` else `~/.cache/nub/store/v11` | `pm_engine/host_settings.rs` (`storeDir`); confirmed by `nub store path` |
| Per-project virtual store: `node_modules/.store` | `mod.rs` `PROJECT_VIRTUAL_STORE_LEAF` → `nub_setting_defaults` (`virtualStoreDir`) |
| Bare `nub node install` provisions the project pin (manual form of the implicit path) | `node-versions.md:33` |
| `nub <file>` / `nub install` auto-provision the pinned Node if absent | `node-versions.md:15,65` |
| Node-version files read: `.node-version` (precedence #1), `.nvmrc`, `package.json` `engines.node`/`volta`/`packageManager` | `node-versions.md:44`, `discovery.rs:123` |
| Sole Node-version env override: `NODE_EXECUTABLE` (no `NUB_*` for node) | `node-versions.md:78` |
| PM cache-dir env override: `NUB_CACHE_DIR` (neutral `npm_config_*` also honored) | `identity.rs:52-59` |
| nub's PM reads standard `.npmrc`: `registry`, `_authToken`, `<scope>:registry`, `NPM_TOKEN` — never a branded file | `publish_family.rs:25-30`; `pm_engine/host_settings.rs` |
| `nub ci` = frozen + clean install (one step) | `pm_engine/mod.rs:2176`, `install_family.rs:177,797` |
| `nub store path` prints the resolved CAS store dir (scriptable — derive cache paths at runtime, don't hardcode) | `store_config_family.rs:13-15` |
| `nub store add <pkg>` populates the global store without touching `node_modules` (a real CI pre-warm primitive) | `wiki/commands/pm/store.md:28` |
| Resolved-Node-binary command is `nub node which` (not `--which-node`) | `cli.rs:3942-3952` |
| `nub --version` → bare `v<semver>` on **stdout**, resolved Node on **stderr** | `setup/action.yml:37-39`; commit `92d1011` |

## Field-by-field map: every `setup-node` input/output

### Inputs

| `setup-node` input | Decision | `setup-nub` treatment + rationale |
|---|---|---|
| `node-version` | **ADAPT** | Rename intent. nub OWNS Node — it provisions the project's pinned version itself. Expose as **`node-version`** (mirror the name for muscle-memory) but redefine semantics: it does NOT pin/override the project's Node; it **pre-provisions** that version into nub's cache (`nub node install <v>`) so the first `nub <file>`/`nub install` is warm. Empty = let nub resolve+provision the pin lazily on first use (the headline path). See [HQ1](#hq1). |
| `node-version-file` | **ADAPT** | nub reads `.node-version`/`.nvmrc`/`package.json` natively — so a passthrough is mostly redundant. Keep it as a thin pre-warm hint: if set, read that file and `nub node install` the resolved version (same warm-cache benefit, explicit file). Most users set neither and rely on nub's own resolution. See [HQ1](#hq1). |
| `check-latest` | **ADAPT** | In `setup-node` this means "re-resolve the spec against the dist index instead of the tool-cache." For nub it maps to: when pre-provisioning, pass nub's "resolve latest matching" rather than reuse a cached match. LOW value (nub's resolver already does the right thing on first run). Recommend **N/A for v1**, add later if asked. See [HQ1](#hq1). |
| `architecture` | **N/A** | nub picks the runner's native arch automatically (the `@nubjs/nub-<platform>` optional dep is os/cpu-filtered by npm; provisioned Node matches the runner). Cross-arch Node on CI is a rare/expert case nub doesn't expose. Document as unsupported rather than add a knob nobody exercises. |
| `registry-url` | **MIRROR (semantics adapted to neutral file)** | Write a temporary `.npmrc` with `registry=<url>` + `//<host>/:_authToken=${NODE_AUTH_TOKEN}`. **This is brand-clean**: `.npmrc` is the neutral, cross-tool file nub's PM already reads (`publish_family.rs:25`). No branded file. See [HQ3](#hq3). |
| `scope` | **MIRROR** | `<scope>:registry=<url>` line in the same `.npmrc`. Falls back to repo owner for `npm.pkg.github.com` (exact `setup-node` behavior). See [HQ3](#hq3). |
| `always-auth` | **MIRROR (as `always-auth`)** | Writes `always-auth=true` into `.npmrc`. (Note: `setup-node`'s current `action.yml` doesn't list `always-auth` as a named input anymore but `authutil` history + many workflows still pass it; mirror for compat. LOW priority.) See [HQ3](#hq3). |
| `token` | **IGNORE v1** | Accepted for setup-node compatibility; not used by the composite action. |
| `cache` | **ADAPT** | `setup-node`'s `cache: npm\|yarn\|pnpm` selects WHICH pm's store to cache. nub has ONE store regardless of incumbent lockfile, so the value is a **boolean** `cache: true`, not a pm name. When true, cache nub's CAS store + PM cache + provisioned toolchains (paths below). See [HQ2](#hq2) — highest-value field. |
| `package-manager-cache` | **N/A** | This is `setup-node`'s auto-enable-npm-caching heuristic. nub's `cache` is explicit opt-in; no auto-detection layer needed. Fold into `cache`. |
| `cache-dependency-path` | **MIRROR** | The lockfile glob that forms the cache key. nub keys on the project's lockfile (`pnpm-lock.yaml` / `package-lock.json` / `bun.lock` / `yarn.lock`, whichever is present) plus the resolved `.node-version`. See [HQ2](#hq2). |
| `mirror` | **ADAPT (defer)** | Alternative Node-binary download host. Only relevant if we pre-provision Node. nub's provisioner has its own dist host config. Recommend **N/A for v1**; revisit if a GHES/air-gapped user asks. |
| `mirror-token` | **ADAPT (defer)** | Same — defer with `mirror`. |

### Outputs

| `setup-node` output | Decision | `setup-nub` treatment |
|---|---|---|
| `node-version` | **ADAPT/MIRROR** | Emit `node-version` = the version nub resolves for the project (from `nub node which` / the resolved pin), so a downstream step reading `steps.x.outputs.node-version` keeps working. Empty if no provisioning happened. See [HQ4](#hq4). |
| `cache-hit` | **MIRROR** | `true`/`false` from `actions/cache` restore. Only meaningful when `cache: true`. See [HQ4](#hq4). |
| — (new) | **ADD** | `nub-version` = installed nub version (`nub --version`, bare `v<semver>`). This is the v1 action's existing `version` output, **renamed** `nub-version` to disambiguate from `node-version`. See [HQ5](#hq5). |

## The 5 hard questions — recommendations + trade-offs

### HQ1 — Node-version inputs (`node-version` / `node-version-file` / `check-latest` / `architecture`)

**The tension:** `setup-node`'s entire reason to exist is provisioning Node. nub does that itself, lazily, from the project's pin. So does `setup-nub` expose `node-version` at all?

**Three options:**
- **(i) Omit it.** nub owns Node; the action only installs nub. Cleanest mental model, smallest surface. Cost: a `setup-node`→`setup-nub` swap that passed `node-version: 20` now silently ignores it — surprising, and breaks the "drop-in" promise.
- **(ii) Expose `node-version`, pre-provision it.** Action runs `nub node install <v>` so the toolchain is cached/warm before the first `nub` call. Honors the input meaningfully (warms the cache, output reflects it) without lying about ownership. Cost: a little surface; a subtle semantic shift (it pre-warms, doesn't pin — if `.node-version` says 18 and the input says 20, nub still runs the project pin 18 at runtime, and we'd warn on the mismatch).
- **(iii) `node-version-file` passthrough.** Read the file, pre-provision. Redundant with nub's own resolution but explicit.

**RECOMMENDATION: (ii) + a thin (iii).** Expose `node-version` and `node-version-file` as **pre-provisioning hints** (they call `nub node install`), documented as warm-cache optimizations, not pins. Empty inputs = nub resolves+provisions lazily (the headline). **Drop `check-latest` and `architecture` from v1** (N/A — nub's resolver and platform selection already do the right thing; add only on real demand). This keeps the drop-in promise (the input is honored), respects nub's ownership (it's a warm-up, not a pin), and keeps the surface minimal. **Mismatch rule (needs maintainer confirmation):** if the pre-provision input disagrees with the project's resolved pin, the action emits a `core.warning` and nub's runtime pin still wins — the action never overrides the project's declared Node.

### HQ2 — `cache` / `cache-dependency-path` (highest-value field)

**What to cache.** nub's three durable, cross-run-reusable directories:

```
$XDG_CACHE_HOME/nub/store/v11    # global CAS store — the big win, content-addressed packages
                                 #   (else ~/.cache/nub/store/v11)
$XDG_CACHE_HOME/nub/pm           # packument cache, git-clone cache, node-gyp tool cache
                                 #   (else ~/.cache/nub/pm)
$XDG_CACHE_HOME/nub/node         # provisioned Node toolchains (else ~/.cache/nub/node)
```

Do NOT cache `node_modules/.store` (the per-project virtual store) — it's reconstructed from the CAS store on each install and is cheap to relink; caching it fights nub's own reflink/relink path.

> **Correction (2026-09-14 verification pass, superseding the 2026-06-16 one):** the packument cache DOES land under `nub/`, and both of the paths above moved. Measured on a sandboxed `HOME` and `XDG_*`: `nub config get cache-dir` answers `$XDG_CACHE_HOME/nub/pm` and the install creates `nub/pm/v11`; `nub store path` answers `$XDG_CACHE_HOME/nub/store/v11`. Nothing in the sandbox carries another tool's name. Two things the earlier pass got wrong and this one corrects: the store is under the CACHE tier, not the data tier, and its version leaf is `v11`, not `v1`. The standing advice is unchanged and is the reason it survived the move — derive both paths at runtime from `nub store path` and `nub config get cache-dir`, never hardcode them.

**Cache key.** Mirror `setup-node`'s shape:

```
nub-<runner.os>-<arch>-<hash(lockfile)>-<hash(.node-version|.nvmrc|resolved pin)>
restore-keys:  nub-<runner.os>-<arch>-<hash(lockfile)>-     # node toolchain reuse across lockfile bumps
               nub-<runner.os>-<arch>-                      # warm store even on a fresh lockfile
```

Keying on the lockfile (`cache-dependency-path`, default = auto-detect the present lockfile) keeps the CAS store fresh; keying additionally on the node-version file lets the toolchain layer reuse via `restore-keys` when only deps change. The `restore-keys` ladder is what makes this a real speed win (partial-hit warm store beats cold every time).

**Input shape.** `cache: true|false` (boolean, default `false` for v1 — opt-in, matching `pnpm/action-setup`'s conservative default) + `cache-dependency-path` (glob, default = auto-detect). NOT a pm-name enum like `setup-node` — nub has one store.

**RECOMMENDATION:** ship `cache` (boolean) + `cache-dependency-path` in v1; cache the three dirs above with the keyed `restore-keys` ladder; default `cache: false`. This is the single most valuable field and worth getting concrete. **Open sub-question (needs maintainer sign-off):** default `cache` to `true` (aggressive, setup-node-like — most CI wants it) or `false` (conservative, pnpm-like — opt-in)? Recommend **`false` for v1**, flip to `true` once the cache paths prove out on the smoke matrix.

**Implementation primitives (don't hardcode paths).** nub exposes scriptable commands the action should lean on instead of literal paths: `nub store path` prints the resolved CAS store dir (`store_config_family.rs:13-15`) — derive the `actions/cache` path from it rather than hardcoding `~/.cache/nub/store/v11`, so an `XDG_CACHE_HOME` override or a future layout change doesn't silently break caching — the store has already moved tier and version leaf once. For an explicit pre-warm beyond restore, `nub store add <pkg>` populates the global store without touching `node_modules` (`wiki/commands/pm/store.md:28`) — though for CI the simpler warm path is just `nub ci` after a cache restore.

### HQ3 — `registry-url` / `scope` / `always-auth` (auth)

**The brand-boundary check passes cleanly.** `setup-node` writes a project-level `.npmrc` with the registry + `:_authToken=${NODE_AUTH_TOKEN}` and exports `NPM_CONFIG_USERCONFIG` to point at it. nub's PM reads exactly that standard `.npmrc` (registry, `_authToken`, `<scope>:registry`, `NPM_TOKEN` — `publish_family.rs:25`, `pm_engine/host_settings.rs`). So `setup-nub` writes the **identical neutral `.npmrc`** `setup-node` does — no branded file, no `nub`-named config, fully consistent with the symmetric brand boundary (nub never emits its brand into your config and reads only neutral fields).

**RECOMMENDATION: MIRROR `registry-url` / `scope` exactly** (same `.npmrc` lines, same `NODE_AUTH_TOKEN` env contract, same `RUNNER_TEMP/.npmrc` + `NPM_CONFIG_USERCONFIG` export). **Mirror `always-auth`** too. The auth story is byte-for-byte `setup-node`'s — that's the point, and it's brand-clean because the file is neutral. **To confirm:** reuse `setup-node`'s `NODE_AUTH_TOKEN` env-var name (yes — it's the ecosystem convention every workflow already sets; a `NUB_AUTH_TOKEN` rename would break the drop-in and add brand surface for no gain).

### HQ4 — Outputs

**RECOMMENDATION:**
- `nub-version` — installed nub (`nub --version`, bare `v<semver>`). (v1's `version`, renamed.)
- `node-version` — the Node version nub resolves for the project (mirrors `setup-node`'s output name so downstream steps keep working). Empty when nothing was provisioned.
- `cache-hit` — boolean from cache restore (only meaningful with `cache: true`).

**To confirm:** is emitting `node-version` worth the extra `nub node which` call on every run even when no input was given? Recommend yes — it's cheap and preserves the drop-in output contract.

### HQ5 — Version-selection naming (`version` vs `node-version`)

**The collision:** v1's input is `version` (nub's version). `setup-node`'s is `node-version` (Node's version). A user swapping actions will be confused about which "version" is which.

**RECOMMENDATION: rename v1's `version` → `nub-version`** (with `version` kept as a deprecated alias for one minor, emitting a `core.warning`). Then the surface reads unambiguously:
```yaml
with:
  nub-version: 0.0.44      # which nub
  node-version: 20         # pre-provision this Node (warm cache)
```
This is the cleanest disambiguation and matches `oven-sh/setup-bun`'s `bun-version` convention. **This is a public-API rename — needs maintainer sign-off**, and the deprecation-alias window is a courtesy since the action is new and barely adopted (could also hard-rename now).

## Proposed `action.yml`

> Draft proposal. Keeps the v1 install mechanism; adds the inputs/outputs above. The pre-provision + cache + npmrc logic shown as bash steps (composite action — no JS bundle needed yet; if the cache logic grows we migrate to a `node24` JS action like `setup-node`).

```yaml
name: "Setup nub"
description: "Install the nub CLI on a GitHub Actions runner, optionally pre-provision Node and cache nub's store."
author: "nubjs"

branding:
  icon: "package"
  color: "purple"

inputs:
  nub-version:
    description: "Version of nub to install — any semver range npm understands (0.0.44, ^0.0, latest). Default: latest."
    required: false
    default: "latest"
  node-version:
    description: "Pre-provision this Node version into nub's cache so the first run is warm. A warm-up hint, NOT a pin — nub still runs the project's declared Node at runtime. Default: nub resolves and provisions the project pin lazily."
    required: false
  node-version-file:
    description: "Read a Node version from this file (.node-version, .nvmrc, package.json) and pre-provision it. Redundant with nub's own resolution; an explicit warm-up alternative to node-version."
    required: false
  cache:
    description: "Cache nub's global store, PM cache, and provisioned Node toolchains across runs. Default: false."
    required: false
    default: "false"
  cache-dependency-path:
    description: "Lockfile path(s) whose hash keys the cache. Supports globs / newline-delimited lists. Default: auto-detect the project's lockfile."
    required: false
  registry-url:
    description: "Registry to set up for auth. Writes a temporary .npmrc (neutral, the file nub's PM reads) and wires auth to env.NODE_AUTH_TOKEN."
    required: false
  scope:
    description: "Scope for a scoped registry. Falls back to the repository owner for GitHub Packages (npm.pkg.github.com)."
    required: false
  always-auth:
    description: "Write always-auth=true into the temporary .npmrc."
    required: false
    default: "false"
  token:
    description: "Accepted for setup-node compatibility; ignored in v1."
    required: false

outputs:
  nub-version:
    description: "The installed nub version (nub --version, a bare v<semver>)."
    value: ${{ steps.install.outputs.nub-version }}
  node-version:
    description: "The Node version nub resolves for the project. Empty when nothing was provisioned."
    value: ${{ steps.provision.outputs.node-version }}
  cache-hit:
    description: "Whether nub's store cache was restored (only meaningful with cache: true)."
    value: ${{ steps.cache.outputs.cache-hit }}

runs:
  using: "composite"
  steps:
    - name: Install nub
      id: install
      shell: bash
      run: |
        set -euo pipefail
        npm install -g "@nubjs/nub@${{ inputs.nub-version }}"
        installed="$(nub --version 2>/dev/null)"
        echo "Installed nub ${installed}"
        echo "nub-version=${installed}" >> "$GITHUB_OUTPUT"

    # Restore step (actions/cache) — paths + key per HQ2. Shown conceptually;
    # the real implementation uses actions/cache@v4 with the restore-keys ladder.
    # paths:
    #   ~/.cache/nub/store/v11        (or $XDG_CACHE_HOME/nub/store/v11)
    #   ~/.cache/nub/pm               (or $XDG_CACHE_HOME/nub/pm)
    #   ~/.cache/nub/node             (or $XDG_CACHE_HOME/nub/node)

    - name: Configure registry auth
      if: inputs.registry-url != ''
      shell: bash
      run: |
        # Write neutral temporary .npmrc (registry, scope:registry, _authToken=${NODE_AUTH_TOKEN},
        # always-auth) into $RUNNER_TEMP/.npmrc and export NPM_CONFIG_USERCONFIG — byte-for-byte
        # the setup-node authutil contract. nub's PM reads this standard file.

    - name: Pre-provision Node
      id: provision
      if: inputs.node-version != '' || inputs.node-version-file != ''
      shell: bash
      run: |
        # Resolve version from node-version (preferred) or node-version-file, then:
        #   nub node install <version>
        # Warn if it disagrees with the project's resolved pin (nub's runtime pin wins).
        # echo "node-version=<resolved>" >> "$GITHUB_OUTPUT"
```

## Testing plan (the smoke matrix)

A `.github/workflows/setup-smoke.yml` (in nub's repo) matrixed across `ubuntu-latest`, `macos-latest`, `windows-latest`, triggered on PRs touching `setup/**` and on a schedule. Asserts:

1. **Install + PATH** — `uses: ./setup` with `nub-version: latest`; then `nub --version` succeeds and the `nub-version` output is a bare `v<semver>` (regex). Same for a pinned `nub-version: 0.0.44`.
2. **Bare run works** — check out a tiny fixture with a `.node-version` + a lockfile; `nub install` then `nub <script.ts>` succeeds (proves nub provisions Node + installs with no other setup).
3. **Pre-provision** — pass `node-version: 20`; assert `nub node ls` shows 20 cached and the `node-version` output reflects the project pin (and a mismatch with the fixture's `.node-version` emits the documented warning).
4. **Cache round-trip** — two sequential jobs (or a re-run) with `cache: true`; assert `cache-hit: false` then `cache-hit: true`, and that the store dirs exist after restore. Measure cold-vs-warm `nub install` wall time as an informational log line (not a hard gate — perf is noisy).
5. **Auth `.npmrc`** — pass `registry-url` + a dummy `scope`; assert `$RUNNER_TEMP/.npmrc` contains the expected `registry`, `<scope>:registry`, and `:_authToken=${NODE_AUTH_TOKEN}` lines, and `NPM_CONFIG_USERCONFIG` is exported. (No real publish — just file-shape assertions.)
6. **Windows specifically** — the action's bash steps run under Git Bash on `windows-latest`; assert the install + `nub --version` + cache paths resolve (`$XDG_*` unset → `~/.cache/nub`, `~/.local/share/nub` under the bash `$HOME`). This is the leg Docker can't cover.

Keep it one tight workflow, real captured assertions, no invented output.

## The `@v0` tag must exist (blocking adoption note)

**`setup/README.md` tells users `uses: nubjs/nub/setup@v0` — but there is NO `v0` tag.** Only `v0.0.13`…`v0.0.44` exist (`git tag | grep '^v0'`). A bare `@v0` ref does NOT resolve to "latest v0.0.x"; GitHub Actions requires an actual `v0` ref (tag or branch) for `@v0` to work. **A floating `v0` tag (or branch) pointing at the latest released `setup/` action must be created and force-updated on each release**, kept SEPARATE from the immutable `v0.0.x` npm-release tags. Until that exists, every documented `@v0` usage 404s. This is a release-process action item, not a design decision — but it blocks the whole adoption story, so flagging it loudly.

Recommended: a `v0` branch (or lightweight tag) the release workflow advances to `main`'s `setup/action.yml` whenever it changes; document `@v0` (moving) and `@v0.0.44` (pinned-to-a-release) both as valid, exactly like `actions/checkout@v4` vs `@v4.1.7`.

## Decisions needing sign-off

1. **`version` → `nub-version` rename** (HQ5) — public-input rename. Hard-rename now, or keep `version` as a deprecated alias for one minor? **Recommend: rename, with a one-minor warning alias** (cheap, action is barely adopted).
2. **Expose `node-version` as a pre-provision hint** (HQ1, option ii) vs omit Node inputs entirely (option i). **Recommend: expose as warm-up hint, document it's not a pin, warn on mismatch.**
3. **Accept and ignore `token` / `check-latest` / `architecture` / `mirror` / `mirror-token` in v1** (setup-node compatibility).
4. **`cache` default** (HQ2) — `false` (opt-in, pnpm-like) vs `true` (aggressive, setup-node-like). **Recommend: `false` for v1, flip after the smoke matrix proves the paths.**
5. **`cache` is a boolean, not a pm-name enum** (HQ2) — confirm nub's single-store model means we diverge from `setup-node`'s `cache: npm|yarn|pnpm`. **Recommend: boolean.**
6. **Cache the three dirs** (`store/v11`, `pm`, `node`), NOT `node_modules/.store`; key on lockfile + node-version with the `restore-keys` ladder (HQ2). **Recommend: as specified.**
7. **Reuse `NODE_AUTH_TOKEN`** (not `NUB_AUTH_TOKEN`) for the `.npmrc` auth token (HQ3). **Recommend: reuse — ecosystem convention, brand-clean, drop-in.**
8. **Emit `node-version` output unconditionally** (extra `nub node which` per run) vs only when provisioning happened (HQ4). **Recommend: unconditional — cheap, preserves contract.**
9. **Create + maintain a floating `v0` ref** separate from `v0.0.x` (release-process, but blocks adoption). **Recommend: do it before promoting the action.**
