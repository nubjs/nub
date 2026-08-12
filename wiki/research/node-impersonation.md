# Research: node-executable impersonation (the `node` PATH shim)

**Status:** v1, 2026-05-16. **Source read:** `bun/` (cloned 2026-05-16 from `oven-sh/bun` — note the codebase is the new Rust port; the report references the canonical Zig under `src/cli/...` since the `src/runtime/cli/...` Rust ports mirror it one-for-one).

## Question

How Bun redirects a child process's `node` invocation into its own runtime, and which parts of that shape Nub should copy.

When `bun` runs a script — directly, via `bun run`, or via a bin shim — and that script's child processes invoke `node` (via shebangs like `#!/usr/bin/env node`, via `package.json` script strings like `"start": "node server.js"`, or via `child_process.spawn("node", ...)`) — how does Bun arrange for those `node` invocations to land inside Bun's runtime, and what shape should Nub copy?

The requirement: running a script, a `package.json` script, or anything else through Nub should temporarily override how the `node` executable resolves, so a shebang encountered inside any script runs under Nub rather than plain Node.

Sub-questions:

- Does the top-level `nub` executable need to be **flag-for-flag** compatible with `node` for this to work?
- Does Bun rewrite `node` to an internal sub-command, or use the same binary with argv-detection?
- Does `bun --bun foo` propagate the "force bun" semantic across spawn boundaries?

## TL;DR

Bun's mechanism is a PATH shim pointing at a symlink of its own binary, dispatched by an `argv[0]` check. It is opt-in behind `--bun`, propagates through inherited env rather than through the flag, and rewrites no script text.

