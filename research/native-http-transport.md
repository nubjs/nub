# Native HTTP transport experiment

**Status:** Complete, 2026-07-21. The experiment tested whether an installable native HTTP transport could improve server throughput for ordinary Node applications while preserving standard Fetch semantics.

## Findings

| Question | Result |
|---|---|
| Can a native transport serve terminal responses quickly from Node? | Yes. The Hyper and uWebSockets.js terminal paths reached the same throughput range as Bun on this host. |
| Does that advantage survive a genuine `Request` and `Response` bridge? | No in these tests. The semantics-complete Hyper/N-API path was slower than raw `node:http` and existing Node adapters while consuming more CPU and memory. |
| Is JavaScript route matching the primary cost? | No for the measured static Fetch path. Native terminal routing was fast, but the Web-object and native-to-JavaScript boundaries dominated once a request entered JavaScript. |
| Does using a battle-tested native engine remove adapter maintenance? | No. The adapter still owns body flow, Web-object construction, response streaming, backpressure, cancellation, header behavior, and errors. |
| Would bundling the addon inside Nub change steady-state throughput? | Not materially. An npm-installed N-API addon and a Nub-bundled N-API addon execute through the same Node/V8 boundary. |

The experiment supports native terminal operations that do not enter JavaScript. It does not demonstrate an advantage for a general-purpose native Fetch server on stock Node.

## Experiment shape

The prototype was a portable Node-API addon rather than a Nub-specific runtime API. The native implementation used Tokio, Hyper, Hyper-util, and matchit. Four passes progressively narrowed the result:

1. The first spike measured native terminal responses, compact JavaScript dispatch, native routing, and an intentionally incomplete Fetch upper bound.
2. The second spike added genuine branded `Request` objects, headers, request bodies, bounded buffering, streamed responses, backpressure, cancellation, errors, duplicate response headers, status text, and disconnect semantics.
3. The existing-engine pass compared raw uWebSockets.js with the `@remix-run/node-serve` and `@whatwg-node/server` adapters.
4. The final pass isolated Web-object construction and matched synchronous versus asynchronous handlers using process CPU time.

The experimental source revisions were `ff44f193db66c97a52585509a0e0ed6c992f66df`, `97b76dac23d06ac450e082799555d1c397b3bad5`, `807b7cc0fcb9c8965361589d8405ccb1d6741050`, and `c8516138d79aa1863d3b71a9222d48f5cc992ab0`. Each raw result records its revision, source tree or diff hash, native binary hash where applicable, runtime versions, order, and host state.

## Method

| Item | Value |
|---|---|
| Host | Apple M1 Max, 10 logical CPUs, 64 GiB RAM, macOS Darwin 25.5.0 |
| Node | 26.5.0 |
| Bun | 1.3.14 |
| Rust | 1.95.0 |
| Load generator | oha 0.5.5 on the same host |
| Saturated runs | Three randomized repetitions, one-second warm-up, five-second measurement, concurrency 50 |
| Low-concurrency runs | Three randomized repetitions, three-second measurement, concurrency 1 |
| Correctness gate | Exact status, body, and content type before every measured server run; any failed or non-2xx request failed the run |
| CPU | Total process CPU time divided by wall time; `1.00` means one logical CPU on average |
| Memory | Resident set size sampled after warm-up and measurement |

The tables report requests per CPU-second as the median of each repetition's paired throughput/CPU ratio. Some early raw JSON summary fields divided independently selected medians; the committed per-repetition measurements are the source of truth for the corrected values below.

The host was contended throughout the work. Load averages ranged from roughly 9 to 31 during the decision-facing server runs, with 12–14 GiB of swap in use. Absolute throughput values therefore have elevated uncertainty, and small differences require confirmation on isolated hardware. Order randomization, repeated runs, process CPU, RSS, and the distinct terminal-versus-Fetch behavior remained consistent enough to locate the architectural boundary.

The final object-cost run occurred at load averages above 230. It uses process CPU rather than wall time, randomizes cases, forces GC between cases, makes constructed values observably escape, and aborts if source changes during execution. Its wall-time values are not interpreted.

## First spike

The first spike established transport and routing ceilings before implementing complete HTTP and Fetch semantics.

### Saturated static response

