# Build jail design

**Status:** Design, 2026-07-27. Describes a nestable allow/deny policy model for dependency lifecycle scripts, covering filesystem, network, and environment, and how it maps onto the enforcement primitives already vendored in `aube-scripts`.

Dependency lifecycle scripts are the sharpest supply-chain edge in an install. Approval answers "may this package build?" This design answers the next question: "with what authority?"

## Baseline

The vendored engine already ships a jail (`vendor/aube/crates/aube-scripts/`). What exists, and what it cannot express:

| Capability | Today | Gap |
|---|---|---|
| Filesystem write | Allowlist: package dir, jail home, per-package `write` grants | Flat list, no deny-inside-allow |
| Filesystem read | Unrestricted (`(allow default)` on macOS, `add_rule(/, read_access)` on Linux) | Any approved script reads `~/.ssh`, `~/.npmrc`, sibling packages, project source |
| Filesystem exec | Not modeled | No toolchain boundary |
| Network | Boolean per package | A package needing one host gets all hosts |
| Environment | Allowlist by key name, plus per-package `env` grants | No value substitution, no brokered credentials |
| Composition | One flat map keyed by package pattern | No layering between user, project, workspace member, and package |
| Process nesting | Kernel inheritance only | A nested package-manager invocation re-derives policy from config |

Unrestricted reads are the largest hole and the one that gates the rest: several mechanisms below assume a script cannot enumerate paths outside its own jail.

## The model

One resolution rule over three domains. Each domain is a hierarchical key space, so the same trie-and-specificity logic serves all three:

| Domain | Operations | Key space | Nests along |
|---|---|---|---|
| Filesystem | read, write, exec | Absolute path | Path components, left to right |
| Network | connect | Host and port | DNS labels, right to left, plus a port axis |
| Environment | read | Variable name | Name prefix |

A rule is `(operation, pattern, effect)` where effect is allow or deny. Within one scope, the **most specific matching rule wins**, and deny wins an exact tie.

Specificity, not source order, decides. Order-dependent rules stop being composable the moment policy merges from several files, and this policy merges from at least five. Specificity-ranked rules make the merge commutative, so the same inputs always produce the same jail regardless of which file was read first.

Nesting inside a domain is then just longest-match:

```jsonc
"fs": { "read": {
  "deny":  ["~/**", "~/.cache/node-gyp/.npmrc"],
  "allow": ["~/.cache/node-gyp/**"]
}}
```

Home is denied, the gyp header cache is carved back out, and one file inside that carve-out is denied again. Three levels, no ordering ambiguity.

## Scope nesting

Policy composes down a chain, outermost to innermost:

```
builtin profile → user global → project nub.jsonc → workspace member
    → package grant (scriptsMeta / dependenciesMeta) → lifecycle hook
```

Each scope carries a **trust rank** derived from who authored it, not from what it says:

| Scope | Rank | May allow |
|---|---|---|
| Interactive CLI flag | 5 | Anything, logged on every use |
| Builtin profile | 4 | Anything |
| User global config | 3 | Anything the builtin floor permits |
| Project `nub.jsonc` | 2 | Anything the builtin floor permits |
| Root-authored `scriptsMeta` | 2 | Same as project |
| Dependency `dependenciesMeta` | 0 | Nothing |

The governing rule is one line: **deny is rank-free, allow is rank-gated.** Any scope may tighten. Only a scope at or above the delegation floor may loosen.

That is what makes `dependenciesMeta` safe to read at all. A package can drop privileges it does not need — worth encouraging, and a signal worth recording in the lockfile — and can never grant itself any. It extends the per-scope capability split already in the project rules, where root-authored config may use dynamic env values and brokered credentials and dependency-authored config may not.

### Resolution

```
effective(request):
    decision = Deny                                  # builtin default-deny
    floor    = builtin_floor(request.operation)      # min rank that may allow
    for scope in chain, outermost first:
        match scope.resolve(request):                # most-specific-wins, within scope
            Deny  -> decision = Deny
            Allow -> if scope.rank >= floor { decision = Allow }
                     else { config_error }
            Unset -> {}
        floor = max(floor, scope.raise_floor(request.operation))
    return decision
```

