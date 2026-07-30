# Structuring nub's sandbox crate for standalone reuse

*2026-07-09. Part of the sandbox prior-art comparison. Investigation-scope, recommend-only — lands no code, decides no maintainer-owned call. Reads the unmerged `nub-sandbox` on the `sandbox-primitives` worktree (`crates/nub-sandbox/`), NOT `main`.*

## TL;DR

- **The reuse boundary is already clean.** `nub-sandbox` is PM-pure by construction: zero `nub_*` / `aube` / `nub_core` / `nub_cli` imports in `src/` (verified). Its whole dependency set is third-party (serde, globset, regex, ipnet, per-OS: libc/landlock/seccompiler/windows-sys). An external consumer pulls in *no* nub baggage. The only nub coupling is (a) Cargo *workspace field inheritance* (`version`/`edition`/`license`/`repository`/`lints` = `workspace = true`) and (b) the `NOTICE`-recorded license provenance — both trivially fixable, neither a code coupling.
- **The two-boundary API is exposed, not buried** — `compile()` (surface → IR) and `apply()` (IR → OS-enforced child), with the `SandboxPolicy` IR fully `pub` + serde-round-trippable. BUT the two boundaries have very different reuse value: `apply` + the IR + matcher + proxy are genuinely tool-agnostic; `compile` encodes *nub's* config grammar (presets, `"..."` spread, `$(…)`, `dependenciesMeta` trust). An external consumer wants the former and almost never the latter.
- **Recommended structure: ONE neutral crate, cfg-gated backends (as today), with the nub-surface compiler behind a default-on `config` feature.** A programmatic consumer sets `default-features = false` and gets a lean IR-build-and-enforce core (drops `regex` and the nub grammar with it). This is the least-churn path from today's crate and the best external ergonomics (one crate, one version, one docs.rs page). A full multi-crate split is available later if the surface layer grows.
- **Naming is a maintainer decision (surfaced below, not decided).** A crate published to crates.io for external reuse *is* a public surface, so the brand boundary points at a NEUTRAL name (nub depending on it), not `nub-sandbox`. Recommendation: neutral name; shortlist + a lead given, pending a crates.io-availability gate.
- **Publishable with a short checklist** — de-inherit `version`/`repository`, fix the license SPDX to `MIT AND Apache-2.0 AND BSD-3-Clause` (the `NOTICE` provenance), add `[package.metadata.docs.rs]` for the three OS targets, add the `[features]` block. No hard `cargo publish` blockers. The lean, no-async-runtime / no-MITM / no-crypto dep tree is itself the selling point.

## 1. Current state (what exists on `sandbox-primitives`)

`crates/nub-sandbox/` — ~an engine + config compiler + 3 OS backends + egress proxy + conformance harness. The public surface (`src/lib.rs:23-34`):

```
pub mod backend; compiler; conformance; matcher; policy; proxy;
pub use backend::{CommandSpec, Degradation, Prepared, apply};
pub use compiler::{CommandRunner, CompileCtx, CompileError, compile};
pub use matcher::Homes;
pub use policy::SandboxPolicy;
pub use proxy::{Decision, EgressProxy, GrantDecider, Host, StaticDecider};
```

The two boundaries the crate is architected around (`src/lib.rs:3-10`, `src/policy.rs:1-18`):

- **Boundary A — `compile(surface: &Value, ctx: &CompileCtx) -> Result<SandboxPolicy, CompileError>`** (`src/compiler/mod.rs:151`). The *only* code that understands surface syntax; discharges presets, `"..."` spread, `$(…)`, trust tiers into a fully-resolved IR.
- **Boundary B — `apply(policy: &SandboxPolicy, spec: CommandSpec) -> Result<Prepared, Degradation>`** (`src/backend/mod.rs:220`). Pure `IR → OS-primitive` translation; dispatches to macOS Seatbelt / Linux Landlock+seccomp / Windows AppContainer, or an env-scrub-only skeleton on any other OS. No PM type crosses this line.

How nub-cli consumes it (`crates/nub-cli/src/cli.rs:2651-2666`) — the entire seam is 5 calls plus 2 host-glue fns:

```rust
let ctx = nub_sandbox::CompileCtx::new(sandbox_homes(&cwd), cwd.clone(), true, ambient_env());
let policy = nub_sandbox::compile(&block, &ctx)?;
let spec = nub_sandbox::CommandSpec::new(program).args(...);
let prepared = nub_sandbox::apply(&policy, spec)?;
let status = prepared.status()?;
```

