# Windows papercut survey

This directory is the working system for manually surveying nub's behaviour on Windows. It complements the `windows-latest` CI leg — CI catches regressions; this survey is run on a clean VM to surface first-run papercuts, install UX issues, and OS-specific edge cases that CI runners (which carry Git-Bash, pre-seeded caches, and elevated PATH) can mask.

## VM model

**Windows 11 ARM64.** A 64-bit ARM VM running Windows natively:

- `arm64` nub binary runs at native speed.
- x64 nub binary runs under Windows-on-ARM x64 emulation — slower but functional; note which arch under test.
- **True x64-native behaviour (cmd.exe quirks, WOW64 paths, PE format on an Intel host) requires the `windows-latest` GitHub Actions leg**, not this VM. Do not claim x64-native results from an ARM host.

Docker is available on the host (macOS/Linux) for Linux-specific tests; it is NOT a substitute for Windows behaviour.

## Prerequisites on the VM

1. **OpenSSH Server** — enable in Settings → Optional Features → OpenSSH Server, then `Start-Service sshd; Set-Service -StartupType Automatic sshd`.
2. **Node.js** (≥ 18.19.0) — install via <https://nodejs.org/en/download> or `winget install OpenJS.NodeJS.LTS`. Confirm: `node --version`.
3. **npm** in PATH — ships with Node; confirm `npm --version`.
4. **nub** — the subject under test:
   ```powershell
   npm install -g @nubjs/nub
   nub --version
   ```
5. **Git for Windows** *(optional)* — needed for the `run-script-shell` check (`sh.exe`). Install from <https://gitforwindows.org/>; the check auto-skips if no `sh.exe` is found.

## Snapshot discipline

Revert to a clean snapshot **before each full run** so the results reflect a cold machine:

- no `~/.cache/nub` from a prior run
- no leftover `node_modules` in the fixture directories
- no stale `~/.nub/shims`

Vagrant, Parallels, and Hyper-V all expose snapshot/restore via CLI; use the mechanism for your hypervisor. Tag the snapshot with the nub version under test.

## Running the survey

```powershell
# On the Windows VM (PowerShell 5.1+)
cd <repo>\tests\windows

.\papercut-survey.ps1
```

Options:

```powershell
# Test a specific binary (e.g. a locally-built debug build)
.\papercut-survey.ps1 -NubBin C:\path\to\nub.exe

# Custom work dir + output JSON
.\papercut-survey.ps1 -WorkDir C:\tmp\run1 -OutputJson C:\tmp\run1\out.json

# Longer timeout for slow networks (default 60s per check)
.\papercut-survey.ps1 -Timeout 120
```

The script prints a colour-coded console report and writes a `results.json` in the work directory. Exit code 0 = no blockers; exit code 1 = at least one blocker failed.

## Checks and what they guard

