# GVS in CI — should Nub keep the `CI` auto-disable of the global virtual store?

**Question (2026-07-07).** Should Nub keep auto-disabling the global virtual store when `CI` is set?

Nub auto-disables the global virtual store (GVS) when `CI` is set, forcing a project-local store + hidden hoist tree instead of symlinking into `~/.cache/nub/pm/virtual-store`. Reconsider: CI caches are often persisted, so GVS-into-a-warm-store could speed CI up (PRO); against that, the multi-stage-Docker `COPY --from` dangling-symlink problem and dev/prod code-path divergence (CON). Also evaluate whether Dockerfiles or the Docker build context can be detected to disable conditionally.

**Verdict: KEEP the CI auto-disable, unchanged.** It is the ecosystem norm (pnpm 11 does the identical thing for the identical reason, stated in its own source comment), the speedup it forgoes is conditional on store persistence that the default CI runner does not provide, the explicit opt-in for users who DO have persistent stores already exists and is respected (`enableGlobalVirtualStore=true` wins over the CI gate — the same escape pnpm names), and every "smarter signal" alternative — Dockerfile-presence, container/build detection (empirically probed), store-warmth gating, relocatable-GVS — is either dead, fragile, or worse than both stable defaults. No code change recommended; two optional refinements at the bottom.

## 1. Current behavior (grounded)

Two independent mechanisms force the store project-local; both are DEFAULTS an explicit user setting overrides:

- **Env-based (`CI` set):** `planned_global_virtual_store = explicit_setting ?? !CI-present` — [`vendor/aube/crates/aube/src/commands/install/gvs.rs:54-60`](../../vendor/aube/crates/aube/src/commands/install/gvs.rs) (checks the env snapshot captured by `aube_settings::values::capture_env()`, `install/args.rs:466`); the linker constructor mirrors it (`Linker::new` → `new_with_gvs(.., !aube_util::env::is_ci())`, [`aube-linker/src/builder.rs:14`](../../vendor/aube/crates/aube-linker/src/builder.rs), `is_ci()` = `var_os("CI").is_some()`, `aube-util/src/env.rs:192-194`). Presence-based: `CI=false` and `CI=""` also count (see §6).
- **Verb-based (`nub ci`):** `engine_session_ci` threads `VirtualStoreLocality::ProjectLocal` → `nub_setting_defaults` pushes the embedder-tier default `enableGlobalVirtualStore=false` (`crates/nub-cli/src/pm_engine/mod.rs:645-652`, `:2224-2226`; test `ci_forces_project_local_store_but_keeps_isolation`, `mod.rs:2869`). This is #241/PR #261: `nub ci` is the frozen deploy-oriented install whose output is COPY-relocated, so it is self-contained regardless of env.

**What actually changes** (the `Materialization` enum, `gvs.rs:78-137`): GVS-on = `Symlink` — `node_modules/.nub/<dep>` are absolute symlinks into the machine-global `~/.cache/nub/pm/virtual-store/`, no hidden hoist tree (a shared store must never carry one — unrepresentable by construction). GVS-off = `Disk { hidden_tree: true }` — `.nub/<dep>` are real project-local directories reflinked/hardlinked from the CAS, plus the pnpm-parity hidden hoist tree `node_modules/.nub/node_modules/`. **The isolated layout — and its phantom-dep protection — is identical in both modes**; only the store's location moves. The CAS (content blobs) is a separate tier and stays machine-global in both modes.

**Override precedence, verified empirically** (2026-06-30): explicit `enableGlobalVirtualStore=true` (`.npmrc` / `pnpm-workspace.yaml` / `npm_config_enable_global_virtual_store`) beats both the CI env gate and the `nub ci` embedder default. The auto-disable is a default, not a force.

## 2. The crux: the speedup is conditional on store persistence, which the default runner doesn't provide

Three measurements bound the question: warm and already materialized, GVS saves ~1.4 s; cold, it saves nothing; and an ephemeral runner pays the materialization either way, in the cache-restore step instead of the install.

**Warm and already materialized, GVS-on buys ~1.4 s:** a 571-pkg fixture bench (, warm offline reinstall, N=8 interleaved) measured GVS 765 ms median vs project-local 2,272 ms — **3×**, mechanically because GVS creates ~513 symlinks where project-local materializes 21,002 files / 243 MB. That is the entire per-install prize.

**Cold, GVS-on buys nothing:** the same bench's cold cells show no meaningful delta (150–176 s, network-dominated, difference within jitter). A cold run materializes into the fresh global store and then symlinks — same file I/O, relocated.

**GitHub Actions default runners are ephemeral** — `~/.cache/nub` is cold every run unless explicitly cached. And `actions/cache` cannot rescue the GVS case: to get the symlink-speed win you must persist not just the CAS but the *materialized virtual store* (those same 21K files / 243 MB), and cache restore = download + untar of exactly those files, **every run, in the restore step** — you pay the materialization anyway, just relabeled, plus the cache-save on the other end. GVS-on cannot net-win on an ephemeral runner even with caching. With GVS-off, caching only the CAS already captures most of the realistic win (offline fetch + reflink/hardlink materialize), and Nub's project-local warm install (~2.3 s) is still ~2× faster than pnpm 11 (~4.6 s) on the same fixture.

