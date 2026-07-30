# Results — rerooted build world, Linux

All **MEASURED** unless tagged otherwise. Host: Ubuntu 24.04.4 LTS, kernel `6.17.0-1021-gcp`,
glibc 2.39, x86-64, non-root user `nub` (uid 1001). World: debian bullseye-slim, glibc 2.31,
Node 22.15.0. Full transcript in `probes-final.log`.

**The design works.** Every property the spike set out to prove holds, at ~2 ms per run.

| # | claim | verdict |
|---|---|---|
| 1 | it reroots | yes |
| 2 | `realpath` / ancestor walks succeed naturally | yes — the point of the whole effort |
| 3 | `$HOME` absent, not denied | yes, 0 entries |
| 4 | native addon builds inside, loads on the host | yes, incl. `better-sqlite3` from source |
| 5 | hardlink store→project under one mount | yes; two mounts → `EXDEV`, same inode |
| 6 | privilege | zero at run time; **one** `apparmor_parser` call one-time on Ubuntu 24.04 |
| 7 | cost | 2.0 ms/run world setup; install 107 s in-world vs 154 s host control |

## 1. It reroots

```
$ jail --root ~/world --bind /tmp/vw-run/work:/work --uid 1000 -- /bin/sh -c 'ls -1 /; hostname; id'
bin boot dev etc home lib lib64 media mnt opt proc root run sbin srv sys tmp usr var work
hostname: nub-world
uid: 1000 (build)
```

Host root at the same moment, for contrast:

```
$ ls -1 /
bin bin.usr-is-merged boot dev etc home lib lib.usr-is-merged lib64 lost+found media mnt opt
proc root run sbin sbin.usr-is-merged snap srv sys tmp usr var
```

`/mnt` appears in both — inside it is the base image's own empty `/mnt`, not the host's.

## 2. `realpath` and ancestor walks work naturally

This is the argument the whole design rests on, and it is the one an allowlist structurally cannot
satisfy (`sandbox-MECHANISM-FACTS.md:779-787`: `EPERM … lstat 'C:\'` from
`realpathSync ← toRealPath ← _findPath ← resolveMainPath`).

```
--- coreutils realpath through a symlink ---
/work/project/deep/a/b/c/d/e/f.txt
--- fs.realpathSync on the same path ---
/work/project/deep/a/b/c/d/e/f.txt
--- lstat EVERY ancestor up to / ---
ancestors lstat OK: 10 (walk terminated at /)
--- resolveMainPath: run a script REACHED THROUGH A SYMLINK ---
main module resolved via realpath: /work/project/deep/main.js
--- opendir/readdir the whole chain to / ---
opendir chain OK
```

Nothing is granted to make this work. The walk terminates at `/` because there is a real `/`.

## 3. `$HOME` is ABSENT, not denied

```
HOME=/home/build   entries=0
/home contains: build
ABSENT  /home/nub/.ssh
ABSENT  /home/nub/.aws
ABSENT  /home/nub/.config
ABSENT  /home/nub
```

The host genuinely has those (`ls -A /home/nub | wc -l` → **64**; `ls -A /home/nub/.ssh` →
`authorized_keys id_rsa`). Inside, they are not refused — the paths do not exist.

`/etc/shadow` inside is the base image's own (`root:*:20647:…` — locked, no hashes). `/root` inside
holds only debian's default `.bashrc` and `.profile`. An exhaustive
`find / -xdev -type f \( -name 'id_*' -o -name .npmrc -o -name .netrc -o -name credentials … \)`
across the whole world returns exactly one path: `/opt/node/lib/node_modules/npm/.npmrc`, npm's own
bundled builtin config.

The environment is scrubbed to eight variables:

```
HOME=/home/build   LANG=C.UTF-8   NUB_WORLD=1
PATH=/opt/node/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
PWD=/work/project  SHELL=/bin/sh  TMPDIR=/tmp  USER=build
```

## 4. A real native addon builds inside and loads on the host

`better-sqlite3` forced to compile from source, plus `express`:

```
added 104 packages in 2m
node_modules/better-sqlite3/build/Release/better_sqlite3.node
REAL_INSTALL_WALL 104.94s
```