Two properties fall out. The floor is non-decreasing down the chain, so an inner scope can never make a grant easier than an outer scope allowed. And a grant the author's rank cannot make is a **config error at parse time**, never a silently dropped line — security config that fails quietly produces false confidence, which is worse than no jail.

Within-scope specificity resolves first, then the chain folds. Cross-scope, tightening always wins: an inner deny beats an outer, more specific allow. The two rules only appear to conflict; they operate at different stages.

Default delegation floors:

| Operation | Floor | Rationale |
|---|---|---|
| Filesystem read, write, exec | Project (2) | Ordinary build needs |
| Network connect | Project (2) | Ordinary build needs |
| Environment read of a secret-shaped name | User (3) | A checked-in project file should not hand a token to a build |
| Brokered credential injection | User (3) | Same |

A project may raise any floor. It may not lower one.

## Process nesting

A build script spawns `node-gyp`, which spawns `make`, which spawns a compiler. Any of them may invoke a package manager again.

Kernel inheritance covers most of this. A Landlock domain is inherited and can only be nested-tightened, and `PR_SET_NO_NEW_PRIVS` blocks setuid escape — both already applied in `linux_jail.rs`. A Seatbelt profile is likewise inherited.

The gap is a nested `nub` invocation, which would otherwise re-read config and compile a fresh, wider policy for its own child scripts. Fix: the parent passes a sealed digest of the compiled policy through an internal environment variable. A nub process that sees the seal treats its own config as a rank-0 child scope, so it can only tighten. The variable is internal cross-process plumbing, which the brand boundary exempts.

The seal is defense in depth for the mediated tier below, not the primary control. It must not be a bearer token — see the broker socket design under Environment.

## Filesystem

Three operations, not two. Execution matters independently: a build legitimately needs a compiler, and confining exec to toolchain prefixes stops a script from running an interpreter it dropped into its own package directory.

Default profile:

| Operation | Allowed |
|---|---|
| Read | Package directory, quarantine build dir, jail home, Node install, toolchain prefixes, curated system base |
| Write | Quarantine build dir, jail home, jail temp, `/dev/null` and friends |
| Exec | Toolchain prefixes, the package's own `node_modules/.bin` |

Everything else is denied, including project source outside the package, sibling packages in the virtual store, the lockfile, and every dotfile in the real home.

Default-deny reads are the expensive part, and the cost is honest to state: it breaks things that no build script explicitly asks for. A curated system read base has to cover at least `/proc/self`, `/dev/urandom`, the CA bundle, locale and timezone data, and `/etc/passwd`, which Node reads through `getpwuid` for `os.homedir()`. Getting that set wrong produces failures with no obvious link to the jail, which is why the diagnostics below are part of the same deliverable rather than a follow-up.

Landlock ABI v2 already supports read restriction. The current profile grants read on `/` by choice, not by limitation, so the enforcement change is small and the compatibility work is the whole job.

### Quarantine

Build the package in a copy, then copy back:

1. Reflink, hardlink, or copy the package into a nub-owned build directory.
2. Run lifecycle scripts with writes confined to that directory and the jail home.
3. Validate the output, then copy back into the linked package directory.
4. Key the side-effects cache on the resulting content digest.

Quarantine is what keeps the write policy simple enough to be correct: for nearly every package, "writes go to the build directory" is the entire rule, and per-package write grants become rare rather than routine.

Step 3 is where output shape is enforced — reject symlinks pointing outside the directory, reject device nodes and setuid bits, cap total output size — and it yields an audit trail, since the diff between input and output is exactly what the build produced.

## Network

Boolean network access is too coarse. A native package that fetches one prebuild host should not also reach the registry, a paste service, or the metadata endpoint of whatever cloud runner it is building on.

```jsonc
"net": { "deny": ["*"], "allow": ["github.com:443", "*.githubusercontent.com:443"] }
```

Enforcing that is harder than expressing it, and the design turns on being explicit about which rules the kernel enforces and which a broker enforces.

**Seccomp cannot do host filtering.** A BPF filter compares syscall arguments by value and cannot dereference the `sockaddr` pointer passed to `connect`. Family-level denial is the ceiling, which is what the current filter implements. No amount of work on the seccomp path yields host granularity.

