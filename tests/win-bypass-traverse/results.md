# Results

Three runs, all on `windows-latest` (Server 2025 Datacenter 10.0.26100, AMD64) and `windows-11-arm` (Windows 11 Enterprise 10.0.26200, ARM64), Node 22.23.1. **Every number below is identical on both images.** Run 3 is the complete one: all 24 properties pass, `FAILURES = 0`.

| run | id | outcome |
| --- | --- | --- |
| 1 | `30506129146` | AppContainer launched; every arm died at `EPERM lstat 'C:\'` before user code, so no read cell was measured |
| 2 | `30506477831` | with the realpath-skipping flags the table ran: deep reads work. One red cell — a piped spawn hung and took the egress table with it |
| 3 | `30507134879` | all properties pass: deep reads, `$HOME` secrets denied, egress denied, zero-setup gate, ACE cost |

## The answer

**A deep read needs no ancestor ACE. `C:\` and `C:\Users` stay exactly as the image ships them.**

With one inheritable grant at `%USERPROFILE%\<project>` — or leaf-only grants that leave the project root ungranted too — the confined child, five components down at `%USERPROFILE%\<proj>\data\proj\node_modules\dep\index.js`:

| operation | result |
| --- | --- |
| `readFileSync` | OK, 812 B |
| `require()` | OK |
| `statSync` / `readdirSync` | OK |
| `writeFileSync` | OK |
| `process.chdir` into it, then a relative read | OK |
| `node <that file>` as the **entry point** | OK |
| a **bare** specifier resolved from inside it | OK |

Nothing was written on `C:\` or `C:\Users`. Every ACE went inside `C:\Users\runneradmin\…`, which the invoking user owns.

## And the mirror image, in the same child

An ancestor opened as a **target** is refused. One `findup-walk` line carries the whole shape:

```
…\proj\node_modules\dep=OK | …\node_modules=OK | …\proj=OK | …\data=OK
  | …\<proj-root>=ERR:EPERM | C:\Users\runneradmin=ERR:EPERM | C:\Users=ERR:EPERM | C:\=ERR:EPERM
```

The traverse skip is real and is exactly what its documentation claims: intermediate components only.

That is why an unflagged confined `node` dies before user code, and it was never a read problem. Run 1, all six arms, both images, byte-identical:

```
Error: EPERM: operation not permitted, lstat 'C:\'
    at Object.realpathSync (node:fs:2749:25)
    at toRealPath (node:internal/modules/helpers:61:13)
    at Function._findPath (node:internal/modules/cjs/loader:760:22)
    at resolveMainPath (node:internal/modules/run_main:39:23)
```

`--preserve-symlinks-main --preserve-symlinks` unblock it, measured as a one-variable differential inside a single run: the `ac-noflags` arm produced 0 ops and `rc=1` and died in the realpath walk, while the same grants with the flags produced 30 ops. `fs.realpathSync` on a **granted** deep file is also `EPERM … lstat 'C:\'`, so it is realpath as a call that is unavailable rather than that file.

Those flags are not shippable. `crates/nub-sandbox/tests/preserve_symlinks_isolated_layout.rs` measures them silently binding a different version of a package under nub's default `Isolated` linker, exiting 0. Realpath under an AppContainer stays an open engineering problem — but it is a Node-interaction problem, not the ancestor-ACE problem it was mistaken for.

## `$HOME` secrets are unreachable

In every granting arm, including the one whose grant sits at a project root directly under the profile:

| path | result |
| --- | --- |
| `~/.ssh/id_rsa` | `EPERM open` |
| `~/.ssh` (listing) | `EPERM scandir` |
| `~/.ssh/id_rsa` (`stat`) | `EPERM stat` |
| `~/.npmrc` | `EPERM open` |

The unconfined arm reads all four, so the denial is the jail and not the files being absent. Deny-by-default, no denylist, nothing enumerated.

## Egress is denied in the same token

Zero capabilities, so no `internetClient`: `connect EACCES 1.1.1.1:443`, `getaddrinfo ENOTFOUND`, and loopback `connect ETIMEDOUT 127.0.0.1:135`. The unconfined arm reaches all three. Both halves — confined reads and denied egress — hold in one token.

## Zero setup, with one caveat

No setup command, no elevation, nothing pre-registered. A first run on a fresh machine works. It is not, however, "writes nothing":

- `DeriveAppContainerSidFromAppContainerName` returns the **same** SID as `CreateAppContainerProfile` and writes nothing at all — no `HKCU` mapping, no `%LOCALAPPDATA%\Packages` directory. The SID is a pure hash of the name.
- `CreateAppContainerProfile` adds exactly one subkey under `HKCU\…\CurrentVersion\AppContainer\Mappings` (46 → 47) plus `%LOCALAPPDATA%\Packages\<name>`. `DeleteAppContainerProfile` removes both.
- A profile-less launch does **not** work. The `ac-derive-only` arm — hash-derived SID, no profile, ACEs written and confirmed present on the deep file — failed `CreateProcessW err=2` on both images, while every profile-registered arm launched from that identical path in the same run. Registration is required per launch.
- ACE residue after all eight arms: **0**.

