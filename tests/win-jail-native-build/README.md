# Native addon build inside the build jail — end-to-end probe

Every other build-jail probe in this repo asserts a **policy**: a grant derives, a launch
succeeds, an egress is refused. None of them compiles anything. That gap is exactly how
Windows shipped a total native-build failure while every Windows probe stayed green.

This probe's verdict is an **artifact on disk**: a real `.node` file, non-empty, that a
Node process outside the jail can `require` and get the value the C++ returns. Exit codes
are deliberately not the verdict — a lifecycle script can exit 0 having done nothing, and
this repo has measured that.

## The chain it closes

1. `npm_config_nodedir` is derived from `npm_node_execpath`, which nub pushes on every
   lifecycle spawn.
2. Every POSIX distribution is `<root>/bin/node`, so the root is the grandparent. The
   Windows zip and MSI are flat — `<root>\node.exe` — so the grandparent walks one level
   above the distribution.
3. The Windows distribution ships **no headers and no `node.lib`** at all (verified against
   `node-v22.20.0-win-x64.zip`: zero `include/`, `.h` or `.lib` entries), so no derivation
   of the root can satisfy node-gyp there.
4. node-gyp **skips its own header download whenever `nodedir` is set**
   (`lib/configure.js`, `getNodeDir`), and with `nodedir` unset it downloads — which the
   Windows jail's net deny-all refuses.

So on Windows the compile failed either way. The fix derives the root from the layout and
prefetches the headers plus `node.lib` out of jail; the jail stays net deny-all.

## Running it

```
cargo build -p nub-cli --profile fast
node tests/win-jail-native-build/probe.mjs target/fast/nub
```

The probe builds its own fixture (a plain N-API C addon with a `binding.gyp`) under
`~/.nub-jail-native-probe-<pid>`, installs it, and prints one
`prop:<shape>/<name>=PASS|FAIL` line per property.

It installs that same fixture through **two delivery shapes**, because the shape is not
neutral on Windows:

- `tarball` — `npm pack`ed and consumed as a `file:` tarball, the ordinary
  fetch-and-extract path. **This is the shape the verdict rides.**
- `dir` — consumed as a `file:` *directory* dependency. On Windows this crashes
  `nub install` outright (`thread 'main' has overflowed its stack`, rc `0xC00000FD`),
  identically in the fixed and poisoned arms — so it is a pre-existing defect in another
  area, not something this branch introduced. It is still run and logged so it stays
  visible, but gating on it would only re-measure someone else's bug.

## The four properties

| Property (per shape) | What it establishes |
| --- | --- |
| `jailed-child-ran` | The confined script wrote a marker into its package dir — the one place it may write. It ran. |
| `jail-enforced` | That same script tried to read a canary outside every grant and was refused. It ran *confined*. |
| `addon-artifact` | A real, non-empty `.node` exists. |
| `addon-loads` | An unconfined Node `require`s it and gets the string the C++ returns. |

Both facts are read back from the marker **file** the child wrote, never from a status the
child reported about itself — a sibling lane was burned by trusting a self-reported bit
(`TokenIsElevated` is a stale copied flag that reports `1` on a genuinely de-elevated
token).

## Why the canary is not under the temp dir

The jail grants a private tmp. A canary placed there would be legitimately readable and
`jail-enforced` would fail for a reason that has nothing to do with enforcement. It lives
beside the fixture under the home directory instead.

## The two arms, and what each platform proves

`.github/workflows/win-jail-native-build-probe.yml` runs the identical probe twice: once
against the branch, once against a build with `poison.patch` applied (which restores the
grandparent derivation and disables the header prefetch).

- **Windows** — the differential *is* the finding: fixed must produce the artifact,
  poisoned must not.
- **Linux** — the fix is a deliberate no-op, because the old derivation was already correct
  for `<root>/bin/node`. So **both** arms must produce the artifact. That is the control
  that the diff changed nothing on POSIX, and it independently proves the harness works on
  a platform where the bug never existed.

`jailed-child-ran` and `jail-enforced` must hold in **every** arm on **every** platform, so
a red Windows verdict cannot be a broken probe.

Every workflow step is `if: always()`, because a red gate earlier in the job must never
hide the end-to-end verdicts — that masking happened on a first poisoned attempt in a
sibling lane.

## Regenerating `poison.patch`

The patch is generated from the committed tree, never hand-written:

```
# with the fix committed and the tree clean
<edit node_layout back to the grandparent, and make node_headers return None>
git diff -- crates/nub-cli/src/pm_engine > tests/win-jail-native-build/poison.patch
git checkout -- crates/nub-cli/src/pm_engine
```
