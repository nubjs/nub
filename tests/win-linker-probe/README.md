# Windows linker probe — nubjs/nub#552 / #566 / #576

Throwaway branch-scoped probe (no PR — see `.claude/skills/ci-adhoc-test/SKILL.md`) that tries to
reproduce the Windows-only `failed to link node_modules` family on a real `windows-latest` runner:

| issue | error | reported path shape |
|---|---|---|
| [#552](https://github.com/nubjs/nub/issues/552) | `os error 32` (`ERROR_SHARING_VIOLATION`) | a **file** leaf: `.store/<dep>/node_modules/@css-render/vue3-ssr/coverage/lcov-report/index.html` |
| [#576](https://github.com/nubjs/nub/issues/576) | `os error 183` (`ERROR_ALREADY_EXISTS`) | a **sibling junction**: `.store/@types+body-parser@1.19.6/node_modules/@types/connect` |
| [#566](https://github.com/nubjs/nub/issues/566) | `os error 183` | remove/`rm` |

Both reported paths sit under `node_modules/.store/<RAW dep_path>/…` with no `.tmp-<pid>-` segment,
which narrows the failing call to the `materialize_into(&aube_dir, &aube_dir, …)` sites — the ones
that write straight into the FINAL virtual-store entry with no temp-then-rename staging
(`vendor/aube/crates/aube-linker/src/link.rs` lines 246, 444, 593, 1003, 1158).

## Scenarios

Each runs against the **published** `@nubjs/nub@0.6.0` (the version both reporters ran), so the probe
measures the shipped bug rather than the current tree.

- `A-add-types` — #576's shape: install a tree with `@types/express` (pulls `@types/body-parser` →
  `@types/connect`), then `nub add -D oxlint`. Reporter says this fails "consistently".
- `B-vitepress-naive` — #552's shape: a VitePress + `naive-ui` tree (pulls `@css-render/vue3-ssr`,
  which ships the `coverage/lcov-report/` tree named in the report), then `nub update`, then
  `nub remove`.
- `C-interrupt-resume` — kill an install mid-link, then re-run. Tests the "half-materialized entry is
  treated as cached forever" hypothesis: the direct-write sites have no `remove_dir_all` cleanup on
  error, unlike the staged ones (`materialize.rs:270-273`, `:429-432`).

Each scenario dumps whether `node_modules/.store/<entry>` came out as a real directory (GVS off /
disk-materialized) or a junction (GVS on), because that decides which call site is live.

## Two controls the first run got wrong

Run [30408428542](https://github.com/nubjs/nub/actions/runs/30408428542) came back all-green and
proved nothing, because both variables the hypotheses depend on were pinned to the wrong value:

1. **Defender real-time protection is OFF on GitHub's Windows runners** (`RealTimeProtectionEnabled :
   False` in that log). RTP is the leading candidate for whoever holds the handle that produces
   `os error 32`, so a green run without it falsifies nothing. The workflow now enables it first.
2. **`CI=true` silently flips the layout.** `Linker::new` defaults the global virtual store to
   `!is_ci()` (`vendor/aube/crates/aube-linker/src/builder.rs:14`), so the runner exercised the
   GVS-OFF path while a real user's machine gets GVS-ON — and the two take *different* materialize
   call sites. The workflow now runs both as a matrix.

Scenario C additionally never launched: `Start-Process -FilePath 'nub'` fails with `%1 is not a
valid Win32 application` because `nub` on PATH is an npm shim. Fixed, plus scenario D tests the same
hypothesis deterministically (hand-truncate an entry) rather than by racing an interrupt.

## Reproduce locally

Windows only. `pwsh tests/win-linker-probe/run.ps1`.

The scenario-D hypothesis is **platform-independent and already reproduced on macOS** — a
half-materialized `.store/<entry>` is treated as complete forever:

```sh
mkdir /tmp/p && cd /tmp/p
printf '{"name":"p","private":true,"version":"1.0.0","dependencies":{"express":"4.21.2"}}' > package.json
echo enable-global-virtual-store=false > .npmrc
nub install
rm node_modules/.store/accepts@1.3.8/node_modules/accepts/index.js \
   node_modules/.store/accepts@1.3.8/node_modules/accepts/package.json
rm -rf node_modules/.store/.nub-state
nub install          # => "✓ Already up to date (69 packages)" — entry is NOT repaired
node -e "require('express')"   # => MODULE_NOT_FOUND
rm -rf node_modules && nub install && node -e "require('express')"   # control: works
```
