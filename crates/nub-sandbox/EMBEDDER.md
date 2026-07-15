# nub-sandbox — the build-jail embedder seam

`nub-sandbox` is the frontend-less OS-enforcement engine (macOS Seatbelt / Linux
Bubblewrap / Windows AppContainer). It has no command grammar, reads no config
file, and carries no package-manager type. A *front-end* — the build-jail
(default-on dep-script confinement), a runtime profile (`nub <file>` / `nub run`), a
`nub sandbox -- <cmd>` launcher, a programmatic spawn API — is the **embedder**: it
discovers + parses config, resolves the host's paths/env, then drives this engine
through a two-call data seam. A Linux embedder also installs the early runtime hook
before application initialization.

This is the integration contract. The `lib.rs` module doc is the authoritative
in-code summary; this file is the expanded guide. Residual bounds live in
[`LIMITATIONS.md`](LIMITATIONS.md); the epic overview is
[`epics/sandbox/README.md`](../../epics/sandbox/README.md) (§3 architecture, §8
closures/residuals, §10 done-gate).

## The two-call seam

The data path has two functions over already-parsed data — the two boundaries of
the design. Linux adds one process-start hook that supplies Boundary B's runtime
authority:

- **Boundary A — surface config → resolved policy.** `compile` is the *only* code
  that understands surface syntax (presets, the `"..."` spread, glob ordering, the
  env grammar). It takes a parsed `serde_json::Value` (nub-cli loads/parses the
  file; the engine never opens one) plus a `CompileCtx` of host context, and emits
  the flat policy IR.

  ```rust
  pub fn compile(surface: &Value, ctx: &CompileCtx) -> Result<SandboxPolicy, CompileError>;
  // …with warnings surfaced instead of discarded:
  pub fn compile_with_warnings(surface: &Value, ctx: &CompileCtx)
      -> Result<(SandboxPolicy, Vec<CompileWarning>), CompileError>;
  ```

- **Boundary B — resolved policy + command → launch-ready child.** The apply seam
  dispatches to the per-OS backend and returns a `Prepared` child, or a fail-closed
  `Degradation` when a required axis cannot be enforced. No PM type crosses this
  line.

  ```rust
  // Linux confinement:
  let runtime = earliest_bootstrap()?; // first main action
  pub fn apply_with_runtime(
      policy: &SandboxPolicy,
      spec: CommandSpec,
      runtime: &RuntimeCapability,
  ) -> Result<Prepared, Degradation>;

  // Other platforms, and unconfined Linux commands:
  pub fn apply(policy: &SandboxPolicy, spec: CommandSpec) -> Result<Prepared, Degradation>;
  ```

The model is **compile-then-apply**: the IR is compiled once and consumed
in-process. It is `serde`-round-trippable (fixtures + a debug dump can serialize it)
but is NEVER deserialized on the enforcement path — there is no config re-read
between compile and apply. One `SandboxPolicy` and one early runtime capability can
drive many apply calls.

## Minimal usage sketch

Grounded in the real signatures (`Homes` is a plain public-field struct the embedder
fills from the host; `CompileCtx::new` takes homes/cwd/trust/ambient-env):

```rust
use nub_sandbox::{apply_with_runtime, compile, earliest_bootstrap, CommandSpec, CompileCtx, Homes};
use serde_json::json;
use std::collections::BTreeMap;

// 0. This must be the embedder's first main action, before threads or logging.
let runtime = earliest_bootstrap()?;

// 1. The embedder assembles host context — parsed data only (Boundary B).
let homes = Homes {
    home: dirs_home,        // per-OS anchors the symbolic roots expand against
    tmp: host_tmp,
    cache: host_cache,
    project: project_root,  // for `./`-relative patterns
};
let ctx = CompileCtx::new(
    homes,
    std::env::current_dir()?,
    /* trusted = */ true,                            // gates `$(…)` — see trust boundary
    std::env::vars().collect::<BTreeMap<_, _>>(),    // ambient env the child env is built from
);

// 2. Compile the surface `sandbox` block → resolved policy (Boundary A discharged).
let surface = json!({ "fs": ["./src"], "net": ["registry.npmjs.org"] });
let policy = compile(&surface, &ctx)?;

// 3. Apply the policy to a command → a launch-ready child (Boundary B).
let spec = CommandSpec::new("node")
    .arg("build.js")
    .cwd(&project_root)
    .deny_search_roots(workspace_and_package_roots);
let prepared = apply_with_runtime(&policy, spec, &runtime)
    .map_err(|deg| /* required axis unenforceable — fail closed, do NOT spawn */ deg)?;

// 4. Surface any degradation, then launch through the supervised seam.
if let Some(warning) = prepared.degradation.warning() {
    eprintln!("{warning}");   // "sandbox running in reduced mode — <axes> not enforced"
}
let status = prepared.status()?;
```

