# Why a Linux package can land at `write:"disk"` without needing to write anything

Eight of 2,196 measured Linux package-versions carry `write:"disk"` in the build-jail capability catalog — no filesystem confinement at all. This doc resolves one of them, `dotnet-2.0.0@1.4.4`, to a single reproduced syscall, and names why the grant cannot be narrowed with the mechanism nub uses today.

**The headline: the package writes nothing outside its own private `$HOME`, and it fails under a narrow grant because a `/proc` read is refused.** The failing process dies on `openat("/proc/self/stat")` returning `EACCES`. Granting `write:"disk"` repairs it for a reason unrelated to writing — that rung clears the filesystem rule set and flips the policy's default effect to allow, which incidentally makes all of `/proc` readable. Adding `/proc` to the read floor and leaving the grant narrow installs the package successfully, which is the control that establishes cause.

This is the Linux counterpart of the shape recorded in [`windows-disk-write-causes.md`](windows-disk-write-causes.md): four of that doc's five mechanisms are read-side or exec-side rather than write-side, and all of them clear at `write:"disk"` because that rung stops confining rather than because it grants a write. The specific primitive differs — Windows loses a traverse ACE and a device path, Linux loses per-process procfs — so the causes are siblings, not the same cause.

## What the corpus recorded

The grant walk produced a completely flat ladder, which is what makes this package unusual and what made it unresolvable from record fields alone.

| | value |
| --- | --- |
| grant | `{"write": "disk", "network": true}` |
| verdict | `MINIMUM` |
| cells | 55 (one control, 54 grant states) |
| cells that passed | 1 — state `write.disk + network`, digest `35c0cb202c22abb0`, 2,064 files, byte-identical to the control |
| cells that failed | 53, of which 51 share digest `33909643cf091472` at 1,694 files |

A package merely starved of a capability shows its failure signature change as grants widen. This one fails identically at every rung, including the richest narrow combination (`write.project + write.userHome + read.disk + network`), whose digest equals the zero-grant cell's exactly. The ladder therefore learned nothing about which capability was missing, because the missing capability is not on the ladder.

**Two record fields are easy to misread here, and neither says what its name suggests.** Both `pathsBlockedWithoutGrant` and `pathsBlockedByPrefix` are `controlOnly(control, floor)` — the set difference between the unjailed control and the *zero-grant* cell, not the paths refused at the cell being read. For this package the two happen to coincide, because every failing cell reproduces the zero-grant cell's output exactly, so the recorded prefixes (`$home/.net/2.0.0`, 185 paths; `$home/.cache/yarn`, 182) describe what a *successful* install produces rather than what confinement refused. All 367 of them land inside the throwaway home, which the base profile already grants read-write at every rung.

## Method

Four arms over one fixture on a fresh Google Cloud VM (Ubuntu 24.04, kernel 6.17, Landlock present in `/sys/kernel/security/lsm`), with a binary built from `sandbox/integration` at `79813999af` — which carries `c26c4edff3`, the fix that makes `read:"disk"` produce real Landlock rules on Linux, so the read rung is genuinely live rather than a launch failure.

Each arm installs into its own fixture directory, so each gets its own per-package private `$HOME` and no arm can warm another's cache. Every jailed arm was checked for the `build-jail catalog OVERRIDDEN` banner and for absence of `REJECTED`, because a malformed override falls back to the compiled-in catalog silently. The lifecycle run is traced with `strace -f -e trace=file,network`.

**Arms are compared on the artifact the script produced, never on the exit code.** For this package that artifact is `<jail-home>/.net/2.0.0/node_modules/dotnet-2.0.0-linux/`, the unpacked .NET runtime — not the package directory, which nub's linker populates whether the script runs or not.

## Measured result

| arm | grant | rc | artifact |
| --- | --- | --- | --- |
| jail off (`install.buildJail: false`) | — | 0 | real .NET runtime, in the user's real `~/.net/2.0.0` |
| narrow | `write:{project, userHome}` + `network` | 1 | jail-home `.net/2.0.0` empty, no yarn cache |
| rich | narrow + `read:"disk"` | 1 | empty |
| disk | `write:"disk"` + `network` | 0 | jail-home `.cache/yarn/v1/npm-dotnet-2.0.0-linux-1.0.5-…` populated |