| Server/path | req/s | CPU cores | req/CPU-s | RSS MiB | p50 ms | p99 ms |
|---|---:|---:|---:|---:|---:|---:|
| Native static terminal | 105,803 | 0.97 | 108,873 | 54.9 | 0.463 | 0.693 |
| Native parameter route terminal, 1k routes | 104,631 | 0.97 | 107,890 | 56.3 | 0.469 | 0.735 |
| Bun Fetch | 97,712 | 0.97 | 100,546 | 34.6 | 0.467 | 1.019 |
| Native minimal Fetch upper bound | 84,445 | 1.70 | 49,742 | 97.0 | 0.574 | 1.213 |
| Native compact JavaScript request | 83,003 | 1.29 | 64,257 | 61.2 | 0.606 | 0.946 |
| Native parameter route to JavaScript, 1k routes | 80,680 | 1.32 | 61,320 | 62.2 | 0.625 | 1.011 |
| Bun parameter route terminal, 1k routes | 76,319 | 0.96 | 79,348 | 41.7 | 0.604 | 1.313 |
| Raw `node:http` | 57,413 | 0.98 | 58,598 | 87.9 | 0.820 | 1.728 |
| srvx Fetch | 42,989 | 1.00 | 43,085 | 98.0 | 1.097 | 2.312 |
| Synthetic Node Fetch adaptation | 39,048 | 1.00 | 39,135 | 110.8 | 1.213 | 2.540 |
| Hono parameter route, 1k routes | 33,319 | 0.99 | 33,594 | 109.1 | 1.432 | 2.938 |

The minimal Fetch row was an upper bound, not an implementation candidate. It omitted request headers and bodies, response metadata, streaming, cancellation, backpressure, and full error semantics.

### Single in-flight static response

| Server/path | req/s | CPU cores | p50 ms | p99 ms |
|---|---:|---:|---:|---:|
| Native static terminal | 25,244 | 0.31 | 0.033 | 0.087 |
| Bun Fetch | 23,686 | 0.33 | 0.036 | 0.092 |
| Raw `node:http` | 19,502 | 0.51 | 0.044 | 0.115 |
| srvx Fetch | 16,233 | 0.56 | 0.054 | 0.124 |
| Native compact JavaScript request | 16,141 | 0.54 | 0.054 | 0.124 |
| Native minimal Fetch upper bound | 13,235 | 0.61 | 0.066 | 0.152 |

The compact native-to-JavaScript path fell below raw Node at concurrency 1. The extra scheduling boundary benefited saturated pipeline throughput but added per-request latency.

## Semantics-complete Fetch bridge

The second spike implemented the standard path used for the general-server comparison. Its default handler receives a genuine branded Web `Request`; the standard response path consumes a Web `Response`. Request buffering was bounded to 8 MiB per request and 64 MiB globally, and response chunks were bounded to 64 KiB before crossing the native boundary. Custom fast responses and the rejected lazy facade remain separate rows.

### Saturated static response

| Server/path | req/s | CPU cores | req/CPU-s | RSS MiB | p50 ms | p99 ms |
|---|---:|---:|---:|---:|---:|---:|
| Native static terminal | 87,308 | 0.86 | 102,038 | 65.6 | 0.505 | 1.371 |
| Bun Fetch | 85,904 | 0.90 | 95,046 | 37.3 | 0.504 | 1.497 |
| Raw `node:http` | 53,648 | 0.96 | 56,014 | 88.3 | 0.847 | 1.922 |
| Native branded Request + FastResponse | 50,637 | 1.72 | 28,582 | 210.8 | 0.865 | 2.574 |
| srvx FastResponse | 45,399 | 0.89 | 50,798 | 89.9 | 0.917 | 3.503 |
| srvx Fetch | 38,262 | 0.96 | 39,700 | 98.0 | 1.172 | 2.986 |
| Native branded Request + Response | 33,429 | 1.59 | 21,003 | 274.1 | 1.262 | 5.238 |

### JSON echo

| Server/path | req/s | CPU cores | req/CPU-s | RSS MiB | p50 ms | p99 ms |
|---|---:|---:|---:|---:|---:|---:|
| Bun Fetch | 63,816 | 0.94 | 67,758 | 40.2 | 0.685 | 1.966 |
| srvx FastResponse | 45,890 | 0.97 | 47,125 | 91.2 | 1.003 | 2.221 |
| Raw `node:http` | 43,943 | 0.98 | 45,033 | 95.1 | 1.035 | 2.448 |
| srvx Fetch | 34,860 | 0.98 | 35,580 | 100.1 | 1.310 | 2.908 |
| Native branded Request + FastResponse | 28,672 | 1.66 | 17,277 | 196.7 | 1.646 | 3.376 |
| Native branded Request + Response | 23,600 | 1.61 | 14,643 | 192.7 | 2.014 | 3.585 |

