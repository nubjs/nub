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

Stripped, `opt-level = "z"` with fat LTO, measured on aarch64-apple-darwin:

| Build | Size |
| --- | --- |
| default | 493 KB |
| `--features tls` | 805 KB |

The budget is decided by TLS and nothing else. The default build links none: it borrows an HTTPS client the host already has, so the resolve, verify, and extract core is all that remains. Turning on `tls` adds a real client — platform TLS on macOS and Windows, rustls elsewhere, where it costs roughly a megabyte and puts a static Linux binary over budget.

## How it reaches the network

`Transport` is a one-method trait. An embedder that already links an HTTP client implements it and pays nothing:

```rust
Microbe::with_transport(MyClient::new())
```

Otherwise `Microbe::new()` detects one, preferring in-binary TLS when compiled, then:

1. **`node`** — one long-lived child running `fetch`. This is the anchor, because whatever gets installed is about to be run by Node anyway.
2. **`curl`** — macOS, Windows 10 and later, most full Linux distributions.
3. **`wget`** — busybox, so Alpine.

Node comes first because it is the only one of the three the use case guarantees. A survey of 16 popular container base images found `curl` on 4 of them, and 8 carried neither `curl` nor `wget`; every Node image carries Node.

## What it implements

Packages land flat under `<dir>/node_modules`. A version conflict nests the loser under its dependent, which is what Node's resolver walks up to find. Resolution is first-wins over `dependencies` plus platform-matching `optionalDependencies`, one abbreviated packument fetch per package name. Tarballs are checked against `dist.integrity`, falling back to the pre-SRI `dist.shasum`. A tarball entry whose path would escape its package directory is refused.

`peerDependencies` are ignored and install scripts are not run. Packages that declare one are named in `Installed::skipped_install_scripts` so the caller can decide what that means — for a prebuilt-binary package like esbuild or biome the postinstall is a no-op, because the platform package carrying the binary is an optional dependency that microbe already installed.

## Tests

`cargo test` runs against an in-memory registry serving real gzipped tarballs with real integrity strings, so the whole install path runs with no network.