**Where it genuinely pays:** a persistent disk — self-hosted runners, long-lived build machines, Nix-style builders. Those users set `enableGlobalVirtualStore=true` once and the gate steps aside. pnpm's source says this verbatim (`pnpm11/config/reader/src/index.ts:705-710`, v11.9.0, verified 2026-07-07):

> ```ts
> if (pnpmConfig.ci && pnpmConfig.enableGlobalVirtualStore == null) {
>   // Using a global virtual store in CI makes little sense,
>   // as there is usually no warm cache in that environment.
>   // However, if the user explicitly enabled GVS (e.g., for Nix builds
>   // or CI systems with persistent caches), respect that setting.
>   pnpmConfig.enableGlobalVirtualStore = false
> }
> ```

One caveat on the opt-in: on *shared* multi-tenant persistent runners, the shared store is cross-project-mutable state (`builder.rs:409-417`: "shared-store writes leak across projects") — a consideration for the opting-in user, not a reason to change the default.

## 3. Ecosystem norm: Nub's gate IS the norm; Nub's GVS-by-default is the outlier that makes the gate load-bearing

Both pnpm 11 and bun default their virtual store to project-local real files, and pnpm forces the global store off under CI for the same stated reason. Nub's gate mirrors that; what diverges is Nub enabling GVS by default at all.

- **pnpm 11:** default virtual store is PROJECT-local (`node_modules/.pnpm`, hardlinked — COPY-safe out of the box); GVS is auto-enabled only for global installs (`config/reader/src/index.ts:428-430`) and **forced off under CI** exactly as above. Nub/aube's `explicit ?? !CI` is a faithful mirror.
- **bun:** default hoisted = real files; `globalStore` is opt-in, OFF by default; its Docker guide relies on the plain-COPY pattern.
- The framing "pnpm does NOT disable its store under CI" is true of pnpm's **CAS** (which Nub also never disables — the CAS stays global in every mode) but not of pnpm's **global virtual store**, the feature actually analogous to Nub's GVS.