### Sixteen-chunk 16 KiB stream

| Server/path | req/s | CPU cores | req/CPU-s | RSS MiB | p50 ms | p99 ms |
|---|---:|---:|---:|---:|---:|---:|
| Bun Fetch | 28,078 | 0.92 | 30,772 | 59.8 | 1.563 | 4.225 |
| Raw `node:http` | 26,978 | 0.87 | 31,761 | 88.0 | 1.524 | 5.153 |
| srvx Fetch | 20,927 | 0.86 | 24,121 | 106.4 | 1.979 | 7.967 |
| Native branded Request + Response | 11,341 | 2.10 | 5,396 | 179.0 | 4.274 | 10.461 |

### Asynchronous handler

| Server/path | req/s | CPU cores | req/CPU-s | RSS MiB | p50 ms | p99 ms |
|---|---:|---:|---:|---:|---:|---:|
| Bun Fetch | 72,535 | 0.82 | 88,260 | 37.3 | 0.540 | 2.628 |
| Raw `node:http` | 50,739 | 0.94 | 53,990 | 88.4 | 0.876 | 2.282 |
| srvx Fetch | 35,860 | 0.96 | 37,519 | 97.9 | 1.253 | 3.144 |
| Native branded Request + Response | 33,787 | 1.65 | 20,484 | 252.8 | 1.304 | 3.667 |

### Single in-flight static response

| Server/path | req/s | CPU cores | p50 ms | p99 ms |
|---|---:|---:|---:|---:|
| Native static terminal | 21,710 | 0.33 | 0.038 | 0.121 |
| Bun Fetch | 19,675 | 0.34 | 0.039 | 0.158 |
| Raw `node:http` | 17,823 | 0.52 | 0.048 | 0.141 |
| srvx Fetch | 13,445 | 0.57 | 0.062 | 0.288 |
| Native branded Request + FastResponse | 9,500 | 0.71 | 0.090 | 0.313 |
| Native branded Request + Response | 9,181 | 0.75 | 0.095 | 0.275 |

The terminal path retained high throughput. The complete Fetch bridge did not: it trailed the raw Node and srvx comparisons, used roughly 1.6–2.1 CPU cores at concurrency 50, and showed the lowest single-request throughput among the decision-facing rows.

## Existing native engine comparison

The third pass tested whether replacing the new Hyper transport with an existing native engine changed the outcome. It added raw uWebSockets.js 20.69.0 and `@remix-run/node-serve` 0.2.0. The published Remix package pins uWebSockets.js 20.66.0, which did not have a loadable Node 26 prebuild in this environment, so the current-runtime comparison explicitly overrides it to 20.69.0.

The raw transport and zero-argument Remix rows are diagnostic ceilings. Raw uWebSockets.js returns a terminal response without a Fetch adapter. The zero-argument Remix optimization deliberately skips incoming `Request` construction.

The uWebSockets.js override also exposed a fixture-level semantic mismatch: a queryless request arrived through the Remix adapter as a URL ending in `?undefined`. The inspected correctness fixture validated the method and path while tolerating that suffix. The exact published Remix/uWebSockets.js dependency combination could not be exercised on Node 26.5.0.

### Saturated static response

| Server/path | req/s | CPU cores | req/CPU-s | RSS MiB |
|---|---:|---:|---:|---:|
| Raw uWebSockets.js terminal ceiling | 74,236 | 0.73 | 101,276 | 60.6 |
| Bun Fetch | 72,657 | 0.81 | 89,165 | 37 |
| Remix/uWS no-Request ceiling | 56,187 | 0.91 | 61,760 | 86.1 |
| Native Hyper terminal ceiling | 53,050 | 0.58 | 95,722 | 65.8 |
| Native branded Request + FastResponse | 52,681 | 1.84 | 28,231 | 224.8 |
| Remix/uWS branded Request + Response | 41,868 | 0.98 | 42,591 | 181 |
| Raw `node:http` | 38,410 | 0.81 | 47,462 | 89 |
| srvx Fetch | 34,633 | 0.93 | 37,386 | 98 |
| Native Hyper branded Request + Response | 33,450 | 1.60 | 20,870 | 253 |

### Single in-flight static response

