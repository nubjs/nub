# Node-API

`@jdxcode/aube-node` exposes aube's installer to Node.js, Bun, Electron, and
other Node-API hosts. It also supports compiled Bun executables.

## Install

```sh
npm install @jdxcode/aube-node
```

```ts
import { install } from "@jdxcode/aube-node"

const result = await install(projectDirectory, {
  add: [
    { name: "react", version: "19.1.0" },
    { name: "typescript", version: "5.9.3", dev: true },
  ],
  offline: false,
  signal: abortController.signal,
  onEvent(event) {
    switch (event.kind) {
      case "phase":
        console.log(event.phase)
        break
      case "progress":
        console.log(`${event.resolved}/${event.total}`)
        break
      case "output":
        console.log(event.level, event.code, event.message)
        break
    }
  },
})

console.log(result.resolved, result.reused, result.downloaded, result.durationMs)
```

With no `add` entries, `install` installs the dependencies already declared in
the project manifest. Added entries are saved at exact versions; `dev: true`
saves an entry to `devDependencies`. Lifecycle scripts are skipped.

Setting `offline: true` refuses registry access and resolves from the local
store and packument caches. A missing cached package rejects the operation.

## Configuration

Register embedder setting defaults before the first `install`:

```ts
import { configure, install } from "@jdxcode/aube-node"

configure({
  defaults: {
    minimumReleaseAge: "259200", // seconds; 3-day cooldown for fresh releases
  },
})
```

Defaults use canonical setting names and sit at the lowest precedence —
environment variables, project files, and user configuration all override
them. Registration is process-global and first-write-wins: calling
`configure` after an install (or a second time) rejects with
`ERR_AUBE_EMBED_ALREADY_INITIALIZED`, and unknown setting names reject with
`ERR_AUBE_EMBED_INVALID_SETTING`. Without a `configure` call the addon
applies its built-in defaults (`nodeLinker=hoisted`, `minimumReleaseAge=0`).

Per-install, `osvTransitiveCheck: true` forces a live transitive OSV check
even when resolution reused every version from the existing lockfile.

## Events

`InstallEvent` is a discriminated union:

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

Event callbacks are non-blocking. An `AbortSignal` requests cooperative
cancellation; cancellation completes at an install boundary that leaves the
project consistent.

## Errors

Rejected promises use the exported error shape:

```ts
interface AubeError extends Error {
  code: string
  diagnostic: string
}
```

`code` is a stable `ERR_AUBE_*` identifier governed by the
[error-code policy](/error-codes).

## Concurrency

Installs for unrelated projects may run concurrently. Operations within the
same workspace wait on its project lock, covering manifest, lockfile, store,
and linker mutations.

## Compiled Bun executables

Use the package's build plugin to select one platform addon for each compiled
target:

```ts
import { bunPlugin } from "@jdxcode/aube-node/bun-plugin"

await Bun.build({
  entrypoints: ["./src/index.ts"],
  compile: { target: "bun-linux-x64", outfile: "./app" },
  plugins: [bunPlugin({ os: "linux", arch: "x64", libc: "glibc" })],
})
```

Install optional dependencies for every target before a cross-platform build:

```sh
bun install --os="*" --cpu="*" @jdxcode/aube-node
```

## Compatibility

The addon targets Node-API 8 and supports Node.js 18 and newer. Package
versions track the aube workspace version.
