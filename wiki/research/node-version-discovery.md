# Node version discovery and pin-file resolution

> Research target: how should Nub discover the user's installed Node binaries across version managers, and how should it resolve project pin-files (`.nvmrc`, `engines.node`, `.tool-versions`) to a specific binary at spawn time?
>
> TL;DR: ship a layered discovery — pin-file parse → PATH probe → known-layout scan — with a small mtime-keyed cache. The mechanism is a CLI behavior, not a runtime augmentation, so it runs identically in compat mode.

Sibling read: [[research/cold-start]] (why every probe costs against the budget).

## 1. Why Nub has to solve this

Nub runs the user's installed Node, and picking *which* Node the user wants is a mess of shell-specific glue:

- `nvm` is a shell function. It doesn't put a `node` on `$PATH` until the user runs `nvm use` (or an `~/.zshrc` hook fires). A Rust binary inheriting a non-interactive shell PATH will often see the wrong Node, or no Node at all.
- `.nvmrc` is a pin file, not a switch. Reading it gives a semver-ish string; mapping that to a binary is still to do.
- `engines.node` is a *range*, not a pin. The user's intent is "any Node in this range," so the operation is matching installed Nodes against the range.
- Volta intercepts `node` via shims and re-routes based on `volta.node` — but only if its shim dir is on PATH and the user invoked through it. Nub cannot assume that.
- mise activates by mutating PATH on prompt redraw. Inside a Rust child process the mutation is already baked in, but only if the shell was activated.
- asdf shims add ~120 ms per `node` invocation and re-exec through `asdf exec`. Spawning the shim makes every Nub invocation pay that cost; bypassing it ignores the user's `.tool-versions` pin.

The design goal: **`nvm use` should be unnecessary.** If the user has `.nvmrc` (or `engines.node`) and has installed the matching Node anywhere a known manager would put it, `nub script.ts` should work with no shell hook, no `use`, and no "please install" prompt for a version they already have.

## 2. Pin-file conventions matrix

Ordered by how common they are in the field and how unambiguous the format is.

A row here documents the ecosystem; it is not a claim that Nub reads the file as a pin. The priority list below names the five sources Nub actually resolves.

| File | Source/owner | Format | Aliases / ranges? | Multi-tool? |
|---|---|---|---|---|
| `package.json` `devEngines.runtime` | the devEngines proposal (npm) | An object, or an array of them, each `{ name, version, onFail? }`. Nub reads the entry whose `name` is `node`. | Both — semver ranges, plus the aliases `latest`, `node`, `lts`, `lts/<codename>`, `rc/<name>`. | Yes — an entry may name a non-Node runtime (bun/deno/workerd). With no node entry, each entry's `onFail` governs: the default is `error` for the object form and the last array element (refuses the run) and `ignore` for earlier elements; `warn` notices and defers to the next pin source. |
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

Out of scope:

