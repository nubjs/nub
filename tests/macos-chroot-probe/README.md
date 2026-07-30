# macOS chroot / "fresh disk" probe

Settles one question: **can macOS give a process a genuinely rerooted filesystem — so `realpath`
and ancestor walks work naturally and no path allowlist is needed — with SIP ENABLED?**

Context: `wiki/research/build-jail-virgin-world.md` §3 and §3a (local-only). The short version —
`chroot(2)` in XNU needs only `suser()`, but AMFI's `execve` hook carries the string
`hardened runtime not allowed in chroot`, and every Apple platform binary runs with `CS_RUNTIME`
implied. A second, independent wall (launch constraints) SIGKILLs a plain copy of a system binary
even outside a chroot. The hypothesis under test is that `codesign -f -s -` strips both at once.

## Why CI and not the dev host

The probe needs `sudo`, mounts a `devfs`, and writes a multi-GB jail. It must not touch the
maintainer's machine. `macos-latest` runners are ephemeral and have passwordless sudo.

**The runner's macOS major may differ from the machine the finding is for.** Section 0.5 greps the
runner's kernel collection for the AMFI chroot strings so a pass can be attributed to the right
build — if the gate strings are absent, a pass proves nothing.

## Running it

Branch-scoped, no PR (see `.claude/skills/ci-adhoc-test`):

```sh
git commit --allow-empty -m rerun && git push   # pushing the branch runs it
gh run list --workflow macos-chroot-probe.yml --branch macos-chroot-probe
```

Locally on a throwaway Mac (never the dev host): `bash tests/macos-chroot-probe/probe.sh`.

## Structure

The script never fails the job; every stage records a verdict line into a ledger printed at the end.

| § | What it establishes |
|---|---|
| 0 | Test-bed validity: macOS version, **SIP status**, AMFI boot-args, whether this kernel carries the chroot gate, volume layout |
| 1 | Baseline — `sudo chroot / /bin/echo`, plus the AMFI kill reason from `log show` |
| 2 | **The ballgame.** 2x2 differential: {plain copy, ad-hoc re-signed} x {outside chroot, inside chroot}, same source bytes in every cell |
| 2b | Mechanism isolation with locally-compiled binaries, which have no Apple identity and so cannot be subject to launch constraints. Varying only `--options runtime` (and the undocumented `com.apple.security.cs.allow-in-chroot` entitlement) tests `CS_RUNTIME` directly |
| 3 | Jail construction: dyld shared cache at the legacy path, rsync + bulk ad-hoc re-sign, synthetic `/private/etc`, `devfs`, first real reroot |
| 4 | Criterion (a) `fs.realpathSync` on deep paths; criterion (b) whether `getpwuid` still leaks the real identity (expected — it is a Mach round-trip to `opendirectoryd`, which `chroot` cannot scope) |
| 5 | Criterion (c) `node-gyp rebuild` producing a **Mach-O arm64** `.node` that loads on the unjailed host, with a host-built positive control |
| 6 | Privilege shape: unprivileged `chroot(2)`, then a setuid-root helper — one-time root vs per-spawn root |

Sections 3+ are skipped if section 2 fails, because a failing §2 means macOS rerooting is closed and
the jail cannot be entered.
