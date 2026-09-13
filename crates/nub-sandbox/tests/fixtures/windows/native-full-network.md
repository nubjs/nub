# Windows native full-network fixture

`native-full-network.cpp` is one executable used unchanged by the plain,
zero-capability raw, and native-adapter AppContainer controls. The parent test
harness must compile it once per target architecture using the command in the
source header, hash the resulting executable, and retain its source, binary,
ordinary-user token/profile output, and terminal CI result.

## Contract

The harness provides only disposable, parent-owned loopback endpoints and
checks request/reply bytes in both directions. The fixture emits
`FULL_NETWORK_ROOT_BROKER_SOCKET` first: this is a `WSASocketW` call that the
adapter routes through its root broker. A failed marker is an adapter
admission/RPC diagnosis, not a LAN or peer-traffic result. The fixture never
opens the private broker pipe, asks about helper rights, retries through a
different route, or changes host policy.

For adapted `net:true`, every listed case must exit zero and report the peer
marker. The retained-session matrix runs that positive arm first, followed by
native `net:false` and hostname-restricted arms using the **same literal
filesystem grants**. Those negative arms must have no peer-observed request and
no completed reply; successful setup APIs alone are not a pass. This catches a
retained-resource identity alias between otherwise-zero-capability policies.
`fs-canary` must report both read and write denied for every confined mode. The
canary is a pre-existing parent-created file outside all project, temporary,
and tool-root grants; the unconfined control must prove that the same path is
both readable and writable. A failure to load the adapter, unsupported native
compatibility, or an adapter diagnostic failure is a failure, never success.

| Case | Owned oracle |
| --- | --- |
| `tcp4`, `tcp6` | Child connects to an owned TCP peer and exchanges bytes. |
| `udp4`, `udp6` | Owned UDP peer receives a datagram then replies. |
| `listen4`, `listen6` | Child announces an ephemeral loopback listener; parent connects and exchanges bytes. |
| `connectex4` | Child uses `ConnectEx` and waits through an IOCP completion. |
| `acceptex4` | Child posts `AcceptEx`, announces its listener, and waits through an IOCP completion. |
| `concurrent4` | Twelve simultaneous TCP round trips; each completion is required. |
| `descendant4` | Root starts a normal descendant; the descendant performs an owned TCP round trip. |
| `fs-canary` | The ungranted parent-created file cannot be opened for either read or write. |

## Deliberate boundary

Loopback is not LAN or public-network evidence. Default fixtures do not claim
DNS coverage: `localhost`, a hosts-file result, or a cached `GetAddrInfoW`
result proves only local resolution. A future bounded DNS probe may use a
parent-owned UDP responder and documented per-query `DnsQueryEx` server-list
options, but only after verifying that the Windows API supports an explicit
server address and port without changing global DNS or host policy. It still
needs an uncacheable name plus an observer query log; it would not establish
ordinary `GetAddrInfoW` routing by itself. LAN/public egress,
multicast/broadcast, provider-specific socket options, and non-loopback
firewall behavior likewise require a separately controlled CI probe. Those
probes must preserve the ordinary-user, source/binary/token provenance used by
the default suite; they must not turn a loopback result into a claim about
external networking.