So the divergence question inverts: Nub is not diverging from the norm by gating GVS in CI — Nub diverged by making GVS the *default for regular installs* (#238; pnpm/bun never do), and the CI gate is one of the guardrails that makes that aggressive default safe. Removing the guardrail while keeping the aggressive default would put Nub in a posture no other PM ships.

## 4. The Docker CON, sharpened — and a finding: the CI gate doesn't protect image builds anyway

Empirical (probe, 2026-07-07, Docker 28.3.3): a BuildKit `RUN` step's environment is CLEAN — no `CI`, no marker vars (see §5).

So `nub install` inside a `docker build` never sees the CI gate even when the build runs on a CI runner; the multi-stage `COPY --from` case (#241) is protected by **`nub ci` in the Dockerfile + docs** (`site/content/docs/deployment/docker.mdx`, `site/content/docs/install/virtual-store.mdx`), not by the env gate.

What the env gate DOES protect: **runner-side relocation flows** — node_modules packed into artifacts/tarballs, serverless bundles (Lambda zips), rsync/scp deploys, docker builds whose context includes node_modules. CI is precisely the environment where a node_modules tree most often leaves the machine, and (per §2) precisely where the warm-store payoff is least likely.

## 5. Every "narrow the signal" alternative, evaluated

**(a) Dockerfile-in-repo presence — REJECT.** Wrong on precision in both directions, and layout-destabilizing:
- *False positives dominate:* most backend/service repos carry a Dockerfile, but the installs that run are mostly NOT the image build — local dev installs and CI test-job installs on the runner. Presence-gating would turn GVS off for all of them, gutting the default exactly where it pays (local dev). Scoped to CI-only it is redundant with the existing `CI` gate.
- *False negatives:* `Dockerfile.prod`, `docker/Dockerfile`, `-f`-specified names, compose-referenced files, image builds driven from another repo, and every non-Docker relocation flow (artifact tarballs, Lambda zips) that presence can't see.
- *Category error:* the risk is "THIS tree is about to be relocated," which repo contents can't indicate — and the actual Docker-build install doesn't need the signal (it runs in a clean-env container where a Dockerfile-presence check inside the build context would fire, but `nub ci` is already the documented deterministic answer there).
- *Spooky action:* GVS mode flips wipe `node_modules` and reinstall from scratch (`reset_on_mode_change`, `gvs.rs:201-239`, WARN_AUBE_GVS_MODE_CHANGED) — `git pull`-ing a commit that adds a Dockerfile would silently nuke and re-link every checkout's node_modules.

**(b) `/.dockerenv` / container detection at install time — EMPIRICALLY DEAD.** Settled by probe (2026-07-07, Docker 28.3.3, `debian:stable-slim`, `--no-cache`):
- **BuildKit (default builder since Docker 23): `/.dockerenv` ABSENT** during `RUN`. Also absent/empty: `/run/.containerenv`, any marker env var (env is exactly `HOME`/`PATH`/`PWD`), and cgroup signal (`/proc/1/cgroup` = `0::/`, cgroup-v2 namespaced).
- **Legacy builder (`DOCKER_BUILDKIT=0`, deprecated, removal announced): `/.dockerenv` PRESENT.** Both claims were half-right; the modern default is the dead case, which kills the heuristic.
- The only BuildKit tell found: `/proc/self/mountinfo` contains `/var/lib/docker/overlay2/...` overlay paths and `/docker/buildkit/executor/resolv.conf` mounted at `/etc/resolv.conf`. Host-config-dependent (data-root, rootless, containerd-snapshotter, non-Docker builders like buildah/kaniko all change or lack it) — a "usually works" heuristic, which is the worst kind here: relocation-safety that varies invisibly run-to-run is itself the footgun, and a flaky signal also triggers the mode-flip wipe of (a). The auto-detect option is retired on this empirical result.

**(c) Store-warmth gating — REJECT.** Warmth at install time doesn't predict whether THIS tree gets relocated (orthogonal question), and it makes layout a function of cache state — nondeterministic mode flips, wipes, unreproducible CI.

**(d) Explicit opt-in/opt-out knob — ALREADY EXISTS, pnpm-identical.** `enableGlobalVirtualStore` (npmrc/workspace-yaml/env) wins over the CI gate in both engines; `nub ci` is the explicit self-contained verb. Nothing to build.

**(e) Relocatable-GVS (the have-your-cake option) — NOT VIABLE.** The `.nub/<dep>` symlinks point OUTSIDE the copied `node_modules` subtree, so relativizing them cannot survive a `COPY --from` — the target simply isn't in the image. Copying the global store verbatim works only at an identical absolute path and ships every project's packages (verified "works but brittle — not recommended"). Any bundling/deploy step that makes the tree self-contained IS materialization — which is exactly what GVS-off/`nub ci`/`nub deploy` already do. A symlink-into-external-store layout is inherently machine-bound; the "second code path" (materialize) is irreducible whenever the tree must leave the machine.

## 6. The divergence CON, weighed

Real but modest, and its sharp direction is benign:
- Resolution semantics, lockfile graph, and the isolated layout (phantom-dep protection) are IDENTICAL in both modes — the divergence is store location + the hidden hoist tree's existence.
- The behavioral edge: GVS-on (local dev) has NO hidden hoist tree, so a phantom `require` that would be rescued by the pnpm-parity fallback fails; GVS-off (CI) builds the tree, so it may pass. I.e. **dev is stricter than CI** — a phantom-dep bug surfaces on the developer's machine first, the benign direction. The reverse (passes locally, breaks only in CI) has no known instance on this axis.
- The genuinely sharp edge is mode FLIPPING on one checkout (`reset_on_mode_change` wipes node_modules): e.g. running `act` or a devcontainer that sets `CI` against your normal dev checkout. Rare, self-healing, and an argument for STABLE deterministic signals — i.e. for keeping `CI` + explicit config, against every heuristic in §5.

Micro-divergence worth noting: aube keys on `CI` **presence** (`CI=false`/`CI=""` count as CI), pnpm on ci-info's `isCI` (excludes `CI=false`, adds vendor-specific detection for CIs that don't set `CI`, e.g. TeamCity/Jenkins defaults). Cosmetic in practice; see refinement (ii).

## 7. Recommendation

**Keep the `CI` auto-disable exactly as-is** — {keep} over {enable-unconditionally, narrow-the-signal, relocatable-GVS}:
- *Enable unconditionally:* forgoes ~nothing real (cold ephemeral runners: zero benefit; cached ephemeral: net-negative once the virtual-store cache payload is paid; persistent runners: already served by the explicit opt-in) while adding relocation fragility to the environment where trees most often relocate, and would make Nub the only PM shipping GVS-on-in-CI.
- *Narrow the signal:* both detection ideas fail — Dockerfile-presence is precision-broken and layout-destabilizing; container-detection is empirically dead under BuildKit (the default builder). `CI` + explicit override IS the narrow signal, and it is char-for-char pnpm's design.
- *Relocatable-GVS:* structurally impossible for the COPY case; "bundle the store" degenerates to materialization, which already exists as `nub ci`.

Optional refinements (small, non-blocking):
1. **Docs:** `install/virtual-store.mdx` documents the auto-disable but not the opt-back-in; add one sentence: persistent/self-hosted runners with a warm `~/.cache/nub` can set `enableGlobalVirtualStore=true` to reclaim symlink-speed installs (mirror pnpm's Nix/persistent-CI carve-out).
2. **Signal hygiene:** align `is_ci()`/the env-snapshot check with ci-info semantics — at minimum treat `CI=false` as not-CI. Low priority; pnpm-parity currently differs on this micro-edge.

## Changelog

Every revision to this document, with the date and what changed.

- 2026-07-07 — Initial write-up. Includes the BuildKit `/.dockerenv` probe (absent under BuildKit, present under the deprecated legacy builder), which retired container auto-detection.
- 2026-08-28 — Removed references to earlier internal notes.