| Server/path | req/s | CPU cores | req/CPU-s | RSS MiB |
|---|---:|---:|---:|---:|
| Native Hyper terminal ceiling | 21,805 | 0.32 | 67,123 | 64.5 |
| Raw uWebSockets.js terminal ceiling | 20,524 | 0.33 | 61,290 | 60.2 |
| Bun Fetch | 20,422 | 0.34 | 60,085 | 35 |
| Remix/uWS no-Request ceiling | 16,329 | 0.44 | 35,132 | 81.2 |
| Raw `node:http` | 16,095 | 0.51 | 31,263 | 83 |
| Remix/uWS branded Request + Response | 14,335 | 0.56 | 25,382 | 170 |
| srvx Fetch | 10,236 | 0.57 | 18,014 | 92 |
| Native branded Request + FastResponse | 9,887 | 0.70 | 13,188 | 161 |
| Native Hyper branded Request + Response | 8,979 | 0.77 | 11,592 | 167 |

Raw uWebSockets.js reached the terminal throughput range without introducing another HTTP parser implementation. Its ordinary Remix Fetch adapter moved back toward the raw Node range when it constructed a real `Request` and consumed a standard `Response`. This reproduces the same boundary with a separate native engine.

Inspection also found that srvx's Node adapter supplies a lazy `NodeRequest` facade rather than a native branded `Request`. Its results remain useful as an existing adapter comparison, but they do not measure the same object-construction requirement as the Remix and native branded paths.

## Web-object CPU isolation

The final pass tested whether Promise scheduling explained the adapter gap. Matched handlers observed and validated the same request method and URL. The clean-source run used five randomized repetitions of 10,000 operations.

All 14 inspected, asynchronous, synchronous, and explicit no-request fixture variants passed real HTTP preflight. Their throughput samples are excluded because host load reached 170–238 and sync/async ordering inverted with run order.

| Operation | Median process CPU μs/op |
|---|---:|
| Loop baseline | 0.03 |
| Async loop baseline | 0.10 |
| AbortController construction | 1.26 |
| Lazy Remix header wrapper | 2.15 |
| URL-only Request construction | 4.50 |
| Request construction with headers | 7.01 |
| Request construction with disconnect signal | 8.13 |
| Full Request construction with headers and disconnect signal | 11.21 |
| Response construction | 8.66 |
| Response construction plus two-read body drain | 10.07 |
| Full request + sync handler + response drain | 27.29 |
| Full request + async handler + response drain | 26.65 |

The complete sync and async paths differed by 2.4% in the opposite direction from a sync advantage. This is treated as no measurable Promise-scheduling improvement, not as evidence that async handlers are faster. The complete object path is in the same range as the 23.48 μs/request CPU envelope implied by the earlier Remix result of 42,591 requests per CPU-second. The cases are not strictly additive, but they locate most of the ordinary adapter cost in Web-object construction and response-body draining rather than HTTP parsing.

## Semantic ownership

A native engine can own HTTP parsing, socket I/O, keep-alive, transport-level backpressure, and native route lookup. A standard Fetch adapter still owns the behavior at the JavaScript boundary:

- URL and authority construction
- Request header exposure and duplicate handling
- Bounded request-body buffering or streaming
- Genuine `Request`, `Headers`, `ReadableStream`, and `AbortSignal` behavior
- Status, status text, duplicate response headers, and forbidden-body cases
- Response streaming and write backpressure
- Client-disconnect propagation and stream cancellation
- Handler, stream, and adapter error behavior
- Graceful shutdown and in-flight lifecycle accounting

The semantics-complete spike needed explicit fixes for lifecycle tombstones, cancellation while a response read was pending, malformed Host rejection, request-body cancellation, bounded per-request and global buffering, bounded response chunking, invalid status fallback, and source/result provenance. Using an existing parser and socket engine does not remove this adapter surface.

## Nub integration depth

Nub augments the user's installed Node through public extension surfaces and does not ship a patched Node or embed libnode. That leaves the following integration options:

| Integration | Steady-state effect |
|---|---|
| Installable Node-API package | Native transport and JavaScript execute in one Node process; every dynamic Fetch request crosses the N-API/V8 and Web-object boundary. |
| Addon bundled with Nub | Loading and distribution can be simpler, but the per-request data path is the same as the installable package. |
| Nub parent process owns the listening socket | Dynamic handlers require cross-process IPC or shared-memory coordination in addition to Web-object construction. |
| Statically extracted terminal route manifest | Static responses, immutable files, redirects, or constrained proxies can complete in native code without constructing a JavaScript request. |
| Patched or embedded Node | The boundary could move into the runtime, but this is a different architecture from Nub's stock-Node augmenter model. |