Two rules the sketch encodes:

- **On Linux, install `earliest_bootstrap` first and pass its capability to
  `apply_with_runtime`.** The hook pins the embedder's runtime authority before
  application threads. Bare `apply` remains sufficient for unconfined Linux commands
  and for other platforms, but a confined Linux request without the capability fails
  closed.
- **`apply_with_runtime` returning `Err(Degradation)` is a hard fail-closed** — a required axis
  could not be enforced. Do not spawn. A NON-empty `prepared.degradation.lost` on the
  `Ok` path is a partial degradation to SURFACE (fail-safe: it denies more, never
  less), not a hard stop.
- **Always launch through `Prepared::spawn`, `Prepared::status`, or
  `Prepared::output`.** The backend command is private so callers cannot bypass Linux
  startup verification, resource ownership, or Windows launch/teardown. A
  `PreparedChild` owns the running child, egress proxy, and private temp directory;
  Bubblewrap setup descriptors close in the parent immediately after spawn. Waiting or
  dropping releases the remaining resources, and dropping first kills and reaps.

## What crosses each boundary

**In — Boundary A (`compile`):**

| Type | Role |
| --- | --- |
| `&serde_json::Value` | the surface `sandbox` block, already parsed by the embedder |
| `CompileCtx { homes, cwd, trusted, ambient_env, runner }` | host context — symbolic-root anchors, cwd, the `$(…)` trust flag, the ambient env snapshot, the command runner (production shells out; tests inject a stub via `CommandRunner`) |
| `Homes { home, tmp, cache, project }` | per-OS anchors `~` / `<tmp>` / `<cache>` / `./` expand against |

**Through — the IR (`SandboxPolicy`):** flat, ordered, per-axis, fully resolved
(no residual surface syntax). `fs` (a last-match-wins `FsRuleSet` + tmp posture),
`net` (`enforce` + ordered `NetRule`s + deny-all base), `env` (the CONSTRUCTED child
env map + the `withheld` names + validation schema), `pid` (Linux env-read isolation
request). Every field is `serde`-round-trippable.

**Out — Boundary B (`apply`):**

| Type | Role |
| --- | --- |
| `CommandSpec { program, args, cwd, deny_search_roots, require_nesting }` | the host-provided command plus the bounded workspace/package roots whose existing direct children are checked for deny globs (no recursive discovery occurs in the engine). Set `require_nesting` when the launch must be able to create nested sandboxes: on an AppArmor-restricted Linux host it forces the dedicated administrator-installed helper at level 1 and fails closed with a precise setup/integrity/profile diagnostic when the host is not set up for nesting (see `setup/linux-nesting/`). Default `false` leaves single-level candidate selection unchanged. |
| `Prepared { degradation, .. }` | the private launch plan; launch through `spawn`, `status`, or `output`. It owns Bubblewrap supervision, data descriptors, the egress proxy, private temp state, and the Windows launch plan. |
| `PreparedChild` | the supervised running child returned by `Prepared::spawn`; it owns runtime resources until wait or drop, while setup-only descriptors close immediately after spawn. |
| `Degradation { lost, reason }` | which axes degraded (`warning()` → the one-line user string). `Err(Degradation)` = hard fail-closed; a non-empty `lost` on `Ok` = a surfaced partial |
| `CompileError` / `CompileWarning` | the `compile` failure / non-fatal-smell channels — each carries the surface path it occurred at |

## The launcher-handoff contract

For some guarantees the engine constructs the child's confinement correctly, but a
COMPLETE guarantee needs the launcher (which owns the parent process and the work-dir
layout) to satisfy an obligation the frontend-less engine cannot. These are NOT
engine defects — they define the seam. Full detail + bounds:
[`LIMITATIONS.md`](LIMITATIONS.md) "Launcher-handoff items".