Loaded on the host, outside the jail:

```
$ file .../hello.node
ELF 64-bit LSB shared object, x86-64, version 1 (SYSV), dynamically linked, not stripped
$ node --version && node -e "…require('.../hello.node').hello()"
v22.15.0
HOST LOAD OK: hello from the virgin world
$ /tmp/node24/bin/node --version && /tmp/node24/bin/node -e "…"
v24.11.0
HOST LOAD OK on a different Node major: hello from the virgin world
$ node -e "const D=require('better-sqlite3'); … db.prepare('select x from t').get()"
HOST LOAD OK: better-sqlite3 -> { x: 42 }
```

The ABI margin, measured rather than assumed — this is the manylinux "build old, run new" rule
holding concretely:

```
$ objdump -T better_sqlite3.node | grep -o 'GLIBC_[0-9.]*' | sort -Vu
GLIBC_2.2.5 GLIBC_2.14 GLIBC_2.28 GLIBC_2.29
$ objdump -T better_sqlite3.node | grep -o 'GLIBCXX_[0-9.]*' | sort -Vu | tail -1
GLIBCXX_3.4.20
world glibc 2.31   →   host glibc 2.39
```

### The uid trap, with its control

The single-uid map is not a detail. Same command, only `--uid` varied:

```
--- --uid 0 ---
gyp ERR! error while extracting tarball TAR_ENTRY_ERROR EINVAL: invalid argument, fchown   (×~70)
gyp ERR! stack Error: There was a fatal problem while downloading/extracting the tarball
--- --uid 1000 ---
  CC(target) Release/obj.target/hello/hello.o
  SOLINK_MODULE(target) Release/obj.target/hello.node
-rwxrwxr-x 2 build build 15992 build/Release/hello.node
```

node-tar only attempts `chown` when it believes it is root. Mapping to a non-root uid removes the
need for a second id entirely, which is the right answer for a build world — `newuidmap`/`newgidmap`
would also work but costs a setuid-root helper (`uidmap`, **not installed** on this host, though
`/etc/subuid` did carry `nub:165536:65536`).

## 5. EXDEV — the topology constraint holds

Design arm: store and project under the single `/work` bind mount.

```
300 8:1 /tmp/vw-run/work /work
HARDLINK OK links=2
```

Control, one variable changed — the *same* store directory mounted a second time:

```
300 8:1 /tmp/vw-run/work /work
301 8:1 /tmp/vw-run/work/store /store
/work/store/blob inode=786444 dev=2049
/store/blob      inode=786444 dev=2049
ln: failed to create hard link … => '/store/blob': Invalid cross-device link
```

Identical inode, identical device, different mount → `EXDEV`. `do_linkat()` compares `vfsmount`, not
superblock. A project-local store inside the one bind mount is therefore not a preference; it is what
makes hardlinking possible at all.

## 6. Privilege — the honest answer

Four rungs, one host, one variable each.

| rung | state | result |
|---|---|---|
| 1 | `restrict=1`, no profile, non-root | **rc=72**, `uid_map write failed: Operation not permitted` |
| 2 | + path-bound AppArmor profile granting `userns` | **rc=0** |
| 3 | profile removed, `restrict=0` instead | **rc=0** |
| 4 | `restrict=1`, no profile, run as root | **rc=0**, `AS_ROOT_OK uid=0` |

So:

- **Where unprivileged user namespaces are unrestricted (rung 3 — most non-Ubuntu distros, Ubuntu ≤
  23.10, and any host where the sysctl is off): zero root, ever.** Nothing to install, no setuid
  helper, no daemon. The whole world is built by an ordinary user.
- **Where Ubuntu 24.04's default restriction is on: exactly one one-time root step**, and the narrow
  form is available — a path-bound AppArmor profile scoped to the single binary:

  ```
  abi <abi/4.0>,
  include <tunables/global>
  profile nub-jail /path/to/jail flags=(unconfined) { userns, }
  ```

  `sudo apparmor_parser -r -W …` once. This leaves the global restriction in force for every other
  executable on the machine. The blunt alternative (`sysctl -w
  kernel.apparmor_restrict_unprivileged_userns=0`) also works and is strictly worse posture. nub
  already ships this exact pattern for its bwrap helper (`/etc/apparmor.d/nub-bwrap-userns`).

