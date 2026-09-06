# Framework build-jail checks

Application fixtures exercise dependency installation with the build jail enabled, production builds, and frozen reinstalls. SSR and API fixtures also serve their production output and verify a response containing application data.

## Coverage

The fixtures use exact direct dependency versions. Each run retains its generated lockfiles, installed versions, policy dumps, application-output hashes, and binary and harness hashes. Transitive versions resolve at install time; the retained lockfile records that run's graph.

| Fixture | Application check |
| --- | --- |
| React, Vue, Solid with Vite | Components, application data, state, CSS, production bundles |
| Qwik | Client rendering, signals, optimizer-generated production bundle |
| Next.js | App Router, dynamic route, API route, production SSR, Sharp image operation |
| Nuxt | Root preparation, server data endpoint, production SSR |
| SvelteKit | Root synchronization, server loader, Node adapter, production SSR |
| Astro | Static routes, hydrated React component, Sharp image operation |
| React Router | Framework-mode route loader and production SSR |
| SolidStart | Server rendering and Nitro production server |
| Angular | Standalone component, signal, Angular compiler and production bundle |
| Expo | React Native components and Metro web export |
| Fastify, Hono, Nest | Bundled API server, live JSON response, Sharp image operation |

The samples follow the upstream [Vite templates](https://github.com/vitejs/vite/tree/main/packages/create-vite), [React Router template](https://github.com/remix-run/react-router/tree/main/packages/create-react-router), and [SolidStart fixtures](https://github.com/solidjs/solid-start/tree/main/apps/fixtures), with application data and functional assertions added. Expo's React and React Native versions follow its SDK dependency map. These are maintained test applications, not unmodified generator output.

Expo coverage stops at web export. Device builds, signing, deployment, and browser interaction are not asserted. Qwik coverage is client-side, not Qwik City routing. Framework releases that require newer Node versions are not forced onto Nub's older supported Node tiers.

## Confinement evidence

Every project includes a uniquely named local dependency whose postinstall writes a proof artifact. Its attempts to read a fixture SSH key, write outside the project, and inherit a fixture credential must fail. The same attempts must succeed in a jail-off control. A root `prepare` hook must retain ordinary project access.

The runner records real registry-package lifecycle launches separately from this sentinel. A framework whose dependencies ship prebuilt files may have no registry lifecycle scripts; that is not reported as registry lifecycle coverage. Skipped scripts and explicit jail opt-outs fail the jailed arm.

The fixtures explicitly approve dependency scripts so the test exercises confinement rather than approval selection. Windows catalog compatibility grants remain valid: passing an application does not imply every dependency ran inside an AppContainer. The separate [build-jail contract suite](../build-jail-corpus/contract.mjs) checks approval/configuration precedence and opt-outs.

## Running

Run on a remote builder or a platform CI runner with a freshly built binary and Node 26:

```sh
export NUB_BIN=/absolute/path/to/nub
export SOURCE_REVISION="$(git rev-parse HEAD)"
export FRAMEWORK_REPORT=/absolute/path/to/new-report-directory
node --test tests/build-jail-frameworks/harness.test.mjs
node tests/build-jail-frameworks/run.mjs
```

Positional arguments select fixtures. By default, a failed jailed arm gets a jail-off control; set `FRAMEWORK_CONTROLS=all` to run paired controls for every fixture.

```sh
FRAMEWORK_CONTROLS=all node tests/build-jail-frameworks/run.mjs next nuxt angular
FRAMEWORK_LINKER=global-virtual-store node tests/build-jail-frameworks/run.mjs vite-react sveltekit
```

Each arm uses a fresh home and cache. The default CI layout is project-local; the linker override exercises the global virtual store separately. Reports identify the installed layout. Do not reuse a report directory.

## Results

The runner exits nonzero unless every selected fixture passes. It also fails if the binary changes during the run.

| Verdict | Meaning |
| --- | --- |
| `PASS` | The jailed arm passed, and any requested control passed |
| `JAIL-FAILED` | The jailed arm failed and the jail-off control passed |
| `CONTROL-FAILED` | The jail-off control failed; inspect the fixture and an incumbent-PM control before blaming confinement |
| `UNRESOLVED` | A failed jailed arm has no control |

The report retains logs and small artifacts. Generated applications and caches remain in the host's temporary directory for diagnosis; use an ephemeral runner for full sweeps.
