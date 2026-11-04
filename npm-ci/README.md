# nubjs/nub/npm-ci

The `npm ci` step, run by [Nub](https://nubjs.com). One line replaces the other:

```diff
       - uses: actions/setup-node@v5
         with:
           node-version: 22
           cache: npm
-      - run: npm ci
+      - uses: nubjs/nub/npm-ci@v0
+        with:
+          lockfile: package-lock.json
       - run: npm test
```

What it does:

- Installs exactly what `package-lock.json` records: every package at the version and from the `resolved` URL in the lockfile, checked against its `integrity` hash. Nothing is resolved against the registry.
- Keeps `npm ci`'s contract: `node_modules` is removed first, every lifecycle script runs as it does under npm, and the install fails when `package-lock.json` and `package.json` disagree (`ERR_NUB_OUTDATED_LOCKFILE`).
- Runs the lifecycle scripts under the job's `node`, the one `setup-node` put on PATH. A `.nvmrc` or `engines` pin in the project is not consulted and no Node is downloaded, as under `npm ci`.
- Never writes the lockfile. The action hashes it before and after the install and fails if a byte changed.
- Leaves everything else alone: `setup-node`, `node`, `npm test`, `npm run` keep running exactly as before. Nub is the install step and nothing more.

## Inputs

| Input | Default | Meaning |
| --- | --- | --- |
| `lockfile` | `package-lock.json` | The npm lockfile in `working-directory` to install from: `package-lock.json` or `npm-shrinkwrap.json`. When both exist, `npm ci` and Nub install from `npm-shrinkwrap.json`, so name that one. Missing, or changed by the install: the action fails. |
| `working-directory` | `.` | Where `package.json` and the lockfile live. |
| `args` | | Flags for `npm ci`, as npm spells them: `--omit=dev`, `--include=optional`, `--no-optional`, `--ignore-scripts`, `--loglevel <level>`. A flag the engine does not honor fails the action. |
| `cache` | `true` | Cache Nub's store across runs, keyed on the lockfile. |
| `cache-key-prefix` | | Scope or bust the cache independently of the lockfile. |
| `nub-version` | `latest` | Any semver range npm understands. |
| `token` | `github.token` | Rate-limit relief when resolving `nub-version`. |

## Outputs

| Output | Meaning |
| --- | --- |
| `nub-version` | The installed Nub version. |
| `cache-hit` | `true` on an exact store-cache hit, `false` on a partial hit, empty on a miss. |
| `lockfile-sha256` | SHA-256 of the lockfile, the same before and after the install. |

## Versioning

`@v0` floats to each stable Nub `v0.x` release; `@v0.x.y` pins the action as of that release; `@<commit>` pins a commit.
