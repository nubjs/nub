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

## Reproduce locally

Windows only. `pwsh tests/win-linker-probe/run.ps1`.
