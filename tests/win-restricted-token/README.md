# Windows build-jail probe — restricted token + low integrity level

How to run this harness. The measurements it produced are in `probe-log.txt`; the
durable conclusions live in `.fray/sandbox-MECHANISM-FACTS.md` §5e.

## What it builds

A minimal unprivileged Windows jail: a restricted token derived from the probe's
own token (deny-only `Administrators` + logon SIDs, `LUA_TOKEN`, all privileges
deleted but `SeChangeNotify`, no `RestrictingSids` — the shape in
`.repos/srt/vendor/srt-win-src/src/token.rs`), dropped to **low** integrity, with
the write allowlist expressed as a **low mandatory label** on the objects that
should be writable. srt runs its child at Medium on purpose; low IL is the one
deliberate divergence, because Medium permits the whole user profile.

## Machine

Any Windows box reachable over SSH, as a **non-admin** user. A GCE
`windows-2025` image works; two setup gotchas cost real time:

- The guest agent does **not** wire Windows SSH on that image (it fails to
  version-detect `sshd`), so `enable-windows-ssh=TRUE` is not sufficient — a
  `windows-startup-script-ps1` has to start `sshd` and place `authorized_keys`.
- Appending directives to the end of the Windows `sshd_config` puts them
  **inside** the trailing `Match Group administrators` block, which is invalid and
  makes `sshd` fail to start. Insert before the first `Match` line.

`powershell -Command -` over SSH silently produced no output for some scripts;
`scp` the file and run `powershell -File` instead.

## Run

```powershell
# once, on the box (as the non-admin user)
csc.exe /nologo /optimize+ /out:Jail.exe Jail.cs      # .NET Framework csc
powershell -File setup.ps1                            # fixture under %USERPROFILE%

powershell -File stage.ps1 -Stage launch   # CreateProcessAsUser vs WithToken x IL
powershell -File stage.ps1 -Stage write    # can an unprivileged owner relabel?
powershell -File stage.ps1 -Stage edge     # TLS / registry / pipes / spawn at low IL
powershell -File stage.ps1 -Stage ops      # the ops that killed the AppContainer route

powershell -File pkg.ps1 -Mode setup       # lay 12 real packages, --ignore-scripts
powershell -File pkg.ps1 -Mode run -Il none   # BASELINE arm
powershell -File pkg.ps1 -Mode run -Il low    # jailed arm
```

`Jail.exe` on its own: `report` (token facts + elevation), `label <path> <low|
medium|remove> [-r]`, `showlabel <path>`, `station`, `launch --il <none|medium|
low|untrusted> --api <asuser|withtoken> [--out F] [--cwd D] [--keep-logon-sid]
[--restrict-sids] [--new-desktop] -- <cmd...>`.

## Reading the output

**Every stage runs an `--il none` baseline arm, and that is not decoration.** All
six launch arms once failed identically with `CreateFileW(NUL) err=2`, which reads
exactly like "the mechanism is unavailable"; the baseline failing too localized it
to a missing `CharSet.Unicode` on one P/Invoke. A treatment arm that fails without
a baseline beside it proves nothing.

Two further traps this harness hit, both worth preserving:

- **Re-run the baseline LAST when arms run sequentially.** core-js appeared to
  break under the jail; a `none → low → none` sweep showed the third arm was
  silent too, because core-js leaves an "already shown" marker outside the tree
  under test.
- **Exit 0 is not success.** Several packages in the corpus swallow their own
  errors and exit 0 having skipped the work. Check artifact bytes, which is what
  `verify`-style artifact listing in `pkg.ps1`'s companion checks are for.

`cpu-features` fails in **both** arms on a box without MSVC. That is a baseline
failure, not a jail failure — do not report it as one.
