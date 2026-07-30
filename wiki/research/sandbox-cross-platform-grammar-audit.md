# Sandbox grammar across macOS, Linux, and Windows

Status: in progress  
Last updated: 2026-07-24

## Goal

Nub should accept one sandbox language with one stated meaning on macOS, Linux,
and Windows.

```text
accepted setting
      ↓
same security promise on every operating system
      ↓
otherwise fail before the child starts
```

A fail-closed launch error is safer than a bypass, but it is still a product
limitation. This audit separates a working portable contract, a fixable backend
gap, a host prerequisite, and a platform limit that requires narrowing or
removing grammar.

The public `nub.jsonc` Schema and live docs intentionally omit every sandbox key.
This report covers the hidden `nub run --sandbox <config-file>` engine.

## Open-items ledger

- [x] Enable and test the non-sandbox `nub.jsonc` surface.
- [x] Keep sandbox fields out of the public Schema, live docs/navigation, sitemap,
  search, LLM exports, and search-engine indexing.
- [x] Audit the portable filesystem, network, environment, and composition grammar.
- [x] Close the Linux fine-network Unix-socket escape in the target seccomp filter.
- [x] Finish and verify the strict environment parser fixes.
- [x] Finish and verify Windows environment construction fixes on the host;
  real Windows execution remains in the stable-SHA CI gate.
- [ ] Prove macOS nested attenuation and full descendant cleanup on real macOS.
- [ ] Prove mandatory Linux three-level composition on a prepared Linux host.
- [ ] Implement the parent-held nested launcher on macOS and Windows.
- [ ] Implement and prove Windows exact nested denies, private temp, and bounded globs.
- [ ] Decide whether Windows fine networking may require administrator-authorized WFP setup.
- [x] Replace the unapproved fixed-header injection prototype with the approved
  exact-host environment-marker broker, including live TLS coverage and clean
  independent security/impact reviews.
- [ ] Resolve the remaining grammar questions in the maintainer decision batch.
- [ ] Reconcile the hidden example-heavy sandbox reference with the final grammar.
- [ ] Run complete macOS, Linux, and Windows ad-hoc fixtures plus durable conformance tests.
- [ ] Run formatting, clippy, targeted tests, full CI-equivalent gates, and impact review.
- [ ] Open the `nub.jsonc`-led PR and keep it active through CI and review.

## Current verdict

The sandbox is not yet one production-ready cross-platform product.

| Area | macOS | Linux | Windows |
|---|---|---|---|
| Coarse filesystem confinement | Enforced | Enforced with Bubblewrap | Enforced with AppContainer |
| Exact nested filesystem denies | Enforced | Enforced | Fixable; current rejection is wrong |
| Non-literal filesystem globs | Live path matching with rename gaps | Bounded startup expansion or rejection | Bounded startup expansion is feasible |
| Private temp | Enforced; incompatible with Apple native builds | Enforced | Fixable; not wired into AppContainer launch |
| Coarse network deny | Enforced | Enforced | Enforced |
| Host/CIDR network rules | Proxy-aware TCP | Proxy-aware TCP | Fixable only with an elevated WFP boundary |
| Environment construction | Enforced | Enforced | Enforced with key-case and startup-key bugs |
| Direct nested sandbox | Unsupported; inner Seatbelt application fails closed | Implemented | Unsupported; inner AppContainer cannot attenuate itself |

## Proposed portable contract

This is a recommendation, not a ratified public API.

### Filesystem

- Exact paths and literal directory subtrees remain live rules.
- Every non-literal glob is expanded once, within bounded roots supplied by the
  launcher.
- Existing matched directories continue governing their descendants.
- A path that begins matching a glob only after launch does not gain or lose
  access.
- Unbounded, out-of-root, unreadable, or over-budget expansion fails before
  launch.
- A denied startup file with more than one hard link fails before launch on
  every operating system.

This startup-snapshot model is less dynamic than the current macOS regular
expressions, but it aligns Bubblewrap mounts, Windows DACLs, and Seatbelt literal
rules. It also closes the known macOS floating/partial-glob rename bypasses for
captured paths.

### Network

- `net: true` and `net: false` are coarse modes and start no proxy.
- Fine-grained host/CIDR rules mean proxy-aware TCP.
- Direct sockets, child-side DNS, UDP, and unsupported protocols fail closed.
- Nub derives the pass-through proxy from the requested capability; users do
  not choose proxy implementation modes.
- Host and CIDR rules are currently port-agnostic.
- Private ranges require an explicit token; link-local/cloud-metadata targets
  remain hard-blocked.
- Host loopback and host filesystem sockets need explicit final decisions.

### Environment

- Nub constructs the complete child environment; withheld values are absent.
- Windows-only startup variables are internal launch mechanism, not
  platform-specific user grammar.
- Windows keys are compared, replaced, and serialized case-insensitively.
- Regex and literal-union validation survive in the compiled representation.
- Malformed regex and wrong JSON types fail while compiling the config.
- `sensitive: true` must either redact every Nub diagnostic/serialization path
  or be removed.
