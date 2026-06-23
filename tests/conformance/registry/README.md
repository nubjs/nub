# Registry / auth differential conformance harness

Differential matrix that exercises every custom-registry + auth config surface and
diffs nub's resolved registry host + `Authorization` header against the reference
package manager (npm / pnpm / yarn-classic / bun) on identical fixtures. A divergence
is a bug: a silently-wrong registry, or a leaked/missing auth token.

## How it works

Each **cell** (`cells.mjs`) is one registry/auth scenario (default `registry=`, scoped
`@scope:registry=`, host-bound `_authToken`, path-prefix Artifactory registry,
env override, basic-auth, etc.). For each cell and each tool, the harness:

1. writes a hermetic fixture (`package.json` + `.npmrc`/`bunfig.toml`/`.yarnrc.yml`)
   and a throwaway `$HOME`,
2. starts a mock registry on a fresh port (`server.mjs`) that logs every request's
   URL + `Authorization` header and returns 404 to halt resolution fast,
3. runs the tool's install/resolve against the fixture,
4. records which `(url, auth)` the tool attempted.

The registry points at `127.0.0.1:<port>`, so the captured request URL tells you
exactly which configured registry the tool resolved to, and the captured header
tells you which credential it sent there.

## Running

```sh
export NUB_BIN=/path/to/target/fast/nub
export NPM_BIN=/usr/local/bin/npm
export PNPM_BIN=/opt/homebrew/bin/pnpm
export YARN_BIN=/usr/local/bin/yarn   # classic yarn 1.x reads .npmrc registry=
export BUN_BIN=/opt/homebrew/bin/bun
node main.mjs                # all cells -> JSON on stdout
node main.mjs <cell-id>      # one cell
node main.mjs | node report.mjs   # human-readable per-cell comparison
```

## Tool-behavior gotchas (learned building this)

- **bun ignores `.npmrc registry=` entirely.** Bun honors ONLY `bunfig.toml`
  `[install] registry` / `[install.scopes]` (and `BUN_CONFIG_REGISTRY`). A `.npmrc`
  `registry=` in a bun project sends bun to `registry.npmjs.org`. So bun cells use
  `bunfig.toml`; nub mirrors bunfig (not `.npmrc`) for a bun-incumbent project.
- **bun caches common packages** (`is-odd` etc.) and skips the metadata fetch — use
  an uncommon/unique package name + a hermetic `BUN_INSTALL_CACHE_DIR` to force a
  registry hit.
- **yarn**: the globally-installed classic yarn (1.22) wins over corepack's berry on
  PATH and DOES read `.npmrc registry=`. Yarn berry uses `.yarnrc.yml`
  `npmRegistryServer` / `npmScopes` / `npmAuthToken`; berry needs `nodeLinker:
  node-modules` + `unsafeHttpWhitelist` for a plain-http mock.
- **nub yarn is READ-ONLY**: `nub install` against a project with an existing
  `yarn.lock` is BLOCKED (it would rewrite the lock). To differentially test berry
  registry resolution use a read-only command (`nub view <pkg>`), which still reads
  `.yarnrc.yml`.
- nub fires a **speculative `HEAD /` TLS-prewarm** at the registry root(s) before
  resolution (`aube-registry/src/client/lifecycle.rs`). It is fire-and-forget,
  unauthenticated, response-discarded — not a resolution request; ignore it when
  diffing.