That leaves three mechanisms:

| Mechanism | Granularity | Availability |
|---|---|---|
| Seccomp family deny | All or nothing per address family | Everywhere on Linux |
| Landlock network access | Port only, no host | Kernel 6.7 and later — verify ABI number before implementing |
| Network namespace plus filtering proxy | Host and port | Requires unprivileged user namespaces |
| Seatbelt `network-outbound` | Port reliably, literal IP with care | macOS |

Recommended shape: a loopback-only network namespace, a filtering CONNECT proxy on the host side, and proxy variables injected into the jail environment. Build tooling overwhelmingly honors proxy variables — `node-gyp`, `prebuild-install`, `curl`, and Node's own fetch all do — and anything that ignores them is left in a namespace with no route, so it fails closed rather than escaping. Where user namespaces are unavailable, host rules degrade to the port axis plus family deny.

### Enforcement tiers

Because the same policy line is enforced differently on different hosts, the tier is part of the resolved policy and is surfaced to the user:

| Tier | Meaning |
|---|---|
| `kernel` | Enforced by the OS; the script cannot bypass it |
| `mediated` | Enforced by a broker the script must traverse; bypass requires deliberately avoiding the broker while raw egress is denied |
| `advisory` | Scrubbed or best-effort, no OS enforcement |

The hard rule: **a policy that cannot be enforced at the tier it requires fails, and never silently downgrades.** The Linux jail already fails closed when the kernel cannot enforce the requested filesystem rules; this extends the same discipline to network and environment. A CI-oriented `--require-tier=kernel` turns any degraded host into an error rather than a quiet loss of protection.

Per-platform reality:

| Platform | Filesystem | Network | Environment |
|---|---|---|---|
| Linux, kernel 5.19+ | `kernel` (Landlock v2) | `kernel` for all-or-nothing, `mediated` for host rules | `kernel` |
| macOS | `kernel` (Seatbelt) | `kernel` for all-or-nothing and port, `mediated` for host rules | `kernel` |
| Windows | `advisory` | `advisory` | `kernel` for the process env |

Windows has no primitive comparable to Landlock or Seatbelt. The honest options are a restricted token plus a job object, which the vendored `windows_job.rs` already begins, or an AppContainer, which is a real boundary but requires a per-build capability profile and breaks common native toolchains. Recommendation: ship restricted token, job object, environment scrub, and a separate temporary home; declare the tier as `advisory`; treat AppContainer as future work rather than pretending the current state is equivalent.

## Environment

Environment is where credentials actually live, so the default is deny-all with a synthesized base rather than inherit-minus-denylist. A denylist cannot hold: new secret-shaped variable names appear continuously, and each one is ambient authority until someone notices. The existing key allowlist is already the right shape and should stay.

Nesting works by name prefix, with the same longest-match rule:

```jsonc
"env": {
  "deny":  ["*", "npm_config__auth*", "npm_config_*token*"],
  "allow": ["PATH", "npm_config_*"]
}
```

The in-tree tests already pin the case that matters here: `npm_config_arch` is inherited and `npm_config__authToken` is not.

### Substitution

Several variables are rewritten rather than passed through, unconditionally and at every rank:

| Variable | Value |
|---|---|
| `HOME` | Jail home |
| `TMPDIR`, `TEMP`, `TMP` | Jail temp, inside the jail home |
| `PATH` | Filtered to allowed exec prefixes |
| `npm_config_cache` | Jail-visible cache view |

Substituted values must be applied after any `env_clear` and must not be overridable by an inherited value of the same name. That ordering invariant is already established and tested in the script runner; the policy layer inherits it rather than reinventing it.

### Brokered credentials

Some builds genuinely need a credential, typically a token for a private prebuild host. Passing the ambient value is the wrong answer, because the ambient token is almost always broader than the build needs and outlives the build.

Instead, a scope at or above the credential floor declares a source rather than a value:

```jsonc
"env": { "set": { "GITHUB_TOKEN": { "from": "broker:github", "scope": "read:packages" } } }
```

Nub mints a scoped, short-lived value for that build. Dependency-authored config cannot use this form at all, which is the rank rule applied to the highest-value capability.

