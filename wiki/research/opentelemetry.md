# OpenTelemetry on Node — what attaching costs, what a launcher can see, and what the runtimes have shipped

**Status:** 2026-08-03. Survey and measurement pass on what a launcher that computes a child process's flags and environment can do for OpenTelemetry that an in-process SDK cannot.

Companions: [[research/preload-ecosystem|preload ecosystem survey]] (the same attach path from the `NODE_OPTIONS` side) and [[research/monkey-patch-prevalence|monkey-patch prevalence]] (the interception layer underneath).

## The headline

Attaching `@opentelemetry/auto-instrumentations-node` to a trivial Node process costs **8.4 seconds** when no collector is listening.

Roughly 85% of that is a shutdown flush against an endpoint that was never going to answer, and another 12% is cloud-metadata probes that cannot succeed off-cloud. Both are decidable before the process starts.

## Measured attach cost

Node 26.5.0, macOS arm64, `@opentelemetry/auto-instrumentations-node` with its 80 transitive OpenTelemetry packages, minimum of 3 runs, on a contended host.

Every cell asserts exit 0 **and** a positive control — the probe reports whether `Symbol.for('opentelemetry.js.api.1')` was registered — so a crash or a silently-absent preload cannot be recorded as a fast run. The OTLP endpoint is pinned to an unused port rather than the default 4318, which was contended.

| Configuration | No collector listening | Collector listening |
|---|---|---|
| baseline, no preload | 80 ms | 84 ms |
| attached, default config | **8,398 ms** | **1,266 ms** |
| attached + `OTEL_SDK_DISABLED=true` | 247 ms | 243 ms |
| attached + `OTEL_METRICS_EXPORTER=none` | 1,272 ms | 1,241 ms |
| attached + `OTEL_NODE_RESOURCE_DETECTORS=none` | 7,440 ms | 253 ms |
| attached + both of the above | 219 ms | 224 ms |

The cost is **three independent, additive terms**, and the arithmetic reconciles in both columns:

| Term | Cost | Behavior |
|---|---|---|
| Attach floor — SDK plus ~40 instrumentation packages loading | **~140 ms** | irreducible |
| Resource detectors — cloud-metadata probes | **~1,020 ms** | times out whenever the process is not on that cloud |
| Metrics-exporter shutdown flush | **~7,150 ms** | vanishes entirely when a collector answers |

The collector-listening column is the control: standing a collector up removes the 7.1 s term entirely and leaves the detectors, confirming two unrelated causes rather than one effect measured twice.

This corrects an earlier reading recorded in the [[research/preload-ecosystem|preload ecosystem survey]], which attributed the ~1.2 s residual left by `OTEL_METRICS_EXPORTER=none` to instrumentation module load. Module load is only ~140 ms of it; the rest is resource detection.

## The ESM attach does not do what its documentation implies

The three documented incantations are not interchangeable, and the difference is invisible at runtime.

| Incantation | What it does |
|---|---|
| `--require @opentelemetry/auto-instrumentations-node/register` | Builds and starts the SDK, patches CommonJS through `require-in-the-middle`. Registers no ESM loader. |
| `--import @opentelemetry/auto-instrumentations-node/register` | The same CommonJS file routed through the ESM loader. Still registers no ESM hook. |
| `--experimental-loader=@opentelemetry/instrumentation/hook.mjs` | The only supported ESM hook — a 16-line file re-exporting `import-in-the-middle`'s async `resolve`/`load`/`initialize`. |

**The failure mode is silent.** Measured on an ESM Express app: with the register entrypoint but no loader flag, the process emits **zero Express spans, exits 0, and writes nothing to stderr**, while `http`, `net` and `undici` spans continue to flow — so every ESM-imported library is uninstrumented and the dashboard still looks alive. There is no published list of which instrumentations work under ESM, and only 8 of 44 contrib instrumentations carry ESM test fixtures, so a user cannot currently determine this from documentation.

Three further properties of the ESM path:

- **OpenTelemetry has not adopted synchronous module hooks.** `import-in-the-middle` shipped `module.registerHooks` support in v3.1.0 (2026-06-17) and OpenTelemetry depends on `^3.0.0`, but there are no `registerHooks` references anywhere in `opentelemetry-js` — it remains async-only and the documentation still prescribes `--experimental-loader`. Node 26 deprecates `module.register()` under DEP0205, so the only ESM attach path is on a deprecation track. Tracking issue: [opentelemetry-js#4933](https://github.com/open-telemetry/opentelemetry-js/issues/4933), open since 2024-08.
- **Every ESM module in the graph gets wrapped.** `InstrumentationBase.enable()` passes `{internals: true}`, which is incompatible with an include-list, and OpenTelemetry never calls `createAddHookMessageChannel`.
- **Wrapping cost scales with module count**, at roughly 0.56 ms per module. On a 300-module app: async wrap-all **+202 ms**, sync wrap-all **+64 ms**, sync with an include-list **+9 ms**.

Version floors are stricter than the presence of the API suggests. `dd-trace` takes the synchronous path only on Node ≥ 22.22.3 / 24.11.1 / 25.1.0 / 26, falling back to async `module.register` below, because `registerHooks` rejected nullish CommonJS source until [nodejs/node#59929](https://github.com/nodejs/node/pull/59929). Sentry's equivalent draws the line at 24.13 / 25.1.

## What Node publishes for free

Verified on Node 26.5.0 with **no OpenTelemetry packages installed at all**. These `diagnostics_channel` channels fire:

| Surface | Channels |
|---|---|
| HTTP server | `http.server.request.start`, `http.server.response.created`, `http.server.response.finish` |
| HTTP client | `http.client.request.created`, `http.client.request.start`, `http.client.response.finish` |
| fetch / undici | `undici:request:create`, `undici:request:headers`, `undici:request:trailers` |
| Sockets | `net.client.socket`, `net.server.socket`, `net.server.listen` |
| Module loading | `module.require`, `module.import` |
| Console | `console.log`, `console.error`, `console.warn` |
| Subprocesses | `child_process.spawn` — **async spawn only**, `spawnSync` publishes nothing |

That covers HTTP server and client spans, fetch spans, W3C context propagation in both directions, console-to-logs, module-load timing, and subprocess spans — from channel subscriptions alone, with no monkey-patching and no `import-in-the-middle`. The channels for `http2` (both directions), `node.test`, and `process.execve` also exist but were not exercised here.

The `diagnostics_channel` API appears in exactly one OpenTelemetry instrumentation (undici), against 192 files in `dd-trace-js`.

## Prior art

What the other runtimes, Node core and the APM vendors have shipped: one built-in implementation, one non-starter, an open Node PR, and a layer-wide move away from monkey-patching.

### Deno — the only runtime with OpenTelemetry built in

Shipped unstable in 2.1.5 (2025-01), announced with `--unstable-otel` in 2.2, stabilized in **2.4.0 (2025-07-01)**. It remains off by default, gated behind `OTEL_DENO=true`.

Auto-instrumented with no user code: `Deno.serve` (SERVER spans), `fetch` (CLIENT), `node:http` server and client, `node:http2` both directions, `Deno.cron`, and `console.*` routed to OTLP **logs**. Deno's own Rust log output is also piped into OTel logs. Not instrumented: `Deno.Command` subprocesses, filesystem, `node:sqlite`, the event loop, and the built-in test runner.

Metrics are emitted with conventional names — `http.server.request.duration`, `http.server.active_requests`, `http.server.request.body.size`, `http.server.response.body.size`, and a `v8js.*` family covering GC duration and heap sizing. A deliberate split, commented in the source: client spans go ERROR at status ≥ 400, server spans only at ≥ 500.

Configuration is **environment variables only — there is no `deno.json` surface**. Deno honors the standard set (`OTEL_SDK_DISABLED`, `OTEL_SERVICE_NAME`, `OTEL_RESOURCE_ATTRIBUTES`, `OTEL_PROPAGATORS`, `OTEL_TRACES_SAMPLER`, the `OTEL_EXPORTER_OTLP_*` family) plus branded switches `OTEL_DENO`, `OTEL_DENO_TRACING`, `OTEL_DENO_METRICS`, `OTEL_DENO_CONSOLE`.

One notable extension: `OTEL_EXPORTER_OTLP_PROTOCOL=console` pretty-prints spans, metrics and logs to stderr — a non-spec fourth protocol value beside `http/protobuf`, `http/json` and `grpc`, and the answer to how a user sees their traces without first standing up a collector.

**The global-registry mechanism, and its consequence.** Deno writes `globalThis[Symbol.for("opentelemetry.js.api.1")]` with version `1.999.999`, filling the `trace`/`context`/`metrics`/`propagation` slots with native providers. That version passes `@opentelemetry/api`'s read check for every 1.x, so `trace.getTracer()` in user code transparently gets Deno's tracer with no setup — and it permanently blocks **writes**, because `registerGlobal` requires an exact version-string match that no user copy of `@opentelemetry/api` can satisfy. Upstream declined to change this ([opentelemetry-js#5454](https://github.com/open-telemetry/opentelemetry-js/issues/5454), closed as not planned), and vendors responded by deleting the registry outright: Sentry's Deno SDK clears any pre-existing registration before installing its own. A related exact-match failure bit the Node flavour too, where a patch-level mismatch silently disabled all tracing ([sentry-javascript#22338](https://github.com/getsentry/sentry-javascript/issues/22338)).

**Operational findings from the implementation**, each traceable to a public thread:

| Finding | Reference |
|---|---|
| Exporter threads interfered with signal delivery and the watch-mode restart loop | [#29478](https://github.com/denoland/deno/issues/29478), [#31540](https://github.com/denoland/deno/issues/31540) |
| A signal-flush fix shipped in 2.4.0 and was reverted in 2.4.1; flush is `before_exit` only | [#31242](https://github.com/denoland/deno/issues/31242) |
| Enabling telemetry caused `deno eval` to hang, across four minor versions | [#33157](https://github.com/denoland/deno/issues/33157) |
| OpenTelemetry env vars were read before `--env-file` was applied, so file-configured endpoints were ignored | [#27851](https://github.com/denoland/deno/issues/27851) |
| ANSI escape codes reached exported span names and log bodies | [#29423](https://github.com/denoland/deno/issues/29423) |
| Being built in means user code cannot reach the SDK — no way to expose collected metrics on an endpoint | [#30487](https://github.com/denoland/deno/issues/30487) |
| The `@opentelemetry/instrumentation-*` packages do not compose with the built-in providers | [#30809](https://github.com/denoland/deno/issues/30809) |
| Subprocess spans and cross-process context propagation are unimplemented | [#32751](https://github.com/denoland/deno/issues/32751), [#32752](https://github.com/denoland/deno/issues/32752) |

### Bun

No built-in OpenTelemetry and no announced plan. The standard JavaScript OpenTelemetry packages are supposed to work and do not.

Instrumentation for Express, Fastify and Koa does not attach, because there is no module-hook equivalent for the instrumentation packages to use ([oven-sh/bun#26536](https://github.com/oven-sh/bun/issues/26536), open). The `node:http` server publishes nothing to `diagnostics_channel` ([#29586](https://github.com/oven-sh/bun/issues/29586)), nor does `node:child_process` ([#32472](https://github.com/oven-sh/bun/issues/32472)). A secondary defect has subscribed channels garbage-collected on the first full GC, so subscriptions silently stop firing. The umbrella request has been open since 2023-07 ([#3775](https://github.com/oven-sh/bun/issues/3775)).

### Node core

Not shipped, but there is an open PR and a settled design consensus.

[nodejs/node#61907](https://github.com/nodejs/node/pull/61907) adds a `node:otel` module gated behind `--experimental-otel`, emitting HTTP server, HTTP client and fetch spans over OTLP/HTTP JSON only — deliberately, to avoid a protobuf dependency — with W3C `traceparent` extracted inbound and injected outbound. **The entire implementation is `diagnostics_channel` subscriptions plus an `AsyncLocalStorage` span store.**

The discussion on [nodejs/node#57992](https://github.com/nodejs/node/issues/57992) is against vendoring the SDK: maintainers from Datadog, Sentry and the OpenTelemetry JS project converged on expanding `diagnostics_channel` coverage instead, on the grounds that bundling the SDK would commit Node to a large API surface with its own versioning and semantic-convention drift, and that OpenTelemetry's vendor neutrality derives from OTLP rather than from any particular SDK implementation. One requirement was raised consistently: whatever a runtime provides, the authoring API must be the official one rather than a runtime-specific dialect.

### The instrumentation layer is moving off monkey-patching

Sentry's Node SDK v10.67 **removed** its dependency on `@opentelemetry/instrumentation-*`, `import-in-the-middle`, and `require-in-the-middle`.

Its integrations now come from `orchestrion`, an SWC-based Rust AST walker that injects `diagnostics_channel` publish calls into third-party library source, delivered either by a runtime hook or a bundler plugin; Sentry then subscribes. Datadog and Sentry co-maintain the shared tracing-hooks layer. With Node core's PR, the three largest Node APM implementations are all moving toward `diagnostics_channel` and away from resolver patching.

### Framework hooks

Next.js calls a project's exported `register()` from its own `instrumentation.ts` bootstrap.

What that buys over a raw preload: one file serving both the Node and Edge runtimes, execution after framework config resolution but before route modules load, bundling by the framework's compiler so it survives a production build, and an `onRequestError` hook. Next then patches the resolved `TracerProvider` so third-party spans exit its internal async storage. A raw preload gets none of that, and Next's `NODE_OPTIONS` reformatting actively destroys repeated same-name flags, as measured in the [[research/preload-ecosystem|preload ecosystem survey]].

## Semantic conventions — what exists

The convention namespaces that touch a command-line build tool, and how far each has matured. None of them covers package-management internals.

| Namespace | Status | Covers |
|---|---|---|
| `cicd.*` | Release Candidate | pipeline run, task, worker |
| `cli.*` (spans) | Development | callee and caller spans for a command-line invocation |
| `vcs.*` | mixed | repository, ref, revision, change |
| `artifact.*` | Development | filename, version, `purl`, hash, attestation |
| `test.*` | Development | suite and case name, result |
| `nodejs.*` | — | event-loop metrics only |

The `cli` span convention requires `process.executable.name`, `process.exit.code` and `process.pid`, recommends `process.command_args`, and makes `error.type` conditionally required when the exit code is non-zero. It explicitly permits a documented custom low-cardinality span-name format.

**There are no package-manager conventions.** A sweep of all 90 semantic-convention model namespaces found nothing for dependency resolution, fetching, linking, content-addressed storage, or lockfiles. The only adjacent attribute is `artifact.purl`, whose own example is an npm package. Conformance therefore covers the outer shell of a build-tool invocation and none of its internals. OTEP 223 proposed CI/CD observability conventions from January 2023 and was closed without merge when the OTEPs repository was archived in November 2025; the work continued through a dedicated SIG and produced today's `cicd.*`, which treats build internals as a later phase that has not happened.

## Emitting OpenTelemetry from a Rust CLI

**Crate stability.** The `opentelemetry` crates are at 0.32.x and have never released 1.0.

Metrics API/SDK and Logs API/SDK are marked Stable; the Metrics and Logs OTLP exporters are Release Candidate; **Traces API, Traces SDK, and the Traces OTLP exporter are all Beta.**

**Binary size**, measured with a release profile matching Nub's (`opt-level=3`, `lto=thin`, `codegen-units=1`, `strip=true`, `panic=abort`). The baseline is a binary that already exercises tokio, reqwest with rustls, and serde_json, so the deltas isolate the telemetry dependency rather than the async and TLS stacks:

| Variant | Size | Δ vs baseline |
|---|---|---|
| hello world | 339,120 B | — |
| baseline: tokio + reqwest/rustls + serde_json, exercised | 2,234,512 B | 0 |
| + hand-rolled OTLP/JSON serialization | 2,234,608 B | **+96 B** |
| + `opentelemetry-otlp` 0.32, `http-proto` | 2,827,712 B | **+580 KiB** |
| + `opentelemetry-otlp` 0.32, `grpc-tonic` | 3,219,280 B | **+962 KiB** |

An earlier run of this measurement was invalid, recorded here because the failure is easy to repeat: the baseline's HTTP code was declared but never called in the telemetry variants, so link-time optimization dead-stripped it and the instrumented binaries measured *smaller* than the baseline. The table above uses an identical live workload in every variant, confirmed by checking that OTLP field names are present in the hand-rolled binary and absent from the baseline.

**Startup**, measured on a contended host and therefore meaningful as ratios rather than absolutes: linking the crates without constructing a provider costs about +0.6 ms, and constructing a provider plus recording and exporting a span costs about +0.7 ms.

**Shutdown is the operational risk.** A refused connection costs nothing — the export fails instantly and shutdown returns cleanly. A *blackholed* endpoint, where packets are dropped rather than refused, costs **5.00 s on every invocation**, because the SDK's flush timeout is hardcoded and carries an in-source note that it is not yet configurable. For a short-lived CLI that is the difference between shippable and not, and it is a narrower hazard than the Node side's: locally-absent collectors are free, remote or VPN-routed ones are not.

**Lighter alternatives exist.** The OTLP File Exporter is a specified format — JSON Lines, OTLP JSON encoding, one signal type per file — that needs no network, no timeout and no Beta SDK, and can be forwarded later by a collector. Precedent for a build tool writing a trace file rather than exporting live: Cargo emits a Chrome trace from its existing `tracing` subscriber, and Bazel's profiler does the same.

## Toolchain telemetry prior art

What comparable build tools emit today, and in what format. Only two of them speak OpenTelemetry.

| Tool | Emits | Format |
|---|---|---|
| Turborepo | OpenTelemetry **metrics**, behind an experimental flag | OTLP gRPC and HTTP |
| BuildKit | OpenTelemetry traces | OTLP, plus a delegated exporter the client pulls from |
| Cargo | Chrome trace from its `tracing` registry | `traceEvents` JSON |
| Bazel | Build Event Protocol, plus a profiler | protobuf event stream; `traceEvents` JSON |
| Gradle Develocity | Build Scans | proprietary |
| npm | `--timing` | flat name-to-milliseconds map, no nesting, no export |
| pnpm, Yarn Berry, Nx | nothing | — |

Turborepo is the closest analogue to a package manager emitting its own telemetry, and its choices are instructive:

- It emits **metrics rather than spans**.
- It treats **cardinality as a first-class design axis**, attaching bounded attributes always and gating unbounded ones (run id, revision, task id, task hash) behind opt-in flags, on the stated grounds that backends charge per unique series.
- It restricts endpoints to `https://` and rejects private, loopback, link-local and cloud-metadata addresses as SSRF targets — including the IPv4-mapped-IPv6 bypass — but still permits the hostname `localhost` for local collectors.
- It locks credentials to the endpoint that introduced them, so a higher-priority configuration source changing the endpoint drops lower-priority auth headers rather than forwarding them elsewhere.

The safety contract from `otel-cli` is the other reusable piece: with no endpoint configured it selects a null client that discards everything and never errors, any telemetry failure exits 0 unless explicitly asked to fail, and the default export timeout is 1 s with a bounded retry. Its stated principle is that telemetry must never break the thing it is observing.

## Context propagation across process boundaries

Carrying trace context in environment variables is standardized: OTEP 0258 was merged 2024-10-22 and defines `TRACEPARENT`, `TRACESTATE` and `BAGGAGE` with formats identical to their W3C header counterparts.

Implementations to model:

- **BuildKit** — extracts the variables at startup and injects the current span when spawning a child, in about 40 lines total.
- **Jenkins OpenTelemetry plugin** — exports them into every build step's environment so downstream tools join the pipeline trace.
- **`otel-cli`** — picks up an ambient `TRACEPARENT`, injects one into child processes, and can substitute it into an arbitrary argument for use as an HTTP header.
- **Gradle plugin** — passes it to exec tasks behind a setting.

The gap is documentation rather than mechanism: the `cli` span convention carries an unresolved placeholder for context propagation pointing at [semantic-conventions#1612](https://github.com/open-telemetry/semantic-conventions/issues/1612), which notes there is no spec text and no propagator implementing it. GitHub Actions does not set `TRACEPARENT` natively; third-party actions do.

Nothing about extract-and-inject requires linking an OpenTelemetry SDK — both directions are fixed-format hex-string handling plus one environment entry.

## A Nub-specific finding

Nub broke OpenTelemetry's ESM instrumentation. An instrumented ESM application under Nub 0.6.0 on Node 26.5.0 exited 1 with `ERR_INVALID_RETURN_PROPERTY_VALUE`, where plain Node with the same loader exits 0.

Filed as [nubjs/nub#669](https://github.com/nubjs/nub/issues/669) and fixed there. The cause is Nub's `commonjs` → `commonjs-sync` relabel, which routes an `import()` of CommonJS through Node's sound synchronous translator. The relabel carries a guard that declines whenever a user asynchronous loader is active, because relabeling would re-route that module's inner `require()` calls through the user's own resolve hook. The guard read two signals, and an `--experimental-loader` delivered through `NODE_OPTIONS` trips neither: the flag registers its loader natively, so no `module.register()` call is observed, and `NODE_OPTIONS` entries are not hoisted into `process.execArgv`. The relabel therefore ran against a live user loader and handed Node a `commonjs-sync` result with a null `source` — a pair `validateSourcePermissive` accepts for `commonjs` alone. The fix routes the guard through the flag scan that already covered both channels, and makes a non-null `source` an independent precondition of the relabel.

Two properties of the failure are worth carrying forward. It was confined to the flag forms, `--experimental-loader` and `--loader` through `NODE_OPTIONS`: the same loader passed on the command line lands in `execArgv` and was always detected, as was the `module.register()` form. And it reached users precisely because the documented attach is the flag form — an attach OpenTelemetry recommends setting through `NODE_OPTIONS`, per the table above.

A recovery path that returns a null `source` alongside a `builtin` format was proposed as the cause and is **not** it: `validateSourceStrict` exempts every URL beginning with `node:` before it examines `source`, and that branch runs for no other URL. Two further explanations were tested and ruled out. The version band that governs whether Nub downgrades to its asynchronous tier is **not** the cause: a plain-Node synchronous `registerHooks` pass-through composed with the same asynchronous loader on Node 26.5.0 exits 0, so hook composition is not broken at that version. And the internal environment variable that forces the asynchronous tier is **not** a workaround, nor can it be tested as one, because Nub strips the variable before the child process reads it — the launcher alone sets it.

## Reproduction

How to re-run each measurement above, including the controls that decide whether a fast run is a real one.

- Attach-cost matrix: install `@opentelemetry/auto-instrumentations-node` into an empty project, then time `node` running a trivial script with `NODE_OPTIONS="--require @opentelemetry/auto-instrumentations-node/register"` across the env-var combinations in the table. Pin `OTEL_EXPORTER_OTLP_ENDPOINT` to an unused port — the default 4318 is a commonly-occupied port and a stray listener silently removes the 7.1 s term. Assert a positive control in every cell: a probe script reporting whether `globalThis[Symbol.for('opentelemetry.js.api.1')]` is set, so an unattached run cannot be recorded as a fast one. A `--require` of an absolute path to the `register` subpath does not resolve, because it is an exports subpath rather than a file; use the bare specifier with the project as the working directory.
- Channel coverage: subscribe to each channel with `diagnostics_channel`, then exercise it *after* subscribing, and report fired-versus-silent. The `tracingChannel` surfaces take a handler object rather than a callback, and `child_process.spawn` requires an asynchronous spawn.
- Deno and Node core behavior: read from source checkouts rather than documentation. The span names, metric names and env-var handling cited above were taken from the runtime source, and the operational findings from the linked issue threads.

## Changelog

Every revision to this document, with the date and what changed.

- 2026-08-15 — Corrected the cause of the Nub-specific finding. The failure is the `commonjs-sync` relabel running against a user asynchronous loader that both detection channels missed, not the `builtin` recovery path the document previously named — Node exempts `node:` URLs from the source check, so that branch cannot raise this error. Recorded the delivery channels that are and are not affected, and that the bug is fixed.
- 2026-08-03 — Initial write-up. Measured the attach-cost decomposition with a positive control and a pinned endpoint; verified `diagnostics_channel` coverage on a clean Node; surveyed Deno, Bun, Node core, Sentry and the framework-hook layer; recorded semantic-convention coverage, Rust crate stability and binary-size cost, toolchain telemetry prior art, and the environment-variable context-propagation standard. Corrects the ~1.2 s residual attribution in [[research/preload-ecosystem]].