The corpus verdict reproduces exactly: everything narrow fails, only the disk rung passes.

## The mechanism

The last two lines of the failing process in the narrow arm:

```
39338 openat(AT_FDCWD, "/proc/self/stat", O_RDONLY|O_CLOEXEC) = -1 EACCES (Permission denied)
39338 +++ exited with 1 +++
```

Process 39338 is a bundled yarn v1, three levels below the lifecycle command. The postinstall is `node -e "try{require('./dist/app.js')}catch(e){}"`; `app.js` spawns `node /node_modules/yarn/bin/yarn.js add dotnet-2.0.0-linux` out of a packed virtual filesystem and forwards the child's exit code with `process.exit(code)`. The child's stdio is `'ignore'`, so yarn's own error never reaches a log — `DEBUG=1` would unmask it, but the jail's env scrub strips `DEBUG`, which is why the trace is the only way in.

Three facts separate this from a coincidence:

- **The narrow arm opens no sockets at all.** Zero `socket()` or `connect()` calls, so the yarn child dies before it reaches the network and the network axis was never the constraint. The disk arm reaches the registry normally (52 `connect(AF_INET)` calls).
- **The disk arm records zero denials.** Not one `= -1 EACCES` or `= -1 EPERM` line across the whole trace.
- **The only denials in the narrow arm are `/proc` reads** — `/proc/version_signature`, `/proc/self/cgroup`, `/proc/self/maps`, `/proc/self/stat`. Node tolerates the first three; the parent postinstall process is refused the same paths and survives. Only the yarn child reads `/proc/self/stat`, and it exits immediately.

**Control, single variable.** Adding `/proc` to the Landlock read floor and rebuilding, with the grant left at `write:{project, userHome}` + `network`, the same fixture now exits 0, `/proc/self/stat` returns a file descriptor, 52 `connect(AF_INET)` calls appear, and the jail-home holds a 151 MB `.net/2.0.0/node_modules/dotnet-2.0.0-linux/`. The residual denials in that run are `/sys/fs/cgroup/…/memory.max`, `/etc/gai.conf`, `/sys/devices/system/cpu/online` and an `AF_UNIX` socket, all tolerated.

**Reading a strace for denials has one trap worth stating.** A plain `grep EACCES` matches the `AT_EACCESS` *flag name* in every `faccessat2(...)` line, so it reports denials that are successful calls. Only `= -1 EACCES` is a refusal. The raw counts across three arms were 26 / 13 / 1; the real counts are 11 / 0 / 0.

## Why the grant cannot be narrowed

The read floor is defined by `PROC_READ_PATHS` in [`crates/nub-sandbox/src/backend/linux_landlock.rs`](../../crates/nub-sandbox/src/backend/linux_landlock.rs), and the comment above it already argues this trade. Granting `/proc` wholesale exposes every same-uid process's `/proc/<pid>/environ` and `cmdline` — the user's shell and editor, environment variables included — because Landlock has no PID namespace. The list grants `/proc/stat`, the system-wide file; the per-process `/proc/self/stat` differs from it by one path component and is not covered.

Two properties make the narrow fix inexpressible rather than merely unimplemented:

1. **Per-process procfs cannot be granted to a descendant tree.** A Landlock rule is attached to a resolved path, and `/proc/self` resolves at rule-creation time to the creating process's own directory. The existing comment notes that the ruleset is built before `fork`, so it would name nub's PID. The limit is deeper than that: each `/proc/<pid>` is a distinct directory, so even a post-`fork` ruleset would cover only the direct child, while the process that fails here is a grandchild. No fixed set of per-process grants covers an unbounded descendant tree.
2. **Landlock cannot subtract a path.** Granting `/proc` minus `environ` and `cmdline` is not expressible — measured separately and recorded in [`linux-full-disk-read.md`](linux-full-disk-read.md), stacked rulesets do not remove a path from an outer allow, and a rule with `allowed_access = 0` returns `ENOMSG`. Rules only ever add exceptions, so the complement has to be enumerated, and a per-PID complement is unbounded.

Everything that expresses "all of `/proc` except X" on Linux does it by overmounting inside a mount namespace, which the Landlock-or-nothing backend decision rules out. Bubblewrap's `--proc` supplied exactly that and is the capability lost in the move.

