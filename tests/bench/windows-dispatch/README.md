# Windows dispatch benchmark

Measures what a `nub` call costs from `cmd.exe` through npm's generated `nub.cmd` shim against the real `nub.exe` the launcher hardlinks beside it.

Every `nub` call from `cmd.exe` used to be `cmd.exe → nub.cmd → node → spawn nub.exe`, and the Node boot is most of that. Since 0.7.0 the launcher places a real `nub.exe` in npm's global bin directory on the first call ([`npm/nub/bin/launch.js`](../../../npm/nub/bin/launch.js), `healWindowsBinDir`); `PATHEXT` resolves `.exe` ahead of `.cmd`, so `cmd.exe` reaches the binary directly. PowerShell and sh-family shells keep preferring npm's other shims and see no change.

## Cells

| Cell | Command | What it is |
| --- | --- | --- |
| `version/cmd` | `nub.cmd --version` | the shim path |
| `version/exe` | `nub.exe --version` | the direct path |
| `run/cmd` | `nub.cmd run noop` | a `package.json` script (`exit 0`) through the shim |
| `run/exe` | `nub.exe run noop` | the same through the `.exe` |
| `bare` | `nub --version` | what a user types; the control that `PATHEXT` picks the `.exe` |
| `node` | `node --version` | plain Node's own boot, for scale |

Each cell is timed by hyperfine under its default Windows shell (`cmd.exe`; hyperfine subtracts the shell's own spawn cost). The cells run round-robin, `--runs` runs each, for `--rounds` rounds, so a drift on the host lands on every cell. The script fails if the bare call times closer to the shim than to the `.exe`, or if the shim path is not slower: either means the run measured something nobody experiences.

## Running it

The harness is Windows-only and runs under Git Bash. The workflow [`bench-windows-dispatch.yml`](../../../.github/workflows/bench-windows-dispatch.yml) runs it on `windows-latest` against the published `@nubjs/nub` and uploads the results JSON as the `windows-dispatch-results` artifact. It runs on demand only: `gh workflow run bench-windows-dispatch.yml`, then `gh run download <id> -n windows-dispatch-results -D tests/bench/windows-dispatch/results`, and commit a run worth keeping.

```sh
npm install -g @nubjs/nub
bash tests/bench/windows-dispatch/run.sh            # writes to a temp dir
bash tests/bench/windows-dispatch/run.sh --save     # writes under results/
```

Requires hyperfine, jq, npm, and an npm prefix with no space in its path.

## Results

Saved runs live under [`results/`](results/), one JSON per run: versions, host, method, and every per-run time for each cell alongside its mean, min and median.
