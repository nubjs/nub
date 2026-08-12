# Embedding Node vs. spawning user Node: viability for Nub

Three ways Nub could get JavaScript running: bundle a Node and spawn it, spawn the Node the user already installed, or dynamic-link the user's `libnode`. Only the first two are shippable.

> Research target: should Nub (a) keep bundling its own Node and spawning it, (b) spawn the user's installed Node, or (c) dynamic-link into the user's Node via `libnode`? TL;DR: **(a) stays the default. (b) is a viable second mode for "additive" installs. (c) is not realistically shippable today.**

## 1. Is `libnode` a real, public, shipped artifact?

No, not on user machines. Node's shared-library build is documented as **unofficial** in [`maintaining-shared-library-support.md`][shlib-doc] and needs `./configure --shared` (or `vcbuild dll`) at build time.

The official `nodejs.org/dist` tarballs **do not ship `libnode.so` / `libnode.dylib`** — only the statically-linked `node` executable. Confirmed by [nodejs/node#52289][libnode-req] ("Please make Node.js embeddable") and the note in [alshdavid/libnode_sys][libnode-sys] that "nodejs does not distribute libnode binaries, so there isn't a stable/trusted URL to fetch them from."

What each distribution ships:

| Distributor | `libnode.*` present? |
|---|---|
| nodejs.org official tarballs / .pkg / .msi | **No.** Static `node` only. |
| Homebrew (node 21+) | **Yes**, `libnode.<abi>.dylib` in `lib/`, with `node` referencing it via `@rpath`. See [nexu-io/open-design#1275][brew-split]. |
| Fedora / Debian system packages | **Sometimes** (`libnode-dev`), but ABI varies wildly per distro. |
| nvm / fnm / n / volta / asdf / mise | **No.** They install vanilla nodejs.org tarballs. Static `node` only. |
| Electron | Yes — but Electron **ships its own** `libnode`, doesn't reuse the system's. See [Electron Internals: Node as a Library][electron-node]. |

Even on macOS — the platform where `libnode.dylib` is most likely to exist on disk — it is only there if the user installed via Homebrew. Most users on Linux/Windows/macOS have **no `libnode` to link against**. The ABI/embedder API carries a [stability disclaimer][embed-doc]: "breaking changes do not follow typical Node.js deprecation policy and may occur on each semver-major release without prior warning." A different `libnode` per minor-version Node is the expected steady state.

The embedder API itself (`InitializeOncePerProcess`, `CommonEnvironmentSetup`, `LoadEnvironment`) is **C++-only**. A C FFI is a request, not a shipped feature: see [nodejs/node#57846][cffi-pr] (open PR) and the note in [napi-rs#2869][napi-libnode]. Rust today consumes libnode through [`libnode_sys`][libnode-sys] / [`edon`][edon], which carry **patched Node builds** with a synthetic C entrypoint. Both depend on prebuilt static binaries authored by a single maintainer.

**Conclusion for question 1:** Nub cannot rely on a user-installed `libnode`. It does not exist on the platforms that matter, the ABI is unstable across Node minor versions, and the C API doesn't exist at all without a patched build. Dynamic-linking to user Node is off the table for v1.

## 2. Subprocess spawn cost on each platform

From [bitsnbites OS-primitive benchmark][osprim] and [val.town's spawn deep-dive][val-spawn]:

- **Linux**: `posix_spawn` / `vfork`+`exec` is the fastest path. Sub-millisecond for tiny binaries; a `node` exec including dyld + static init is ~6–10 ms before any JS runs.
- **macOS arm64**: `posix_spawn` is **~10× slower than Linux** for the same workload. Apple has heavy code-signing / SIP / dyld closure costs. Locally measured (see [`cold-start.md`](cold-start.md)) `node -e ''` is 26.7 ms; the irreducible dyld + InitializeOncePerProcess floor is ~10 ms.
- **Windows**: `CreateProcess` is **>20× slower than Linux** for spawn, and is "very sensitive to Windows Defender / antivirus" — real-world Windows users see 50–150 ms just to launch a process before any Node code runs.

**Nub-on-top overhead.** A Rust binary doing arg parsing, config discovery, and `posix_spawn` of `node` adds:
- macOS arm64: ~3–5 ms (Rust binary dyld + Nub's own startup work)
- Linux: ~1–2 ms
- Windows: ~10–30 ms

This is the cost Nub already pays with bundled-Node spawning. Switching from bundled to user Node doesn't change it. **The dominant cost is Node's own startup, not the spawn.**

## 3. Discovering and validating user-installed Node

Doable but fiddly. The probe surface:

1. **PATH search** — covers Homebrew, system packages, nvm-when-shell-loaded, fnm. Misses asdf/Volta/mise if their shim dir isn't on the non-interactive shell PATH (and Rust binaries inherit non-interactive PATH).
2. **`.nvmrc` / `.node-version` / `.tool-versions` / `package.json#engines`** — Nub must parse all of these to respect the user's pin.
3. **Version manager–specific probes:**
   - **nvm**: shell function only, no binary. Read `~/.nvm/alias/default` + `~/.nvm/versions/node/`.
   - **fnm**: `fnm current` / `fnm exec`. Has a Rust binary; cheap to probe.
   - **Volta**: shims in `~/.volta/bin/`. `volta which node` works but adds ~30 ms.
   - **asdf**: shim re-execs; `asdf which node` is ~50 ms.
   - **mise**: `mise which node` is fast (Rust). Has a JSON API.
4. **Validation:** `node -p 'process.versions.node + " " + process.versions.modules'` — costs one full Node startup (~30 ms macOS, ~150 ms Windows). Cache this aggressively by absolute path + mtime.

What can go wrong:
- `.nvmrc` says 18, but the shell hasn't loaded nvm → user expects 18, PATH has 20.
- Volta intercepts the `node` shim and re-routes based on the project's `volta.node` pin — if Nub spawns the shim, Volta runs; if Nub resolves past the shim, it doesn't.
- asdf/mise project-local pins live in `.tool-versions` which is multi-language.
- Multiple `node` in PATH (Homebrew + system + Volta) — order matters.

**Strategy:** Probe in this order: project pin file → version-manager native binary if present → first `node` on PATH. Always run the version+ABI probe once and cache. If the user has both a pin and a different `node` on PATH, warn (don't silently re-route — that breaks the trust contract).

## 4. Cold-start cost of dynamic-linking or `node --eval`

Hypothetical: Nub dynamically loads its Rust addon into a Node process.

- `node --import file:///path/to/nub-hooks.mjs script.js` — adds the [loader-hook customization worker cost][loader-overhead], which benchmarks at **+400 ms** for a no-op hook (synchronous worker IPC). This is the single largest startup tax in Node today.
- `node -r nub-shim.cjs script.js` — `--require` is in-process, ~1 ms per CJS file. Much cheaper than `--import`.
- `process.dlopen` of a Rust `.node` addon — ~2–5 ms incremental, plus whatever the addon does in its init. Has to be invoked from JS; can't be triggered from outside.

**"Warm Node, add Nub extensions" path:** Not possible without a daemon. Every `node` invocation rebuilds the isolate from snapshots. The only way to amortize is a long-lived process holding a warm V8 isolate and accepting work over IPC — a real design space, but orthogonal to "embed vs spawn."

For the additive path: **a `--require` CJS shim, plus a snapshot/preamble trick if ESM hooks are ever needed.** `--import` is too expensive to enable by default.

## 5. What the new permission/SEA/config-file model implies

- **`--permission`** (graduated from `--experimental-permission` in Node 24, per [v24 release notes][node24]): purely opt-in inside the runtime. Doesn't constrain what an outside process can do, but also doesn't give Nub new outside-the-runtime levers.
- **`--experimental-policy`**: **deprecated, will be removed.** Don't build on it.
- **SEA** ([`single-executable-applications.md`][sea-doc]): injects a blob into a `node` copy, conceptually what Nub-bundled-Node already does. Nub could ship as a SEA-prepped Node with a Rust frontend, but it adds nothing, and SEA still supports only a single CJS entrypoint.
- **`--experimental-config-file` / `node.config.json`**: Node reads a config file that can set permission flags. Nub's own config can carry `node.config.json` content and forward it, but the file gates nothing externally.

None of these change the embedding picture — they are all configured from inside a Node process Nub has already launched.

## 6. Prior art: anyone wrapping user-installed Node?

After searching: **no.** Everyone who embeds `libnode` in production **ships their own copy**:
- **Electron** — ships `libnode` (built against Chromium's V8, not bundled V8) per [Electron Internals][electron-node].
- **libelectron** ([ccifra/libelectron][libelectron]) — same, but for hosting in existing apps.
- **alshdavid/edon** + **libnode_sys** — ships prebuilt patched static binaries.
- **NW.js**, **Tauri's optional Node bridge** — ship their own.

Tools that wrap *user* Node (nvm, fnm, Volta, mise, ts-node, tsx, npm, pnpm, Yarn, turbo, nx, vite CLI) **all spawn it as a subprocess**. No production tool `dlopen`s the user's installed Node — ABI instability and the absence of `libnode` on most systems make it a non-starter.

## Recommendation

Five decisions: the bundled-Node default stays, one opt-in mode is added, dynamic linking is closed off, and the shim mechanism is pinned to `--require`.

1. **Keep bundling and spawning Node by default.** It's the only path that gives Nub the compatibility-trust contract.
2. **Add an opt-in "use system Node" mode** (`nub --use-system-node` or `nub.config.json` setting) that spawns the user's resolved `node` via PATH + version-manager probe + pin-file resolution. Cache the probe. Honor Volta/asdf/mise shims.
3. **Do not link `libnode` dynamically.** Revisit only if (a) Node ships a stable C FFI and (b) official tarballs include `libnode`. Both are speculative; neither is on a roadmap.
4. **Use `--require` (CJS) and a snapshot-baked preamble for shims**, not `--import`, until the loader-hook worker cost is fixed upstream ([nodejs#51661][loader-discussion]).
5. **Daemon mode** (warm isolate, IPC) is the only real "skip Node startup" path. Track it separately.

---

## Sources

Node's own embedding and shared-library docs, the libnode tracking issues and PRs, the spawn-cost benchmarks, and the projects that ship their own `libnode`.

- [Node.js embedding docs (v26.1.0)](https://nodejs.org/api/embedding.html)
- [maintaining-shared-library-support.md][shlib-doc]
- [nodejs/node#52289 — Please make Node.js embeddable][libnode-req]
- [nodejs/node#57846 — libnode C FFI entrypoint PR][cffi-pr]
- [napi-rs#2869 — libnode support][napi-libnode]
- [alshdavid/libnode_sys][libnode-sys]
- [alshdavid/edon][edon]
- [Electron Internals: Using Node as a Library][electron-node]
- [Homebrew node libnode split, nexu-io/open-design#1275][brew-split]
- [Benchmarking OS primitives — bitsnbites][osprim]
- [val.town: Why is spawning a new process in Node so slow?][val-spawn]
- [Node 24 release notes — --permission stable][node24]
- [Single executable applications][sea-doc]
- [Loader-hook performance discussion (nodejs#51661)][loader-discussion]
- [ESM loader hooks startup overhead (Medium)][loader-overhead]
- [ccifra/libelectron][libelectron]

[shlib-doc]: https://github.com/nodejs/node/blob/main/doc/contributing/maintaining/maintaining-shared-library-support.md [libnode-req]: https://github.com/nodejs/node/issues/52289 [cffi-pr]: https://github.com/nodejs/node/pull/57846 [napi-libnode]: https://github.com/napi-rs/napi-rs/issues/2869 [libnode-sys]: https://github.com/alshdavid/libnode_sys [edon]: https://crates.io/crates/edon [electron-node]: https://www.electronjs.org/blog/electron-internals-using-node-as-a-library [brew-split]: https://github.com/nexu-io/open-design/issues/1275 [osprim]: https://www.bitsnbites.eu/benchmarking-os-primitives/ [val-spawn]: https://blog.val.town/blog/node-spawn-performance/ [node24]: https://nodejs.org/en/blog/release/v24.0.0 [sea-doc]: https://nodejs.org/api/single-executable-applications.html [loader-discussion]: https://github.com/orgs/nodejs/discussions/51661 [loader-overhead]: https://medium.com/@Quaxel/esm-loader-hooks-can-quietly-wreck-startup-b6fa96be8629 [libelectron]: https://github.com/ccifra/libelectron [embed-doc]: https://nodejs.org/api/embedding.html

## Changelog

One entry, recording this doc's move out of the internal research corpus.

- 2026-07-30 — Migrated from the internal research corpus. Internal planning links, private attributions and reference-checkout paths were rewritten; findings and measured values are unchanged.
