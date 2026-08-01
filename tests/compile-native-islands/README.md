# Compiled native package-islands corpus

A real compiled-app fixture for `sharp@0.35.3`. It is a standalone harness: it creates a throwaway npm-compatible project, installs with the supplied Nub candidate, compiles with the supplied matching launcher, removes the installed `node_modules` and hides the source, then invokes only the artifact from a foreign working directory.

## Why sharp alone

sharp is the whole point: its `.node` addon lives in `@img/sharp-<platform>` while the libvips shared
library it loads ships in a SEPARATE `@img/sharp-libvips-<platform>` package, so it exercises the
cross-package island geometry. A second package was dropped: `better-sqlite3` resolves its addon through
the `bindings` package, which walks up from the calling module for a `package.json` — a lookup a bundled
chunk cannot satisfy, and one the compiler documents as unembeddable (use `--external`). It could never
pass here, and `--external` is incompatible with this fixture's source-hiding by construction.

## Invocation

Unix:

```sh
tests/compile-native-islands/run.sh --nub /path/to/nub --launcher /path/to/nub-launcher --target 24.11.1 --mode prebuilt
```

PowerShell:

```powershell
./tests/compile-native-islands/run.ps1 --nub C:\path\to\nub.exe --launcher C:\path\to\nub-launcher.exe --target 24.11.1 --mode prebuilt
```

Pass `--platform darwin-arm64`, `linux-x64`, `linux-x64-musl`, or `win32-x64` when compiling for an explicit target. The supplied candidate and launcher must both be built for that target; cross-produced artifacts are compiled but can only be executed on a matching host. `--keep` retains the temporary workspace and its `native-islands-proof.json`; alternatively set `NUB_NATIVE_ISLAND_WORKSPACE` to a caller-owned empty directory.

`--mode prebuilt` is the only currently supported mode; it makes a missing package prebuild a hard failure. Windows can also pass `--verify-companion-dll-rename`; before the cold and warm runs, the harness renames the outer `.exe`, verifies its hash is unchanged, and records the old/new names plus preserved sharp companion-DLL evidence.

`COMPILE_NATIVE_LOG_DIR=<directory>` copies the successful proof there. On failure it writes a compact command/install/compile/run stdout+stderr transcript there; it never copies `node_modules`.

The launchers also accept `NUB_NATIVE_ISLAND_NUB`, `NUB_NATIVE_ISLAND_LAUNCHER`, `NUB_NATIVE_ISLAND_TARGET`, and `NUB_NATIVE_ISLAND_PLATFORM`. The test-only launcher-template override is passed only to the compiler process, matching the existing compile CI convention.

## Proof contract

`app.cjs` reads metadata from an embedded 1×1 PNG with `sharp`, resizes it to 2×3 raw pixels, and checks the dimensions and output byte signature.

The driver fails rather than skips when the installed package tree lacks any of these native prebuild inputs:

- a `sharp` / `@img/sharp-*` `.node` addon; and
- the target-specific companion shared library: `@img/sharp-libvips-*` on macOS and Linux, and
  `@img/sharp-win32-*` on Windows, where sharp ships the `.dll` inside the platform addon package
  instead of a separate libvips package.

Because Windows keeps the addon and its libraries in one package, only the macOS, Linux, and musl legs
exercise the cross-package island property.

After the cold artifact run, it records the artifact hash and every relevant source native file's path, hash, byte length, and matching extracted-cache paths in `native-islands-proof.json`. Each listed source hash must occur in the artifact's extracted compile cache, so the matrix proves both companion-library presence and byte preservation. It then runs the same artifact warm against that cache and requires the same application proof.

The fixture records no runtime result until the caller runs it. `package.json` pins both top-level dependencies exactly; the harness intentionally starts with ordinary `nub install`, requires that it emit a lock input, and records that lock input hash in the proof file. In `prebuilt` mode it also rejects install output showing a node-gyp fallback.
