# npm-incumbent corpus

Real projects whose package manager is npm, pinned by commit, frozen-installed under nub from their own `package-lock.json` on a cold store, and then checked against that lockfile. The verdict per project is the contract `npm ci` gives them today: the install completes, lifecycle scripts included, and every dependency each importer declares resolves on disk at the version the lockfile pins. This is the drop-in claim for an npm project, measured on the projects themselves rather than on synthetic fixtures; `tests/conformance/` covers the lockfile round-trip with real npm as the judge, and this corpus covers what real lockfiles contain.

## What it runs

`run.sh <nub-binary> [owner/repo ...]` — with no repos, the whole of `corpus.tsv`. Per project:

1. shallow-clone the pinned commit;
2. `nub install --frozen-lockfile` with a fresh `XDG_DATA_HOME`/`XDG_CACHE_HOME`, so the store is cold and the number is the one a CI runner without a cache step sees;
3. `check-tree.mjs`: starting from the root and every workspace member, follow every dependency edge the lockfile records, resolving each edge in the lockfile by npm's path keys and on disk by the `node_modules` walk Node performs from the dependent's real directory; the two must agree on the version at every edge. The check is layout-agnostic on purpose — npm hoists everything to the root, nub's isolated linker keeps only direct dependencies there, and both satisfy the same edges. Optional dependencies are skipped (a platform mismatch drops them legitimately). A required peer must be present and satisfy the dependent's declared range, and its exact version is reported rather than judged: npm resolves a peer by where hoisting placed the dependent, nub from the dependent's own context, and both meet the package's contract. A workspace link is judged by the member directory's manifest, as `npm ci` installs it, not by the version the lockfile recorded for it.

A red is classified before it counts. `CONTROL=on-failure` (the default) runs `npm ci` on the same clone with a cold npm cache; if npm fails there too, the verdict is `XFAIL-ENV` — the runner or the pin, not nub — and it does not fail the run. `CONTROL=always` runs the control for every project and prints both wall-clock times. `expected-failures.txt` lists the known reds with a reason; a listed project that passes is `XPASS-STALE` and fails the run, so a fix is recorded by deleting the line.

`matrix.mjs` turns `corpus.tsv` into the GitHub Actions matrix `npm-corpus.yml` fans out over, one job per project on the Node version that project's own CI uses.

## Adding a project

One tab-separated line in `corpus.tsv`: `owner/repo`, the full 40-character commit, the Node version its CI runs, and a note on what the project exercises. Pin a commit whose lockfile `npm ci` accepts, and prefer projects that add a lockfile shape the corpus does not have yet — a workspace with peer-dependency importers, a `prepare` hook that builds a native addon, a `packageManager` pin — over another single-package library. Update the pin when the project's lockfile changes shape, not on every upstream commit.

## Running it locally

```sh
tests/npm-corpus/run.sh target/fast/nub mochajs/mocha avajs/ava
CONTROL=always KEEP=1 tests/npm-corpus/run.sh target/fast/nub motdotla/dotenv
```

Each project's logs land in the work dir (`<owner>__<repo>.nub.log`, `.npm.log`, `.clone.log`), which is kept on failure. The full corpus is a network-bound sweep of a few thousand packages per project; run it in CI (`gh workflow run npm-corpus.yml`, optionally with `repos=`) rather than on a development machine.
