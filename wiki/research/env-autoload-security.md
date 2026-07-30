# Env-file auto-load security — the NODE_OPTIONS RCE on committed `.env.{mode}` files

**Status:** v1, 2026-07-07. The RCE vector was confirmed empirically against a local development build on Node 26.2. The mitigation is a product security decision recorded elsewhere.

## TL;DR

- **The RCE vector is LIVE.** A committed, attacker-controlled env file containing `NODE_OPTIONS=--require ./evil.js` executes `evil.js` when a maintainer or CI runs `nub <file>` (or the hijacked `node <file>`) in that project. Confirmed empirically against `target/fast/nub` on Node 26.2 — the marker file was written. The realistic exploit: a reviewer checks out a malicious PR branch and runs the code to test it.
- **The dangerous surface is the COMMITTED mode files** — `.env.production` / `.env.development` / `.env.test`, which projects routinely commit. Plain `.env` is usually gitignored, but the mode files are not, and nub auto-loads them all. Verified: `NODE_ENV=production nub main.js` fires from an attacker `.env.production`.
- **Root cause (code):** `load_env_files` strips only `NODE_ENV` (#263/#267) — no other filtering — and the resulting map is applied to the child command UNFILTERED, overwriting nub's own `NODE_OPTIONS`.
- **What accidentally saves you:** the "shell env wins" rule. Any variable already present in the ambient environment is not overridable — so `PATH` and pre-exported CI secrets are safe, and the attack is neutralized outright if `NODE_OPTIONS` happens to be pre-set. `NODE_OPTIONS` is normally unset, which is exactly why it is the live hole.
- **Blocked by:** `--node`, `NODE_COMPAT=1`, no `package.json`. `nub run <script>` is also transitively safe (nub pre-sets `NODE_OPTIONS`, so the inner `node`'s auto-load skips the hostile value under shell-wins).
- **Peers:** Bun auto-loads `.env` and exposes the hostile `NODE_OPTIONS` in `process.env`, but does NOT self-RCE (Bun isn't Node; it doesn't honor `--require` in `NODE_OPTIONS` for its own process). Node's `--env-file` DOES honor `NODE_OPTIONS` (RCE) but is explicit opt-in, not auto-load. Deno does not auto-load env files at all and gates env reads behind `--allow-env`.
- **Recommendation:** ship a dangerous-variable DENYLIST (`NODE_OPTIONS` at minimum) as the floor — it is cheap, closes the RCE escalation, and is independent of the larger question of whether committed mode files should be auto-loaded at all.

## The threat

nub AUTO-loads `.env`, `.env.local`, and `.env.{mode}` by default when it runs code. On a public repo, a malicious PR can add or edit a committed env file. `.env` is conventionally gitignored, but `.env.production` / `.env.development` / `.env.test` are commonly committed (they hold non-secret per-environment config), and a PR that touches one draws little scrutiny. When a maintainer or CI runs the branch under nub, nub auto-loads the attacker's file and injects its variables into the child Node process. The worst escalation is `NODE_OPTIONS`, because Node honors `--require`/`--import` there — turning env injection into arbitrary code execution before the target program runs a line.

Secondary escalations if injection is possible at all: overriding a variable a CI job relies on (redirecting a token, flipping `NODE_ENV`), or shadowing a config value the app reads.

## nub's real exposure — grounded

All results below are from running `target/fast/nub` (built 2026-07-07) against throwaway fixtures on Node 26.2 (fast tier). Every claim is a marker-file / stdout observation, not a code inference.

### The load-bearing RCE test

Fixture: a project (`package.json`), a `.env` containing `NODE_OPTIONS=--require <dir>/pwn.js`, a `pwn.js` that writes a `PWNED` marker, and a benign `main.js`.

```
$ nub main.js
app ran; NODE_OPTIONS=--require /…/pwn.js
$ cat PWNED
rce-fired at 2026-07-07T22:20:54.927Z
```

**RCE fired.** Note the printed `NODE_OPTIONS` is ONLY the attacker's value — nub's own preload token is gone. The `.env` value did not merely append; it fully clobbered nub's `NODE_OPTIONS`. Augmentation broke as a side effect, but the RCE fires regardless (nub's preload is not what triggers it).

### The committed mode-file variant (the realistic surface)

```
$ NODE_ENV=production nub main.js      # attacker-controlled .env.production
!!! .env.production RCE FIRED
```

Fires. This is the important one: `.env.production` is commonly committed, so no "add a new gitignored `.env`" step (which a reviewer would notice) is needed.

### What is NOT exploitable (shell-env-wins)

`load_env_files` skips any key already present in the process environment (`std::env::var_os(&key).is_some()`, `env.rs:127`). Consequences, all verified:

- **Existing CI secrets are safe.** With `AWS_SECRET_ACCESS_KEY=REAL_CI_SECRET` exported, a `.env` line `AWS_SECRET_ACCESS_KEY=ATTACKER_VALUE` does NOT override it (`AWS=REAL_CI_SECRET`). A *new* variable the environment did not set (`APP_NEW_VAR`) IS injected.
- **`PATH` is safe.** `PATH=/attacker/bin` in `.env` has no effect — `PATH` is always ambiently set, so shell-wins skips it. (nub also sets `PATH` itself; the shell-wins skip is the primary reason.)
- **The attack self-neutralizes if `NODE_OPTIONS` is already exported.** `NODE_OPTIONS=--title=x nub main.js` → the `.env` `NODE_OPTIONS` is skipped, no RCE.

The protection is therefore incidental — a function of "is this variable ambiently set?", not a security boundary. `NODE_OPTIONS` is the catastrophic case precisely because it is (a) normally unset and (b) an RCE primitive.

### Escape hatches and preconditions (verified)

| Condition | RCE? |
| --- | --- |
| `nub main.js` (default) | **fires** |
| `NODE_ENV=production nub main.js` + `.env.production` | **fires** |
| hijacked `node main.js` (nub on PATH, `NODE_OPTIONS` unset) | fires (same path) |
| `nub --node main.js` | blocked (compat skips auto-load) |
| `NODE_COMPAT=1 nub main.js` | blocked (compat skips auto-load) |
| no `package.json` (no detected project) | blocked (no auto-load) |
| ambient `NODE_OPTIONS` already set | blocked (shell-wins) |
| `nub run <script>` where the script spawns `node` | blocked (nub pre-sets `NODE_OPTIONS`; inner load skips under shell-wins) |

The live vector is specifically the DIRECT file run: `nub <file>` and the top-level hijacked `node <file>` when `NODE_OPTIONS` is not already in the environment. That is exactly the invocation a maintainer or CI uses to run/test a checked-out branch.

### Mechanism in the code

- `crates/nub-core/src/workspace/env.rs` — `load_env_files` / `load_env_files_raw_reporting`. Strips ONLY `NODE_ENV` (line 137, added for #263/#267 to match dotenv/Next/Vite). Shell-wins skip at line 127. No other denylist exists.
- `crates/nub-cli/src/cli.rs:2439` — the `nub <file>` path calls `load_env_files` into `auto_env`, folded through `merge_child_env` into `env_vars`.
- `crates/nub-core/src/node/spawn.rs:789` — `for (k, v) in config.env_vars { cmd.env(k, v); }` applies the map to the child UNFILTERED, AFTER nub assembles and sets its own `NODE_OPTIONS` (~line 778). So a `.env` `NODE_OPTIONS` overwrites nub's.

## What Bun, Node, and Deno do

All three rows below were tested directly (Bun 1.3.14, Node 26.2, Deno) with the same hostile-`.env` fixture, in addition to the survey of docs/source.

| Tool | Auto-loads env files? | `NODE_OPTIONS` from an env file → self-RCE? | Dangerous-var handling |
| --- | --- | --- | --- |
| **nub** | **Yes** (`.env`, `.env.local`, `.env.{mode}`) | **Yes — RCE** | none (strips only `NODE_ENV`) |
| **Bun** | Yes (`.env` → `.env.{NODE_ENV}` → `.env.local`) | No self-RCE (doesn't read `--require` from `NODE_OPTIONS` for its own runtime — by omission, not design) | none; no denylist. But propagates the whole `.env` map into spawned children → RCE one hop down |
| **Node `--env-file`** | No — explicit opt-in | Yes — RCE (tested); `--require` is `kAllowedInEnvvar` | none (no dotenv denylist) |
| **Deno `--env-file`** | No — explicit opt-in | Not reachable (execution-order accident, not a gate) | Explicit `ENV_FILE_DENYLIST` reasoning about THIS threat; still misses its own `DENO_V8_FLAGS` |

Grounded notes:

- **Bun.** Bun auto-loads `.env` like nub (`.env` → `.env.{NODE_ENV}` → `.env.local`; `docs/runtime/environment-variables.mdx`, `src/dotenv/env_loader.rs:708-747` at HEAD `8706328b`). In the fixture, `bun main.js` printed the hostile `NODE_OPTIONS` in `process.env` — so Bun DOES surface the injected value — but `pwn.js` did NOT run. Bun is not Node and does not process `--require`/`--import` from `NODE_OPTIONS` at its own startup; the string is inert for Bun's own process. This is **omission, not mitigation**: there is no denylist anywhere in Bun's `src/`, three PRs that would add `NODE_OPTIONS` reading are unmerged (oven-sh/bun #24177, #28818, #28830; tracking issue #28817), and `NODE_TLS_REJECT_UNAUTHORIZED` IS read unrestricted. The sharper risk: Bun feeds the whole `.env` map into spawned children by default (`Bun.spawn`, lifecycle scripts — `src/runtime/api/bun/js_bun_spawn_bindings.rs`, `src/install/lifecycle_script_runner.rs`), so the RCE fires one hop downstream the moment a Bun program spawns a real Node child without an env override. **Answer: Bun has no mitigation** — it is merely not self-vulnerable to the Node-specific RCE. nub is strictly worse because nub's entire job is to spawn Node with that environment, so the injection lands on a Node process on the first hop, every time.
- **Node `--env-file`.** Explicit opt-in — Node never auto-discovers `.env`. But once you load a file, `NODE_OPTIONS=--require ./pwn.js` inside it DOES execute (`node --env-file=envf main.js` fired the marker; corroborated independently). This is deliberate: `src/node_dotenv.cc` copies the value unguarded and `src/node.cc` treats `--require` as `kAllowedInEnvvar` — the same trust level as a real shell-exported `NODE_OPTIONS`. Node's `kAllowedInEnvvar`/`kDisallowedInEnvvar` split (which blocks e.g. `--jitless` from any `NODE_OPTIONS`) does NOT help — `--require`/`--import` are on the allowed side. So there is **no Node dotenv denylist to copy**; the safety of Node's model comes entirely from it being opt-in (you chose to trust that file), not from filtering.
- **Deno `--env-file`.** No auto-load — `deno run main.ts` (even `-A`) did NOT load `.env` (`NODE_OPTIONS` empty). Explicit `--env-file` is required; env READS also need `--allow-env` and subprocess spawns `--allow-run`. Crucially, Deno is the **one tool with a purpose-built denylist**: `ENV_FILE_DENYLIST` (`DENO_CONNECTED`, `DENO_DEPLOY_TUNNEL_ENDPOINT`) carries a code comment reasoning about exactly this threat class — a `.env` shipped alongside code must not redirect the runtime's own control vars. It is incomplete (its own `NODE_OPTIONS`-analog `DENO_V8_FLAGS` slips through on an execution-order accident), but it is the **citable precedent** for the recommended mitigation.
- **Vite / Next.js / dotenv / dotenvx.** Distinct concern — do not conflate. Vite's `VITE_` prefix and Next's `NEXT_PUBLIC_` prefix gate CLIENT-side EXPOSURE (which vars are inlined into the browser bundle); they do NOT restrict what reaches the server process, and neither strips `NODE_OPTIONS`. dotenv/dotenvx are always explicit (`require('dotenv').config()`), load whatever is in the file with no dangerous-var filtering; dotenvx's security posture is encryption-at-rest (`.env.keys`) so committed files aren't plaintext secrets — orthogonal to injection. The general CI discourse treats committed env files as a known footgun and leans on gitignore + secret-store conventions, not on the loader filtering.

### Prior art — this shape gets exploited

- **OpenAI Codex CLI** shipped exactly this class: a committed `.env` redirecting `CODEX_HOME` turned an innocent repo into a persistent backdoor that fired whenever a developer ran `codex`, no prompt. Fixed in Codex CLI 0.23.0. The closest real-world proof that "committed env file → control-var redirect → silent RCE on checkout" is a found-and-exploited pattern, not a hypothetical.
- **GitHub Actions** restricted `NODE_OPTIONS` from `$GITHUB_ENV` (2023-10-05) — a platform precedent for denylisting `NODE_OPTIONS` specifically out of an attacker-influenceable env channel.
- **CVE-2024-21892** (Linux-capability privesc in Node's env handling) is adjacent but distinct (not committed-file-sourced).
- No prior public write-up describes this exact class for a Node RUNTIME's auto-load; nub's auto-load-by-default makes it the sharpest instance among Node-compatible runtimes.

## Mitigation options, weighed

### (a) Dangerous-variable denylist — RECOMMENDED as the floor

Refuse to inject a fixed set of process-controlling variables from an auto-loaded `.env*` file. At minimum `NODE_OPTIONS`. Candidate wider set (all Node/loader-controlling): `NODE_OPTIONS`, `NODE_REPL_EXTERNAL_MODULE`, `NODE_EXTRA_CA_CERTS`, and the loader/inspector-adjacent knobs. `PATH`, `LD_PRELOAD`, `DYLD_*` are already protected by shell-wins in practice (always ambiently set) but belong in the list for defense-in-depth, since shell-wins is incidental.

- **Pro:** small, surgical, kills the RCE escalation directly (downgrades "env injection" from catastrophic-RCE to merely-bad-var-injection). Sits exactly where the `NODE_ENV` strip already is (`env.rs:137`) — one more filtered key. No workflow breakage: nobody legitimately sets `NODE_OPTIONS` in a committed `.env` expecting nub to honor it (and if they do, shell-export still works). **Deno's `ENV_FILE_DENYLIST` is the citable precedent**, and GitHub Actions' `NODE_OPTIONS`-out-of-`GITHUB_ENV` restriction is a second one — this is an established defense, not a novel one.
- **Con:** does not address non-RCE injection (a hostile committed var the app reads). It is a floor, not a complete answer.
- **Note:** there is no Node *dotenv* denylist to copy verbatim (Node forwards `NODE_OPTIONS` deliberately); the list is nub's own call, modeled on Deno's. Keep it small and documented. Include `NODE_TLS_REJECT_UNAUTHORIZED` (disables TLS verification — a MITM primitive, and one Bun reads unrestricted).

### (a′) Extend the denylist to the whole augmentation subtree — the "one hop down" gap

The Bun finding sharpens (a): even a runtime that doesn't self-honor `NODE_OPTIONS` still hands the hostile `.env` map to spawned `node` children, where it fires. nub already propagates its augmentation env (`NODE_OPTIONS` + PATH shim) tree-wide, so the denylist must apply to the auto-loaded map at the point of load (`env.rs`), not just to the direct child — that way a hostile var never enters the map that flows down to every descendant. This is not a separate option; it is the correct SCOPE for (a): filter at load, so the whole subtree is covered by construction.

### (b) Don't auto-load committed env files / only auto-load gitignored `.env`

Restrict auto-load to files git does not track (i.e. the developer's private `.env`), on the theory that the committed mode files are the attack surface.

- **Pro:** targets the exact threat (committed = attacker-reachable via PR).
- **Con:** inverts real usage — many projects legitimately commit `.env.production`/`.env.development` with non-secret defaults and expect them loaded. Detecting "tracked by git" needs a `git check-ignore` / index probe on every run (cost + a git dependency on the run path), and degrades gracefully-poorly outside a git repo. High breakage risk for a partial win. Not recommended as the primary lever.

### (c) Schema-as-boundary — only declared variables pass

Tie auto-load to an env schema (an env-schema direction tracked separately): the schema IS the allowlist, so an undeclared `NODE_OPTIONS` an attacker adds never passes.

- **Pro:** the principled long-term answer — turns the env surface into a declared contract, solves injection generally (not just RCE), and composes with validation/typing.
- **Con:** opt-in and unbuilt; can't be the default mitigation for the current exposure. Right north star, wrong timeframe for closing this hole.

### (d) Warn on first auto-load

Print a one-time notice when nub auto-loads a `.env*` file.

- **Pro:** cheap, raises awareness.
- **Con:** does not stop the RCE (the notice prints as the code already runs). Alert fatigue on a per-project routine. Weak on its own; acceptable as a complement to (a).

### (e) Don't auto-load under CI

Detect CI (`CI=true`) and skip auto-load there.

- **Pro:** removes the CI leg of the threat.
- **Con:** CI is a primary place people rely on `.env.{mode}` loading; silently changing behavior under CI is surprising and breaks real pipelines. Also leaves the local maintainer-runs-a-PR case fully open. Not recommended.

## Recommended posture

1. **Ship (a)/(a′) the dangerous-var denylist now, filtered at load** so the whole augmentation subtree is covered — `NODE_OPTIONS` + `NODE_TLS_REJECT_UNAUTHORIZED` at minimum. It closes the RCE escalation cheaply, next to the existing `NODE_ENV` strip, with no workflow cost, and follows Deno's `ENV_FILE_DENYLIST` precedent. This is the concrete fix for the reported threat.
2. **Consider (d) a one-time auto-load notice** as a low-cost complement (surfaces that files were loaded), not a substitute.
3. **Track (c) schema-as-boundary** as the principled general solution via `varlock-runtime` (already the design-of-record there; `nub.jsonc` `sandbox.env` doubles as the allowlist); it supersedes the denylist when it lands.
4. **Do not pursue (b) or (e)** as primary levers — both break legitimate committed-mode-file / CI workflows for a partial security gain.

The denylist-versus-broader-policy choice is a product security default; this document recommends the floor and defers the ceiling.

## Changelog

- 2026-07-07 — Initial write-up. RCE vector confirmed live against `target/fast/nub` (Node 26.2) via NODE_OPTIONS in an auto-loaded `.env`/`.env.production`; Bun (exposes but does not self-RCE; feeds `.env` to spawned children → RCE one hop down), Node `--env-file` (opt-in but honors NODE_OPTIONS, no dotenv denylist), and Deno (no auto-load; has a purpose-built `ENV_FILE_DENYLIST` — the citable precedent) tested directly and cross-checked against source. Prior art: OpenAI Codex CLI's `CODEX_HOME`-via-committed-`.env` RCE (fixed 0.23.0), GitHub Actions restricting NODE_OPTIONS from `$GITHUB_ENV`. Recommends a load-time dangerous-var denylist (NODE_OPTIONS + NODE_TLS_REJECT_UNAUTHORIZED) as the floor.
