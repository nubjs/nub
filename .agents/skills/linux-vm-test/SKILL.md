---
name: linux-vm-test
description: >-
  Run ad-hoc Nub tests and debugging probes on real local Linux guests. Use for
  Linux-specific sandbox, kernel, distro, architecture, package, or runtime
  behavior that Docker cannot reproduce faithfully. Selects persistent Lima
  for source-mounted iteration or ephemeral Tart for clean-image reproduction.
metadata:
  internal: true
---

# Linux VM testing

Use a real VM only when the behavior depends on a Linux kernel or guest OS. Prefer Docker for userspace-only clean-cache/config experiments and `ci-adhoc-test` for unavailable architectures, distributions, or release-platform proof.

Two local backends serve different purposes:

- Lima `landlock-vm`: persistent Ubuntu arm64 debugger with the host home mounted read-only.
- Tart: disposable Ubuntu arm64 guest for clean-machine or alternate-image reproduction.

## Preflight

Inspect both backends before changing VM state:

```sh
limactl list
tart list
limactl shell landlock-vm -- bash -lc \
  'uname -srm; . /etc/os-release; echo "$PRETTY_NAME"; df -h "$HOME" /tmp'
```

Confirm the source mount before treating it as writable:

```sh
limactl shell landlock-vm -- bash -lc \
  'findmnt -T /Users/colinmcd94/Documents/projects/nub'
```

The known Lima mount is read-only. Never build in `/Users/colinmcd94/**`. If the guest disk is full, report it and ask before reclaiming space. Never stop, delete, or clean an existing VM merely to make room.

## Persistent Lima debugging

Run a short probe synchronously against the exact host source state:

```sh
limactl shell landlock-vm -- bash -lc \
  'cd /Users/colinmcd94/.cache/nub/worktrees/<worktree> && <probe>'
```

Trace a guest command without modifying the host checkout:

```sh
limactl shell landlock-vm -- bash -lc \
  'strace -f -o /tmp/nub.strace -- <command>'
limactl shell landlock-vm -- bash -lc \
  'bpftrace -l "tracepoint:syscalls:sys_enter_*" | head'
```

For a Linux Rust build, copy the exact source state into guest-local storage and use a guest-only target directory:

```sh
limactl shell landlock-vm -- bash -lc '
  mkdir -p "$HOME/src" "$HOME/.cache/nub/linux-vm-target"
  src=/Users/colinmcd94/.cache/nub/worktrees/<worktree>
  dest="$HOME/src/nub-<slug>"
  rsync -a --delete \
    --exclude .git --exclude target --exclude node_modules \
    "$src/" "$dest/"
  cd "$dest"
  export CARGO_TARGET_DIR="$HOME/.cache/nub/linux-vm-target/<slug>"
  cargo build -p nub-cli --profile fast
  cargo test -p nub-cli --test <test_stem>
'
```

Repeat the copy after host edits. Use the exact host worktree under test, a distinct guest target for each branch or probe, and direct Cargo commands because the copied tree intentionally omits Git metadata. Remove only probe-owned files after capturing evidence; do not sweep guest caches or unrelated targets.

Start or stop the persistent VM only when required:

```sh
limactl start landlock-vm
limactl stop landlock-vm
```

## Clean Tart reproduction

Let the checked-in helper own lifecycle:

```sh
tests/vm/tart-vm.sh up <name> <image>
tests/vm/tart-vm.sh exec <name> -- bash -lc '<probe>'
tests/vm/tart-vm.sh run <name> ./probe.sh
tests/vm/tart-vm.sh down <name>
tests/vm/tart-vm.sh rm <name>
```

Use a unique VM name. The `run` operation copies one script when `sshpass` is installed and otherwise streams its body; it does not copy a fixture tree or checkout. Fetch or copy multi-file fixtures deliberately. Finish a throwaway run with `rm`; `down` intentionally preserves the disk.

## Evidence

Record:

- Host commit plus dirty-state summary.
- Guest distro, kernel, and architecture.
- Exact command and environment overrides.
- Exit status, stdout, and stderr.
- Relevant trace or log path.

Copy concise results to the host before deleting a Tart guest. Do not claim x86_64 coverage from these Apple-Silicon arm64 VMs. Use `ci-adhoc-test` when the claim requires x86_64-native behavior or another unavailable platform.