## ACE cost

Fixture of 4,000 entries / 3,880 files. The shipping backend writes leaf grants, so the per-launch figure is the last two rows.

| operation | windows-latest | windows-11-arm |
| --- | --- | --- |
| inheritable `Modify` grant, whole 4,000-entry tree | 617–1102 ms | 452–453 ms |
| revoke (purge by trustee), same tree | 754–844 ms | 395–468 ms |
| grant on a single 97-entry leaf dir | 20–24 ms | 13–29 ms |
| revoke, same leaf | 20–102 ms | 12–17 ms |

Inheritance means new files pick the ACE up at creation, so the recursive pass only covers pre-existing content. Scope grants to the package or project directory.

## Two things measured along the way

**Why CI works and SSH does not.** `session-id = 2` on both runners — an interactive session with an attachable window station. Over OpenSSH you land in services session 0, which has none, and every LowBox launch returns `0xC0000142`. Both runners were also elevated, which cannot be what made any cell pass: no write was attempted above `%USERPROFILE%`, and the unconfined-vs-confined differential sits inside one identical privilege context.

**A piped spawn hangs.** `child_process.spawnSync` with piped stdio never returns under the AppContainer. In run 2 it swallowed every operation after it and timed the arm out; moving it after the completion marker recovered the table, and in run 3 it is absent from every confined arm while the unconfined arm returns normally. Every npm lifecycle script spawns piped children, and an indefinite hang is a worse failure mode than a refusal.

## The piped-spawn hang: two different defects, read out of libuv's source

The hang and the `stdio: 'ignore'` refusal have been carried as one `ERROR_ACCESS_DENIED` story. They are not the same defect, and the distinction decides whether the `\Device\Null` candidate can be the hang's cause at all. From `deps/uv/src/win/process-stdio.c` (libuv 1.52.1):

`uv__create_nul_handle` — the only `CreateFileW(L"NUL", …)` in libuv's spawn path — is called from exactly one place, the `UV_IGNORE` branch of `uv__stdio_create`, and only for fds 0–2. `UV_CREATE_PIPE` takes a different branch entirely, `uv__create_stdio_pipe_pair`, which never touches the device.

So the shapes fail in unrelated places:

| stdio shape | object touched | failure |
| --- | --- | --- |
| `'ignore'` | `\Device\Null`, opened fresh **in the spawning process** | clean `EPERM` — measured, run `30473523088` |
| `'pipe'` | the global NPFS namespace, via `uv__pipe_server` | **spins forever** |
| `'inherit'` | neither — the parent's already-open handles are passed down | works |
| `[0, fd, fd]` | neither — a real file descriptor | works |

The spin is in `uv__pipe_server` (`deps/uv/src/win/pipe.c`), which names the pipe `\\?\pipe\uv\<ptr>-<pid>` — the **global** namespace — and then:

```c
if (err != ERROR_PIPE_BUSY && err != ERROR_ACCESS_DENIED) goto error;
random++;
```

A permission denial is indistinguishable from a name collision to that loop, so it increments and retries with no bound, inside `uv_spawn`, before any timer arms. Node's own `timeout` option therefore cannot break it. Run `30473523088` measured the shape directly: `HUNG killed_after_ms=15059 cpu_ms=14906` — **cpu ≈ wall, so a busy spin, not a block.**

That run also isolated the gate to the **namespace** rather than to any flag: eleven `CreateNamedPipeW` cells differing only in `dwOpenMode`/`dwPipeMode` were all `ERROR_ACCESS_DENIED` under the jail, and adding **only** `LOCAL\` to the name produced `CREATED` — including `\\.\pipe\LOCAL\uv\…`, libuv's own shape.

**Consequence for the `\Device\Null` candidate:** it is predicted to fix `stdio: 'ignore'` and *not* the hang, because the piped path never opens the device. The `obj-*` arms assert those separately rather than as one "the repair works" cell, and the selftest's `obj-nul-fixes-the-hang-too` world exists so a broader-than-predicted effect would be loud rather than absorbed.

## What this does not establish

- **Which mechanism performs the traverse skip.** `crates/nub-sandbox/src/backend/windows.rs` credits `SeChangeNotifyPrivilege` and `FILE_DEVICE_ALLOW_APPCONTAINER_TRAVERSAL` on the volume device. Both predict this observable and this probe cannot separate them.
- **Anything off a local `C:` volume.** No network path, no non-`C:` volume, no filter-driver device — and the volume-flag hypothesis predicts traverse *would* be enforced on a device lacking the flag.
