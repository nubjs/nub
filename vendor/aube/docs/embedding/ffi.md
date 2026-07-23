# C ABI

`@jdxcode/aube-ffi` exposes aube through a stable C ABI for Bun FFI, Deno FFI,
Node.js FFI loaders, Python `ctypes`, and native applications. JavaScript hosts
that support Node-API should generally use [`@jdxcode/aube-node`](/embedding/node);
the C ABI is intended for hosts that need a universal dynamic-library boundary.

## Distribution

```sh
npm install @jdxcode/aube-ffi
```

The root package selects the matching macOS, Windows, glibc Linux, or musl
Linux platform package and exports:

```ts
import { headerPath, libraryPath, target } from "@jdxcode/aube-ffi"
```

The same target-named libraries and `aube.h` are attached to GitHub releases
for non-npm consumers. Package versions track the aube workspace version.

## Header and operation model

The hand-written [`aube.h`](https://github.com/jdx/aube/blob/main/crates/aube-ffi/include/aube.h)
declares these operations:

```c
typedef void (*aube_event_cb)(const char *event_json, void *ctx);

int32_t aube_init(const char *host_json);
uint64_t aube_install(const char *options_json, aube_event_cb cb, void *ctx);
uint64_t aube_add(
    const char *project_dir,
    const char *packages_json,
    const char *options_json,
    aube_event_cb cb,
    void *ctx);
char *aube_wait(uint64_t handle);
int32_t aube_cancel(uint64_t handle);
void aube_string_free(char *value);
```

`aube_install` and `aube_add` copy their input strings, return immediately, and
run on an internal multi-threaded runtime. `aube_wait` blocks until completion
and consumes the handle. `aube_cancel` requests cooperative cancellation.
A callback-driven host should call `aube_wait` after receiving the terminal
event; `aube_wait` then returns immediately and releases the completed handle.

Initialize the host once before starting operations by passing `aube_init` a
JSON host profile:

```json
{
  "name": "my-host",
  "version": "1.0.0",
  "defaults": {
    "minimumReleaseAge": "0"
  }
}
```

Initialization and operation start are synchronized. If an operation starts
before `aube_init`, aube permanently selects its standalone host defaults for
the process and later initialization calls are no-ops.

Install options require `projectDir` and may include `frozenMode`, `prodOnly`,
`devOnly`, `omitOptional`, `ignoreScripts`, `runRootLifecycle`, `dryRun`,
`lockfileOnly`, `force`, `offline`, `strictNoLockfile`,
`dangerouslyAllowAllBuilds`, and `osvTransitiveCheck`.

Add options may include `saveDev`, `saveExact`, `saveOptional`, `savePeer`,
`ignoreScripts`, `force`, `dangerouslyAllowAllBuilds`, `offline`, `prodOnly`,
`devOnly`, `omitOptional`, and `osvTransitiveCheck`. `packages_json` is an array
of package specifier strings.

## Ownership and threading

- Inputs are UTF-8, NUL-terminated, borrowed only for the duration of the call,
  and copied before an asynchronous start function returns.
- The string returned by `aube_wait` belongs to aube and must be passed exactly
  once to `aube_string_free`.
- Event strings are borrowed and valid only during the callback.
- Callbacks run on aube-managed threads. The callback and its context must be
  thread-safe and remain valid until `aube_wait` returns.
- Callbacks must return normally without throwing, panicking, or unwinding
  across the C boundary. A callback must not call `aube_wait` for its active
  operation; signal another host thread to wait or wait after the callback
  returns.
- Every exported function catches Rust panics. Release FFI libraries use an
  unwind-enabled profile so a panic cannot cross the C boundary or abort the
  host process.

## Events and results

Both Node-API and C ABI transports use the same event JSON schema:

```ts
type InstallEvent =
  | { kind: "phase"; phase: "resolving" | "fetching" | "linking" | "complete" }
  | {
      kind: "progress"
      resolved: number
      total: number
      reused: number
      downloaded: number
      downloadedBytes: number
      estimatedBytes?: number
    }
  | {
      kind: "output"
      level: "info" | "warning" | "error"
      code?: string
      message: string
    }
```

`aube_wait` returns one of:

```json
{ "ok": true }
```

```json
{
  "ok": false,
  "code": "ERR_AUBE_INSTALL_CANCELLED",
  "message": "install cancelled"
}
```

Codes follow the published [error-code stability policy](/error-codes).

## Polled events

Hosts that cannot receive callbacks on foreign threads (Bun FFI, some ctypes
setups) can poll instead. Start the operation with `bufferEvents: true` and
drain the queue from any host thread:

```ts
const handle = library.symbols.aube_install(
  ptr(c(JSON.stringify({ projectDir: ".", bufferEvents: true }))),
  null,
  null,
)
let terminal = false
while (!terminal) {
  for (;;) {
    const raw = library.symbols.aube_events_next(handle)
    if (!raw) break
    const event = JSON.parse(new CString(raw).toString())
    library.symbols.aube_string_free(raw)
    console.log(event)
    terminal ||=
      (event.kind === "phase" && event.phase === "complete") ||
      (event.kind === "output" && event.level === "error")
  }
  if (!terminal) await new Promise((resolve) => setTimeout(resolve, 50))
}
// The terminal event arrived, so aube_wait returns immediately and
// releases the handle.
const result = library.symbols.aube_wait(handle)
```

The terminal event is the `complete` phase on success or the `error` output a
failed operation emits last.

`aube_events_next` returns the next buffered event as JSON (same schema as the
callback transport), or null when no event is pending or the handle is unknown
or already consumed. Each returned string must be freed with
`aube_string_free`. The buffer holds 4096 events and drops the oldest when
full; events still buffered when `aube_wait` returns are discarded with the
handle, so drain before waiting (or poll from a different thread than the one
that waits).

## Bun FFI

```ts
import { libraryPath } from "@jdxcode/aube-ffi"
import { CString, dlopen, ptr } from "bun:ffi"

const library = dlopen(libraryPath, {
  aube_init: { args: ["ptr"], returns: "i32" },
  aube_install: { args: ["ptr", "ptr", "ptr"], returns: "u64" },
  aube_wait: { args: ["u64"], returns: "ptr" },
  aube_events_next: { args: ["u64"], returns: "ptr" },
  aube_cancel: { args: ["u64"], returns: "i32" },
  aube_string_free: { args: ["ptr"], returns: "void" },
})

const c = (value: string) => Buffer.from(`${value}\0`)
library.symbols.aube_init(ptr(c(JSON.stringify({ name: "my-host", version: "1.0.0" }))))

const handle = library.symbols.aube_install(
  ptr(c(JSON.stringify({ projectDir: ".", offline: true }))),
  0,
  0,
)
const resultPointer = library.symbols.aube_wait(handle)
const result = JSON.parse(new CString(resultPointer).toString())
library.symbols.aube_string_free(resultPointer)
library.close()
```

Bun's FFI and arbitrary-thread JavaScript callbacks remain experimental. In
Bun, poll with `bufferEvents`/`aube_events_next` (above) instead of passing a
callback, or use the Node-API package.

## Deno FFI

```ts
import { libraryPath } from "npm:@jdxcode/aube-ffi"

const library = Deno.dlopen(libraryPath, {
  aube_install: { parameters: ["buffer", "function", "pointer"], result: "u64" },
  aube_wait: { parameters: ["u64"], result: "pointer", nonblocking: true },
  aube_string_free: { parameters: ["pointer"], result: "void" },
} as const)

const events: unknown[] = []
const callback = Deno.UnsafeCallback.threadSafe(
  { parameters: ["pointer", "pointer"], result: "void" } as const,
  (event) => {
    if (event) events.push(JSON.parse(new Deno.UnsafePointerView(event).getCString()))
  },
)
const input = new TextEncoder().encode(`${JSON.stringify({ projectDir: "." })}\0`)
const handle = library.symbols.aube_install(input, callback.pointer, null)
const resultPointer = await library.symbols.aube_wait(handle)
if (!resultPointer) throw new Error("aube_wait returned null")
const result = JSON.parse(new Deno.UnsafePointerView(resultPointer).getCString())
library.symbols.aube_string_free(resultPointer)
callback.close()
library.close()
```

Run Deno with `--allow-ffi`.
