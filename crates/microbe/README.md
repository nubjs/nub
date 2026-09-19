# microbe

The smallest embeddable npm package installer. One crate, pure Rust, no async runtime: it fetches a package and its dependency tree from the registry into a directory the caller names, verifies integrity, and reports where the bin entries landed.

It is built for a tool that needs to install something and then run it — the `npx`-shaped job. It does not reconcile with an existing `node_modules`, keep a store, write a lockfile, or run lifecycle scripts. Those are the parts of a package manager that take up the space.

```rust
let installed = microbe::Microbe::new()?.install("esbuild@^0.25", Path::new("/tmp/tools"))?;
// installed.bins["esbuild"] -> /tmp/tools/node_modules/esbuild/bin/esbuild
```

```
microbe <name[@spec]> <dir> [--registry <url>]
```

## Size

Stripped, `opt-level = "z"` with fat LTO:

| Target | default | `--features tls` |
| --- | --- | --- |
| aarch64-apple-darwin | 526 KB | 821 KB |
| x86_64-unknown-linux-gnu | 670 KB | 1.76 MB |
| x86_64-unknown-linux-musl (static) | 680 KB | 1.74 MB |

The budget is decided by TLS and nothing else. The default build links none: it borrows an HTTPS client the host already has, so the resolve, verify, and extract core is all that remains, and even a fully static musl binary stays well under a megabyte. Turning on `tls` adds a real client — platform TLS on macOS and Windows, rustls and ring elsewhere, where it costs about 1.1 MB and takes a Linux binary over budget.

## Speed

Two phases. The plan phase walks the dependency graph breadth-first, fetching each level's packuments in parallel and deciding every package's directory before anything is downloaded. The materialize phase then downloads, verifies and extracts every planned tarball in parallel, sixteen at a time by default (`Microbe::concurrency`). Cold installs into an empty directory, measured back to back on one machine in one minute, so the numbers are comparable to each other and to nothing else:

| Package | microbe | npm 11 | pnpm 10 |
| --- | --- | --- | --- |
| express (71 packages) | 2.2 s | 2.2 s | 1.7 s |
| eslint (77 packages) | 5.7 s | 8.5 s | 8.6 s |
| vite (16 packages, native binaries) | 27.8 s | 51.5 s | 23.0 s |

## How it reaches the network

`Transport` is a one-method trait. An embedder that already links an HTTP client implements it and pays nothing:

```rust
Microbe::with_transport(MyClient::new())
```

Otherwise `Microbe::new()` detects one, preferring in-binary TLS when compiled, then:

1. **`node`** — one long-lived child running `fetch`, with requests multiplexed over its stdio so the parallel install actually runs in parallel and undici reuses connections. This is the anchor, because whatever gets installed is about to be run by Node anyway.
2. **`curl`** — macOS, Windows 10 and later, most full Linux distributions.
3. **`wget`** — busybox, so Alpine.

Node comes first because it is the only one of the three the use case guarantees. A survey of 16 popular container base images found `curl` on 4 of them, and 8 carried neither `curl` nor `wget`; every Node image carries Node.

## What it implements

Packages land flat under `<dir>/node_modules`. A version conflict nests the loser under its dependent, which is what Node's resolver walks up to find, and placement is deterministic: the same request always produces the same tree. Resolution is first-wins over `dependencies` plus platform-matching `optionalDependencies`, one abbreviated packument fetch per package name. A name listed under `optionalDependencies` is optional even when it also appears under `dependencies`, because `npm publish` mirrors it there; a name listed under `bundleDependencies` ships inside its parent's tarball and is never fetched. Tarballs are checked against `dist.integrity`, falling back to the pre-SRI `dist.shasum`. A tarball entry whose path would escape its package directory is refused.

`peerDependencies` are ignored and install scripts are not run. Packages that declare one are named in `Installed::skipped_install_scripts` so the caller can decide what that means — for a prebuilt-binary package like esbuild or biome the postinstall is a no-op, because the platform package carrying the binary is an optional dependency that microbe already installed.

## Tests

`cargo test` runs against an in-memory registry serving real gzipped tarballs with real integrity strings, so the whole install path runs with no network.