**The trap, measured, because it will otherwise produce a false green:**

```
$ unshare --user /bin/true; echo rc=$?
rc=0
$ unshare --user --map-root-user /bin/true; echo rc=$?
unshare: write failed /proc/self/uid_map: Operation not permitted
rc=1
```

On a restricted host `unshare -U` **exits 0** — it creates an inert namespace with no uid map. Any
capability check that reads that exit code concludes "userns works" and is wrong. Judge on the
`uid_map` write, or on the pivot actually happening.

## 7. Cost

**World setup, per run** (overlay lower + tmpfs upper + namespaces + `pivot_root`), 5 reps:

```
TIMINGS_MS unshare=0.2 uidmap=0.1 rootfs=0.6 world=1.1 pivot=0.4 overlay=true → TOTAL 2.3
                                                                                TOTAL 2.2
                                                                                TOTAL 2.3
                                                                                TOTAL 1.7
                                                                                TOTAL 2.0
```

**2.0 ms median.** For scale, `bwrap` is 2.2 ms and `docker exec` was measured at 293–423 ms.

Without overlayfs — `cp -a` the 581 MB rootfs into tmpfs — the same setup costs **1275–1301 ms**
warm (one cold-page-cache outlier at 32 s, discarded as unrepresentative). Overlayfs is a **~650×**
difference and is what makes per-run freshness free.

**One-time world build:** 35 s total, 581 MB on disk.

```
TIME rootfs-pull                  1.72s     (89 MB debian bullseye-slim, no Docker daemon)
TIME node-download                3.99s     (193 MB Node 22.15.0)
TIME toolchain-unprivileged      25.23s     (build-essential + python3 + ca-certificates, 343 MB /usr)
TOOLCHAIN_PRIVILEGE=none
```

`apt-get install` ran **inside the rerooted world with no root** — the prep step needs the same
userns grant as any other run and nothing more.

**Install, same fixture, both cold npm cache:**

| | wall |
|---|---|
| in-world (`better-sqlite3 --build-from-source` + `express`) | **106.97 s** |
| host control, no jail, fresh cache | **154.29 s** |

The in-world run is not slower. Both runs are dominated by compiling sqlite3; the spread is
compile-time noise on a shared VM, so read this as "no measurable overhead", not as a speedup.

## 8. Full reign inside, and it does not leak out

```
$ … -- /bin/sh -c 'echo pwned > /etc/passwd; echo pwned > /usr/lib/x; rm -rf /usr/share/doc'
wrote to /etc, /usr, /var and deleted /usr/share/doc — all permitted, zero policy
$ cat /etc/passwd
pwned
```

After exit:

```
$ head -1 ~/world/etc/passwd
root:x:0:0:root:/root:/bin/bash
$ ls ~/world/usr/lib/x
ls: cannot access '/home/nub/world/usr/lib/x': No such file or directory
$ head -2 /etc/passwd            # the real host
root:x:0:0:root:/root:/bin/bash
daemon:x:1:1:daemon:/usr/sbin:/usr/sbin/nologin
```

The prepared lowerdir is untouched because every write landed in a tmpfs upper layer that is
discarded with the namespace.

## Open items this spike did not close

- **Egress is unsolved, not solved.** `CLONE_NEWNET` gives a genuinely empty network — measured,
  `DNS_FAIL` inside vs `DNS_OK` with the host netns, one variable. But an install needs the
  registry, so the real choice is between sharing the host netns (no egress control at all) and
  wiring a userspace network path (`slirp4netns`, or a proxy on a unix socket). The prototype ships
  the flag and does not answer the question.
- **Toolchain drift.** The world pins one glibc and one compiler. `node-gyp` re-downloads Node
  headers into the fresh `$HOME` on every run; a real implementation wants those baked into the
  lower layer.
- **`prebuild-install`/`detect-libc` key off the world's libc**, so a musl world silently hands a
  glibc host an unloadable tree (already measured in the research doc §2a). The world must be glibc
  and pinned at or below the oldest supported host.
- **Linux only.** Nothing here transfers to macOS or Windows; those remain open.
