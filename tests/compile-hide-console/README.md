# `nub compile --hide-console` — no console window on Windows

A GUI app, a tray icon or a file-association handler cannot ship with a console window attached to it. The flag gives the executable the GUI subsystem and tells its launcher to spawn Node with `CREATE_NO_WINDOW`.

## What this covers that the unit tests cannot

`crates/nub-cli/src/compile/inject.rs` proves the subsystem byte survives libsui's header rebuild, with a console-subsystem control beside it. That is one byte in a file nothing ran.

The half that follows from it only exists on a real Windows host. Flipping the subsystem makes the launcher a GUI process, so its standard handles are no longer a console's, and it goes on to spawn Node — plus, on a first run, `curl` and a version probe — with console creation suppressed. The artifact still has to run, exit with the program's status, and deliver output through a redirect.

## "No window appeared" is not what this asserts

A headless runner has no desktop to observe, so the absence of a window is not measurable there. What the harness measures instead is whether the launcher **took** the suppressing path, read out of the `__NUB_LAUNCHER_TIMING` trace.

Without that arm the harness would pass identically on a runner that owns a console — where the launcher deliberately suppresses nothing, every other assertion still holds, and the feature was never exercised. That case is reported as INCONCLUSIVE rather than failed: inheriting a console is the correct behavior when the user is watching one, and it is not the harness's business to fail over.

## The four arms

| | |
| --- | --- |
| 1 | `--hide-console` leaves the artifact GUI-subsystem (`Subsystem` = 2). |
| 2 | **Negative control.** Without the flag it stays a console application (`Subsystem` = 3). Otherwise arm 1 would pass against a template that was already GUI-subsystem before nub touched it. |
| 3 | The hidden artifact runs, prints through a redirect, and exits with the program's own status. |
| 4 | The launcher reports that it suppressed its children's consoles — or says plainly that it did not, and why. |

The arm that is missing from that list is the user-visible one, and it is missing on purpose — see below.

The subsystem is read back by `read-subsystem.mjs`, which shares no code with nub. nub's own reader runs inside `verify_artifact` on every compile, so using it here would only prove it agrees with itself.

## The claim itself needs a desktop

`observe-console.ps1` asks the question directly: launch the artifact through ShellExecute — what a double-click in Explorer does — and count the console windows that appear, with a build made without the flag as the control.

It is deliberately **not** wired into CI. A hosted runner has no interactive desktop, so a console window may be un-creatable there and every assertion would pass without meaning anything. The script detects that itself: if the control opens no console either, it reports inconclusive rather than claiming a pass. Run it on a real Windows desktop or the local Windows VM (`windows-vm-test`):

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File observe-console.ps1 `
  -Hidden .\hidden.exe -Shown .\shown.exe
```

## Running it

CI runs it on the `win32-x64` and `win32-arm64` legs of `compile-native.yml`, which already build the launcher this needs. Locally on Windows:

```sh
NUB=target/release/nub.exe \
  __NUB_LAUNCHER_TEMPLATE=crates/nub-launcher/target/release/nub-launcher.exe \
  tests/compile-hide-console/run.sh 26
```

The subsystem is set by byte-editing rather than by calling Windows, so arms 1 and 2 also run from macOS or Linux with `COMPILE_PLATFORM=win32-x64`. That path cross-compiles and cannot execute the artifact, so arms 3 and 4 are skipped there.

It builds with `--smol` so no Node is downloaded: the subsystem lives in the launcher's PE either way.
