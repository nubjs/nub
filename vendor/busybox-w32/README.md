# vendor/busybox-w32

Prebuilt [busybox-w32](https://github.com/rmyorston/busybox-w32) binaries — a
native-Win32 POSIX shell with no MSYS/Cygwin runtime — bundled as the shell
`nub run` uses to execute `package.json` script bodies on Windows. On
macOS/Linux nub uses the system `/bin/sh`; Windows ships no POSIX shell, so nub
carries this one. See `crates/nub-cli/src/cli.rs` (`build_script_command`,
`resolve_bundled_busybox`) for how it is invoked.

## Upstream

- **Project:** busybox-w32 (`rmyorston/busybox-w32`), the Windows port of BusyBox.
- **Version:** `1.38.0-FRP-6075-g169694ebd` (commit `169694ebd`, 2026-05-06).
- **Source:** https://frippery.org/busybox/ · https://github.com/rmyorston/busybox-w32
- **License:** GPLv2-only (see `LICENSE`). Spawned as a separate process, so it
  does not affect the license of nub's own code (FSF mere-aggregation /
  fork-and-exec). GPLv2 §3 corresponding source is vendored alongside as
  `busybox-w32-FRP-6075-g169694ebd.tgz`.

## Files

| File | Ships as | SHA-256 |
| --- | --- | --- |
| `busybox64.exe` | win32-x64 `bin/busybox.exe` | `07bb1e5b095b00d68a695481f9240879f33c5724b40aa2308f999d54ed78f075` |
| `busybox64a.exe` | win32-arm64 `bin/busybox.exe` | `e67f873d19d58c535cc9f0c4965ffd622e19b7bab87e3da89cb2185fb54464d7` |
| `busybox-w32-FRP-6075-g169694ebd.tgz` | (not shipped) GPLv2 §3 corresponding source | `44401413c86a839deeec3eba088af244a1594f18ff9fd0622811100e4cc2e7b4` |
| `SHA256SUM` | (not shipped) upstream-published checksums | — |
| `LICENSE` | GPLv2 text (from the source tarball) | — |

The binaries were downloaded from `https://frippery.org/files/busybox/` and
verified byte-for-byte against upstream's published `SHA256SUM` before
committing. `busybox64.exe` is machine `0x8664` (x64); `busybox64a.exe` is
machine `0xAA64` (arm64). The release workflow (`.github/workflows/release.yml`)
copies the arch-appropriate binary to each win32 package's `bin/busybox.exe`
next to `nub.exe`.

## Updating

Pick the new `busybox64.exe` / `busybox64a.exe` from
`https://frippery.org/files/busybox/`, re-verify both against that directory's
`SHA256SUM`, replace the binaries + source tarball + `SHA256SUM` here, and update
the version, hashes, and table above. Keep the two published hashes pinned in
`.github/workflows/release.yml`'s assemble step in sync (they are the release-time
integrity check).

A future option is to move these binaries out of the tree into an SRI-pinned
`external-tools.json` entry fetched from a `nubjs` mirror release (the same
supply-chain machinery the `soak` skill governs); this in-tree vendoring keeps v1
self-contained with no new repo.
