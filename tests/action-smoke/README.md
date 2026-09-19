# action-smoke

Fixtures for `.github/workflows/action-smoke.yml`, which exercises the GitHub Actions [`setup-node/`](../../setup-node) and [`install/`](../../install) on real runners. The workflow runs on a push to `main` that touches either action, this directory, or itself; on a pull request once the `ci` label asks for a run; and weekly.

- `install/fixture/` — an npm project (`package-lock.json`) with one registry dependency and one `file:` dependency, installed by `install` in the drop-in, not-frozen and cache jobs. The jobs write a per-run `run.txt` into it to force a cache miss where one is needed.
- `install/fixture-pnpm/` — the same project pinned to `pnpm@10.15.1` in `packageManager`, with a `pnpm-lock.yaml`, for the job that installs through the shims.

Both lockfiles make the fixture its own project root; the repository's `nub.lock` above them is not read.