`sandbox_homes()` (`cli.rs:2672`) and `ambient_env()` (`cli.rs:2690`) are host-provided data (Boundary B keeps the engine from reading the process env / probing the host itself). This tiny, data-only seam is *why* the extraction is low-risk.

## 2. Reuse-readiness audit — is the boundary free of nub/aube coupling?

**Yes. It is PM-pure and nub-pure at the code level.**

- **No nub/aube imports.** `grep -rn "use nub_\|use aube\|nub_core\|nub_cli\|aube::" src/` → nothing. The crate's own doc-comment asserts this and an impact-analysis review leg is meant to enforce the dep graph (`Cargo.toml` header comment).
- **Dependency set is entirely third-party and lean** (`crates/nub-sandbox/Cargo.toml`):
  - Always: `serde`, `serde_json` (with `preserve_order` — load-bearing for last-match-wins object folds), `tracing`, `globset` (ripgrep's glob engine), `regex` (env `/regex/` grammar), `ipnet` (net CIDRs).
  - `cfg(linux|macos)`: `libc`. `cfg(linux)`: `landlock` 0.4, `seccompiler` 0.5. `cfg(windows)`: `windows-sys` 0.61 (raw FFI, no COM). `dev`: `tempfile`.
  - **No async runtime, no reqwest, no rustls/openssl.** The egress proxy is thread-per-connection over `std::net` and reads SNI in the clear (NO MITM — `src/proxy/mod.rs:1-11`). This is a genuinely small, boring dep tree for a cross-platform sandbox — a real advantage over hand-rolling and over heavier alternatives.
- **The per-OS heavy deps are already `[target.'cfg(...)']`-gated**, so a macOS consumer never compiles `landlock`/`windows-sys`, etc. Cargo handles this without any feature flag — the target-gating is correct as-is.

**The only nub coupling, and it is not code:**

1. **Workspace field inheritance.** `version.workspace`, `edition.workspace`, `rust-version.workspace`, `license.workspace`, `repository.workspace`, `[lints] workspace = true` resolve against nub's root `Cargo.toml` (`version = "0.4.5"`, `repository = github.com/nubjs/nub`, `license = "MIT"`, edition 2024, rust-version 1.88). Cargo *flattens* `workspace = true` into the published tarball at `cargo publish`, so this is not a hard blocker — but a standalone lib wants its OWN `version` line and `repository`, not nub's. De-inherit those two; keep or restate the rest.
2. **License provenance (`NOTICE`).** The macOS Seatbelt base profile (`src/backend/macos_seatbelt_base.sbpl`, `include_str!`-ed at `src/backend/macos.rs:40`) and the glob→Seatbelt-regex translation are **adapted from OpenAI Codex (Apache-2.0)**, itself **derived from Chromium (BSD-3-Clause)**. The workspace `license = "MIT"` under-states this. For crates.io the honest SPDX is `MIT AND Apache-2.0 AND BSD-3-Clause`, with `NOTICE` packaged. A publish-correctness item, addressed in §6.

**One duplicated-logic coupling to note (not an import):** the compiler's `classify_string()` (`src/compiler/mod.rs:290`) is documented as needing to stay byte-identical to nub-cli's `project_config::classify_sandbox_string` — the file-ref-vs-preset disambiguation is split between engine and caller. This lives entirely inside the *surface compiler* (Boundary A), so it disappears from the reusable core the moment the `config` layer is feature-gated (§4) — an external consumer building IR directly never touches it. Worth flagging so the split is drawn on the right side of it.

## 3. Public API — what an external consumer imports, and is the useful part reachable?

The useful API is reachable, but the crate currently presents *two* front doors of very different reuse value:

**The high-value, tool-agnostic core (what an external consumer actually wants):**

- Build a policy directly — `SandboxPolicy` is `pub` with all-`pub` fields and `Default` (`src/policy.rs:25`). `fs: FsPolicy`, `net: NetPolicy`, `env: EnvPolicy`, `pid: PidPolicy`, each independently composable, each serde-round-trippable.
- Enforce it — `apply(&policy, CommandSpec::new(prog).args(...).cwd(dir)) -> Result<Prepared, Degradation>`; then `Prepared::status()` (the uniform launch verb; Windows rides `status`, not `command`, `src/backend/mod.rs:154-176`). `Degradation` is the fail-safe-with-degradation signal (`src/backend/mod.rs:57`, `.warning()` gives user-facing text) — a standout feature: the engine never silently drops an axis it claimed.
- The net decision seam — `GrantDecider` (`Fn(&Host) -> Decision`, `src/proxy/mod.rs:57`) with `StaticDecider` for policy-driven egress and `EgressProxy` if a consumer wants the loopback proxy standalone. Swapping in an interactive prompt is a trait impl, no fork.
- Matcher primitives — `matcher::path::{expand_symbolic, canonicalize_glob_prefix, PathMatcher, Homes}` and `matcher::HostMatcher` (`src/matcher/mod.rs`), for a consumer that wants to reuse the glob/host semantics without the backends.

**The nub-flavored front door (low external reuse value):** `compile()` + `CompileCtx` + `CompileError` + `CommandRunner`. This is nub's config grammar — the bool/preset/object trichotomy, `"..."` spread, `$(…)` substitution, `trusted`/`dependenciesMeta` tiers, curated secure defaults (`src/compiler/mod.rs`, `defaults.rs`, `preset.rs`, `env_grammar.rs`). An external project almost certainly defines *its own* surface (flags, its own YAML/TOML) and builds the IR itself, or hand-constructs `SandboxPolicy`. It does not want nub's `dependenciesMeta` vocabulary.

**Verdict on "is the API buried":** no — but the crate should *signal* which door is the reusable one. Two small ergonomics gaps for a programmatic consumer building IR by hand (both optional polish, not blockers):

- **Glob canonicalization contract.** `CanonGlob` (`src/policy.rs:225`) is "already-resolved" by contract — a hand-built policy that skips `expand_symbolic` + `canonicalize_glob_prefix` can mis-match on case-folding OSes. Either add a `CanonGlob::from_surface(pattern, &homes)` constructor or document the required pre-processing.
- **Builder helpers.** Constructing `FsRule { matcher, effect, access }` lists by hand forces the consumer to know last-match-wins ordering. A thin `FsPolicy::allow_read(glob)` / `deny(glob)` / `NetPolicy::allow_host(..)` builder surface would make the core pleasant without exposing internals. Nice-to-have.

## 4. Crate split — recommendation

**Recommend: a single neutral crate with cfg-gated backends (unchanged from today) + the surface compiler behind a default-on `config` feature.**

Options weighed:

| Option | Shape | Verdict |
|---|---|---|
| **(a) Single crate, cfg-gated backends + a `config` feature** | Today's crate, renamed + de-inherited; `compile`/`CompileCtx`/`preset`/`env_grammar` (and `regex`) gated behind `feature = "config"` (default on) | **Recommended.** Best external ergonomics (one crate/version/docs page), least churn to in-flight work, and a lean `default-features = false` core for programmatic consumers. |
| (b) IR crate + per-OS backend crates + facade | `sandbox-ir`, `sandbox-macos`, `sandbox-linux`, `sandbox-windows`, `sandbox` facade | Rejected. Per-OS crates that only compile on their OS is an anti-pattern in Rust — cfg-gating backends *within one crate* is the idiomatic norm (rustix, tokio, mio all do this). 4-5 crates to version in lockstep, worse docs.rs story, no compile-time win over target-gating. |
| (c) Two crates: neutral engine + nub-surface compiler | `<neutral>` = IR + apply + matcher + proxy + conformance; a nub-side `nub-sandbox-config` depends on it and holds `compile`/presets/`$(…)`/trust | The clean *long-term* target if the surface grows. More ceremony than (a) buys today; a `config` feature gets ~all of (c)'s benefit inside one crate. Keep as the escalation path. |

**Why (a):**

- **Compile-time / dep isolation is already handled by target-gating** for the heavy per-OS deps — no split needed for those. The *one* dep a split could shed is `regex` (used ONLY in `compiler/env_grammar.rs`), which the `config` feature-gate drops for programmatic consumers. That is the whole meaningful compile-time lever, and a feature flag captures it without a second crate.
- **External ergonomics.** `cargo add <neutral>` → the full thing; `cargo add <neutral> --no-default-features` → IR + `apply` + matcher + proxy, no nub grammar, no `regex`. One crate, one docs.rs page rendering the whole story.
- **In-flight work is preserved almost verbatim** (§7) — the module tree does not move; only the crate manifest and a `#[cfg(feature = "config")]` on the `compiler` module change.

Suggested feature layout:

```toml
[features]
default = ["config"]
config  = ["dep:regex"]     # nub-style JSON surface compiler (compile/CompileCtx/preset/env grammar)
# proxy is std-only + always useful → keep un-gated (no dep cost)
# conformance harness is tiny pure code → keep un-gated, or gate behind `testing` if you prefer
tracing = ["dep:tracing"]   # OPTIONAL: single use site (matcher/path.rs); gate for the leanest consumers
```

`tracing` appears at exactly one site (`src/matcher/path.rs`) — making it optional is a cheap courtesy to minimal-dep consumers; keeping it always-on is also defensible (it is near-universal and light). Maintainer's call; low stakes.

## 5. Naming — DECISION for the maintainer (recommend-only, do NOT lock without sign-off)

**The brand-boundary tension is real.** [`AGENTS.md`](../../AGENTS.md) "brand boundary" governs *public surfaces* — "no `@nub/*` scope … no nub-branded public surface a third party imports/depends on." Internal crate names carrying the brand (`nub-sandbox`, `nub-core`) are explicitly fine *as internal crates*. But a crate **published to crates.io for external reuse is exactly a public dependency surface** — a third party would write `cargo add nub-sandbox` and `use nub_sandbox::…`. That is the branded-public-import the boundary exists to prevent (crates.io is a different registry from npm, but the *spirit* — no nub-branded thing others import — applies identically). It is also just weaker positioning: nobody reaches for `nub-sandbox` as a general-purpose sandbox.

Two sub-decisions, both the maintainer's:

**Decision 1 — publish under a NEUTRAL name (recommended) or keep `nub-sandbox`.**
- *Recommendation: neutral.* Consistent with the brand boundary's treatment of published surfaces, and better for adoption. nub then depends on the neutral crate; internally nub-cli can either use `<neutral>::` directly (5 line changes) or add `use <neutral> as nub_sandbox;` to avoid call-site churn. This mirrors how nub already keeps a brand-clean public distribution surface while owning the code.

**Decision 2 — which neutral name** (only if Decision 1 = neutral).
- Existing sandbox crates to avoid colliding with / being confused for: `gaol`, `birdcage`, `extrasafe`, `rust-landlock`, `cap-std`. So the name must be checked free on crates.io first — **a crates.io availability + collision sanity check is a hard gate before any name is locked** (I cannot run it from here).
- Shortlist (coined, containment-evoking, short): **`warden`** (lead — an active enforcer/guard; matches the "engine that enforces a policy" role), **`cordon`** (to cordon off), **`bulwark`**, with `hedge`, `palisade`, `redoubt`, `enclave` as alternates. Descriptive fallbacks if all coined names collide: `os-sandbox`, `proccage`, `sandkit`.
- *Recommendation: `warden` pending availability*, else `cordon` / `bulwark`. I am NOT deciding this — it is a naming + light-brand call the maintainer owns.

## 6. Publishability

No hard `cargo publish` blockers. Checklist, roughly in order:

1. **De-inherit the identity fields.** Give the crate its OWN `version` (start `0.1.0`, decoupled from nub's `0.4.5`) and `repository`. `edition`/`rust-version`/`license`/`lints` can stay inherited (cargo flattens them at publish) or be restated for a standalone repo.
2. **Fix the license SPDX to reflect `NOTICE`.** Set `license = "MIT AND Apache-2.0 AND BSD-3-Clause"` (the Codex-adapted Seatbelt base + Chromium-derived policy) and ensure `NOTICE` is packaged. This is correctness, not optional — the current `MIT` under-states the provenance.
3. **Package the data + docs files.** `src/backend/macos_seatbelt_base.sbpl` is under `src/` so cargo includes it by default (it is `include_str!`-ed at build, `src/backend/macos.rs:40`); `NOTICE` and `LIMITATIONS.md` at crate root are included by default too. Add an explicit `include = [...]` only if you later add an `exclude` — but verify `cargo package --list` shows the `.sbpl`, `NOTICE`, and `LIMITATIONS.md` before first publish.
4. **`[package.metadata.docs.rs]`.** docs.rs builds on `x86_64-unknown-linux-gnu` by default, so the macOS + Windows backends `#[cfg]` out and their items vanish from the rendered docs. Add:
   ```toml
   [package.metadata.docs.rs]
   all-features = true
   targets = ["x86_64-apple-darwin", "x86_64-unknown-linux-gnu", "x86_64-pc-windows-msvc"]
   ```
   so all three backends render. The public `apply`/`compile`/IR signatures are already OS-stable (the cfg-dispatch is internal, `src/backend/mod.rs:228-235`), so the doc'd surface is consistent across targets.
5. **`[features]` block** per §4 (`default = ["config"]`, `config = ["dep:regex"]`, optional `tracing`).
6. **MSRV honesty.** Edition 2024 + `rust-version = "1.88"` is recent — declare it explicitly on the crate. `landlock` 0.4 / `seccompiler` 0.5 / `windows-sys` 0.61 / `globset` 0.4 / `ipnet` 2 / `regex` 1 are all mature; nothing forces a bleeding MSRV beyond edition 2024 itself. Note in the README that 1.88 is the floor.
7. **Docs.** Add a crate-level `//!` "getting started" (build a `SandboxPolicy`, call `apply`, read `Degradation`) and a `README.md` (crates.io card) that leads with the no-MITM / no-async-runtime / fail-safe-with-degradation posture and the OS-support matrix. `LIMITATIONS.md` is already an honest residuals record — link it from the README.
8. **Conformance module.** `run_fixture(&Fixture, &CompileCtx)` takes caller-supplied fixtures (nub's live in nub's `tests/`), so nothing to package; it is small pure code. Leave un-gated or gate behind `testing` — minor.

Non-blockers worth a line in the README, from `LIMITATIONS.md`: the engine is honest about bounded residuals (Linux `/etc` wholesale, macOS `KERN_PROCARGS2` ascendant-env, Windows program-dir subtree read) and launcher-handoff contracts — surfacing these builds trust rather than hurting adoption.

## 7. Migration plan — from today's `nub-sandbox` to the recommended structure, without breaking in-flight work

The extraction is low-risk because the consumer seam is 5 call sites + 2 glue fns (`crates/nub-cli/src/cli.rs:2651-2692`) and the crate is already dependency-clean.

1. **Do NOT block the sandbox epic on this.** Land `sandbox-primitives` as the internal `nub-sandbox` first (it unblocks the undocumented `nub run --sandbox` epic). The reuse extraction is a *follow-up refactor*, not a prerequisite — the code does not change, only the packaging.
2. **Rename + de-inherit** (one PR). `crates/nub-sandbox` → `crates/<neutral>` (stay in nub's monorepo initially). De-inherit `version`/`repository`; add the `[features]`, `[package.metadata.docs.rs]`, license-SPDX fixes from §6.
3. **Feature-gate the surface compiler.** `#[cfg(feature = "config")]` on `pub mod compiler;` and its re-exports (`src/lib.rs:24,31`), and `dep:regex` under `config`. nub itself keeps default features, so nub's `compile()` call is unaffected. Verify `cargo check --no-default-features` builds the lean core.
4. **Point nub at the new name.** Either update the 5 `nub_sandbox::` references in `cli.rs` to `<neutral>::`, or add `use <neutral> as nub_sandbox;` for zero call-site churn. `sandbox_homes`/`ambient_env` glue stays nub-side unchanged.
5. **Publish location.** Start by publishing from the monorepo subdir (path dep for nub, `cargo publish` from `crates/<neutral>`) — same low-overhead pattern nub already uses for its platform packages. Extract to its own repo (aube-style mirror) ONLY if it gains external traction; do not pay repo-split overhead up front.
6. **Optional polish** (§3): `CanonGlob::from_surface` + `FsPolicy`/`NetPolicy` builder helpers, so programmatic-only consumers don't hand-assemble rule vectors. Can trail the first publish.

Ordering keeps every step independently shippable and never regresses the sandbox epic: land internal → rename/de-inherit → feature-gate → repoint nub → publish.

## Open decisions for the maintainer (summary)

1. **Publish neutral vs keep `nub-sandbox`** — recommend neutral (brand boundary treats a published crate as a public surface). *Decision 1, §5.*
2. **Which neutral name** — recommend `warden` (else `cordon`/`bulwark`), *pending a crates.io-availability + collision check*. *Decision 2, §5.*
3. **`tracing` optional vs always-on** — low stakes; recommend optional for the leanest core. *§4.*
4. **Publish from the monorepo subdir vs a dedicated repo** — recommend subdir first, extract only on traction. *§7 step 5.*

None of these blocks landing the internal crate or the sandbox epic; all are the *reuse-extraction* follow-up.

## Changelog

- 2026-07-09 — Initial write-up. Prong E of the sandbox prior-art/reuse investigation. Audited the unmerged `crates/nub-sandbox` (`sandbox-primitives` worktree): confirmed PM-pure / nub-pure boundary, sketched the reuse-facing public API, recommended a single neutral crate with cfg-gated backends + a `config`-gated surface compiler, framed the neutral-naming decision for maintainer sign-off, and laid out the publishability checklist + a non-breaking migration path.
