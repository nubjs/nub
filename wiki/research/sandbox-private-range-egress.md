# Private-range network egress — what other sandboxes do by default

*2026-07-12. Investigation-scope, recommend-only — no code proposed here lands without a
maintainer posture call. Answers one question: does a sandbox/isolation tool allow, block,
or gate-behind-opt-in egress to RFC1918 private ranges (`10/8`, `172.16/12`, `192.168/16`),
loopback (`127/8`), link-local (`169.254/16`, `fe80::/10`), and IPv6 ULA (`fc00::/7`) —
by default?*

Sources read directly (file:line or doc-page cited throughout), never asserted from memory.
Local checkouts used: `codex`, `sandbox-runtime`, `srt`,
`bubblewrap`, `firejail`. Web sources cited inline.

## TL;DR

The landscape splits cleanly into two families, plus a "no opinion" middle:

1. **SSRF-hardened cloud/serverless egress (block-by-default, narrow opt-in).** Cloudflare
   Workers, OpenAI Codex's sandbox, and every SSRF-guard HTTP-client library surveyed block
   loopback + RFC1918 + link-local + reserved ranges unless the destination is individually,
   non-wildcard allowlisted. This is the class of tool built to confine an **adversarial or
   semi-trusted workload's outbound reach** — exactly nub's sandbox's job.
2. **Dev/container tooling (allow-by-default or binary namespace switch).** Docker's default
   bridge, bubblewrap, firejail, and Anthropic's own SRT/Claude-Code sandbox either grant
   full LAN/private reachability by default or offer only an all-or-nothing network
   namespace toggle — no private-range concept at all. This is the class built to run
   **code the developer already trusts**, where reaching a local Postgres or a sibling
   container is the common case, not the threat.
3. **Mechanism-only, no opinion.** gVisor and Firecracker provide network *isolation
   primitives* (a userspace netstack, a TAP device) with zero built-in address policy — the
   orchestrator wiring them decides reachability. GitHub Actions-hosted runners and
   un-VPC'd AWS Lambda are a degenerate case of this: there is no private range present by
   default at all, so "blocked vs allowed" doesn't apply.

