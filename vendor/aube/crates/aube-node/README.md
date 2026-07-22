# aube Node-API bindings

`@jdxcode/aube-node` embeds aube's installer in JavaScript hosts, including
Node.js, Bun, Electron, and compiled Bun executables.

```ts
import { install } from "@jdxcode/aube-node"

const result = await install(projectDirectory, {
  add: [
    { name: "react", version: "19.1.0" },
    { name: "typescript", version: "5.9.3", dev: true },
  ],
  signal: abortController.signal,
  onEvent(event) {
    switch (event.kind) {
      case "phase":
        console.log(event.phase)
        break
      case "progress":
        console.log(event.resolved, event.downloaded)
        break
      case "output":
        console.log(event.level, event.code, event.message)
        break
    }
  },
})

console.log(result.durationMs)
```

The addon creates an empty package manifest when needed, saves added packages
at exact versions, and installs declared dependencies. Added entries with
`dev: true` are saved to `devDependencies`. Root and dependency lifecycle
scripts are always skipped. Registry/auth settings and dependency-section
omission come from the project's `.npmrc`; `omit=dev` and `omit=optional` are
honored.

Independent projects install concurrently using invocation-scoped state.
Operations targeting the same workspace serialize on its project lock.
`onEvent` uses a non-blocking Node-API thread-safe function, and `signal`
cooperatively cancels at a safe install boundary.

Rejected promises are `AubeError` objects with a stable `code` and a
human-readable `diagnostic`. Published `ERR_AUBE_*` values follow aube's
[error-code stability policy](https://github.com/jdx/aube/blob/main/docs/error-codes.md).

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

## Compiled Bun executables

Add `bunPlugin({ os, arch, libc })` from `@jdxcode/aube-node/bun-plugin` to each
target's `Bun.build` plugins. The plugin resolves the root import to the
selected native platform package so each executable embeds one addon.

Cross-compilation hosts must install every optional platform package before
building targets for other operating systems and architectures:

```sh
bun install --os="*" --cpu="*" @jdxcode/aube-node@$AUBE_NODE_VERSION
```

## Compatibility and stability

- The addon targets Node-API 8.
- Node.js 18 and newer are supported.
- Bun 1.3.11 is exercised by direct and compiled-executable smoke tests.
- macOS arm64/x64, Windows arm64/x64, Linux glibc arm64/x64, and Linux musl
  arm64/x64 packages are published.
- Package versions track the aube workspace version.
- The TypeScript API follows semantic versioning; stable error identifiers are
  not removed or repurposed.

## Development

Run the direct Bun and compiled-executable smoke tests from the repository
root:

```sh
crates/aube-node/poc/run.sh
```

CI also installs the generated npm tarballs in Node and Bun consumers and
type-checks the public declarations. For support, use
[GitHub Discussions](https://github.com/jdx/aube/discussions).