The broker is reached over a unix socket, and that choice has a sharp constraint. Landlock ABI v2 does not gate `connect()` to a filesystem socket path — the in-tree Linux jail documents this in the context of `/var/run/docker.sock` — so a jailed script can connect to any socket path it can name. Identity therefore cannot rest on anything the script could forward, including the process seal. Instead, each build gets its own socket, placed inside its own jail home, and identity is established by which socket accepted the connection. Under default-deny reads, no other build can name that path. This is a direct dependency: brokered credentials are not sound until the read profile lands.

## Compiled policy

The boundary between the nub policy layer and the vendored engine is a **compiled artifact, not config passthrough**. Nub resolves the full chain, applies rank gating, selects the enforcement tier, and hands the engine a flattened, sealed jail description. The engine's `ScriptJail` grows read and exec path sets and a host-and-port network rule set in place of the current boolean, and gains no knowledge of scopes, ranks, or delegation.

This keeps the fork delta thin and correctly placed. Read and exec sets plus tier-aware failure are default-preserving engine changes an upstream would accept on their own merits. Ranks, delegation, and the broker are nub-specific and stay above the boundary, in the layer that already owns config scoping and brand policy.

Configuration lives in the project `nub.jsonc` sandbox block and its global counterpart, matching the existing per-scope capability split. No engine-branded environment knob is exposed.

## Diagnostics

An unexplained build failure is what gets a sandbox turned off, so diagnostics are part of the feature, not a follow-up.

**Explain.** Print the resolved effective policy for a package with per-rule provenance: which scope, which file, which line granted or denied each capability, and at which tier it will be enforced. A jail nobody can inspect is a jail nobody will keep enabled.

**Audit mode.** A third setting alongside off and enforce, which runs the build and records what a stricter policy would have denied, so a project can collect the grants it actually needs before turning enforcement on. Fidelity is per-platform and should be stated plainly rather than smoothed over: macOS writes Seatbelt denials to the unified log, recent Linux kernels can log Landlock denials to the audit subsystem, and older kernels record nothing. Where no denial record exists, the fallback is a `diagnose` path that re-runs the failing script under a syscall tracer when one is present.

**Suggested grants.** When a jailed script fails, the resolved policy is known, so the error can name the capabilities the script most likely needed and print the exact config block that would grant them, scoped to the smallest pattern that covers the denial. This is the single highest-value piece of the diagnostic surface and is achievable on every platform, because it derives from the policy rather than from a denial record.

## What this does not stop

- **Malicious build output.** A package can still write a backdoor into its own package directory; that is the code the project asked to install. The jail bounds blast radius, not intent. Provenance and signing are the answer to intent.
- **Resource exhaustion.** Compute, memory, and disk are a separate axis handled by resource limits, not by the jail.
- **Unix socket reach on older Linux.** The `connect()` gap described above is real until a later Landlock ABI, and is mitigated by namespacing away sockets rather than by policy.
- **Host filtering without user namespaces.** Degrades to the port axis; the tier reports it rather than hiding it.
- **Windows.** Advisory until an AppContainer profile exists.

## Rollout

Each phase is independently shippable and independently useful.

| Phase | Change | Unblocks |
|---|---|---|
| 1 | Policy compiler: three domains, specificity resolution, rank gating, parse-time rejection of over-rank grants; compile to the existing flat jail | Everything below |
| 2 | Explain and suggested-grant diagnostics | Adoption of every later phase |
| 3 | Default-deny reads with the curated system base, behind audit mode first | Broker sockets, sibling-package isolation |
| 4 | Quarantine build directory with output validation | Simple write policy, audit trail, cache keying |
| 5 | Exec confinement to toolchain prefixes | Interpreter-drop defense |
| 6 | Host-level network via namespace and proxy, with tiers surfaced | Per-host grants |
| 7 | Brokered credentials | Scoped tokens replacing ambient ones |
| 8 | Process seal for nested invocations | Closes the nested-manager re-derivation gap |
| 9 | Windows restricted token and job object; AppContainer evaluated separately | Windows moves off pure advisory |

Enforcement defaults to off through phase 3, audit through phase 4, and enforce thereafter. The escape hatch stays, and stays noisy: disabling the jail turns an approved dependency build back into ambient code execution, and CI output should say so.