| id | label | severity | Windows risk |
|---|---|---|---|
| `install-version` | `nub --version` on PATH | blocker | exe resolution, PATH fixup by postinstall.js |
| `install-nubx-path` | `nubx --version` on PATH | blocker | second bin entry |
| `install-which-nub` | binary path sanity | minor | install location |
| `install-bin-arch` | ARM64 vs x64 binary | minor | arm64 / emulated x64 note |
| `file-js` | plain JS file runner | blocker | basic spawn |
| `file-ts` | TypeScript just-works | blocker | TS transpile on Windows paths |
| `file-stdin` | stdin execution (`nub -`) | major | stdin plumbing |
| `run-greet` | `nub run` plain script | blocker | script dispatch |
| `run-posix-ism` | `FOO=val node` via cmd.exe | major | cmd.exe cannot inline-assign env; expected degradation |
| `run-script-shell` | `--script-shell <sh>` with `sh.exe` | minor | Git-for-Windows sh.exe path (harness locates sh, passes it explicitly) |
| `run-install-fixture` | `nub install` for subsequent checks | blocker | PM install on Windows |
| `run-cmd-bin` | `.cmd` shim via `nub exec` | major | `cmd /C` dispatch for `.cmd`/`.bat` bins |
| `nubx-cowsay` | DLX fetch-and-run | major | network + temp extraction on Windows |
| `pm-native-install` | native dep (esbuild) postinstall | major | lifecycle scripts + binary download |
| `pm-native-bin` | esbuild binary after install | major | postinstall output is executable |
| `pm-add` | `nub add` dep | blocker | PM add on Windows |
| `pm-ci` | `nub ci` frozen install | blocker | CI install path |
| `pm-remove` | `nub remove` dep | major | PM remove |
| `node-ls` | `nub node ls` | minor | cache dir listing |
| `node-install` | provision Node 22 from nodejs.org | major | ARM64 tarball download + extract |
| `node-pin` | `nub node pin` writes `.node-version` | minor | file write |
| `node-uninstall` | remove from cache | minor | dir removal |
| `upgrade-dry-run` | `nub upgrade --dry-run` | minor | channel detection; self-owned unsupported on Windows |
| `watch-restart` | file watcher restarts on touch | major | NTFS change notification via Node `--watch` |
| `workspace-install` | install in workspace root | blocker | workspace discovery |
| `workspace-run-recursive` | `nub run -r build` | major | recursive dispatch |
| `workspace-filter` | `nub run --filter alpha build` | major | filter selector |
| `shim-detection` | `nub pm shim --help` | minor | shim subcommand reachable |

## Known Windows-specific risks (from cli.rs)

- **`.cmd`/`.bat` bin shims** — `launch_bin` in `cli.rs` dispatches via `cmd /C <path>` for `.cmd`/`.bat` extensions; any regression here silently breaks every npm bin on Windows (most bins land as `.cmd` shims). Check `run-cmd-bin`.
- **`.ps1` bin shims** — dispatched via `powershell -NoProfile -ExecutionPolicy Bypass -File <path>`. Execution policy on the VM may block this even with `-Bypass`; if so that is a papercut worth reporting.
- **`nub run` POSIX-ism scripts** — default shell on Windows is `cmd.exe`, which does not support `FOO=1 node …` inline env syntax. Passing `--script-shell <path-to-sh>` routes the script body through that shell (e.g. a Git-for-Windows / WSL `sh.exe`); the `run-script-shell` check locates an `sh.exe` and hands it over explicitly. Under the default `cmd.exe` the POSIX-ism script fails — the degraded-but-not-crashed posture is what `run-posix-ism` checks.
- **`nub upgrade` self-owned channel** — explicitly documented as unsupported on Windows (the `~/.nub` tarball self-replace cannot overwrite a running `.exe`). `upgrade --dry-run` notes this in its output; actual `upgrade` falls back to printing the `npm install -g` command. Not a blocker but worth confirming the message is clear.
- **`nub upgrade` via npm** — `npm_upgrade_command_invocation` uses `cmd /C npm install -g …` on Windows (not `sh -c …`), because `npm.cmd` only resolves through `cmd.exe`. A regression here would surface as "program not found" on plain Windows boxes without Git-Bash.
- **NTFS file watcher** — Node's `--watch` uses `ReadDirectoryChangesW`; on ARM64 VMs under Hyper-V this can be slow to coalesce events. `watch-restart` allows 4 seconds for the restart to appear; adjust `-Timeout` if the VM is slow.
- **Path separators** — nub Rust code uses `std::path` throughout, which normalises to backslashes on Windows; if any path is constructed with hardcoded `/` and passed to an OS API (not a URL), it may silently fail. The file-runner and workspace checks exercise the most path-heavy code.

## What this survey cannot verify

- **True x64-native behaviour** — Windows-on-ARM emulates x64 but is not bit-for-bit identical to an Intel machine. Test on `windows-latest` CI for that.
- **Interactive GUI / UAC prompts** — the survey is fully non-interactive (`-NonInteractive`, no prompt reads). Any flow that requires a UAC elevation or console UI is out of scope here.
- **Windows ARM32** — not a supported target; out of scope.
- **cmd.exe / PowerShell script authors targeting nub** — the survey tests nub's consumption of scripts written for cmd.exe/POSIX, not scripts authored in PowerShell.
