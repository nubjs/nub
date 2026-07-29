# Lifecycle scripts under the build jail on Windows — end-to-end probe

The sibling `win-jail-native-build` probe asks whether a native addon can **compile**
inside the jail. This one asks the question that sits in front of it: whether a dependency
lifecycle script can **start** at all.

The answer on Windows was no, for every script, because of a double encoding.

## The defect

Windows has no argv. `CreateProcessW` takes one command-line string and each program
splits it however it likes. Rust's encoder targets the `CommandLineToArgvW` rules, which
`cmd.exe` does not implement — it reads a `\"` escape as two literal characters. So aube
builds the `cmd.exe` line itself with `CommandExt::raw_arg`
(`vendor/aube/crates/aube-scripts/src/lib.rs`, `spawn_shell_with_settings`), handing over
`/d /s /c "`, the script, and `"` as raw pieces.

A **jailed** spawn never goes through `spawn_shell`. nub rebuilds the command as a
`CommandSpec` and the Windows launcher runs it through its own `CreateProcessW` encoder,
which re-quotes those pieces. cmd.exe then receives `\""` as its first token:

```
'\""' is not recognized as an internal or external command
```

Two facts made this larger than it looks. The pieces that get corrupted are the `/d /s /c "`
**prefix**, not the script — so the failure is total and has nothing to do with whether a
particular script contains quotes. And it sits **upstream** of the header/toolchain chain
the sibling probe was written for, which is why that probe could not produce its
artifact-on-disk verdict on Windows.

## The fix

A verbatim command line is carried end to end instead of re-encoded:

