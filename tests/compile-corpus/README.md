# Compile corpus

Compiles a set of real npm packages and runs each artifact **with `node_modules` deleted**,
comparing its output against the same program on plain Node.

```sh
NUB=$(scripts/rust-build.sh --print-target)/fast/nub tests/compile-corpus/run.sh
```

Needs `__NUB_LAUNCHER_TEMPLATE` pointing at a built release launcher, and Node >= 24.

## What it is checking

`nub compile` bundles everything it can and ejects only what it cannot — a package that
loads a native addon by computing a path at run time. Two things have to hold at once, and
each hides a different failure:

- **Ejected packages must still work.** They ship unbundled, in their installed layout, and
  are found by ordinary Node resolution. If that breaks, the binary fails on a user's
  machine and nothing said so at build time.
- **Everything else must actually get bundled.** A package needlessly ejected is a silent
  regression: it costs startup and artifact size, and nothing fails, so only a file count
  catches it.

The corpus covers pure JavaScript, classic node-gyp packages, a napi-rs wrapper whose addon
lives in a sidecar package, a package that requires a computed path into a sibling, and one whose
native payload is an executable it spawns rather than an addon it loads — that last one is the
only fixture whose correctness depends on a file's executable bit surviving the payload.

## Reading the output

```
FIXTURE          RESULT OUTPUT                   EJECTED
a-express        PASS   ok:function              -
a-sharp          PASS   ok:true                  @img,detect-libc,semver,sharp
```

The `EJECTED` column is the interesting one. A pure-JavaScript fixture must show `-`: if a
package name appears there, detection has over-fired and that package lost its bundling for
no reason.

## Why every fixture is run on plain Node first

A binary that crashes on startup exits fast and prints nothing, which is indistinguishable
from a fast, correct one unless you compare against a known-good result. So each fixture's
output on plain Node is captured as the control and the artifact must reproduce it exactly.
Checking only the exit code would pass a binary that silently printed the wrong answer.

## Two harnesses

`run.sh` varies **which package** is compiled. `layouts.sh` varies **the shape of the tree** it
sits in — a nested duplicate version, a scoped napi-rs package, a symlinked workspace member:

```sh
NUB=$(scripts/rust-build.sh --print-target)/fast/nub tests/compile-corpus/layouts.sh
```

One of those shapes is an **isolated install** — the tree `nub install` itself produces, where
`node_modules/<pkg>` is a symlink and the real package lives in `.store/` with its dependencies
beside it rather than hoisted. Every other fixture here installs with npm, and npm's flat tree
happens to put a transitive dependency exactly where a naive lookup would find it. That
coincidence hid a defect in which an ejected package shipped with none of its dependencies: the
artifact compiled clean and died at run time on `Cannot find module`. Cover both installers.

That second axis is not redundant. The payload path for an ejected package is derived from where
it sits on disk, so a tree shape can produce a collision no ordinary package would. The nested
case found exactly that: two versions of one package at different depths resolved to the same
payload path, one silently replaced the other, and the artifact still exited 0 printing the right
answer because those two versions happened to be compatible.

That is why each layout inspects the payload rather than only running the artifact. Running it
would have passed.

## Adding a fixture

Drop `a-<name>.mjs` in `fixtures/`, add the package to the `npm i` line in `run.sh`, and
print a single deterministic `ok:<something>` line. Avoid anything that varies between runs
or between machines — the control comparison is exact.
