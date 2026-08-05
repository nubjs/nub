# Node version discovery and pin-file resolution

> Research target: how should Nub discover the user's installed Node binaries across version managers, and how should it resolve project pin-files (`.nvmrc`, `engines.node`, `.tool-versions`, etc.) to a specific binary at spawn time?
>
> TL;DR: ship a layered discovery — pin-file parse → PATH probe → known-layout scan — with a small mtime-keyed cache. The mechanism is **a CLI behavior, not a runtime augmentation**, so it runs identically in compat mode. Recommended for **v0 Phase 1**: it is the credible delivery vehicle for the "Nub makes the awkward parts of using Node go away" pitch, and Volta proves the UX bar.

Architecture anchor: `../architecture/augmenter-not-fork.md`. Sibling reads: [`node-embedding-vs-spawn.md`](node-embedding-vs-spawn.md) (covers the discovery surface at high level), `competitive-mise.md` (mise is both prior art and partly a competitor), [`cold-start.md`](cold-start.md) (why every probe costs us against the budget).

## 1. Why Nub has to solve this

Nub runs the user's installed Node. Today, picking *which* Node the user actually wants is a mess of shell-specific glue:

- `nvm` is a shell function. It doesn't put a `node` on `$PATH` until the user runs `nvm use` (or an `~/.zshrc` hook fires). A Rust binary inheriting a non-interactive shell PATH will often see the *wrong* Node — or no Node at all.
- `.nvmrc` is a pin file, not a switch. Reading it gives you a semver-ish string; you still have to map that to a binary.
- `engines.node` is a *range*, not a pin. The user's intent is "any Node in this range," and "match my installed Nodes against the range" is the operation.
- Volta intercepts `node` via shims and re-routes based on `volta.node` — but only if its shim dir is on PATH and the user invoked through it. Nub cannot assume that.
- mise activates by mutating PATH on prompt redraw. Inside a Rust child process, the mutation is already baked in, but only if the shell was activated.
- asdf shims add ~120 ms per `node` invocation and re-exec through `asdf exec`. If Nub spawns the shim, every Nub invocation pays the shim cost; if it bypasses the shim, the user's `.tool-versions` pin is ignored.

The design goal: **`nvm use` should be unnecessary.** If the user has `.nvmrc` (or `engines.node`) and they've installed the matching Node *anywhere a known manager would put it*, `nub script.ts` should Just Work, with no shell hook, no `use`, no "please install" prompt for a version they already have.

## 2. Pin-file conventions matrix

These are the conventions Nub should respect, in order of how common they are in the field and how unambiguous the format is.

| File | Source/owner | Format | Aliases / ranges? | Multi-tool? |
|---|---|---|---|---|
| `.nvmrc` | nvm | One line: version string. Optionally `v`-prefixed. Comments after `#` ignored. | Yes — `lts/*`, `lts/gallium`, `lts/iron`, `node` (latest), `system`. | No (Node only). |
| `.node-version` | nodenv, fnm, n, avn, others | One line: version string. Same shape as `.nvmrc` in practice, though `lts/*`-style aliases are unevenly supported. | Mostly version only. fnm honors `lts-latest`. | No (Node only). |
| `package.json` `engines.node` | npm convention | Semver range string (`">=22.15.0 <24"`). | Range, not alias. | Multi-engine but Node is the one we care about. |
| `package.json` `volta.node` | Volta | Exact version string. | Exact only. | Multi-tool (`volta.npm`, `volta.yarn`, `volta.pnpm`). |
| `package.json` `packageManager` | corepack / proposal | Tool@version. Doesn't pin Node directly but corepack reads it. | Exact. | Single PM entry. |
| `.tool-versions` | asdf, mise (compat) | Lines of `<tool> <version>`. Multiple versions per tool legal. Comments after `#`. | Versions, plus mise-extended (`lts`, `latest`). | Yes — many languages. |
| `mise.toml` / `.mise.toml` | mise | TOML, `[tools]` table: `node = "22"` or `node = "lts"` or array of versions. | mise-extended: `lts`, `latest`, semver-prefix, `prefix:22.15`, `sub-1:lts`, `ref:<git>`. Also `[env]` and `[tasks]` we ignore. | Yes. |
| `.mise.local.toml` / `mise.local.toml` | mise | Same shape as `mise.toml`, gitignored override. | Same. | Yes. |
| `.tool-versions` (mise variant) | mise | Same as asdf format but mise reads it too. | Same as asdf. | Yes. |
| `engines-strict` field in `.npmrc` | npm | Boolean. Doesn't pin a version, but turns `engines.node` into a hard error in npm. | N/A. | N/A. |