| Hop | Before | After |
| --- | --- | --- |
| aube → embedder | `LifecycleSandboxSpawn.args: Vec<OsString>` (`Command::get_args` erases `raw_arg`'s marker) | `LifecycleSpawnArgs::{Argv, WindowsVerbatim}` |
| nub → sandbox | `CommandSpec::args(...)` | `CommandArgs::{Argv, Verbatim}`, set via `verbatim_command_line` |
| sandbox → OS | `build_command_line` quotes every token | a `Verbatim` tail is copied through untouched |

It is **not** a general "skip the quoting" hatch. `validate_apply_inputs` refuses a
verbatim tail off Windows, and refuses one whose program is not the Windows command
interpreter — the only program nub launches that parses its own line. Both refusals are
covered by unit tests in `crates/nub-sandbox/src/backend/mod.rs`, which run on any host.

`build_command_line` is shared by both Windows mechanisms, so the dedicated-account
backend (`backend/windows_account/launch.rs`) carried the same defect and is fixed by the
same change. This probe does not exercise it: the build jail's policy is a pure allowlist
with coarse-deny net, which selects the admin-free AppContainer, and the account backend
additionally needs a one-time elevated provisioning step no runner has.

## The three fixtures

Each installs a `file:` tarball dependency whose `install` script writes a marker file, and
each changes exactly one variable:

| Fixture | Jailed | Script | What it establishes |
| --- | --- | --- | --- |
| `jailed-quoted` | yes | `node -e "require('…')('…')"` | The headline. Verbatim the shape aube's own doc comment names as the one that mangles. |
| `jailed-plain` | yes | `node <abs-path> <tag>` | The bug breaks scripts with **no** quoting too, because the corruption is in the prefix. |
| `unjailed-quoted` | no (`dependenciesMeta.<pkg>.sandbox: false`) | same as the first | The both-arms control. |

`unjailed-quoted` is the control that matters. An unquoted script cannot serve as one — it
fails in the poisoned arm as well, for the reason `jailed-plain` exists to demonstrate. The
unjailed fixture instead keeps the whole harness identical (pack, install, approve, script,
marker) and toggles only the jail, so it must pass in **every** arm on **every** platform.
Its canary read is also the positive half of the enforcement differential: jailed must be
`refused:EPERM|EACCES`, unjailed must be `allowed:<n>`.

## The six properties, per fixture

| Property | What it establishes |
| --- | --- |
| `dependency-materialized` | The install actually ran. A run on this epic once reported success with `rc=127` on every arm, having executed nothing. |
| `shell-ran-the-line` | **The gated verdict.** The confined shell parsed its command line and began executing it, evidenced by a marker only a shell builtin wrote — no Node, no interpreter grant, no toolchain. |
| `script-ran` | The marker file exists — read from disk, never from a status the child reported about itself. |
| `tag-round-trip` | The argument reached the script intact, so the line was not merely accepted but parsed correctly. |
| `confinement-as-configured` | The jailed fixtures were refused a canary outside every grant; the unjailed one was allowed it. |
| `cwd-is-package-dir` | The child started where it was told to. Compared against the store realpath behind `node_modules/<pkg>`. |
| `mangled-signature` | cmd.exe's own parse-failure message appears in nub's output. Expected ABSENT with the fix, PRESENT without it on Windows. |

Every path the child touches — the canary, its own entry point — is **baked in as an
absolute literal**. A sibling lane was burned passing a canary path through the
environment: the jail replaces the env axis with a constructed lifecycle env, so the path
arrived `undefined`, `readFileSync(undefined)` threw `ERR_INVALID_ARG_TYPE`, and a naive
"did it throw?" test read that as a refusal. The marker records the error **code** and the
assertion demands the code a real denial produces.

The work directory is pid-keyed and each fixture is packed separately with its own absolute
path embedded, so the three tarballs differ in content as well as in path. That is what
forces a genuine re-run: the store serves a previously-built copy otherwise, and
`rm -rf node_modules` alone does not invalidate it.

## `cwd-is-package-dir` also confirms a sibling's fix

`strip_verbatim_prefix` (`crates/nub-sandbox/src/backend/windows.rs`) stops
`std::fs::canonicalize`'s extended-length `\\?\C:\…` path from reaching the child as its
working directory — cmd.exe refuses one and silently runs in the Windows directory instead.
It had no behavioural coverage on Windows because this blocker stopped every jailed script
before it could report anything. `cwd-is-package-dir` on the two jailed fixtures is that
coverage — pending on Windows until the interpreter defect below clears, since it is read
from a marker only the script's body can write.

## Running it

```
cargo build -p nub-cli --profile fast
node tests/win-jail-cmdline/probe.mjs target/fast/nub
```

`.github/workflows/win-jail-cmdline-probe.yml` runs it twice on `windows-latest` and
`ubuntu-latest` — once against the branch, once with `poison.patch` applied. No pull
request is needed; the workflow is scoped to the branch.

- **Windows** — the differential is the finding: the jailed fixtures' shell must run the
  line with the fix, must not without it, and the poisoned arm must carry cmd.exe's own
  parse-failure message.
- **Linux** — the fix is a deliberate no-op (no cmd.exe; the tail was always `sh -c` argv),
  so **both** arms must run every script in full. That is the control that the diff changed
  nothing on POSIX.

Every workflow step is `if: always()`, because a red gate earlier in the job must never hide
the end-to-end verdicts.

## What clearing this blocker exposed

Run `30440103670` is the measurement that settled it, and the two arms differ exactly where
they should:

| Fixture | Poisoned | Fixed |
| --- | --- | --- |
| `jailed-quoted` | `The system cannot find the path specified.` — cmd.exe could not parse the line | the line ran; `node` was reached |
| `jailed-plain` | `'\" node …\""' is not recognized as an internal or external command` | the line ran; `node` was reached |
| `unjailed-quoted` | full pass | full pass |

So the command line is fixed. What the fixed arm hits **next** is a different defect, one
this branch uncovered by clearing the thing in front of it:

```
Error: failed to detect Node version:
  "C:\hostedtoolcache\windows\node\22.23.1\x64\node.exe": Access is denied. (os error 5)
```

The confined shell reached `node`, and nub's own version detection was then refused execute
on the toolchain interpreter under the AppContainer. That is why `script-ran` is **not**
gated in the Windows fixed arm — gating on it would re-measure someone else's bug — and why
the gated property is the shell's rather than the script's. Both are printed as diagnostics
so neither is designed around.

The same run makes the sibling `strip_verbatim_prefix` fix verifiable at last: with the
command line clear, cmd.exe accepts the working directory it is handed and no
`UNC paths are not supported` message appears in either arm. `cwd-is-package-dir` records
it on POSIX today and will on Windows once the interpreter defect above clears.

## Regenerating `poison.patch`

The patch is generated from the committed tree, never hand-written. It reverts **one**
function so that the verbatim plumbing stays compiled in both arms and the only variable is
whether aube reports the encoded line at all:

```
# with the fix committed and the tree clean
<edit verbatim_tail's #[cfg(windows)] body to return None>
git diff -- vendor/aube/crates/aube-scripts/src/lib.rs > tests/win-jail-cmdline/poison.patch
git checkout -- vendor/aube/crates/aube-scripts/src/lib.rs
```

Reverting there rather than in nub reproduces the pre-fix behaviour exactly, through the
real code path: `lifecycle_sandbox_spawn` falls back to `Command::get_args`, which yields
the three `raw_arg` pieces with their rawness erased — which is the erasure the whole defect
came from.

## A separate, still-open Windows defect this probe works around

`nub install` exits `0xC00000FD` (`STATUS_STACK_OVERFLOW`, `thread 'main' has overflowed
its stack`) on Windows under the unoptimized `fast` profile — a debug-build artifact of
Windows's 1MB main-thread stack against Linux's 8MB. The workflow links the probe binary
with `-C link-arg=/STACK:8388608`, as the sibling lane did. That is a harness
accommodation, not a product change; the product defect is open and unowned.