- `.n-node-version` (the `n` tool): historically unused; `n` reads `.nvmrc` and `.node-version`.
- `proto.toml` (moonrepo's proto). Niche enough that Nub does not read it.
- `tea.lock` / `pkgx.lock`. Both can pin Node, but for an entirely different installation system; pkgx users aren't asking Nub to discover from their pkgx store.
- Renovate/Dependabot config files. These describe *update policy*, not an active pin.

### Priority order when multiple are present

When a project carries several at once, Nub picks the most specific signal. This is the order that shipped, in [[crates/nub-core/src/node/discovery.rs#resolve_pin_chain]] with the middle three resolved by [[crates/nub-core/src/node/discovery.rs#walk_up_for_pin]]:

1. `package.json` `devEngines.runtime` (explicit, structured, and the only one that can name a non-Node runtime).
2. `.node-version` (Node-specific).
3. `.nvmrc` (Node-specific, most common).
4. `.tool-versions` `nodejs` line (asdf/mise, polyglot — one tool among many).
5. `package.json` `engines.node` (range, often advisory rather than pin).

The gradient is specificity of intent: a deliberately-added Node-specific pin file outranks the polyglot asdf/mise file. A project carrying only `.tool-versions`, the common asdf/mise case, never hits the conflict.

Nub reads no `volta.node` field and no `mise.toml`; Volta and mise participate only through the shell `PATH`, which the discovery probe consults first.

A user with both `.nvmrc` (v22.15.0) and `engines.node` (`>=22`) gets v22.15.0 — the nvmrc is the *operational* pin, `engines.node` the compatibility floor. With only `engines.node` present, Nub resolves the range against installed Nodes (§5).

Nub emits a one-line note when files disagree only if the chosen file's version doesn't satisfy the other file's constraint; otherwise it stays silent.

## 3. Version manager install-layout matrix

Verified by local inspection (the machine checked has nvm), project docs, and 2025–2026 write-ups. Paths are parameterized on `$HOME` (Linux/macOS) and `%LOCALAPPDATA%` / `%APPDATA%` / `%USERPROFILE%` (Windows).

| Manager | macOS / Linux layout | Windows layout | Override env var | Notes |
|---|---|---|---|---|
| **nvm** | `$NVM_DIR/versions/node/v<X.Y.Z>/bin/node`, default `~/.nvm` | n/a (nvm-windows is a separate project) | `NVM_DIR` | Aliases in `$NVM_DIR/alias/`; `default` file contains the default alias. Plain text. |
| **nvm-windows** | n/a | `%NVM_HOME%\v<X.Y.Z>\node.exe`, plus a `nodejs` symlink at `%NVM_SYMLINK%` | `NVM_HOME`, `NVM_SYMLINK` | Different project from nvm-sh/nvm. Symlink layout, not per-version `bin/` dir. |
| **fnm** | `$XDG_DATA_HOME/fnm/node-versions/v<X.Y.Z>/installation/bin/node`; default `~/.local/share/fnm/...` on Linux, `~/Library/Application Support/fnm/...` on macOS | `%LOCALAPPDATA%\fnm\node-versions\v<X.Y.Z>\installation\node.exe` | `FNM_DIR` | Note the `installation/` subdir under each version. On Windows there is no `bin/` subdir. |
| **Volta** | `$VOLTA_HOME/tools/image/node/<X.Y.Z>/bin/node`, default `~/.volta` | `%LOCALAPPDATA%\Volta\tools\image\node\<X.Y.Z>\node.exe` | `VOLTA_HOME` | Versions stored unprefixed (`22.15.0`, not `v22.15.0`). Shim dir at `$VOLTA_HOME/bin`. |
| **asdf** | `$ASDF_DATA_DIR/installs/nodejs/<X.Y.Z>/bin/node`, default `~/.asdf` | `%USERPROFILE%\.asdf\installs\nodejs\<X.Y.Z>\node.exe` (asdf on Windows is best-effort) | `ASDF_DATA_DIR` | Plugin name is `nodejs`, not `node`. Shims at `$ASDF_DATA_DIR/shims/node`. |
| **mise** | `$MISE_DATA_DIR/installs/node/<X.Y.Z>/bin/node`, default `${XDG_DATA_HOME:-~/.local/share}/mise/installs/node/...` | `%LOCALAPPDATA%\mise\installs\node\<X.Y.Z>\node.exe` | `MISE_DATA_DIR`, `XDG_DATA_HOME` | Plugin name is `node` (not `nodejs`). On macOS without `XDG_DATA_HOME` set, mise still uses `~/.local/share/mise`; it does not follow Apple's `~/Library/Application Support` convention. |
| **n** | `$N_PREFIX/n/versions/node/<X.Y.Z>/bin/node`, default `/usr/local/n/versions/node/...` | n/a (n is POSIX-only) | `N_PREFIX` | Unprefixed version dirs. `n` also drops a `node` directly into `$N_PREFIX/bin/`, which is what PATH usually points at. |
| **nodenv** | `$NODENV_ROOT/versions/<X.Y.Z>/bin/node`, default `~/.nodenv` | n/a (POSIX-only in practice) | `NODENV_ROOT` | Shims at `$NODENV_ROOT/shims`. The plugin layer is irrelevant to discovery; read `versions/`. |
| **nvs** | `$NVS_HOME/node/<X.Y.Z>/<arch>/bin/node`, default `~/.nvs` on POSIX | `%LOCALAPPDATA%\nvs\node\<X.Y.Z>\<arch>\node.exe` | `NVS_HOME` | Per-arch subdir (`x64`, `arm64`). Multi-remote support (`node`, `nightly`, `chakra`) — we want the `node/` remote. |
| **Homebrew** | `/opt/homebrew/opt/node@<X>/bin/node` (Apple Silicon), `/usr/local/opt/node@<X>/bin/node` (Intel). Unpinned `node` formula at `/opt/homebrew/bin/node`. | n/a | `HOMEBREW_PREFIX` | Versioned formulae are major-only (`node@22`, `node@24`). Exact-version discovery from brew is unreliable; treat as fallback. |
| **System package mgr** | `/usr/bin/node`, `/usr/local/bin/node` | `C:\Program Files\nodejs\node.exe` | — | Single version per machine; treat as last resort. |
| **Corepack / npmjs.org tarball** | wherever the user extracted it | wherever | — | Out of scope for autodetection. User can pass `--node`. |

Easy places to get this wrong:

- **fnm**'s `installation/` subdir sits between `v<X.Y.Z>/` and `bin/`. nvm does not have this subdir.
- **mise** uses plugin slug `node`; **asdf** uses `nodejs`. Shared scanning code must parameterize the slug.
- **nvs** has an architecture subdir between version and `bin/`.
- **Volta** stores versions unprefixed; nvm prefixes with `v`. Normalize at parse time.
- The XDG fallback for fnm and mise on macOS is `~/.local/share`, not `~/Library/Application Support`, despite fnm's docs mentioning the macOS path. Defaults differ across releases; scan both.

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

- **Step 1 fast path.** If PATH already resolves to a satisfying Node, the scan is skipped — the common case (the developer already ran `nvm use`, or the project's pin matches the system default). Costs one `which` plus one cached `node --version`: ~1 ms warm, ~30 ms cold.
- **Step 2 scan.** Each manager probe is a directory read, no spawn: list `<install-root>/<scan-pattern>` and parse version strings from directory names, skipping managers with non-existent install roots. On a typical dev machine 2–3 managers have anything; cost is dominated by directory enumeration — measure, but expect <5 ms total.
- **Step 3 choose.** Heuristics in order:
  1. Active-shell match wins (least surprise). The user's `which node` *is* the answer if it's compatible.
  2. Same-manager preference. If the pin file is `.nvmrc`, prefer an nvm install over a same-version asdf install; the pin file's author probably uses the matching manager. Skipped if the matching install isn't present.
  3. Highest semver in the satisfying set, so range pins pick up the freshest install.

### Per-invocation PATH adjustment

When discovery resolves a non-PATH Node, Nub prepends the chosen `bin/` dir to the child's PATH only. It does not touch the parent shell's PATH (it can't anyway) and does not write to the user's rc files.

So `nub script.ts` is self-contained: the child Node, npm, and any spawned subprocess see the right PATH; the user's shell sees what it always saw.

This composes with hijack-by-default: Nub's own shim dir is prepended ahead of the discovered Node, so `child_process.spawn('node', ...)` from inside the script re-enters Nub with the same discovery cache.

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

The "you appear to use" line picks the manager with the most installed Nodes — a cheap heuristic with good results.

## 5. Semver resolution for `engines.node`

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

- **Active Node wins if it satisfies.** A user with `engines.node: ">=22"` and PATH-Node = 24.0 gets 24.0, not the highest installed 25.x. Consistent with `npm install`, whose engines check uses the active Node.
- **Never error on engines alone.** The `engines.node` field is advisory: if no installed Node satisfies, warn loudly and run with the active Node. The exception is Nub's own augmented-mode floor (Node 22.15+), a hard error with a different message.
- **Multiple ranges (peer deps).** Out of scope — only the root `package.json` is read. Workspace package.jsons' `engines.node` are ignored; peer-dep range conjunction is npm's job.
- **The `engines-strict` field in `.npmrc`.** Ignored. It is an npm-install concept and Nub's runtime path doesn't enforce it. Nub may warn on `engines-strict=true` + violation, but does not refuse to run.

### Alias resolution

For `.nvmrc` containing `lts/*` or `node`:

- `lts/*` → highest installed Node whose major is a current LTS (lookup table updated with each Node major release).
- `node` → highest installed Node, period.
- `lts/<codename>` → mapping table (`iron` → 20, `jod` → 22, `<future>` → …).
- `system` → first `node` on PATH outside any known manager root.

The LTS-name → major-version table lives in Nub's source; bumping it is a one-line change per Node major. For an unrecognized alias, shell out to nvm/fnm/asdf/mise (`<manager> which`) only as a last resort, and only if that manager provided the pin file. Tolerable cost: ~30–150 ms on the cold path, cached.

## 6. Caching

Discovery is hot — every `nub` invocation does it. The cache:

- **In-memory per-invocation.** Trivial.
- **On-disk across invocations.** Single cache file at `${XDG_CACHE_HOME:-~/.cache}/nub/node-discovery.json` (Linux/macOS) / `%LOCALAPPDATA%\nub\Cache\node-discovery.json` (Windows).
- **Cache key:** `(cwd-pin-file-content-hash, set of (manager-root, manager-root-mtime), $PATH, OS)`.
- **Value:** resolved Node binary path + version.

Invalidation:

- Any pin-file content change → recompute.
- Any version-manager install-root mtime change → recompute (user installed/removed a Node).
- PATH change → recompute.
- The `node --version` result is itself cached by `(path, mtime)` for auto-flag-injection; the discovery cache piggybacks on it.

Steady-state cost: one stat per manager root seen (typically 3–5), one read of the cache file, one PATH compare. Sub-millisecond.

Cold-cache cost: full scan. On the machine checked, 12 nvm versions + 2 `n` versions in `~/.nvm` and `/usr/local/n` cost <1 ms of directory enumeration — negligible against Node's own ~27 ms macOS spawn floor.

## 7. Compat mode interaction

Discovery is a CLI behavior, not a runtime augmentation. Under `--node` / `NODE_COMPAT=1`:

- Discovery still runs; the user's pin-file intent is honored.
- Flag injection does not run. The discovered Node is spawned with the user's argv plus nothing.
- Hooks/preloads are not registered.
- The PATH adjustment is still applied, so child processes spawned by the user's script also see the discovered Node.

Rationale: a user with `.nvmrc` saying v22 who runs `nub --node script.js` expects Nub to find v22 — they're asking for vanilla Node, not "vanilla Node plus please ignore my nvmrc." The compat flag turns off the runtime layer, not the CLI layer.

One exception: in compat mode a discovered Node older than Nub's augmented floor (22.15+) is fine. The floor is enforced, as a hard error, in augmented modes only.

## 8. How other version managers solve this

- **Volta** is the closest UX precedent, auto-switching Node from `volta.node` in `package.json` whenever the user invokes `node` through its shim. Differences: (a) Volta reads only its own pin — `.nvmrc`/`.tool-versions`/`engines` are ignored; (b) Volta requires its shim on PATH, while Nub is the binary the user invokes (`nub script.ts`); (c) Volta installs Nodes itself, and Nub never does.
- **mise** does universal pin-aware discovery, but only through shell activation or `mise exec`. Good UX if the shell hook was set up; Nub bypasses the hook requirement.
- **fnm** auto-switches on `cd` with a shell hook. Same caveat as mise.
- **Bun** ignores all of this — it *is* the Node, so there is no discovery problem. Same for **Deno**.


Nub explicitly does not install Nodes — that stays the manager's job, and Nub's error message points at the right install command for the manager the user appears to use.

## 9. Failure modes worth naming

The cases where discovery must produce an explicit error or accept a known cost, rather than fall through to whatever `node` happens to be on PATH.

- **Pin file names a version no manager installed.** Hard error with an install hint. Silently falling through to "whatever node is on PATH" is a bug-attractor.
- **PATH `node` is a Volta/asdf/mise shim that re-execs.** The PATH probe sees the shim, runs `--version`, gets the right answer, and uses the shim path. Costs one shim-exec (~30–120 ms) per cache miss. Resolving through the shim instead would need manager-specific knowledge.
- **User has `nvm` in the shell but Nub launched from a GUI (no rc sourced).** Discovery finds the nvm install dir directly and ignores PATH. This is the main case Nub wins on.
- **Stale cache after `nvm install <x>`.** The manager-root mtime changes when a version is added; the cache busts on the next probe.
- **Windows path-separator landmines.** All paths above are parameterized; the parser normalizes `/` vs `\` and case-folds drive letters.
- **Symlinks.** `which node` returns the symlink and `realpath` the manager-store path. Key the cache on `realpath` so Homebrew bottle relinks and nvm symlink shuffles don't poison it forever.

## Decisions captured here

What this document settles: the pin-file priority order, the ordering of the discovery layers, and the per-manager scan order within the known-layout layer.

- **Pin priority order (as shipped):** `devEngines.runtime` > `.node-version` > `.nvmrc` > `.tool-versions` > `engines.node`. Range beats exact only when the exact pin doesn't exist on disk. No `volta.node` or `mise.toml` pin is read.
- **Discovery layer ordering:** PATH match → known-layout scan → not-installed error. Default per-manager scan order: nvm → fnm → Volta → mise → asdf → n → nodenv → nvs → Homebrew.
- **The `engines.node` field is advisory.** Active Node wins if it satisfies; otherwise highest installed satisfier; otherwise warn and run with active.
- **Cache by pin-file hash + manager-root mtimes + PATH.** Disk cache in the XDG cache dir.
- **Compat mode still discovers.** Discovery is CLI, not runtime.

## Changelog

Every revision to this document, with the date and what changed.

- 2026-08-14 — **Correction:** the pin priority order recorded here was a strawman that the implementation did not adopt. It listed `volta.node` and `mise.toml` as pin sources — Nub reads neither — and ranked `.tool-versions` above the Node-specific files, which is the reverse of what shipped. Both statements of the order now match `discovery.rs`.
- 2026-07-30 — Initial publication.
- 2026-08-28 — Trimmed to the measured findings and current behavior.
