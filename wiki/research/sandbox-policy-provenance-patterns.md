# Sandbox policy provenance & self-tamper — patterns from the wild

**Status:** research, 2026-07-10. Feeds the sandbox config-source design decision: a sandboxed process that can write its own policy file could relax its own confinement on the *next* run (a persistence/self-escalation vector). Surveys how ~15 real sandbox systems source and protect policy, names the recurring patterns, and answers three design questions — (a) does self-tamper apply to nub's model, (b) how do others make policy authoritative-and-unwritable, (c) how do others map per-task profiles ergonomically without a Deno-style per-invocation flag tax. **Recommend-only** — surfaces patterns + trade-offs; the product decision sits elsewhere. This doc is the cross-system provenance cut.

Grounded in local clones (`srt`, `codex`, `bubblewrap`, `firejail`, `nsjail`, `rust-landlock`, `deno`, plus `systemd`/`runc` clones for verification) and primary docs. Every claim cites a source; unverified items are flagged, not laundered into fact.

## 1. The axis that actually matters: byte-source writability at read-time

The intuitive framing — "is nub the *immutable-ratchet* class (safe) or the *file-re-read* class (unsafe)?" — is a **red herring**. Landlock/seccomp's ratchet (rules applied by a process to itself, tighten-only, unsheddable, inherited by descendants) only protects the **currently-running process tree's lifetime**; it has zero persistence layer, so it says nothing about the next invocation ([kernel Landlock doc](https://docs.kernel.org/userspace-api/landlock.html): *"Once a thread is landlocked, there is no way to remove its security policy; only adding more restrictions is allowed."*). Neither model protects the next run *by default*.

The real question is narrower and sharper: **is the policy's byte-source writable by the code about to be confined, at the moment before confinement (re-)applies?** nub is an *external supervisor* — it compiles a policy in the trusted parent (nub itself, in-process) then applies it to the child. So for nub the question reduces entirely to **where nub reads the surface config from**:

- In-project `package.json` / `nub.jsonc`, when the confined principal has project write (the runtime-profile / coding-agent case) → source is writable by the soon-to-be-confined principal → **vulnerable**.
- Parent-supplied in-memory object, CLI args, a control-fd, or a file *outside* the writable scope → **safe**.

Being an "external supervisor" buys nothing by itself: if the supervisor's compile step reads a file the *previous* confined run could write, model (b) is exactly as exploitable as model (a). The fix is a config-integrity property nub must own directly — policy source outside the confined process's writable roots, or re-validated/pinned before it is trusted again.

## 2. The named patterns

| # | Pattern | Exemplars | How it defeats self-tamper |
|---|---|---|---|
| 1 | **Argv-only, no policy file** | bubblewrap, nsjail, Deno `--allow-*` | No artifact exists on disk for the child to modify; policy lives only in the parent's process image as `exec()` args |
| 2 | **Root-owned profile loaded by a privileged principal into kernel state** | AppArmor, SELinux, systemd units, firejail's `/etc/firejail/*` half | Confined process runs in a lower trust domain with no path to the source file *and* no privilege to re-trigger the load |
| 3 | **Self-sandboxing immutable ratchet** | seccomp-bpf, Landlock | Kernel makes the policy irreversible within-process; provenance shifts to "was the right policy installed *before* untrusted code ran" (a sequencing question) |
| 4 | **Supervisor supplies policy out-of-band; child never has a path to the source** | gVisor (`config.json` host-side), Firecracker (host REST API), Codex (`$CODEX_HOME` outside workspace), srt (`--control-fd`) | The governing config is categorically outside the confined process's addressable world — stronger than "forbidden by permission bits" |
| 5 | **Policy baked into a signed artifact** | macOS App Sandbox entitlements | Embedded in the code signature; any self-modification invalidates it and the app won't relaunch — tamper-*evidence*, cryptographic not filesystem-based |
| 6 | **Invoker-supplied grant with a documented, unmitigated config-file backdoor** | Deno `deno.json` `-P` | *Not* a template — the cautionary case: same self-tamper hole nub fears, shipped and acknowledged (§5) |

Patterns **1 and 4 are the strongest fit for nub** (supervisor resolves policy, hands it down out-of-band). Pattern 2 is the OS-daemon model nub can't fully adopt (no privileged daemon). Pattern 3 is what nub's *enforcement* already is per-process; it just doesn't address next-run provenance.

## 3. Provenance & protection table

| System | Policy location | Author vs confined | Re-read per run vs immutable | Protection from the confined principal |
|---|---|---|---|---|
| **bubblewrap** | CLI argv | parent builds argv | applied once at exec | not a file — argv in parent's image only |
| **nsjail** | protobuf config file / CLI | operator authors | read once at start | parent holds it; not bind-mounted into the jail |
| **firejail** | `.profile` files; `~/.config/firejail/*` **before** `/etc/firejail/*` | user + distro/root | **re-read every run** | **WEAK** — user-writable profile checked first; a confined proc with `$HOME` write can plant one to weaken the next run |
| **systemd units** | `/etc/systemd/system/*.service` directives | root / package | read once at unit start | root-owned file; confined non-root service can't rewrite it; PID 1 applies at exec |
| **AppArmor / SELinux** | `/etc/apparmor.d`, `/etc/selinux` | admin | loaded into **kernel state**, not per-exec | confined process has no path to kernel-resident policy |
| **seccomp** | none (BPF via syscall) | process, on itself | **monotonic ratchet** | irreversibility *is* the protection; no file to guard |
| **Landlock** | none (syscalls, self-applied) | process, on itself | ratchet, per-tree lifetime | no file; but says nothing about next run (§1) |
| **macOS Seatbelt (legacy `.sb`)** | app-supplied profile via `sandbox_init()` | launching app | applied once at launch | self-applied |
| **macOS App Sandbox (modern)** | entitlements in the code signature | developer, at build | OS re-verifies signature each launch | cryptographic — self-modification invalidates the signature |
| **gVisor** | host `config.json` | host container mgr | read once at container start | Sentry reimplements every syscall; guest has no host path to it |
| **Firecracker** | host REST API | host VMM / jailer | at VM boot | full KVM boundary; guest has zero access to the API socket |
| **Deno `--allow-*`** | CLI argv | invoker | per-invocation | unreachable from the running script |
| **Deno `deno.json` `-P`** | project config file | **whoever last wrote `deno.json` — possibly the code itself** | **re-read per run** | **none** — documented, unmitigated backdoor (§5) |
| **Codex CLI** | `$CODEX_HOME/config.toml` **+** in-tree read-only carve-out | user | read at session start | config lives outside the workspace; `.git`/`.agents`/`.codex` forced read-only inside the writable root (§4) |
| **srt** | `~/.srt-settings.json` / in-memory / `--control-fd` | parent/harness | per CLI invocation, or in-memory | own settings file protected only by living outside `allowWrite`; hardcoded escalation-file denylist (§4) |

Verification notes: **systemd and runc are listed as Landlock adopters on landlock.io but neither actually implements it** in current `main` (grepped both clones; systemd only allow-lists the syscalls in its `@sandbox` seccomp group, `TODO.md` lists Landlock-backed `ProtectSystem=` as future work). Firejail's file-based `.profile` (the one shape resembling nub's worry) is externally reported but its primary doc 404'd this session — the `~/.config/firejail`-before-`/etc/firejail` resolution order is from `firejail/src/man/firejail-profile.5.in`.

## 4. The two closest precedents — both do the same two-mechanism combo

**Codex CLI** (`codex-rs/protocol/src/permissions.rs:1593`, `default_read_only_subpaths_for_writable_root`) is the closest structural match to nub — an external supervisor confining a semi-trusted coding agent, config-driven. It defends self-tamper with **both**:
1. **Trust-domain separation** — authoritative config at `$CODEX_HOME` (`~/.codex/`), *outside* the `workspace-write` sandbox's writable roots; the agent has no filesystem path to the file governing its own policy.
2. **In-tree escalation-file carve-out** — `.git`, `.agents`, `.codex` forced read-only *even inside an otherwise-writable root*. Verbatim: *"folders containing files that could be modified to escalate the privileges of the agent (e.g. `.codex`, `.git`, notably `.git/hooks`) under a writable root are not modified by the agent."*

**srt** (Anthropic sandbox-runtime, `src/sandbox/sandbox-utils.ts:11-39`) does the same shape: a hardcoded `DANGEROUS_FILES` / `DANGEROUS_DIRECTORIES` write-deny (`.mcp.json`, `.claude/commands`, `.claude/agents`, shell rc, `.git/hooks`, `.git/config`) appended **unconditionally on top of** whatever `allowWrite` grants — `allowWrite: ["."]` still cannot make `.mcp.json` writable. On macOS it also blocks `file-write-unlink`/`file-write-create` on the path and its ancestors (`generateMoveBlockingRules()`), closing the delete-and-recreate and symlink-swap bypasses. **Gap worth noting:** srt's *own* settings file gets no such protection — it is safe only by living in `$HOME` outside the default `allowWrite`. Point `allowWrite` at `$HOME` and it is exposed. This is the exact gap nub has if a confined principal can write `nub.jsonc`.

Both independently converge on: **policy source outside the writable scope (primary) + an unconditional escalation-file denylist inside writable roots (belt-and-suspenders).**

## 5. The cautionary tale: Deno `deno.json`

Deno ships *both* the clean pattern (`--allow-*` argv) and a weaker variant (`deno.json` `-P` permission sets) side by side, and its own docs are candid ([docs.deno.com](https://docs.deno.com/runtime/reference/deno_json/#permissions)): *"The threat model for permissions in the config file is similar to `deno task`, in that a script could modify the `deno.json` to elevate permissions."* Its mitigation is purely **procedural** — require explicit `-P` opt-in every run, don't load it implicitly — with **no** read-only carve-out and **no** separate trust domain. This is the most directly comparable real-world precedent to nub's `package.json`/`nub.jsonc` situation, and it is a documented-but-unsolved instance of exactly the problem, not a template to copy.

## 6. Ergonomics — how the authoritative systems avoid the Deno flag tax

**Every genuinely authoritative system uses a trust-domain split, not a file-permission trick.** The lower-trust actor (confined code, or the workload author) writes into a surface a *different, higher-trust enforcement point* evaluates and which the lower-trust actor cannot widen by rewriting its own layer. Policy is declared **once, ambiently, attached to the entity** (unit file, job, pod spec, namespace label, settings scope) and read automatically at exec/admission/build time — nothing restates flags per call. Two variants:

- **Ceiling / cap** — k8s Pod Security Admission (writable pod spec clamped by an admin-owned namespace label; admission *rejects* anything exceeding the level), GitHub org default-permissions (a restrictive org default disables the permissive repo option), Chrome `managed/` (unoverridable) vs `recommended/` (overridable default), Claude Code's 5-tier *"Managed (highest): can't be overridden by anything,"* Nix's `__noChroot` request gated by a daemon-level `sandbox = relaxed` toggle in root-owned `nix.conf`.
- **Named-profile indirection** — systemd vendor-unit + root-owned `.d/` drop-ins; Claude Code's `managed-settings.d/*.json` alphabetical merge. Closest to a "writable name → unwritable definition" design, though none does a clean literal lookup.

**k8s Pod Security Admission is the closest full analogue to nub's tighten-only design** — effective posture = most-restrictive-of(writable spec, admin-owned ceiling). **docker-compose is the negative control**: ergonomic (per-service `security_opt`/`cap_drop`, declared once) but *zero* built-in ceiling — a single writable trust domain end-to-end. It proves ergonomics alone doesn't buy authority; you need the second trust domain.

The authority is free *because* the write path for "policy" and the write path for "the code that runs under it" are different trust domains. That is why none of these needs a per-invocation flag.

## 7. Recommendations for nub, per front-end (recommend-only)

- **Programmatic API (coding-agent harness, incl. the bash-tool case):** parent supplies policy out-of-band — an in-memory object, or a control-fd à la srt's `--control-fd`. No file the child can reach (patterns 1/4, the dominant industry choice). This surface needs no config separation. For a bash/shell tool confining *untrusted agent-issued commands*, the tight default (read-mostly + a scratch dir, net-deny/allowlist, env-scrubbed) is the norm and matches srt's posture.
- **Runtime profile / `nub sandbox` reading in-project config:** keep `sandbox` **ergonomic and ambient** in `package.json`/`nub.jsonc` (the writable pod-spec equivalent) — do *not* force `*.sandbox.json` + explicit `--sandbox` everywhere (that imports the Deno tax for no security gain). Get *authority* from the **ceiling/clamp**: the escape axes (env / net / fs-outside-project) are floored by an **unwritable higher tier** (`~/.config/nub` outside the project; managed `/etc/nub`), tighten-only, effective = most-restrictive-of(project, ceiling). This is nub's already-decided tighten-only layering = the k8s PSA pattern, now with industry precedent.
- **All in-project-file front-ends:** add Nub's own config homes (`nub.jsonc`, `package.json`, any future `*.sandbox.json`) to the build-jail-style mandatory write-deny set — exactly what SRT (`DANGEROUS_FILES`) and Codex (`default_read_only_subpaths_for_writable_root`) both do, including the delete-recreate/symlink-swap closure. Linux can mask startup-existing paths in its Bubblewrap view. macOS expresses the carve-out natively. Windows does not currently enforce a deny inside a broadly granted arbitrary project; the protected-DACL inheritance-break idea remains unimplemented, so a load-bearing front-end must reject that policy until a controlled-root or equivalent mechanism lands. This is why the unwritable-tier ceiling remains the primary mechanism and the in-tree denylist secondary.
- **Enablement vs the flag tax:** explicit *enablement* (is the sandbox on — `--sandbox` / `NUB_SANDBOX` / programmatic) is a reasonable posture and is **not** the Deno tax; the Deno tax is restating *per-permission* grants per invocation. Ambient enablement (config-presence) is also safe *provided* the escape floor is unwritable. Don't conflate the two.

## 8. Direct answers to the design questions

- **(a) Does self-tamper apply to nub?** Yes — but not because of any ratchet-vs-file distinction (a red herring). nub is an external supervisor that compiles then applies; the vector exists precisely when nub reads its surface config from a location the soon-to-be-confined principal can write. It does **not** apply to the programmatic/out-of-band path, and is closed for build-jail by its own-package-dir write scope. It is live only for the in-project-file front-ends. Deno proves it's a real, shipping-product hole when unaddressed.
- **(b) Standard ways to make policy authoritative-and-unwritable:** (1) don't put it on disk — argv/in-memory/control-fd (patterns 1/4); (2) put it in a different trust domain the confined principal can't write — outside the writable scope (Codex `$CODEX_HOME`, gVisor/Firecracker host-side) or root-owned (systemd, AppArmor); (3) an unconditional escalation-file denylist inside writable roots (srt, Codex); (4) cryptographic tamper-evidence (macOS entitlements) if evidence rather than resistance is wanted. Not file-permission tricks alone — a trust-domain split.
- **(c) Per-task profiles without the Deno tax:** declare policy once, ambiently, attached to the entity; a higher-trust enforcement point reads it automatically and clamps (ceiling pattern) — k8s PSA is the closest analogue and equals nub's tighten-only layering. A writable *assignment* (`scriptsMeta.<name>.sandbox`) referencing an unwritable *profile definition* is not done cleanly by any surveyed system but is an unproblematic extension of the ceiling + scope-precedence models; it stays safe as long as the assignment can only tighten, never loosen past the floor.

## Unverified / flagged

- Claude Code's bash-tool → srt wiring (per-call CLI vs in-process library + `--control-fd`) could not be confirmed from the srt clone alone; Claude Code source isn't present. srt's CLI path *does* re-read `~/.srt-settings.json` from disk on every invocation with no cache.
- systemd & runc listed as Landlock adopters on landlock.io but **not** implemented in current `main` (stale claims).
- Firejail's `.profile` self-tamper exposure: resolution order verified from the man source; the primary Landlock-support doc 404'd — the specific "confined proc can plant `~/.config/firejail/*.profile`" claim is structural inference from the resolution order, not a quoted advisory.
- Exact file modes (SELinux `/etc/selinux/*`, Nix `/etc/nix/nix.conf` root:root 644) not quoted from primary docs — strongly implied by the daemon/root model, not verbatim.
- GitHub Actions: whether an explicit workflow `permissions:` can exceed a restrictive org default (hard ceiling vs fallback-default) not primary-sourced; the org-cascade-disables-permissive-option behavior *is* quoted.
- Codex's own upstream `config.toml` writability (is `$CODEX_HOME` ever agent-writable) not chased.

## Changelog

- 2026-07-13 — **REVERSAL:** updated the per-OS config self-tamper mechanism. Linux now uses startup-existing Bubblewrap masks; Windows inheritance-break is an unimplemented proposal, not a current guarantee, so load-bearing in-project config denies must reject there.
- 2026-07-10 — Initial write-up. Four-prong survey (srt, Landlock, the broader landscape, ergonomic profile-mapping); prong findings in `/tmp/sbx-research/{srt,landlock,landscape,ergonomics}.md`.
