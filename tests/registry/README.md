# Offline test registry

A [Verdaccio](https://verdaccio.org) registry served from the committed `storage/` directory, with no uplink. Integration tests and the per-PR conformance fixtures resolve from it instead of registry.npmjs.org, so they run without network access and see the same packuments on every run.

## Run the tests against it

```sh
source tests/registry/start.bash      # sets NUB_TEST_REGISTRY once the registry serves
cargo test -p nub-cli --test package_extensions --test install_engine --test info_engine --test pm_verbs
stop_registry
```

Tests that use it self-skip when `NUB_TEST_REGISTRY` is unset. Network tests whose packages are not seeded stay `#[ignore]` and still talk to registry.npmjs.org — run those with `-- --ignored` and `NUB_TEST_REGISTRY` unset.

The conformance tight set uses the same variable: `NUB_TEST_REGISTRY=http://localhost:4874 tests/conformance/run.sh <nub> <fixtures…>`.

## Add a package

Every seeded version is checked against Socket's malware API first, so seeding needs `SOCKET_API_KEY`.

```sh
cd tests/registry && npm ci
node seed.mts $(node resolve-fixtures.mts <fixture>…)   # or: node seed.mts <name>@<version>…
node scan.mts                                            # re-check everything in storage/
```

Commit the new files under `storage/`. Seeding keeps each version's original publish time, so release-age gates behave as they would against npm. A version whose `os`/`cpu` exclude every host the suite runs on (linux-x64, linux-arm64, darwin-arm64) is seeded as a packument without its tarball: the package managers only read its metadata to record a skipped optional dependency.

`npm test` here runs the scripts' own unit tests.
