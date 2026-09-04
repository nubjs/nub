# linux-supervisor-probe — proof harness for the zero-privilege egress supervisor

Verifies [`crates/nub-sandbox/src/backend/linux_supervisor.rs`](../../crates/nub-sandbox/src/backend/linux_supervisor.rs) — the Rust port of the reviewed `route.c` prototype — on a real Linux host. The module is a transparent per-hostname TCP egress supervisor built on a seccomp `USER_NOTIF` filter at zero privilege (no user namespace, no capability, no subprocess proxy).

This is not a workspace member (the nub workspace lists members explicitly), so it is invisible to a root `cargo build` and never gates unrelated CI. It pulls the module under test in via `#[path]`, so it always compiles the exact committed code. It requires a Linux kernel with `close_range`, `pidfd_getfd`, seccomp `USER_NOTIF` + `ADDFD` (≥ 5.9), and `io_uring` enabled — run it on a Linux VM, never on macOS.

## Run

```sh
cd tests/linux-supervisor
cargo build --release
B=./target/release/probe

$B control          # UNSUPERVISED curl to the deny-arm host — must succeed (discriminator)
$B allow            # supervised curl to an ALLOWED host   — must succeed
$B deny             # supervised curl to a DENIED host      — must be blocked at connect
$B iouring          # supervised io_uring_setup             — must return EPERM (bypass closed)
$B iouring-control  # UNSUPERVISED io_uring_setup           — must succeed (discriminator)
```

Each arm runs in its own process so the supervisor's global observed-DNS/socket state starts clean. The exit code is the verdict; the `SUP …` decision lines on stderr are the supervisor's own trace.

## What each arm proves

| Arm | Expected | Meaning |
| --- | --- | --- |
| `control` | curl exit 0, `HTTP=200` | the deny-arm host is reachable with no supervisor — so a block in `deny` is the supervisor's doing, not a dead host |
| `allow` | child exit 0, `SUP ALLOW …:80 name=<allowed>` | an unmodified client reaches an allowlisted host through the supervisor's own dialed socket |
| `deny` | child exit 7, `SUP DENY … -> EPERM`, `curl: (7) Failed to connect` | a connect to a non-allowlisted host is refused with `EPERM` at `connect()` |
| `iouring` | child exit 42 (`EPERM`) | `io_uring_setup` is denied by a scalar seccomp `EPERM`, closing the io_uring egress-bypass |
| `iouring-control` | child exit 0 | `io_uring` works on this box with no supervisor — so the `iouring` denial is the filter, not a missing feature |

`deny` and `control` must target hosts with **distinct** IPs (default `example.com` allowed vs `www.google.com` denied): the supervisor attributes a TCP address to a name via observed DNS, so two names sharing an IP would void the attack arm.

Measured working on Ubuntu 24.04, kernel 6.17, x86_64.