1. **Mechanism is a PATH shim**, not an internal subcommand. Bun creates `/tmp/bun-node-<sha>/node` as a **symlink back to its own binary** (hard link on Windows, since symlinks need admin), prepends that dir to the child process's `PATH`, and detects on startup that `argv[0]` ends in `node` — at which point it dispatches to a "pretend to be node" code path.
2. **Bun's default is conservative; Nub's should not be.** If a real `node` is on the user's `PATH`, Bun **defers to it** — the shim is not installed unless `--bun` / `-b` is passed. That default fits Bun, which has never committed to drop-in Node compatibility (early Bun seg-faulted on real apps, so silently substituting it for `node` would have broken user scripts). **Nub's posture is the opposite: drop-in compat IS the trust contract.** If the user invoked `nub` anywhere in the chain, children resolving `node` should hit Nub by default — opt-out, not opt-in. Full argument: [Implications for Nub](#implications-for-nub) §1.
3. **The `--bun` flag does not need to propagate as a flag.** It mutates the child's `PATH` and `NODE`/`npm_node_execpath` env vars, which inherit. Grandchildren see "bun is node" because the env survived, not because the flag did.
4. **Flag compatibility: Bun is permissive; Nub should be exhaustive.** Bun teaches its CLI parser the common Node flag spellings (`--require`, `--import`, `--inspect*`, `--preserve-symlinks*`, `-e`/`-p`, `--conditions`, …) and **silently ignores everything else** when running as node — acceptable for an opt-in hijack where the user knew they were swapping runtimes. Nub's drop-in compat contract means every documented Node flag must work; there are no "unrecognized" flags to ignore. The `--node` compat mode covers the inverse direction, restoring Node *semantics* rather than just flag handling.
5. **Package.json scripts are not textually rewritten.** `"node foo.js"` stays `node foo.js`; the PATH shim does the redirect. Bun's only rewrites are `npm run` / `yarn run` / `npx` → `bun run` / `bun x`, for performance rather than correctness.
6. **Shebangs in `node_modules/.bin` are left untouched on POSIX.** Bun normalizes CRLF in the shebang but does not change `node` → `bun`. The PATH shim handles the redirect at exec time. On Windows it ships an embedded shim `.exe` per bin entry that rewrites `node` → `bun` in the `CreateProcessW` command line when `node.exe` is not found.

## How the shim works (POSIX)

**Directory.** `/tmp/bun-node-<short-git-sha>` (macOS: `/private/tmp/...`, Linux: `/tmp/...`, Android: `/data/local/tmp/...`). Per-build-SHA so upgrades don't collide. Source: `src/cli/run_command.zig:603-615`.

**Contents.** Two symlinks, both pointing at the running `bun` executable's absolute path: a `node` entry and a `bun` entry. Created by `RunCommand.createFakeTemporaryNodeExecutable` (`src/cli/run_command.zig:643-768`). Lazy — created on first invocation that needs it; reused if already present (`error.PathAlreadyExists` is swallowed).

**PATH wiring.** The temp dir is appended into the in-process PATH buffer (`run_command.zig:700`), and the caller appends the user's original `PATH` after it (`:982`) — net effect: bun's shim sits ahead of system `PATH` but behind `node_modules/.bin` (which is inserted earlier).

**Env vars also set** (`run_command.zig:955-959`):

- `NODE` = the symlink path
- `npm_node_execpath` = the symlink path
- `npm_execpath` = the actual bun binary

These cover tools (npm-lifecycle, node-gyp) that read `process.env.NODE` instead of running `which node`.

**Skip conditions.** The shim is *not* installed when:

1. `pretend_to_be_node` is already true on this process — an ancestor set it up already (`run_command.zig:648`).
2. A real `node` is on `PATH` *and* the user did not pass `--bun` / `-b`. See `run_command.zig:913-918`:

   ```zig
   const found_node = this_transpiler.env.loadNodeJSConfig(
       this_transpiler.fs, if (force_using_bun) bun_node_exe else "",
   );
   var needs_to_force_bun = force_using_bun or !found_node;
   ```

The consequence: **without `--bun`, `bun run "node foo.js"` runs the user's real system `node`, not bun.** The hijack is opt-in by default.

## How the shim works (Windows)

Same idea, different primitives. The temp dir is computed via `GetTempPathW() + "\\bun-node-<sha>"`.

Inside it, bun creates two **hard links** (not symlinks, which require admin) named `node.exe` and `bun.exe`, both pointing at the running `bun.exe` (`run_command.zig:701-767`, using `CreateHardLinkW` at `:740`). Windows resolves bare `node` → `node.exe` natively, so PATH lookup works the same way.

A **separate** Windows mechanism exists for `node_modules/.bin` entries — see [Bin shims](#bin-shims-nodemodulesbin) below.

## argv0 detection and dispatch

On startup, `Command.which()` (`src/cli/cli.zig:390-414`) checks `argv[0]`:

```zig
pub fn isNode(argv0: []const u8) bool {
    if (Environment.isWindows) {
        return strings.endsWithComptime(argv0, "node.exe")
            or strings.endsWithComptime(argv0, "node");
    }
    return strings.endsWithComptime(argv0, "node");
}
```

If true, two globals flip: `CLI.pretend_to_be_node = true`, and clap's `warn_on_unrecognized_flag = false`. Dispatch goes to `.RunAsNodeCommand` → `RunCommand.execAsIfNode` (`src/cli/run_command.zig:1979-2023`), which supports `-e`/`--eval`, `-p`/`--print`, and a positional script, then calls `Run.boot(ctx, normalized_filename, null)`. **No REPL.** Missing script with no eval → friendly error.

There is **no internal `bun node` subcommand**. The exact same binary runs; the difference is which dispatch path `Command.which()` picks.

## Flag handling under node-impersonation

Same clap parser, same flag table (`runtime_params_` in `src/cli/Arguments.zig:79-131`). Bun adds the common Node flag spellings as parser entries:

- `--require <STR>`, `--import <STR>` → routed to bun's preload mechanism alongside its own `--preload`.
- `--inspect`, `--inspect-wait`, `--inspect-brk` → debugger options.
- `-e` / `--eval`, `-p` / `--print`.
- `--preserve-symlinks`, `--preserve-symlinks-main`, `--no-deprecation`, `--throw-deprecation`, `--title`, `--zero-fill-buffers`, `--use-system-ca`/`--use-openssl-ca`/`--use-bundled-ca`, `--unhandled-rejections`, `--no-addons`, `--dns-result-order`, `--expose-gc`, `--max-http-header-size`, `--conditions`.
- `--loader` is **explicitly skipped** in node-mode (`Arguments.zig:716-721`: *"Node added a `--loader` flag … completely different from ours"*).
- `--port` is repurposed in node-mode: `node --port <script>` is interpreted as `--print <script>` (Node treats `--port` as a print expression; comment marks this as a TODO).

**Unrecognized flags are silently dropped.** With `warn_on_unrecognized_flag = false`, clap's streaming parser consumes unknown `--flags` and continues, so `node --experimental-vm-modules`, `node --no-warnings` and `node --max-old-space-size=4096` all "work" in the sense that the script runs — they do nothing.

**Implication for Nub.** The drop-in compat contract means flag handling has to be **exhaustive**, not merely permissive: every documented Node CLI flag needs a real implementation, not a silent no-op. The Bun-style swallow-unknowns posture is available to Bun precisely because Bun is opt-in. Nub's hijack-by-default amplifies the cost of a silent mismatch — a script depending on `--max-old-space-size` that happens to run under Nub via a shebang should not OOM in production because the flag was ignored. So there is no need for a warn-on-unknown-flag mode: either Nub implements the flag or it is not drop-in compat. The `--node` compat mode is the semantic-level escape.

Mechanically, `nub node` and the shim entry path share one dispatch site: **node-impersonation does *not* spawn a second process or route through `nub node` textually.** When invoked via the shim or a shebang, Nub's main entry detects `argv[0]` ending in `node` and dispatches to the same code path `nub node` uses — same binary, no extra layer.

## The `--bun` flag (and why it isn't an env var)

**What it does.** Sets `ctx.debug.run_in_bun = true` → `force_using_bun` in `RunCommand` (`Arguments.zig:1609-1611`, `run_command.zig:1714`). Two effects:

1. `loadNodeJSConfig(..., bun_node_exe)` is called with the bun-node path instead of empty, which forces `NODE` / `npm_node_execpath` to the shim path even when a real `node` is on `PATH` (`run_command.zig:913-916`).
2. `needs_to_force_bun = force_using_bun or !found_node` flips true unconditionally, so `createFakeTemporaryNodeExecutable` always installs the shim dir (`run_command.zig:918, 937-962`).

**How it propagates to children.** Not directly — there is no `BUN_FORCE_BUN` env var. (`BUN_BE_BUN` exists for a different purpose: making a single-file-compiled bun executable behave as plain bun.) What propagates is the **mutated `PATH`** containing the shim dir, plus `NODE` and `npm_node_execpath`. A grandchild running `spawn("node", ...)` hits the shim via PATH; one running `spawn("/usr/local/bin/node", ...)` bypasses it. So "is bun" propagates via env inheritance, not flag inheritance.

**Where else `--bun` is honored.** `bunx --bun` (`bunx_command.zig:60`), `bun create --bun` (forwarded to bunx, `cli.zig:1402`), per-package in `bun --filter` workspace runs (`filter_run.zig:483`, `multi_run.zig:609`), and via `bunfig.toml`'s `run.bun = true` (`bunfig.zig:876`). `--target=bun` on build commands also sets it (`Arguments.zig:1196` — surprising side effect).

**Where it is *not* honored.** `bun install` lifecycle scripts ignore it; `PackageManager.zig:354` runs its own probe and uses real node if present, with no `--bun` plumbing. So `bun install --bun` does **not** force `postinstall` to run under bun.

**User-visible detection.** None directly; no env var to read. User code can sniff `process.env.NODE` and check whether it points inside `bun-node-<sha>/`, or check `typeof Bun !== "undefined"`. This is deliberate: the trust contract is "bun is being node," not "bun is advertising itself as bun."

## Package.json scripts

When `bun run start` executes `"start": "node server.js"`, Bun does **not** rewrite `node` to `bun`. The textual rewrite function `RunCommand.replacePackageManagerRun` (`run_command.zig:87-205`) matches and rewrites:

- `yarn run X` / `yarn X` → `bun run X`
- `npm run X` → `bun run X`
- `pnpm run X` / `pnpm dlx X` / `pnpx X` / `npx X` → `bun run X` / `bun x X`

The bare word `node` is not in that list, so `node server.js` passes through verbatim to bun's shell interpreter or `sh -c` / `cmd /c`, which resolves `node` via the inherited `PATH` — landing on the shim.

**Implication for Nub.** Do not textually rewrite `node` → `nub` in script strings; rely on the PATH shim. Less code, fewer edge cases (`node -e "..."`, `npx tsx`, `${NODE} foo.js`), and behavior identical to plain Node.

## Bin shims (`node_modules/.bin`)

Bin-entry linking differs by platform: POSIX symlinks leave the package's shebang alone, while Windows ships an embedded shim `.exe` that rewrites `node` to `bun` when `node.exe` is not found.

**POSIX.** Bun creates plain symlinks from `node_modules/.bin/<name>` to the package's target file (`src/install/bin.zig:593-594`). It does **not** rewrite the target's shebang from `#!/usr/bin/env node` to `#!/usr/bin/env bun` — it only normalizes CRLF on the existing shebang line (`tryNormalizeShebang`, `bin.zig:618-704`). Execution relies on the PATH shim being present at the time the bin is invoked.

**Windows.** Bun ships an embedded `bun_shim_impl.exe` (~13 KB, built into bun itself via `@embedFile`) and a sidecar `.bunx` metadata file per bin entry (`src/install/bin.zig:706-787`, `src/install/windows-shim/BinLinkingShim.zig`). The shim parses the package's shebang at install time and stores an `is_node_or_bun` flag in the sidecar. At launch:

- If parent process is `bun --bun ...`, the shim direct-launches the script in-process — no second `CreateProcessW` (`bun_shim_impl.zig:643-663`, fed via `ctx.debug.run_in_bun` passed in at `run_command.zig:2175`).
- Otherwise it `CreateProcessW("node \"path\" args")`. If that fails with `FILE_NOT_FOUND` and the shebang was `node`, it rewrites `"node"` → `"bun "` in the command line buffer (literally three UTF-16 chars and a pointer bump) and retries (`bun_shim_impl.zig:798-830`).

Neither piece is necessary for v1 Nub; the POSIX symlink plus the Windows hard link in the PATH shim are the load-bearing parts.

## Clever bits

Four details worth copying: the shim survives a `which node` round-trip, error messages are attributed back to `bun`, the temp dir is keyed per build SHA, and an already-absolute `argv[0]` saves a syscall.

- **Running `which node` returns the shim.** User code calling `execSync("which node")` gets the shim path back. Re-executing it triggers the same argv0 detection, so Bun recursively spawns itself, with `pretend_to_be_node` already true so the second pass skips re-creating the shim dir.
- **Error attribution.** `basenameOrBun` (`run_command.zig:373-380`) rewrites `bun-node/node` back to `"bun"` in error messages, so stack traces and "command not found" don't lie about which interpreter ran.
- **Per-SHA temp dir** means upgrades don't collide; old shim dirs persist as harmless cruft. Cleanup is a non-issue for v1.
- **argv0 absolute-path heuristic.** If `argv[0]` is already absolute (shebang invocation), bun reuses it as the symlink target rather than calling `selfExePath()`. Skips a syscall.

## Implications for Nub

Eight decisions: hijack by default rather than opt-in, one binary dispatched by argv0, exhaustive rather than permissive flag support, no script-text rewrites, and an opt-out flag in place of Bun's opt-in.

1. **Default behavior: hijack `node` whenever the entry point was `nub`.** Deliberately the opposite of Bun's default. Bun defers to real Node because it has not committed to drop-in Node compatibility — early Bun seg-faulted and missed API surface, so silently substituting it for `node` would have broken too many user scripts. For Nub, drop-in Node compatibility *is* the trust contract: if a script ran under Node, running it under Nub should be safe, and the wins of integrated transforms, caching and PM-controlled execution only materialize when children are also Nub. So if the user invoked `nub` anywhere in the chain (`nub run`, `nub script.ts`, `nubx foo`, an `nub install` lifecycle script), child processes that say `node` resolve to `nub`, and the PATH shim installs by default.

   Consequences for the surface:

   - The flag inverts: rather than `--nub` to opt *in*, an opt-*out* flag (`--no-node-shim`? `--real-node`?) for users who deliberately want children to hit system `node`. The name is deferred; the default is the load-bearing decision.
   - Compatibility expectations sharpen. With Nub running transparently as `node` for arbitrary child scripts, every Node CLI flag those scripts depend on must either work or fail loudly. Bun's silently-ignore-unknown-flags posture becomes a bug source; at minimum Nub warns when a recognized-but-unsupported Node flag is ignored.
   - Bundled-Node fallback still matters: where Nub cannot handle a script (a native-addon ABI mismatch, say), it should delegate to its bundled Node rather than fail. See [open questions](#open-questions).

2. **The `nub` and `node` entry points are the same binary, dispatched by argv0.** No separate `nub-node` executable. The PATH shim is a symlink (hard link on Windows) from a temp dir; Nub's main detects argv0 ending in `node` / `node.exe` and dispatches to the node-compat entry path, which is also what `nub node` exposes. One code path, so the two cannot drift.

3. **Exhaustive Node flag compat, not permissive.** Every documented Node CLI flag — the full set, not the common ~15 — has to do the right thing; there are no "unrecognized" flags, since silently ignoring some would contradict the drop-in contract. The `--node` compat mode flips off Nub-side enhancements at the *semantic* level: suppressed tuned defaults, hidden Nub globals, no naked-specifier built-in resolution, no preprocessing, and no node-impersonation shim, so children hit real Node too.

4. **No textual rewrites in `package.json` script strings.** `"start": "node server.js"` stays as written; the PATH shim handles it. (Bun's `npm run` → `nub run` rewrites are a separate nicety, orthogonal to node-impersonation; decision deferred.)

5. **Bin shims:** on POSIX, leave package authors' shebangs alone when `nub install` writes `node_modules/.bin/*` symlinks. On Windows, defer the embedded-shim-exe pattern to v1.x — for v0/v1, requiring a real `node.exe` on PATH for Windows bin entries is acceptable, since a Nub install may carry a bundled Node anyway.

6. **Env vars on spawn:** set `NODE` and `npm_node_execpath` to the shim path, `npm_execpath` to the actual `nub` binary. node-gyp and npm-lifecycle scripts read these.

7. **Opt-out flag instead of opt-in.** Bun has `--bun` to force the hijack; Nub takes the inverse — `--no-node-shim` or `--real-node` to *suppress* it for users with a specific reason to run children on system Node. Default-on means no `--nub` flag to enable what is already the default. A repo-level opt-out persists as `"nub": { "shim": false }` in `package.json`.

8. **No `NUB_FORCE_*` env var.** Bun's `--bun` does not propagate as a flag because env inheritance already does the right thing. The only env signal needed is `PATH` containing the shim, with `NODE`/`npm_node_execpath` pointing at it.

## Open questions

Five unsettled points: the opt-out flag's name, the failure mode when a script truly needs real Node, bundled versus system Node under that opt-out, `nubx` on the hijack path, and hook registration when argv0 is `node`.

- **Opt-out flag name.** `--no-node-shim`, `--real-node`, `--passthrough-node`, `--node=system` all read awkwardly. Defer until someone needs it; the hypothesis is that drop-in compat means nobody does.
- **Failure mode when a script truly needs real Node.** For a script loading a native addon whose ABI Nub does not match, or hitting an unimplemented Node API: (a) crash loudly with a pointer to `--real-node`, (b) auto-fall back by re-spawning under bundled real Node, or (c) crash silently as Node would. (b) is appealing but invisible; (a) is honest, and is the likely v1 choice.
- **Should the shim respect an existing real `node` on PATH at all?** Bun defers when real node is present. Nub's inverted default needs no such check. The wrinkle: under `--real-node`, use *bundled* Node (reproducible) or the user's system Node (matches their other tools)? Probably bundled, with `--system-node` as a further escape hatch.
- **Semantics of `nubx`.** Running `nubx some-tool` executes a package whose bin is `#!/usr/bin/env node`, so the shim must be installed before spawn, same as `nub run`. Mechanically straightforward, but worth stating that `nubx` is on the hijack path.
- **Loader-hook layer interaction.** Nub injects TS/JSX/CSS hooks via `--import nub-hooks.mjs` when it spawns Node. When Nub is invoked as `node` via the shim it is already in-process, so the hooks must register themselves directly rather than via a flag. The argv0-is-node entry path has to call the same hook-registration code `nub run` uses.

## Changelog

Every revision to this document, with the date and what changed.

- 2026-07-30 — Migrated from the internal research corpus. Internal planning links, private attributions and reference-checkout paths were rewritten; findings and measured values are unchanged.