Static route extraction can improve registration and developer experience, but it changes per-request performance only when the route is terminal and never invokes JavaScript. The same terminal manifest can be consumed by a portable package; Nub-specific packaging is not required for the throughput benefit.

Intercepting standard `node:http` calls through a preload would require replacing or monkey-patching existing Node semantics. Holding the socket in the Nub CLI would add another process boundary. Neither approach removes the measured Web-object cost while preserving ordinary Node behavior.

## Routing and client scope

Native matchit routing handled 1,000 exact or parameterized routes without reducing terminal throughput in the first fixture. Routing a matched request back into JavaScript retained the JavaScript scheduling and object costs. Literal route registration at startup is sufficient to compile patterns once; source-code analysis is only useful when it can prove that an entire response is terminal.

The experiment did not benchmark an HTTP client. Node's Fetch implementation already uses Undici, and a native client returning standard Fetch objects would introduce another semantic bridge. Client performance remains an independent question and cannot be inferred from the server results.

## Limitations

- The server runs need isolated-host confirmation before small differences are treated as significant.
- The prototype implemented HTTP/1 but not TLS, HTTP/2, WebSocket upgrades, trailers, graceful in-flight draining, or cross-platform prebuilt packages.
- The initial minimal Fetch result is an incomplete upper bound and must not be compared as a standards-complete server.
- The explicit `FastResponse` path is a custom response API, not a standard Fetch result.
- The prototype marks a `FastResponse` lifecycle complete when its bytes enter Hyper rather than after socket delivery.
- The bounded response fallback may retain one additional 64 KiB copy per active stream.
- An application promise that ignores disconnect indefinitely retains its terminal-state tombstone alongside the promise.
- The lazy request facade defers or omits native Web-object work and is excluded from the general Fetch conclusion.
- The Remix comparison uses an explicit uWebSockets.js version override because its published pin could not load on Node 26.5.0 in this environment.
- The `@whatwg-node/server` comparator and continuation throughput probes are present in raw data but omitted from summary tables where host contention made their rates non-decision-facing.

## Raw data

The committed JSON files contain every repetition, randomized order, preflight metadata, summary, process sample, source identity, runtime version, and host sample used in this document.

| Pass | Raw data |
|---|---|
| First spike, concurrency 50 | [`first-static-c50.json`](data/native-http/first-static-c50.json) |
| First spike, concurrency 1 | [`first-static-c1.json`](data/native-http/first-static-c1.json) |
| Complete Fetch static, concurrency 50 | [`fetch-static-c50.json`](data/native-http/fetch-static-c50.json) |
| Complete Fetch JSON, concurrency 50 | [`fetch-json-c50.json`](data/native-http/fetch-json-c50.json) |
| Complete Fetch streaming, concurrency 50 | [`fetch-stream-c50.json`](data/native-http/fetch-stream-c50.json) |
| Complete Fetch async handler, concurrency 50 | [`fetch-async-c50.json`](data/native-http/fetch-async-c50.json) |
| Complete Fetch static, concurrency 1 | [`fetch-static-c1.json`](data/native-http/fetch-static-c1.json) |
| Existing native adapters, concurrency 50 | [`adapters-static-c50.json`](data/native-http/adapters-static-c50.json) |
| Existing native adapters, concurrency 1 | [`adapters-static-c1.json`](data/native-http/adapters-static-c1.json) |
| Existing adapter workload and correctness probes | [`adapters-correctness.json`](data/native-http/adapters-correctness.json) |
| Web-object process CPU isolation | [`web-object-cpu.json`](data/native-http/web-object-cpu.json) |

## Conclusion

The native terminal paths validate Hyper, matchit, and uWebSockets.js as high-throughput transport and routing substrates. The semantics-complete portable Fetch path does not retain that terminal advantage once each request crosses into Node and becomes a standard Web request and response.

Bundling the addon inside Nub can reduce installation and startup friction, but it does not change the steady-state boundary measured here. A material improvement for arbitrary Fetch handlers would require a Node-core server-to-Fetch integration or a runtime architecture that moves Web-object creation below the public addon boundary. Terminal native routes remain the demonstrated fast path on stock Node.
