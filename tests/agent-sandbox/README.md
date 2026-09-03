# Agent-sandbox e2e harness

Runs nub's main flows inside the **default sandboxes of coding agents** — the Codex CLI's Seatbelt sandbox and Claude Code's sandbox runtime (`srt`) — and snapshots the behavior. Modeled on vite-task's harness ([voidzero-dev/vite-task#561](https://github.com/voidzero-dev/vite-task/issues/561)): install the real agent sandbox CLIs, run the tool inside each **default profile with zero extra allowances**, and commit the snapshots — including failures. A failing flow is a pinned fact about current behavior, not a broken harness; when a fix lands, the snapshot updates with it.

## Why

A growing share of nub invocations happen inside an agent's command sandbox. Those sandboxes are deny-by-default: writes limited to the workspace and temp directories, no network (Codex) or a domain allowlist (Claude Code). A tool "supports" them by degrading gracefully under those rules. This harness is the regression guard for that environment.

## Running it

macOS only (both sandboxes use Seatbelt here).

```sh
cd tests/agent-sandbox
npm install            # pins the real @openai/codex and @anthropic-ai/sandbox-runtime CLIs
node run.mjs           # regenerate snapshots/
node run.mjs --check   # regenerate to a tmp dir, diff against committed snapshots
```

`NUB_BIN` selects the binary under test (default: `nub-dev` on `PATH`). Build it with `make install-dev` first.

## The matrix

| | flows |
| --- | --- |
| `none` (unsandboxed control) | `nub main.ts` (twice — warm path), `nub run hello`, `nub install` with a `file:` dep, `nub install` of a lockfile-pinned registry dep the control run has already warmed into the host store, `nub x cowsay@1.6.0` with a cold workspace-local cache |
| `codex` (`codex sandbox`, `sandbox_mode="workspace-write"`, hermetic `CODEX_HOME`) | same |
| `claude-srt` (`srt` with a settings file shaped like Claude Code's defaults: workspace + temp writable, no network) | same |

The control run separates "the flow is broken" from "the sandbox broke it" — never read a sandbox snapshot without comparing the control.

## What the snapshots pin today

- The **runtime flows work sandboxed**: `nub <file>.ts` and `nub run` pass under both sandboxes. The transpile cache degrades silently when `~/.cache` is read-only (best-effort writes in `crates/nub-native/src/cache.rs`).
- **`nub install` fails on the CAS store write**: `~/.local/share/nub/store` is outside both sandboxes' writable roots, so even a network-free `file:` install fails with `Operation not permitted`. `store-dir` relocates the store; `NUB_CACHE_DIR` does not.
- **`nubx` with a cold cache fails on the network deny** (the cell forces a workspace-local `NUB_CACHE_DIR` so it never reuses the host cache); with a warm host cache the same command works sandboxed, because reuse is read-only.

## Regeneration notes

- Snapshots are regenerated **manually on a maintainer Mac**, like vite-task's (their Claude snapshot explicitly waits for an `srt` binary to regenerate). They are not wired into CI: the sandbox CLIs are pinned devDependencies, but their default profiles change on upstream's schedule, so a CI leg would go red on upstream releases rather than on nub regressions.
- The unsandboxed `nubx` control needs network access to the npm registry.
- Output is normalized (ANSI, paths, versions, durations, hashes, sizes, progress lines) in `run.mjs`. If a snapshot diff is pure noise, extend `normalize()` rather than hand-editing a snapshot.
- Bumping the pinned `@openai/codex` / `@anthropic-ai/sandbox-runtime` versions is a deliberate act: regenerate snapshots in the same commit and read the diff — it is the changelog of what the agent sandboxes changed underneath nub.
