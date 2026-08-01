# win-fsnet-ceiling — can a package get the widest unprivileged filesystem access AND no network?

The two axes are coupled on Windows through exactly one mechanism: coarse egress denial is a
**withheld AppContainer capability** (`internetClient`), so declining the LowBox token to widen the
filesystem hands the package the network as well. This probe measures whether a "widest grantable"
tier exists that keeps the token — and what, precisely, keeping it costs.

## The contradiction it settles

`.fray/sandbox-MECHANISM-FACTS.md` §5i reports `ERR 5 ACCESS_DENIED` de-elevated at `C:\`,
`C:\Users` and `C:\ProgramData`, and in the same section that `C:\` and `C:\ProgramData` are "both
of which a standard user CAN do WITHOUT the token". Those are **two different operations** —
installing an ACE (`SetNamedSecurityInfoW`, needs `WRITE_DAC`) versus creating an object (needs a
write right on the parent) — and the earlier harness measured `mkdir` for one and a DACL write for
the other, never a file CREATE at the same target under both instruments. Here every location gets
all four of create-file / create-dir / read / list, under raw Win32 **and** a real launched child,
in every arm.

## Run it

Branch-scoped, no PR: push to `probe/win-fsnet-ceiling` and
`.github/workflows/win-fsnet-ceiling-probe.yml` fires on `windows-latest` and `windows-11-arm`.
CI is the only venue — a LowBox token cannot attach a window station in services session 0, so
every launch over SSH returns `0xC0000142` and the standing `nub-win` VM cannot answer this
(MECHANISM-FACTS §5e/§5h). No Rust build: PowerShell plus a C# P/Invoke launcher.

## The arms

| arm | AppContainer | ACEs | `internetClient` | base token |
| --- | --- | --- | --- | --- |
| `plain-elev` | no | — | n/a | the elevated runner (proves canaries and paths are real) |
| `no-token` | no | — | n/a | restricted, **medium IL**, admin deny-only |
| `ac-bare` | yes | scaffold only | no | elevated |
| `ac-broad-net` | yes | **broad** | **yes** | elevated |
| `ac-broad-nonet` | yes | **broad** | no | elevated |
| `ac-broad-{net,nonet}-dv` | yes | broad | yes / no | restricted, medium IL |
| `plain-elev-2` | no | — | n/a | elevated, re-run **last** |

`ac-broad-net` and `ac-broad-nonet` share **one** AppContainer profile, one sid and one installed
ACE set, so the ACEs are literally the same bytes rather than two equivalent sets. The only
difference between them is the capability array.

## Why each guard is not optional

- **A positive control in every arm.** A uniformly-denied matrix reads exactly like "the ceiling is
  zero", and that misread has cost this effort two runs. `armtmp` is granted in every arm and must
  succeed; `C:\Windows` create must fail in every unelevated arm. An arm failing either is void.
- **An engagement proof per arm.** Each launch reads the CHILD's own primary token back through its
  process handle: `IsAppContainer`, package sid, **capability list**, integrity level.
  `tests/win-bypass-traverse/launcher.ps1` declared a `capabilitySids` parameter and then wrote
  `CapabilityCount = 0` unconditionally, so every arm it ever ran was a zero-capability arm. Only a
  read-back off the child's token makes that class of bug impossible to repeat.
- **Medium integrity on every de-elevated context.** `CreateRestrictedToken` *copies* the integrity
  level, so on an elevated runner a "de-elevated" token is still HIGH IL. Mandatory policy is
  checked before the DACL, so leaving it High would silently widen every de-elevated row.
- **Exact Win32 numbers, not just libuv's.** libuv collapses several distinct Win32 statuses onto
  one errno. The raw pass runs `CreateFileW` / `CreateDirectoryW` / `FindFirstFileW` in the parent,
  under the same impersonated de-elevated token, and reports `GetLastError()` verbatim.
- **The baseline re-runs last.** MECHANISM-FACTS §5e lost most of a run to a persistent side effect
  that presented as confinement; a repeat baseline is what catches it. It compares OK/ERR **class**,
  not exact values — the arms in between legitimately add directory entries.
- **The child must reach user code.** An unflagged confined `node` dies in `resolveMainPath`'s
  realpath before running a line (§5h). `--preserve-symlinks-main` repairs the entry point and
  `child.js` imports only `node:`-prefixed builtins, which never enter `_findPath`, so the rejected
  tree-wide `--preserve-symlinks` is not needed. `child.js` also contains no `child_process`: a
  piped `spawnSync` never returns under an AppContainer and swallows every op after it.

## Verified before pushing

The C# compiles under `pwsh` on any host (`. launcher.ps1`), `probe.ps1` parses, and the whole
control flow was exercised against a stubbed `Fx` class on macOS with a real `child.js` launch —
which is how the baseline-drift check was caught comparing directory-entry COUNTS and manufacturing
a failure.