1. **macOS toolchain read-confine.** The program auto-grant exposes the program FILE
   only (the parent-dir over-grant is deliberately closed). A non-system interpreter
   (Homebrew/nvm Node) then needs its toolchain directory in the read-allow set to
   load its own libraries; the engine does not probe the host for it. The launcher
   supplies that dir. A system interpreter is covered by the essential base.

2. **macOS parent-env scrub.** A sandboxed child can read nub's OWN argv+environ via
   `sysctl(KERN_PROCARGS2, getppid())` (not routed through Seatbelt). The engine
   constructs the CHILD's env least-privilege; it cannot scrub nub's own environ. The
   launcher must not hold ambient secrets in nub's environ at spawn (scrub pre-spawn
   or clean-env re-exec).

3. **Windows per-host egress and MITM.** An AppContainer child cannot reach a loopback egress proxy without a package-wide exemption. That exemption exposes every loopback listener, not just the proxy port, so the engine rejects per-host and per-request policies before launch. It reports `net-per-host` and, when applicable, `net-per-request`.

4. **Windows clean-DACL work root.** A confined work dir must sit under a CLEAN-DACL
   root — no inherited `ALL APPLICATION PACKAGES` allow-ACE (an AAP grant satisfies
   the LowBox check before default-deny, so an ungranted secret under an AAP-inheriting
   `%TEMP%`/profile tree stays readable). The launcher provides a clean root (e.g.
   strip inherited ACEs, or a nub-owned store). Ancestor traverse grants are NOT
   needed (traverse-bypass covers intermediate dirs).

5. **Untrusted-config trust boundary.** The engine CANNOT detect trust — the CALLER
   decides. `CompileCtx::trusted` gates `$(…)` command substitution: TRUE only for the
   user's own config (`nub.jsonc` / `scriptsMeta`), FALSE for a `dependenciesMeta`
   grant (an untrusted `$(…)` is a hard `CompileError::UntrustedSubstitution`, never
   exec'd). The launcher is responsible for securing untrusted-config usage (e.g.
   PR-CI, where the config itself is attacker-influenced). A future untrusted tier's
   tighten-only axis defaults are a front-end posture, not an engine mechanism.

## Net axis — proxy and the MITM tier

When a policy enforces per-host net (enforcing + at least one allow rule), `apply`
starts a loopback `EgressProxy` and stashes it on `Prepared` so it outlives the
child. The proxy does **no MITM**: it gates the CONNECT/SOCKS target host and the
cleartext TLS SNI (both must pass), then blind-forwards the tunnel byte-for-byte; a
pure deny-all policy needs no proxy (nothing is reachable). The per-host verdict is a
`GrantDecider` seam — wired to `StaticDecider` (the resolved `NetPolicy`,
last-match-wins) in this epic; the build-jail thread swaps in an interactive prompt
through the same seam without touching the proxy.

A capability-derived **MITM tier** (credential brokering — an ephemeral CA passed to
the child via an env bundle so the proxy can inject auth into allowed upstreams) is a
landed-but-held extension to the net/apply surface, PR #414. It rides the same
`GrantDecider` seam and platform-specific proxy routing; the core
compile/apply data seam is unchanged by it. Treat it as a forward reference until
it merges.

## PM-purity invariant

`nub-sandbox` is **PM-pure by construction** — the property the done-gate asserts and
the thing that keeps this seam clean:

- **No PM dependency.** `Cargo.toml` declares no `nub-cli` / `nub-core` /
  `vendor/aube` dependency (only serde/serde_json/tracing/globset/regex/ipnet and the
  per-OS libc/seccompiler/windows-sys dependencies, plus SHA-256 verification for the
  bundled Linux helper).
- **No PM type on the public API.** Everything the seam moves is plain data owned
  here: a `serde_json::Value` in, the `SandboxPolicy` IR through,
  `Prepared`/`Degradation`/`CompileError`/`CompileWarning` out. No aube/PM type
  appears in any signature.
- **Verification.** `cargo metadata --no-deps` shows only the non-PM deps above; the
  sole `nub-cli`/`aube` mentions in the source are doc-prose (the seam description
  and a byte-identical-classifier note), not code. An impact-analysis review leg
  asserts the dependency graph on change.

Do NOT add a PM dependency to this crate — it would collapse the boundary the future
build-jail embedder relies on.