- Shell-string `$(...)` is not portable: it means `sh -c` on POSIX and `cmd /C`
  on Windows. The recommended replacement is a trusted-only direct argv form.

### Composition

An inner sandbox may only preserve or tighten the outer sandbox.

1. The OS boundary must force the intersection.
2. A nesting-specific capability must be proven before target release.
3. The inner compiler sees only the already-narrowed environment and view.
4. The outer owner reaps the full nested process tree.
5. An inner `allow all` must fail to recover outer filesystem, network, or
   environment access in real per-OS tests.

## Confirmed findings

### Windows filesystem gaps are mostly implementation bugs

The current backend rejects a deny below an allowed parent at
`crates/nub-sandbox/src/backend/windows.rs:424-436`. That premise is incorrect:
Windows orders explicit deny ACEs before explicit allows and before inherited
ACEs.

For existing denied leaves/directories, Nub can add an explicit deny ACE for its
unique per-run AppContainer SID. Teardown must remove only that run's deny ACE;
restoring an old whole DACL would overwrite concurrent external ACL changes.

Positive wildcard grants can be materialized over bounded startup matches.
Windows DACLs cannot apply a future pathname predicate to a file that does not
exist yet; full live glob semantics would require a signed filesystem
minifilter.

The shared helper creates a private temp directory, but the actual
`WindowsLaunch` builds its environment without it. The AppContainer profile's
own temp folder can supply `TMPDIR`, `TMP`, and `TEMP`; real Windows tests must
also prove direct access to the old host temp remains denied.

Primary sources:

- https://learn.microsoft.com/en-us/windows/win32/secauthz/order-of-aces-in-a-dacl
- https://learn.microsoft.com/en-us/windows/win32/api/aclapi/nf-aclapi-setentriesinacla
- https://learn.microsoft.com/en-us/windows/win32/secauthz/implementing-an-appcontainer

### Windows hostname rules have an elevated path

AppContainer capabilities alone cannot expose only Nub's random loopback proxy
port. The package-wide loopback exemption exposes every local listener.

Windows Filtering Platform can match the per-run AppContainer SID plus remote
address, protocol, and exact port at `ALE_AUTH_CONNECT`. A dynamic session can:

1. permit only Nub's random loopback proxy port;
2. block every other TCP/UDP/IPv4/IPv6/loopback connection for that SID;
3. disappear if the owning process dies.

This requires administrator-authorized setup or a narrow privileged helper.
Without it, host rules must fail before launch. It needs a real Windows suite
covering direct IP, UDP, IPv6, a second loopback listener, concurrent runs,
crash cleanup, and enterprise firewall conflicts.

Primary sources:

- https://learn.microsoft.com/en-us/windows/win32/fwp/about-windows-filtering-platform
- https://learn.microsoft.com/en-us/windows/win32/fwp/filtering-conditions-available-at-each-filtering-layer
- https://learn.microsoft.com/en-us/windows/win32/fwp/object-management

### macOS filesystem residuals can mostly share the snapshot fix

Seatbelt matches paths rather than inode identity. A denied file with a
pre-existing allowed hard-link alias is readable through the alias
(`tests/macos_enforcement.rs:771-805`). A startup `nlink > 1` preflight can
reject this, mirroring Linux at `backend/linux.rs:970-1003`. A same-UID process
can still race setup; that is the same class as Linux derive-to-mount TOCTOU.

Live macOS regex rules cannot safely pin containers for floating
`**/secrets/**` and partial `sec*/x.key` directory components
(`tests/macos_moveblock.rs:273-365`). Expanding them to literal startup matches
lets the existing literal ancestor pins close the rename path.

One real limitation remains: Apple's native compiler tooling requires the
non-redirectable `DARWIN_USER_TEMP_DIR`, while private/deny temp must hide that
same shared directory (`backend/macos.rs:241-265,380-393`). Nub cannot provide
both promises. A native build must use shared temp or fail before launch.

### Linux Bubblewrap single-level path is credible; nesting proof is optional

For single-level runs, Linux tries trusted system Bubblewrap and then a
digest-verified bundled copy (`backend/linux.rs:1562-1725`). Every candidate
passes the real monitor-backed behavior probe before use
(`backend/linux.rs:668-750`). Existing CI exercises system and bundled paths.

For `require_nesting`, current code always requires the administrator-installed
`/usr/libexec/nub/nub-bwrap`, even on hosts without the AppArmor restriction.
Hosted CI allows nesting tests to skip. A configured Ubuntu runner must install
the helper and require the three-level composition test.

The fine-network Unix-socket escape is now closed. The target-only
`PER_HOST_PERMITTED` set contains only `AF_INET` and `AF_INET6`; arbitrary
filesystem and abstract `AF_UNIX` sockets fail. The trusted bridge still works
because it is forked outside the target's seccomp filter
(`linux_monitor.rs:4557-4566`), and local `socketpair` IPC remains available
because the filter governs `SYS_socket`, not `SYS_socketpair`. The regression
also carries an unrestricted-network control.

