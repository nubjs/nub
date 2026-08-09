# busybox-run-probe — the integrated `nub run` busybox shell, on real Windows

A throwaway, branch-scoped CI probe (see `.claude/skills/ci-adhoc-test`). No PR is
needed: pushing to the `busybox-shell` branch runs
`.github/workflows/busybox-run-probe.yml` on a real `windows-latest` runner.

## What it validates

`nub run` script bodies run through a bundled busybox-w32 POSIX `sh` on Windows.
This probe builds a real `nub.exe`, stages `busybox.exe` next to it exactly as the
win32 npm package lays it out, and drives the actual `nub run` — so it exercises
the production sidecar resolution (`resolve_bundled_busybox`, no
`__NUB_BUSYBOX_EXE` override), the `/tmp` + `TMPDIR` glue, and the shell selection
end-to-end, not a standalone shell.

`tests/win-shell-probe/` answered the earlier, different question — which candidate
shell to bundle — by driving each shell standalone. This probe is the acceptance
test for the choice that won (busybox), through the integrated CLI.

### Both shell layouts, plus a negative control

The binary resolves its shell relative to itself, and there are two places that
shell legitimately lives. The full case matrix runs against each:

| layout | shape | who produces it |
| --- | --- | --- |
| `sidecar` | `busybox.exe` beside `nub.exe` | the win32 npm package, the release `.zip` behind `install.ps1`, `nub upgrade`'s `~/.nub/bin` |
| `relocated` | `nub-sh/busybox.exe`, **no** sibling | the npm launcher, after hardlinking `nub.exe` into npm's global bin dir for the PATHEXT fast path (`healWindowsBinDir`) |

The `relocated` layout is [#687](https://github.com/nubjs/nub/issues/687): 0.7.0
started relocating the binary and the resolution could not follow, so every
`nub run` on a Windows npm install failed from the second invocation onward. The
sidecar-only probe passed throughout — it never built the shape that breaks.

`no_busybox_anywhere_fails_cleanly` is the negative control, and it is what makes
the other two rows mean anything: a `nub.exe` with neither candidate present must
fail with the shell-resolution error. Without it, a binary that found a shell by
some unintended third route would show all-green while the resolution under test
did nothing.

Cases (`node run-probe.mjs`), each `nub run <script>`, run once per layout:

| case | body | asserts |
| --- | --- | --- |
| `shim_both` | `mytool --flag value` | `node_modules/.bin` triplet shim resolves |
| `shim_only_cmd` | `onlycmd --flag value` | PATHEXT-style `.cmd`-only resolution |
| `posix_rm_mkdir` | `rm -rf dist && mkdir dist && echo made` | busybox coreutils applets |
| `posix_env_prefix` | `NODE_ENV=production node -e …` | inline env prefix |
| `posix_braced_default` | `echo mem=${NODE_MEM:-4096}` | braced parameter expansion |
| `posix_cmd_subst` | `echo sub=$(node -e …)` | command substitution |
| `posix_for_loop` | `for i in a b c; do …; done` | control flow |
| `posix_tmp_redirect` | `echo hi > /tmp/x && cat /tmp/x` | the `/tmp` glue |
| `default_shell_is_busybox` | `echo ${BASH_VERSION:-none}` | default is busybox (`none`), not a system bash |
| `script_shell_override_reaches_bash` | same, `--script-shell <git-bash>` | the override bypasses busybox to a native shell |

## Reproducing locally

The runner is cross-platform; against a POSIX `/bin/sh`-backed `nub` it validates
the case bodies themselves (the two Windows shim rows fail on Unix, as expected):

```sh
node tests/busybox-run-probe/run-probe.mjs "$(command -v nub)" /tmp/bbrp
```

On Windows the workflow builds `nub`, copies `vendor/busybox-w32/busybox64.exe` to
`busybox.exe` beside it, and runs the probe against that binary. Staging that
sidecar is a precondition — the probe derives the `relocated` layout's copy from it
and exits 2 with a setup error if it is missing.
