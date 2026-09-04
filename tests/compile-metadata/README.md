# `nub compile --metadata` — Windows executable metadata

What Explorer's Details tab shows for a compiled binary, and what `(Get-Item app.exe).VersionInfo` reads. The fields come from the nearest `package.json` by default; `--metadata Key=value` overrides one and `Key=` drops one.

## What this covers that the unit tests cannot

The encoder has its own round-trip tests, and `crates/nub-cli/src/compile/inject.rs` proves the resource stays findable beside an icon. Neither runs the CLI. Three layers only meet in a real build — the flag, the manifest defaults, and the PE that libsui finally writes — so this drives `nub compile` and reads the finished file back.

`verify_artifact` already refuses a compile whose version resource is unreachable, so a zero exit proves the resource is *there*. What is left, and what this asserts, is that it carries the right **values**.

Read back by `read-versioninfo.mjs`, which shares no code with nub. nub's own reader runs inside `verify_artifact` on every compile, so using it here would only prove it agrees with itself.

## The three arms

| | |
| --- | --- |
| 1 | The manifest supplies `ProductName`, `CompanyName`, `FileDescription` and the version. `--out` supplies `OriginalFilename`. A prerelease tag survives in the `FileVersion` string and is truncated only in the four-`u16` block, which has nowhere to put it. |
| 2 | `--metadata` replaces one field and `Key=` drops another. |
| 3 | **Negative control.** An empty manifest earns no resource at all. Without it the first two arms prove nothing: a Windows binary could carry a version resource for reasons of its own and every assertion would still pass. |

## Running it

CI runs it on the `win32-x64` and `win32-arm64` legs of `compile-native.yml`, which already build the launcher this needs. Locally on Windows:

```sh
NUB=target/release/nub.exe \
  __NUB_LAUNCHER_TEMPLATE=crates/nub-launcher/target/release/nub-launcher.exe \
  tests/compile-metadata/run.sh 26
```

The resource is written by byte-editing rather than by calling Windows, so the harness also runs from macOS or Linux with `COMPILE_PLATFORM=win32-x64`. That path cross-compiles and cannot execute the artifact, which this harness never does anyway.

It builds with `--smol` so no Node is downloaded: the version resource lives in the launcher's PE either way, and the whole run takes a couple of seconds.