Out of scope but worth noting:

- `.n-node-version` (the `n` tool): historically unused; `n` reads `.nvmrc` and `.node-version`.
- `proto.toml` (moonrepo's proto). Niche enough we can skip in v0 and add later if requested.
- `tea.lock` / `pkgx.lock`. Both can pin Node but for an entirely different installation system; users who run pkgx aren't asking Nub to discover from their pkgx store. Skip.
- Renovate/Dependabot config files. These describe *update policy*, not active pin.

### Priority order when multiple are present

If a project somehow has every file at once (it happens — devs inherit configs), Nub picks the most specific signal:

1. `package.json` `volta.node` (exact pin, explicit author intent).
2. `mise.toml` / `.mise.toml` / `.mise.local.toml` `[tools].node` (explicit, structured).
3. `.tool-versions` `nodejs` line (explicit, multi-tool but Node entry is direct).
4. `.nvmrc` (Node-specific, most common).
5. `.node-version` (Node-specific, second-most-common).
6. `package.json` `engines.node` (range, often advisory rather than pin).

A user who has both `.nvmrc` (v22.15.0) and `engines.node` (`>=22`) gets v22.15.0 — the nvmrc is the *operational* pin; `engines.node` is the compatibility floor. If only `engines.node` is present, Nub resolves the range against installed Nodes (algorithm in §5).

Nub emits a one-line note when files disagree only if the chosen file's version *doesn't* satisfy the other file's constraint — otherwise it's silent (loud diagnostics on every invocation are worse than the underlying problem).

## 3. Version manager install-layout matrix

Verified by combination of local inspection (this machine has nvm), project docs, and recent (2025–2026) write-ups. Paths are parameterized on `$HOME` (Linux/macOS) and `%LOCALAPPDATA%` / `%APPDATA%` / `%USERPROFILE%` (Windows).

| Manager | macOS / Linux layout | Windows layout | Override env var | Notes |
|---|---|---|---|---|
| **nvm** | `$NVM_DIR/versions/node/v<X.Y.Z>/bin/node`, default `~/.nvm` | n/a (nvm-windows is a separate project) | `NVM_DIR` | Aliases in `$NVM_DIR/alias/`; `default` file contains the default alias. Plain text. |
| **nvm-windows** | n/a | `%NVM_HOME%\v<X.Y.Z>\node.exe`, plus a `nodejs` symlink at `%NVM_SYMLINK%` | `NVM_HOME`, `NVM_SYMLINK` | Different project from nvm-sh/nvm. Symlink layout, not per-version `bin/` dir. |
| **fnm** | `$XDG_DATA_HOME/fnm/node-versions/v<X.Y.Z>/installation/bin/node`; default `~/.local/share/fnm/...` on Linux, `~/Library/Application Support/fnm/...` on macOS | `%LOCALAPPDATA%\fnm\node-versions\v<X.Y.Z>\installation\node.exe` | `FNM_DIR` | Note the `installation/` subdir under each version. On Windows there is no `bin/` subdir. |
| **Volta** | `$VOLTA_HOME/tools/image/node/<X.Y.Z>/bin/node`, default `~/.volta` | `%LOCALAPPDATA%\Volta\tools\image\node\<X.Y.Z>\node.exe` | `VOLTA_HOME` | Versions stored unprefixed (`22.15.0`, not `v22.15.0`). Shim dir at `$VOLTA_HOME/bin`. |
| **asdf** | `$ASDF_DATA_DIR/installs/nodejs/<X.Y.Z>/bin/node`, default `~/.asdf` | `%USERPROFILE%\.asdf\installs\nodejs\<X.Y.Z>\node.exe` (asdf on Windows is best-effort) | `ASDF_DATA_DIR` | Plugin name is `nodejs`, not `node`. Shims at `$ASDF_DATA_DIR/shims/node`. |
| **mise** | `$MISE_DATA_DIR/installs/node/<X.Y.Z>/bin/node`, default `${XDG_DATA_HOME:-~/.local/share}/mise/installs/node/...` | `%LOCALAPPDATA%\mise\installs\node\<X.Y.Z>\node.exe` | `MISE_DATA_DIR`, `XDG_DATA_HOME` | Plugin name is `node` (not `nodejs`). On macOS without `XDG_DATA_HOME` set, mise still uses `~/.local/share/mise` (it does not follow Apple's `~/Library/Application Support` convention). |
| **n** | `$N_PREFIX/n/versions/node/<X.Y.Z>/bin/node`, default `/usr/local/n/versions/node/...` | n/a (n is POSIX-only) | `N_PREFIX` | Unprefixed version dirs. `n` also drops a `node` directly into `$N_PREFIX/bin/`, which is what PATH usually points at. |
| **nodenv** | `$NODENV_ROOT/versions/<X.Y.Z>/bin/node`, default `~/.nodenv` | n/a (POSIX-only in practice) | `NODENV_ROOT` | Shims at `$NODENV_ROOT/shims`. Plugin layer is irrelevant to discovery; we read `versions/`. |
| **nvs** | `$NVS_HOME/node/<X.Y.Z>/<arch>/bin/node`, default `~/.nvs` on POSIX | `%LOCALAPPDATA%\nvs\node\<X.Y.Z>\<arch>\node.exe` | `NVS_HOME` | Per-arch subdir (`x64`, `arm64`). Multi-remote support (`node`, `nightly`, `chakra`) — we want the `node/` remote. |
| **Homebrew** | `/opt/homebrew/opt/node@<X>/bin/node` (Apple Silicon), `/usr/local/opt/node@<X>/bin/node` (Intel). Unpinned `node` formula at `/opt/homebrew/bin/node`. | n/a | `HOMEBREW_PREFIX` | Versioned formulae are major-only (`node@22`, `node@24`). Exact-version discovery from brew is unreliable; treat as fallback. |
| **System package mgr** | `/usr/bin/node`, `/usr/local/bin/node` | `C:\Program Files\nodejs\node.exe` | — | Single version per machine; treat as last resort. |
| **Corepack / npmjs.org tarball** | wherever the user extracted it | wherever | — | Out of scope for autodetection. User can pass `--node`. |

Spot checks that bit me while writing this:

- **fnm**'s `installation/` subdir between `v<X.Y.Z>/` and `bin/` is the easiest place to get this wrong. nvm does *not* have this subdir.
- **mise** uses plugin slug `node`. **asdf** uses `nodejs`. If we share scanning code between them, parameterize the slug.
- **nvs** has an architecture subdir between version and `bin/`.
- **Volta** stores versions unprefixed; nvm prefixes with `v`. Normalize at parse time.
- The XDG fallback for fnm and mise on macOS is `~/.local/share`, *not* `~/Library/Application Support`, despite fnm's docs mentioning the macOS path. Different defaults across releases; scan both.

## 4. Discovery algorithm (strawman)

Goal: given a parsed pin (exact version, range, or alias), return a verified absolute path to a Node binary, or a structured "not installed" error.

```
discover(pin: ResolvedPin) -> Result<NodeBinary, NotInstalled>:
    candidates: Vec<NodeBinary> = []

    # Step 1. Fast path: PATH-resolved node, if it matches.
    if let Some(node_on_path) = which("node"):
        if let Some(version) = probe(node_on_path):  # cached by path+mtime
            if pin.matches(version):
                return Ok(NodeBinary { path: node_on_path, version })
        candidates.push(NodeBinary { path: node_on_path, version: ... })

    # Step 2. Enumerate installed versions across known layouts.
    for manager in [nvm, fnm, volta, asdf, mise, n, nodenv, nvs, homebrew_versioned]:
        for install in manager.scan():
            candidates.push(install)  # path + version; do not run `--version` yet

    # Step 3. Resolve pin against candidates.
    let matches: Vec<NodeBinary> = candidates.filter(|c| pin.matches(c.version))
    if matches.is_empty():
        return Err(NotInstalled { pin, scanned: managers_seen })

    # Prefer:
    #  (a) the active-shell node if it matches the pin
    #  (b) the manager whose pin file we read (e.g. .nvmrc → prefer nvm install)
    #  (c) the highest semver
    return Ok(choose(matches))
```

Notes on each step:

- **Step 1 fast path.** If PATH already resolves to a satisfying Node, we skip the scan. That's the common case (developer already ran `nvm use`, or the project's pin matches the system default). This step costs one `which` plus one cached `node --version`. Real cost ~1 ms on warm cache, ~30 ms cold.
- **Step 2 scan.** Each manager probe is a directory read (no spawn). We list `<install-root>/<scan-pattern>` and parse version strings from directory names. Skips managers with non-existent install roots. On a typical dev machine 2–3 managers actually have anything; cost is dominated by directory enumeration — measure but expect <5 ms total.
- **Step 3 choose.** Heuristics in order:
  1. Active-shell match wins (least surprise). The user's `which node` *is* the answer if it's compatible.
  2. Same-manager preference. If the pin file is `.nvmrc`, prefer an nvm install over a same-version asdf install. The pin file's author probably uses the matching manager. Skip this rule if the matching install isn't actually present.
  3. Highest semver in the satisfying set. Range pins should pick up the user's freshest install.

### Discovery order configurability

Make it overridable but not loudly. A `~/.config/nub/config.toml` entry like:

```toml
[node-discovery]
order = ["mise", "fnm", "nvm", "volta", "asdf", "system"]
```

…lets the user pin a manager when they have, say, both nvm and asdf installed and want asdf to win. Default order is the table above (Volta first for explicit pins, then the user's manager, then everything else). No CLI flag in v0; this is a setting people configure once or never.

### Per-invocation PATH adjustment

When discovery resolves a non-PATH Node, Nub prepends the chosen `bin/` dir to the child's PATH **only**. We do not touch the parent shell's PATH (we can't, anyway), and we do not write to the user's rc files. This makes `nub script.ts` a self-contained operation: the child Node, npm, and any spawned subprocess see the right PATH; the user's shell sees what it always saw.

This composes with hijack-by-default: Nub's own shim dir is prepended ahead of the discovered Node, so `child_process.spawn('node', ...)` from inside the script still re-enters Nub with the same discovery cache.

### Not-installed error

```
nub: this project's .nvmrc requests Node v22.15.0, but no
     matching install was found.

     Scanned: nvm (3 versions), fnm (not installed), Volta (1
     version), asdf (not installed), mise (not installed).

     To install:
       nvm install 22.15.0     # you appear to use nvm
       # or override for this run:
       nub --node script.ts    # use whatever node is on PATH
```

The "you appear to use" line picks the manager with the *most* installed Nodes already; cheap heuristic, surprisingly good UX.

## 5. `engines.node` semver resolution

A range pin (`">=22.15.0 <24"`) has no canonical answer. Strategy:

```
resolve_range(range: SemverRange) -> ResolvedPin:
    installed = enumerate_all_installed_nodes()  # same Step 2 scan
    satisfying = installed.filter(|n| range.matches(n.version))

    if let Some(active) = which("node").and_then(probe):
        if range.matches(active.version):
            return Exact(active.version)  # least surprise

    if satisfying.is_empty():
        # never error on engines violations alone — degrade gracefully
        return BestEffort(active.unwrap_or(installed.highest()))

    return Exact(satisfying.highest().version)
```

Key decisions:

- **Active Node wins if it satisfies.** A user with `engines.node: ">=22"` and PATH-Node = 24.0 gets 24.0, not the highest installed version 25.x. Most consistent with how `npm install` behaves (engines check uses your active Node).
- **Never error on engines alone.** Per `../runtime/target-version.md`, `engines.node` is advisory. If no installed Node satisfies, we warn loudly and run with the active Node anyway. The exception: Nub's own augmented-mode floor (Node 22.15+). That's a hard error with a different message.
- **Multiple ranges (peer deps).** Out of scope. We read the root `package.json` only. Workspace package.jsons' `engines.node` are ignored in v0 — fixing peer-dep range conjunction is npm's job.
- **`engines-strict` in `.npmrc`.** Ignored. It's an npm-install concept; Nub's runtime path doesn't enforce it. We may warn on `engines-strict=true` + violation, but we don't refuse to run.

### Alias resolution

For `.nvmrc` containing `lts/*` or `node`, Nub needs to map alias to version. Strategy:

- `lts/*` → highest installed Node whose major is a current LTS (lookup table updated with each Node major release).
- `node` → highest installed Node, period.
- `lts/<codename>` → mapping table (`iron` → 20, `jod` → 22, `<future>` → ...).
- `system` → first `node` on PATH outside any known manager root.

The LTS-name → major-version table lives in our source; bumping it is a 1-line change per Node major. If a user has an alias we don't recognize, we shell out to nvm/fnm/asdf/mise (`<manager> which`) only as a last resort and only if that manager actually provided the pin file. Tolerable cost: ~30–150 ms on the cold path, cached.

## 6. Caching

Discovery is hot — every `nub` invocation does it. The cache:

- **In-memory per-invocation.** Trivial.
- **On-disk across invocations.** Single cache file at `${XDG_CACHE_HOME:-~/.cache}/nub/node-discovery.json` (Linux/macOS) / `%LOCALAPPDATA%\nub\Cache\node-discovery.json` (Windows).
- **Cache key:** `(cwd-pin-file-content-hash, set of (manager-root, manager-root-mtime), $PATH, OS).
- **Value:** resolved Node binary path + version.

Invalidation:

- Any pin-file content change → recompute.
- Any version-manager install-root mtime change → recompute (user installed/removed a Node).
- PATH change → recompute.
- `node --version` itself is cached by `(path, mtime)` per `../runtime/auto-flag-injection.md`; the discovery cache piggybacks.

Steady-state cost: one stat per manager root we've seen (typically 3–5), one read of the cache file, one PATH compare. Sub-millisecond.

Cold-cache cost: full scan. On the machine I checked, 12 nvm versions + 2 `n` versions in `~/.nvm` and `/usr/local/n` cost <1 ms of directory enumeration. Negligible compared to Node's own ~27 ms macOS spawn floor.

## 7. Compat mode interaction

Discovery is a **CLI behavior**, not a runtime augmentation. Under `--node` / `NODE_COMPAT=1`:

- Discovery still runs. The user's pin-file intent is honored.
- Flag injection does not run. The discovered Node is spawned with the user's argv plus nothing.
- Hooks/preloads are not registered.
- The PATH adjustment is still applied, so child processes spawned by the user's script also see the discovered Node.

Rationale: if a user has `.nvmrc` saying v22 and runs `nub --node script.js`, they expect Nub to find v22 — they're asking for *vanilla Node*, not "vanilla Node plus please ignore my nvmrc." The compat flag turns off the runtime layer, not the CLI layer.

The one exception worth calling out: in compat mode, if the discovered Node is older than Nub's augmented floor (22.15+), that's fine. We don't enforce the floor in compat. The hard-error "Nub requires 22.15+" applies to augmented modes only.

## 8. Strategic differentiation

Where Nub lands vs prior art:

- **Volta** is the closest UX precedent. Volta auto-switches Node based on `volta.node` in the project's `package.json` whenever the user invokes `node` through its shim. Differences from Nub: (a) Volta only reads its own pin; .nvmrc/.tool-versions/engines are ignored. (b) Volta requires its shim on PATH; Nub is the shim by being the binary the user invokes (`nub script.ts`). (c) Volta installs Nodes itself; Nub never does.
- **mise** does universal pin-aware discovery, but only through shell activation or `mise exec`. mise + Node = good UX *if* you remembered to set up the shell hook. Nub bypasses the hook requirement.
- **fnm** auto-switches on `cd` (with a shell hook). Same caveat as mise.
- **Bun** ignores all of this. It is the Node. No discovery problem to solve.
- **Deno** same.

Nub's wedge: **"Whatever manager you have, whatever pin-file you have, `nub` Just Works without a shell hook."** Volta's autoswitch UX, but on any pin file. mise's universality, but you don't have to remember `mise install` first if the version is already there.

What Nub explicitly does *not* do: install Nodes. That's still your manager's job. Nub's error message points the user at the right install command for the manager they appear to use.

## 9. Failure modes worth naming

- **Pin file says version not installed by any manager.** Hard error with install hint. Don't silently fall through to "whatever node is on PATH" — that's a bug-attractor.
- **PATH `node` is a Volta/asdf/mise shim that re-execs.** Nub's PATH probe sees the shim, runs `--version`, gets the right answer, and uses the shim path. Costs one shim-exec (~30–120 ms) per cache miss; OK. Alternative — resolve through the shim — needs manager-specific knowledge we'd rather not maintain.
- **User has `nvm` in shell but Nub launched from GUI (no rc sourced).** Discovery finds the nvm install dir directly, ignores PATH. This is the *main* case we win on.
- **Stale cache after `nvm install <x>`.** Manager-root mtime changes when a new version is added; cache busts on next probe.
- **Windows path-separator landmines.** All the paths above are parametrized; the parser normalizes `/` vs `\` and case-folds drive letters.
- **Symlinks.** `which node` returns the symlink; `realpath` returns the manager-store path. We key cache on `realpath` so that Homebrew bottle relinks and nvm symlink shuffles don't poison the cache forever.

## 10. v0 vs Phase 2 recommendation

**Ship a v0 (Phase 1) baseline. Defer the long tail to Phase 2.**

The v0 cut, ordered by ROI:

1. PATH probe + `node --version` cache. (Already required by auto-flag-injection.)
2. `.nvmrc` and `.node-version` parsing.
3. `package.json` `engines.node` range resolution against PATH-Node only (no scan in v0).
4. nvm, fnm, Volta install-layout scan — the three managers that cover the bulk of solo-Node-developer users.
5. The not-installed error with manager hint.

Phase 2 additions:

1. `mise.toml` and `.tool-versions` parsing (mise + asdf users).
2. nodenv, nvs, n, Homebrew versioned formulae scanning.
3. Alias resolution via manager shellout (`nvm which lts/*`) for the long tail.
4. User-configurable discovery order.
5. Workspace-aware `engines.node` resolution (currently root-only).

The argument for v0 inclusion is the pitch: **"Nub replaces the awkward parts of using Node."** Asking the user to `nvm use` before `nub script.ts` is exactly the awkwardness Nub is supposed to remove. Without this feature, Nub is "a faster CLI plus TS support." With it, Nub is "I don't think about Node versions anymore." The latter is the marketing story.

Cost to build the v0 cut: probably 1–2 weeks of one engineer. Three pin-file parsers (`.nvmrc`/`.node-version` share a parser, `engines.node` is one line of semver, the rest is a glob+sort scanner per manager). The cache layer is shared with auto-flag-injection. None of it touches Node source — discovery is pure pre-spawn CLI work. It composes cleanly with the rest of Phase 1.

Open questions left for implementation:

- Whether to ever `realpath` chase through Volta shims when the user's project has both a `.nvmrc` and a `volta.node` that disagree. (Probably honor Volta because it's the *exact* pin, but emit a one-line diagnostic. Decide at implementation time.)
- Whether to expose a `nub which node` subcommand that prints the resolved binary. Trivial to add; very useful for debugging. Recommend yes for v0.
- Whether `nub --node` should *also* report the resolved Node when in verbose mode. Nice-to-have.

## Decisions captured here

- **Pin priority order:** `volta.node` > mise > `.tool-versions` > `.nvmrc` > `.node-version` > `engines.node`. Range > exact only when the exact pin doesn't exist on disk.
- **Discovery layer ordering:** PATH match → known-layout scan → not-installed-error. Default per-manager scan order: nvm → fnm → Volta → mise → asdf → n → nodenv → nvs → Homebrew.
- **`engines.node` is advisory.** Active Node wins if it satisfies; otherwise highest installed satisfier; otherwise warn and run with active.
- **Cache by pin-file hash + manager-root mtimes + PATH.** Disk cache in XDG cache dir.
- **Compat mode still discovers.** Discovery is CLI, not runtime.
- **v0 scope: nvm + fnm + Volta + `.nvmrc` + `.node-version` + `engines.node`.** Long tail (mise, asdf, nodenv, nvs, Homebrew versioned, aliases) lands in Phase 2.

## Changelog

- 2026-07-30 — Migrated from the internal research corpus. Links to internal planning documents were removed and reference-checkout paths rewritten; findings, tables and measured values are unchanged.
