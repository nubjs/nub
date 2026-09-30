# action-smoke

Fixtures for `.github/workflows/action-smoke.yml`, which exercises the GitHub Actions [`setup-node/`](../../setup-node), [`install/`](../../install) and [`npm-ci/`](../../npm-ci) on real runners. The workflow runs on a push to `main` that touches either action, this directory, or itself; on a pull request once the `ci` label asks for a run; and weekly.

- `install/fixture/` — an npm project (`package-lock.json`) with two registry dependencies (one of them, `debug`, with a transitive `ms` that the hoisted layout places at the root), a `file:` dependency and a devDependency, installed by `install` in the drop-in, not-frozen and cache jobs and by `npm-ci` in its drop-in, `--omit=dev`, v1-lockfile and refusal jobs. The jobs write a per-run `run.txt` into it to force a cache miss where one is needed.
- `install/fixture-pnpm/` — the same project pinned to `pnpm@10.15.1` in `packageManager`, with a `pnpm-lock.yaml`, for the job that installs through the shims.

Both lockfiles make the fixture its own project root; the repository's `nub.lock` above them is not read.

Every job installs a RELEASED Nub, the one the `nub-version` input names (`latest` unless a job pins one; `setup-node` resolves it from npm itself, `npm-ci` through `nubjs/setup-nub`), so the smoke verifies the actions' own shape against the engine users get today and cannot gate an engine change that has not shipped yet. That gate is `tests/npm-corpus/`, whose workflow builds Nub from the branch and runs every project in `npm-ci` mode too, and the shim tests under `crates/nub-cli/tests/`.
