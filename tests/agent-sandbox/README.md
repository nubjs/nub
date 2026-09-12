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
| `none` (unsandboxed control) | `nub main.ts` (twice — warm path), `nub run hello`, `nub install` with a `file:` dep, `nub install` of a lockfile-pinned registry dep the control run has already warmed into the host store (once with integrity in the lockfile, once without), `nub install` of two local tarballs where one imports the other without declaring it, then a probe of which path was materialized, `nub x cowsay@1.6.0` with a cold workspace-local cache |
| `codex` (`codex sandbox`, `sandbox_mode="workspace-write"`, hermetic `CODEX_HOME`) | same |
| `claude-srt` (`srt` with a settings file shaped like Claude Code's defaults: workspace + temp writable, no network) | same |
| `claude-srt-registry` (the same `srt` settings with `registry.npmjs.org` allowlisted — the usual "let the agent install packages" setup; requests go through srt's local proxy) | same |

The control run separates "the flow is broken" from "the sandbox broke it" — never read a sandbox snapshot without comparing the control. The two `install` flows that need the registry (the cold phantom-eject install and the cold `nubx`) run with a fresh, empty data home and a workspace-local cache, so every cell is cold and the host's own store never leaks in.

## What the snapshots pin today

- The **runtime flows work sandboxed**: `nub <file>.ts` and `nub run` pass under every sandbox. The transpile cache degrades silently when `~/.cache` is read-only (best-effort writes in `crates/nub-native/src/cache.rs`).
- **`nub install` works with an unwritable global store.** `~/.local/share/nub/store` is outside the sandboxes' writable roots, so nub warns (`WARN_NUB_STORE_FALLBACK`) and writes new packages to `node_modules/.nub-store`, reading through to the global store for anything it already holds — CAS files, package indexes, and the URL→sha512 bindings an integrity-less lockfile entry needs. The `file:` install and both warm registry installs succeed under every sandbox.
- **A cold install through Claude Code's proxy works, phantom ejection included.** Under `claude-srt-registry` the eject flow fetches `@firebase/database@1.0.8` (which imports `@firebase/app` without declaring it) into the project-local store and still ejects it: the probe shows it materialized inside the project while its control package stays linked out to the virtual store. TLS inside `srt` verifies against nub's bundled roots, because the macOS trust daemon is unreachable from that sandbox.
- **A denied network fails fast and says so.** With no registry access (Codex, and `srt` without an allowlist) the cold flows fail in one attempt — no retry ladder — and the error names the deny (`network access denied … tunnel error: unsuccessful`) with help that names the sandbox instead of suggesting `npm login`. The snapshots for the two no-network sandboxes differ only in the sandbox's name and the OS-level cause.

## Regeneration notes

- Snapshots are regenerated **manually on a maintainer Mac**, like vite-task's (their Claude snapshot explicitly waits for an `srt` binary to regenerate). They are not wired into CI: the sandbox CLIs are pinned devDependencies, but their default profiles change on upstream's schedule, so a CI leg would go red on upstream releases rather than on nub regressions.
- The unsandboxed `nubx` control needs network access to the npm registry.
- Output is normalized (ANSI, paths, versions, durations, hashes, sizes, progress lines) in `run.mjs`. If a snapshot diff is pure noise, extend `normalize()` rather than hand-editing a snapshot.
- Bumping the pinned `@openai/codex` / `@anthropic-ai/sandbox-runtime` versions is a deliberate act: regenerate snapshots in the same commit and read the diff — it is the changelog of what the agent sandboxes changed underneath nub.