Linux also rejects non-whole wildcard filesystem allows, despite the hidden
syntax page showing them. The portable snapshot compiler must cover positive
and negative glob rules.

### Environment compiler has seven fixable correctness gaps

1. Windows essentials are added for `env: false`, but not every constrained
   array/object form.
2. Windows may emit case-equivalent duplicates such as `Path` and `PATH`.
3. `sensitive: true` is metadata while raw values remain `Debug`/`Serialize`.
4. Regex and literal-union constraints disappear from compiled IR.
5. Malformed regex may pass when its optional variable is absent.
6. Non-boolean `optional` and `sensitive` values silently default.
7. The documented "actual child environment" omits proxy, CA, and temp
   overlays.

These are implementation defects, not reasons for different grammar.

### Fine-grained networking is a proxy capability

The proxy supports HTTP CONNECT and SOCKS5 CONNECT, not SOCKS UDP associate
(`proxy/handshake.rs:1-9,102-140`). It resolves and pins DNS before connecting
(`proxy/mod.rs:409-438`).

macOS permits only the proxy loopback port; Linux uses an empty network
namespace plus a bridge. Direct TCP and child DNS fail closed on both. Windows
requires the WFP design above.

Current rules are port-agnostic. Current `*` and explicit loopback CIDRs can
reach host loopback through the proxy; `<private>` does not govern loopback.
NAT64/6to4 translation may wrap private/link-local addresses in a public-looking
IPv6 address. Network-specific NAT64 prefixes cannot be identified completely.

### Composition is production-ready only on Linux

Linux directly stacks mount/network/PID/seccomp restrictions and compiles the
inner environment from the already-scrubbed outer environment. Its retained
monitor owns transitive cleanup. The three-level harness attacks outer
filesystem, network, environment, PID, and process-lifetime ceilings.

Real macOS probes disprove direct composition. An ordinary child inherits the
outer Seatbelt profile, but applying a second profile from inside it fails with
`sandbox_apply: Operation not permitted`. Granting `process-exec` with
`no-sandbox` makes the inner command run by stripping the outer protection; the
probe then reads the outer-denied secret. That escape must never be granted.

A parent-held launcher proof successfully applied the exact outer-and-inner
intersection to a sibling process. SBPL's `require-all`, `require-any`, and
`require-not` combinators can preserve each ordered policy layer rather than
flattening away last-match behavior. A private authenticated Unix socket maps
each child to immutable parent-held authority, and deeper requests receive new
narrower context tokens.

Cleanup remains incomplete: a dedicated process group covers ordinary
descendants, but a compiled `setsid()` probe escapes process-group and launchd
cleanup while remaining Seatbelt-confined. Public unprivileged macOS APIs do
not offer a kill-on-close process-tree primitive; coalitions require privilege.

Windows cannot directly attenuate an AppContainer from inside that
AppContainer. The child normally inherits the same token, the low-level LowBox
constructor requires Medium integrity or higher, and Microsoft's newer sandbox
API rejects AppContainer callers. The smallest sound design is an authenticated
parent-held session launcher: the original Nub process retains the immutable
outer ceiling, gives the child only an unguessable inherited pipe handle, checks
each requested inner policy as a strict subset, launches a fresh AppContainer
for the intersection, and owns all children in one kill-on-close Job.

## Required verification before launch

### macOS

- explicit pre-launch rejection until a parent-held nested launcher is proven;
- parent-held intersecting-launcher proof, if adopted;
- descendant cancellation leaves no survivor;
- snapshot glob rename and future-match boundary;
- hard-link preflight;
- private temp ordinary command and native-build failure.

### Linux

- mandatory prepared-host three-level nesting;
- system and bundled Bubblewrap candidate paths;
- arbitrary filesystem Unix-socket egress;
- snapshot wildcard allow/deny and hard links;
- proxy-aware allowed/denied host plus direct/UDP/DNS negatives.

### Windows

- explicit nested deny plus allowed sibling and exact DACL cleanup;
- bounded wildcard startup materialization;
- private temp plus old-host-temp negative;
- WFP exact proxy-port permit and all other transport negatives;
- concurrency/crash cleanup;
- nested composition or explicit pre-launch rejection.

## Open decisions

The complete answerable batch lives in
the sandbox grammar question draft
until it is sent to the maintainer. It covers:

- portable glob snapshots;
- Windows and Linux administrator setup;
- proxy-aware TCP, ports, loopback, private ranges, NAT64, and host IPC;
- private temp;
- secret floors and partial objects;
- substitutions and environment redaction;
- source layering and nested composition;
- credential-marker brokering and its HTTPS inspection requirement.

## Changelog

- 2026-07-24 — Initial write-up from the Windows filesystem/network, macOS
  filesystem, Linux Bubblewrap, environment grammar, network grammar, and
  composition audits.
