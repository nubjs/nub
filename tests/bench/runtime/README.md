# Runtime benchmarks

Measurements of the augmentations Nub applies to a running Node process, each against plain `node` on the same box. These are the numbers behind the runtime figures on the site; the charts are drawn with the `nub-charts` skill from a saved run, never from typed-in values.

| Script | What it measures |
|--------|------------------|
| `async-context-frame.sh` | `AsyncLocalStorage` with and without the context-frame implementation (Node 24's default, which Nub switches on for 22.9–23.x): a request-shaped store/await loop, and Fastify 5 + OpenTelemetry SDK under autocannon. |
| `threadpool.sh` | `UV_THREADPOOL_SIZE` at Node's fixed 4 versus the core count Nub installs: Fastify 5 routes that queue on the pool (pbkdf2, gzip, file read, stat) under autocannon, then a `dns.lookup` burst. Plain `node` with the variable set, so only the pool size varies. |
| `pool-bound.sh` | The same pool on workloads heavy enough to be bound by it: bcrypt, scrypt and pbkdf2 logins, sharp thumbnails, gzip and brotli of a 1 MB response, and an RSA-2048 key pair per request, `node` against `nub`. The pool size in each row is the `libuv-worker` thread count measured inside the server, since `nub` removes its own `UV_THREADPOOL_SIZE` from the environment once the pool exists. Meant for a box with more than 4 cores (`--machine c3d-standard-16`); on 4 cores or fewer Nub sets nothing different. |
| `pool-priority.sh` | Whether demoting the pool threads beyond Node's four to a low priority (Linux `nice` through `os.setPriority(tid)`) keeps a bigger pool from taking CPU that other processes on the box are using: a bcrypt route with pool 4, pool 16, and pool 16 with the extra twelve at nice 10 or 19, on an idle box and beside twelve busy-loop processes, reporting the server's and the co-tenants' throughput. |
| `pool-priority-nub.sh` | The same measurement with `nub` itself against plain `node` (needs a binary built from a tree with the threadpool augmentation, `--source <worktree>`): the server's req/s and the co-tenants' throughput, idle and busy. |

Both run on the latest Node by default (`NODE_VERSION=v22.x` overrides it). An augmentation that applies to every Node is measured on the latest major, so the figure is about Nub and not about an old Node. A version-gated one, like the context-frame flag, is measured on the line it applies to and the figure names that line; the latest-Node run is then the control that shows the two conditions equal where the gate is closed.

Both run on a Linux box at the repo root with `NUB_BIN` set, which is what `remote-build --job adhoc` provides:

```sh
nub scripts/remote-build.ts --job adhoc --script tests/bench/runtime/threadpool.sh --detach
nub scripts/remote-build.ts --attach <vm-name>
```

Every measurement is also printed as a `ROW {...}` JSON line. Save a run worth keeping under `results/<date>.json` with the machine, the Node version and the per-round values, and point a chart generator at that file.

The load generator shares the box with the server, so a route bound by the event loop can read a few percent lower when more pool threads compete for the same cores. Report that alongside the routes that gain; it is part of the result.