**nub today:** metadata (`169.254.169.254`, `fd00:ec2::254`) + link-local are hard-blocked
with anti-rebinding pinning; loopback is a deliberate permanent carve (the proxy's own
listener lives there); broad RFC1918 is open-by-design, admitted iff the active policy
admits the host (`crates/nub-sandbox/LIMITATIONS.md` at
[`850ebe62`](https://github.com/nubjs/nub/commit/850ebe622f041e608bb4134148bdd17235ff8b89)).
The single closest reference for nub's actual use case — a sandbox around
agent-driven/untrusted code execution, not general containerization — is **Codex**, and
Codex blocks RFC1918 by default with a precise, non-wildcard per-host opt-in.

## Comparison table

| Tool | Category | RFC1918 default | Loopback | Metadata (`169.254.169.254`) | Opt-in shape | Source |
|---|---|---|---|---|---|---|
| **OpenAI Codex** (`network-proxy` crate) | agent-sandbox proxy | **BLOCK** | **BLOCK** (same gate as RFC1918) | BLOCK (subset of link-local) | exact hostname/IP in `allowed_domains`; wildcards (`*`, `*.foo`) explicitly rejected for local entries | `codex/codex-rs/network-proxy/src/{policy.rs:45-98,runtime.rs:395-435,893-908,config.rs:148,173}` |
| **Cloudflare Workers** `connect()` (TCP Sockets) | serverless egress | **BLOCK** | **BLOCK** (listed alongside private IPs as disallowed) | BLOCK (link-local) | Workers VPC binding (Tunnel/Mesh/WAN on-ramp) — a separate, explicitly-bound private network object | [developers.cloudflare.com/workers/runtime-apis/tcp-sockets](https://developers.cloudflare.com/workers/runtime-apis/tcp-sockets/), [workers-vpc/configuration/vpc-networks](https://developers.cloudflare.com/workers-vpc/configuration/vpc-networks/) |
| **SSRF-guard libs** (`ssrf-req-filter`, `request-filtering-agent`, `ssrf-agent-guard`) | HTTP-client hardening | **BLOCK** | **BLOCK** (bundled into "private/reserved") | BLOCK | `allowPrivateIPAddress`/`allowIPAddressList` flags, off by default | [npmjs.com/package/request-filtering-agent](https://www.npmjs.com/package/request-filtering-agent), [github.com/y-mehta/ssrf-req-filter](https://github.com/y-mehta/ssrf-req-filter) |
| **nub-sandbox** (current, `main`) | agent-sandbox proxy | **OPEN** (admitted iff policy admits the host) | **ALWAYS ALLOW** (structural carve — proxy's own listener) | **BLOCK** + anti-rebind pin | — | `crates/nub-sandbox/{LIMITATIONS.md,src/proxy/mod.rs}` @ `850ebe62` |
| **Anthropic SRT** / Claude Code sandbox | agent-sandbox proxy | **ALLOW** (no IP classification at all — pure domain allowlist, DNS resolves wherever) | ALLOW (same — no special case) | **ALLOW** (no guard; not evaluated in scope) | n/a — no private-range concept exists to opt into | `sandbox-runtime/src/sandbox/*` |
| **Docker** default bridge | container networking | **ALLOW** (NAT'd egress to host LAN/internet by default) | container's own loopback only (namespace-isolated from host) | **incidentally blocked** on AWS EC2 only, via IMDSv2's hop-limit=1 TTL trick — not a Docker policy | `--network=none`, custom bridge + firewall rules, or a network policy add-on | [docs.docker.com/engine/network/drivers/bridge](https://docs.docker.com/engine/network/drivers/bridge/); IMDSv2 hop-limit mitigation: [aws.amazon.com/blogs/security/…ec2-instance-metadata-service](https://aws.amazon.com/blogs/security/defense-in-depth-open-firewalls-reverse-proxies-ssrf-vulnerabilities-ec2-instance-metadata-service/) |
| **bubblewrap** | Linux sandbox primitive | **ALLOW** (shares host netns by default) | shares host loopback | ALLOW (no distinction) | `--unshare-net` — binary, all-or-nothing | `bubblewrap/` (no per-range concept in the tool at all) |
| **firejail** | Linux sandbox | **ALLOW** (shares host network unless configured) | shares host loopback | ALLOW (no distinction) | `--net=none` / `--netfilter` — binary or hand-rolled iptables | `firejail/README*` (`net none`, `--netfilter`) |
| **gVisor** | container-runtime netstack | **N/A** — mechanism, no policy | depends on netstack config | depends on config | orchestrator-supplied (net namespace, allowlist proxy, or no route) | [gvisor.dev/docs/user_guide/networking](https://gvisor.dev/docs/user_guide/networking/) |
| **Firecracker** | microVM | **N/A** — mechanism, no policy | depends on host TAP/iptables setup | depends on setup | fully manual TAP + iptables on the host | Firecracker networking docs (manual setup, no built-in policy) |
| **GitHub Actions**-hosted runners | CI egress | **N/A** — no private range present by default | runner's own loopback | not reachable (no cloud VPC attachment by default) | opt into org-level private networking (VNet injection) to gain a private range at all | [docs.github.com/…/private-networking](https://docs.github.com/en/actions/concepts/runners/private-networking) |
| **AWS Lambda** (no VPC config) | serverless | **N/A** — not VPC-attached by default, no private range reachable | n/a | metadata service is the **host's**, not the function's; IMDSv2 hop-limit mitigates | attach to a VPC to gain (and then must separately guard) a private range | AWS IMDSv2 defense-in-depth blog (above); general Lambda networking model |

## Detail per finding that needed real digging

### Codex blocks RFC1918 the same way nub's maintainer is considering

`is_non_public_ipv4`/`is_non_public_ipv6` in
`policy.rs:45-98`
classify loopback, RFC1918, link-local, unspecified, multicast, broadcast, CGNAT
(`100.64/10`), the RFC5737 TEST-NET blocks, and IPv6 ULA (`fc00::/7`) as non-public. The
enforcement in `runtime.rs:395-435` runs this classification **on the resolved IP**, not
just the literal, and applies it in `NetworkProxyConfig::allow_local_binding` (default
`false`, `config.rs:173`) — so even a domain on the allowlist is blocked if it resolves to
a private address, unless the destination has its own **exact**, non-wildcard entry in
`allowed_domains` (`is_explicit_local_allowlisted`, `runtime.rs:893-908`, which explicitly
rejects `*` and `*.foo` patterns as local-allowlist entries). This is the same
DNS-rebinding-safe pattern nub's `850ebe62` SSRF hardening already applies to metadata —
Codex just extends the identical mechanism to the whole non-public range.

### Anthropic's own SRT has zero private-range concept

Despite being Anthropic's reference sandbox, SRT's egress control (`sandbox-manager.ts`,
`mux-proxy.ts`, `sandbox-config.ts`) is a pure hostname allowlist with no IP-literal
classification, no CIDR matcher, and no loopback/private special-casing — confirmed by
grep across the Sandbox Runtime sources, and independently corroborated. SRT is not a counter-example
to "SSRF-hardened tools block by default" — it's evidence that even a well-resourced,
security-conscious agent sandbox can ship with this gap open when the design center is
domain filtering, not IP-address confinement.

### Cloudflare Workers treats loopback and private IPs identically — both need the VPC escape hatch

`connect()` "disallows connections to private network IPs" and separately lists
`localhost` among disallowed addresses; the only way back in is a **Workers VPC** binding —
an explicit, named private-network object wired through Cloudflare Tunnel/Mesh/WAN
on-ramp, not a per-call bypass. This is the closest real-world analog to the maintainer's
floated `<private>`/`<local>` symbolic net target: a single named object a user opts into,
rather than a wildcard flag or raw CIDR entry.

### Docker's private-range reachability is a side effect of NAT, not a decision

The default bridge (`172.17.0.0/16` typically) NATs container egress through the host, so a
container reaches the host's LAN and the internet with no configuration — this is
foundational to Docker's use case (build tooling, sibling containers, host-mounted
services) and was never framed as an SSRF boundary. The one place private-range reachability
is *incidentally* narrowed is AWS's IMDSv2 hop-limit=1 default, which is an EC2-side TTL
trick (misconfigured proxies/NAT/bridges won't forward a TTL-1 packet) — it happens to also
block a default-bridge Docker container from reaching the metadata endpoint, but it is not
a network-egress policy Docker itself ships, and it doesn't touch the other RFC1918 ranges
at all.

### gVisor/Firecracker/bubblewrap/firejail are mechanism, not policy — orthogonal to this decision

These four give an embedder a network-isolation *primitive* (userspace netstack, TAP
device, netns unshare) with no opinion on which addresses are reachable once network access
is granted at all. They are not evidence for either side of the block/allow default
question — they simply don't operate at the address-classification layer nub's proxy
already does. Citing them as precedent for "allow by default" would be a category error:
they don't have a "default" here any more than a firewall chip has a default ruleset before
someone writes rules into it.

## Where nub already sits, and the actual decision remaining

nub is **not** starting from scratch on this axis. `850ebe622f` already closed the sharpest
SSRF vector (cloud-metadata + link-local + DNS-rebinding) fail-closed, unconditionally,
regardless of policy. The maintainer's open call is narrower than "block private ranges,
yes or no" — it's specifically:

1. Whether `10/8`, `172.16/12`, `192.168/16` join the always-blocked set (like metadata does
   today), or stay policy-admitted.
2. If blocked, what the opt-in looks like — nub's matcher already supports CIDR entries
   (`ipnet::contains`), so the mechanical lift is
   small; the design question is the *shape* (a raw CIDR literal in the policy vs. a named
   symbolic target).
3. Loopback is **not** part of this decision for nub the way it is for Codex/Cloudflare —
   nub's proxy is architecturally loopback-resident, so loopback must stay reachable
   regardless of the RFC1918 call. This is a genuine, deliberate divergence from every
   block-by-default tool surveyed (all of which gate loopback behind the same opt-in as
   RFC1918) — worth stating explicitly rather than letting it look like an oversight.

## Recommendation (non-binding)

Block RFC1918 by default, following Codex's shape rather than inventing a new one — it is
the only surveyed tool whose threat model (confining agent-driven/untrusted code execution,
not general containerization) matches nub's, and its design already resolves the exact
tension the maintainer is weighing (legitimate local-service reachability vs. SSRF safety)
with a mechanism nub can adopt cheaply:

- **Default:** extend `is_blocked_egress_ip` (`crates/nub-sandbox/src/proxy/mod.rs`) to also
  reject the resolved IP when it falls in `10/8`, `172.16/12`, `192.168/16` — same
  fail-closed, anti-rebind-pinned gate metadata already gets. Loopback stays exempted (per
  above — it is not equivalent to Codex's loopback-gated case; nub's proxy needs it
  structurally).
- **Opt-in:** the maintainer's floated symbolic net target (e.g. `<private>` / `<local>`)
  as a policy entry that expands to the private ranges, rather than requiring a raw CIDR
  literal per project — cheaper for the common "let me reach my local Postgres" case than
  Codex's exact-hostname-only allowlist, while still being an explicit, visible opt-in
  rather than a silent default-open. Mirror Codex's one real discipline worth copying:
  reject `*`/wildcard patterns from expanding into the private-range grant, so a broad
  `net: ["*"]` allow doesn't silently re-open RFC1918 the way it currently silently admits
  it — the opt-in has to be as deliberate as the block.
- This is additive to, not a replacement for, the CIDR support nub's matcher already has —
  a project that wants finer-grained control (`10.0.0.0/24` only) can still write that
  directly.

## Follow-ups

1. **Maintainer sign-off needed** — this is exactly the security-posture/default decision
   this doc was scoped to inform, not resolve. The concrete next step if the maintainer
   agrees with the recommendation: implement the RFC1918 block + the symbolic opt-in target
   as a normal `nub-sandbox` PR (worktree flow), following the same shape as `850ebe62`
   (unit coverage for the classifier + an e2e SSRF case in `linux_proxy.rs`/`macos_proxy.rs`
   with an admitted-loopback negative control).
2. If adopted, update `crates/nub-sandbox/LIMITATIONS.md`'s "OPEN BY DESIGN" RFC1918 section
   to reflect the new default and cross-link this doc.
3. No CI/push implications from this doc alone — it lands no code.

## Changelog

- 2026-07-12 — Initial write-up.
