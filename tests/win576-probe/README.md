# win576-probe — throwaway Windows probe for nubjs/nub#576

Read-only investigation harness. Lives on the `win576probe` branch only; not intended for `main`.

**The bug** ([#576](https://github.com/nubjs/nub/issues/576)): `nub add -D <pkg>` after a successful `nub install` fails with

```
failed to link node_modules
I/O error at \\?\D:\...\node_modules\.store\@types+body-parser@1.19.6\node_modules\@types\connect:
Cannot create a file when that file already exists. (os error 183)
```

Two independent halves, both run by `.github/workflows/win576-probe.yml` on `windows-latest`:

- **`repro.ps1`** — builds an `@types/express` fixture, runs `nub install` twice, dumps the on-disk state of `node_modules/.store` (reparse points, `fsutil reparsepoint query`, the exact failing path), then runs `nub add -D oxlint` under `RUST_LOG=debug`. One invocation per cell; the workflow runs 0.6.0 and `canary` across `D:`/`C:` and GVS on/off, since the report's project-local `.store` path implies either GVS off or a cross-drive fallback.
- **`src/main.rs`** — a standalone Rust probe (junction pinned to `=2.0.0`, the version `Cargo.lock` resolves for `aube-linker`) that prints the real behavior of every filesystem assumption the linker makes: `read_link` error classification for real dirs, the exact target string a junction reports, `junction::create` over an occupied path, `Path::exists()` on dangling junctions / held-open handles / >MAX_PATH paths, and `remove_dir` semantics.

Run the Rust half locally on a Windows box with `cargo run --release` from this directory.
