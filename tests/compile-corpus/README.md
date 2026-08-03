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
lives in a sidecar package, and a package that requires a computed path into a sibling.

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

## Adding a fixture

Drop `a-<name>.mjs` in `fixtures/`, add the package to the `npm i` line in `run.sh`, and
print a single deterministic `ok:<something>` line. Avoid anything that varies between runs
or between machines — the control comparison is exact.