## Disposition

**A structural limit of the confinement primitive, not a nub defect.** The named primitive is Landlock's lack of a PID namespace together with its inability to express per-process procfs for a descendant tree. The catalog entry is correct as recorded: with today's backend, `write:"disk"` is the narrowest grant that installs this package.

The levers that exist are all product decisions rather than fixes:

| lever | cost |
| --- | --- |
| add `/proc` to the read floor | every jailed script can read same-uid processes' `environ` and `cmdline` |
| restore a PID namespace | reverses the Landlock-or-nothing backend decision; needs unprivileged user namespaces, which Ubuntu restricts by default |
| leave the grant at `write:"disk"` | this package, and any other whose toolchain reads per-process procfs, runs with no filesystem confinement |

Worth noting when weighing the first row: this tail costs more on Linux than the filesystem axis alone, in the same way the Windows tail does. The rung that clears the failure is the rung that stops confining.

## What this does not cover

**Seven of the eight Linux disk grants are unresolved.** Only `dotnet-2.0.0@1.4.4` was traced. The full set is `@nuxt/components@2.1.0`, `@opencode-ai/cli@0.0.0-next-16573`, `@tensorflow/tfjs-backend-wasm@1.4.0-alpha2`, `codeceptjs@1.1.3`, `dotnet-2.0.0@1.4.4`, `iedriver@4.0.0`, `postman-code-generators@2.1.1` and `react-native-purchases@1.5.4`. Whether the `/proc` mechanism generalizes is untested; an attempt on `@nuxt/components@2.1.0` was inconclusive because its postinstall calls `yarn` from `PATH` and the probe VM had no yarn installed, so it failed at exit 127 before reaching any confinement question.

**A macOS record showing no disk grant is not evidence that macOS confines more gently.** For this package the darwin record is `BROKEN-WITHOUT-JAIL-TOO` — the unjailed control itself exits 1, so the walk never ran and no grant field exists. Any cross-OS asymmetry argument over these packages needs the darwin verdict checked first.

## Bounds

- One package, one Linux kernel (6.17), one reproduction. The corpus record was measured on a different kernel and a different binary.
- The corpus harness pinned Node 10 for this package from its `engines` field; the reproduction ran the host's Node 22. Both reach the same grant and the same failing syscall, so the mechanism is not Node-version-specific, but the two runs are not otherwise identical.
- Two of the 53 failing corpus cells carry file counts of 1,695 rather than 1,694 with distinct digests. The record's `unstablePathCount` is 0, so these are not flagged as noise; they are unexplained and nothing here turns on them.
- The `/proc` control was run with a locally patched read floor, not a proposed change. No nub source change is recommended by this doc.

## Reproducing

```sh
# One fixture per arm; each gets its own private $HOME.
mkdir -p arm && cd arm
echo '{"name":"fix","version":"1.0.0","dependencies":{"dotnet-2.0.0":"1.4.4"}}' > package.json
cat > cat.json <<'JSON'
{"packages":{"dotnet-2.0.0":{"default":{"write":{"project":true,"userHome":true},"network":true}}}}
JSON

NUB_BUILD_JAIL_CATALOG="$PWD/cat.json" nub install
strace -f -e trace=file,network -s 300 -o trace.txt \
  env NUB_BUILD_JAIL_CATALOG="$PWD/cat.json" nub approve-builds --all
echo "rc=$?"

grep -c 'catalog OVERRIDDEN' trace.txt   # assert engagement, and assert REJECTED is absent
grep '= -1 EACCES' trace.txt             # NOT a bare `grep EACCES` — that matches AT_EACCESS
```

Swap `cat.json` for `{"write":"disk","network":true}` for the passing arm. Check the artifact under the jail home rather than the exit code:

```sh
find ~/.cache/nub/jail-home/*/.net/2.0.0 -maxdepth 3 -name 'dotnet-2.0.0-linux'
```

## Changelog

- 2026-08-05 — Initial write-up. Resolves `dotnet-2.0.0@1.4.4`'s Linux `write:"disk"` grant to a refused `/proc/self/stat` read, with a positive control showing a narrow grant succeeds once `/proc` is readable.
