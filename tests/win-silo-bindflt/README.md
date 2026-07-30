# Windows application silo + per-silo `bindflt` — prototype spike

Tests whether a job object promoted with `JobObjectCreateSilo` (35), carrying per-silo `bindflt`
mappings, gives a private filesystem view under a **normal token** — and whether that dissolves the
three measured AppContainer blockers in `.fray/sandbox-MECHANISM-FACTS.md` §5e–§5h.

Measured on GCE `nub-win`: **Windows Server 2022 Datacenter, 10.0.20348, x86_64**, Node v24.11.0.
Not CI — the probe needs three different primary tokens (elevated admin, SYSTEM, standard user),
which a GitHub runner cannot give without the same Task Scheduler scaffolding used here.

## Verdict

| Question | Answer |
| --- | --- |
| Silo creation privilege | **None.** A standard user with 2 privileges promotes a silo. |
| `bindflt` mapping privilege | **Administrators membership.** Not a privilege — `SeTcbPrivilege` held *and enabled* still gets `ACCESS_DENIED`. |
| One-time or per-spawn? | **One-time.** An elevated helper can create silo + bindings and `DuplicateHandle` the job to an unprivileged process, which then launches into it. |
| realpath blocker | **Dissolved.** Unflagged `node <deep file>` reaches user code; `realpathSync('C:\')` returns `C:\`. |
| piped `spawnSync` | **No hang.** 80 ms, status 0. |
| realpath of a bind-linked path | Returns the **virtual** path, not the backing path. |
| `\\?\` and `\\?\GLOBALROOT\Device\HarddiskVolumeN\` | **Redirected, not escapes.** |

## Layout

- `probe/` — standalone zero-dependency Rust crate (own `[workspace]`). All Win32 declared by hand;
  `bindfltapi.dll` has no import library so `BfSetupFilter`/`BfRemoveMapping` come from
  `GetProcAddress`, as go-winio does.
- `run-arms.ps1` — SYSTEM and standard-user arms via Task Scheduler.
- `run-stduser.ps1` — standard-user arm, including the `SeBatchLogonRight` grant a fresh local
  account needs before a task will run at all (scaffolding, not a result).
- `run-stdpriv.ps1` — the privilege-vs-admin differential plus its positive control.
- `run-handoff.ps1` — the elevated-helper → unprivileged-client job-handle handoff.

## Reproduce

```sh
cargo build --release --target x86_64-pc-windows-gnu   # cross-compiles from macOS
scp target/x86_64-pc-windows-gnu/release/silo-probe.exe <win-host>:C:/silo-probe/
ssh <win-host> 'C:\silo-probe\silo-probe.exe --arm admin --root C:\silo-probe\admin \
    --node C:\node\node.exe --out C:\silo-probe\out-admin.txt'
```

Every arm carries its own controls: a host-side check that the mapping is invisible outside the
job (mirroring hcsshim's `TestSiloFileBinding`), a no-silo baseline run of the identical script,
and — in the handoff arm — the client attempting its own binding and being refused.

## Gotchas that cost a cycle

- **`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` is a prerequisite for the promote.** Without it,
  `SetInformationJobObject(job, 35, NULL, 0)` returns `ERROR_INVALID_PARAMETER`, which reads as a
  bad call shape rather than a missing setting. hcsshim's `Create()` sets it first and says so.
- **A policy-granted privilege lands in the token disabled**, and kernel checks test the enabled
  bit — so a "grant the right and retry" arm needs `AdjustTokenPrivileges` or it reports a false
  negative.
- **`TokenIsElevated` lies here too**: the standard-user task reported `token-elevated = true`
  while holding no admin authority. Gate on group membership, never the flag (same finding as
  `.fray/sandbox-MECHANISM-FACTS.md` §5h).
