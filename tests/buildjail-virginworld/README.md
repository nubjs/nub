# Rerooted build world — Linux prototype spike

A standalone prototype that runs an `npm install` inside a **rerooted filesystem** instead of behind
an allowlist over the real machine. Not nub code, not wired into nub: a spike that answers whether
the design in `wiki/research/build-jail-virgin-world.md` §2 actually works, and at what cost.

The world:

```
/               overlayfs — lower = a prepared base rootfs, upper = a per-run tmpfs
├── bin lib usr  debian bullseye-slim + build-essential + python3   (glibc 2.31)
├── etc          one passwd entry, one group, resolv.conf
├── tmp          tmpfs
├── home/build   EMPTY. This is $HOME.
├── opt/node     Node 22.15.0 from nodejs.org
└── work         THE ONE bind mount: project + project-local store, single-rooted
```

`unshare(CLONE_NEWUSER|CLONE_NEWNS|CLONE_NEWPID|CLONE_NEWUTS|CLONE_NEWIPC[|CLONE_NEWNET])` then
`pivot_root`. Inside there is no allowlist and no denylist — the world simply contains nothing worth
denying.

## Files

| file | what it is |
|---|---|
| `jail.rs` | the whole prototype. Plain `rustc jail.rs -O -o jail`, zero crates. |
| `pull-rootfs.sh` | fetches an OCI image's layers from a registry into a directory. No Docker daemon, no root. |
| `prep-world.sh` | one-time world build: base rootfs + Node + compiler toolchain. |
| `probes.sh` | P0–P8, the main evidence run. |
| `probes-extra.sh` | ABI margin, egress posture, warm cost of the no-overlay fallback. |
| `probes-home.sh` | focused re-run of the `$HOME`-absence claim. |
| `probes-final.log` | verbatim output of `probes.sh` from the run the findings cite. |
| `results.md` | the findings. |

## Reproduce

Needs a Linux host with `curl`, `jq`, `tar`, and a Rust compiler. Measured on Ubuntu 24.04.4,
kernel 6.17, glibc 2.39, x86-64.

```sh
rustc -O jail.rs -o jail
./prep-world.sh ~/world                 # one-time, ~35 s, 581 MB
WORLD=~/world ./probes.sh               # ~4 min (two real installs)
WORLD=~/world ./probes-extra.sh
WORLD=~/world ./probes-home.sh
```

On a host with `kernel.apparmor_restrict_unprivileged_userns=1` (Ubuntu 24.04 default) `probes.sh`
first loads a path-bound AppArmor profile granting `userns` to the `jail` binary — one `sudo
apparmor_parser` call, which P6 then removes again to show the failure it papers over.

## Two gotchas the prototype exists to have found

**Map to a non-root uid inside.** An unprivileged user namespace maps exactly one uid. If that uid
is 0, node-tar believes it is root and calls `fchown` on every entry it extracts, which `EINVAL`s on
every id it cannot map — `node-gyp` cannot even unpack the Node headers, so no native package builds
at all. `--uid 1000` makes node-tar skip ownership entirely and everything works. A build world has
no reason to want a second uid, so this is the fix, not `newuidmap`.

**Do the mounting from inside the new PID namespace.** `CLONE_NEWPID` only affects children, and
procfs refuses to mount unless the mounter's PID namespace is owned by its current user namespace.
Building the rootfs in the parent gets `EPERM` on `/proc`; `fork()` first, then build everything from
the child, as bubblewrap and runc do.
