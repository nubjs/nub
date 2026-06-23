# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.23.0](https://github.com/jdx/aube/compare/aube-resolver-v1.22.0...aube-resolver-v1.23.0) - 2026-06-21

### Other

- refresh benchmarks for v1.22.0 ([#907](https://github.com/jdx/aube/pull/907))

## [1.22.0](https://github.com/jdx/aube/compare/aube-resolver-v1.21.0...aube-resolver-v1.22.0) - 2026-06-17

### Fixed

- *(embedder)* honor the embedder profile in the install banner and cache/name sites ([#888](https://github.com/jdx/aube/pull/888))
- *(install)* close pnpm-lock.yaml parity and re-resolution gaps ([#896](https://github.com/jdx/aube/pull/896))
- *(lockfile)* close pnpm-lock.yaml formatting and field parity gaps ([#893](https://github.com/jdx/aube/pull/893))
- *(install)* repair member installs under sharedWorkspaceLockfile=false ([#891](https://github.com/jdx/aube/pull/891))
- *(lockfile)* only write time: under resolution-mode=time-based ([#892](https://github.com/jdx/aube/pull/892))

### Other

- refresh benchmarks for v1.21.0 ([#890](https://github.com/jdx/aube/pull/890))

## [1.21.0](https://github.com/jdx/aube/compare/aube-resolver-v1.20.0...aube-resolver-v1.21.0) - 2026-06-13

### Added

- *(lockfile)* emit packageExtensionsChecksum and pnpmfileChecksum for pnpm parity ([#883](https://github.com/jdx/aube/pull/883))

### Fixed

- *(install)* map peer-suffixed source deps to their canonical store index ([#885](https://github.com/jdx/aube/pull/885))
- *(packaging)* restore endevco npm scope ([#887](https://github.com/jdx/aube/pull/887))

## [1.20.0](https://github.com/jdx/aube/compare/aube-resolver-v1.19.0...aube-resolver-v1.20.0) - 2026-06-13

### Added

- *(pnpmfile)* run readPackage on importer manifests (full pnpm parity) ([#881](https://github.com/jdx/aube/pull/881))

### Other

- link to all sponsors ([#876](https://github.com/jdx/aube/pull/876))
- refresh benchmarks for v1.19.0 ([#866](https://github.com/jdx/aube/pull/866))

## [1.19.0](https://github.com/jdx/aube/compare/aube-resolver-v1.18.2...aube-resolver-v1.19.0) - 2026-06-11

### Added

- *(runtime)* node version switching and aube self-version management ([#861](https://github.com/jdx/aube/pull/861))

### Other

- refresh benchmarks for v1.18.2 ([#851](https://github.com/jdx/aube/pull/851))

## [1.18.2](https://github.com/jdx/aube/compare/aube-resolver-v1.18.1...aube-resolver-v1.18.2) - 2026-06-08

### Other

- migrate project links to jdx ([#845](https://github.com/jdx/aube/pull/845))

## [1.18.1](https://github.com/jdx/aube/compare/aube-resolver-v1.18.0...aube-resolver-v1.18.1) - 2026-06-07

### Other

- refresh benchmarks for v1.18.0 ([#841](https://github.com/jdx/aube/pull/841))

## [1.18.0](https://github.com/jdx/aube/compare/aube-resolver-v1.17.1...aube-resolver-v1.18.0) - 2026-06-04

### Added

- add sponsors command ([#824](https://github.com/jdx/aube/pull/824))

### Fixed

- *(resolver)* preserve remote tarball integrity ([#825](https://github.com/jdx/aube/pull/825))

### Other

- refresh benchmarks for v1.17.1 ([#820](https://github.com/jdx/aube/pull/820))

## [1.17.1](https://github.com/jdx/aube/compare/aube-resolver-v1.17.0...aube-resolver-v1.17.1) - 2026-05-31

### Other

- *(ci)* switch back to namespace runners ([#819](https://github.com/jdx/aube/pull/819))

## [1.17.0](https://github.com/jdx/aube/compare/aube-resolver-v1.16.1...aube-resolver-v1.17.0) - 2026-05-31

### Added

- *(resolver)* trust staged publishes ([#810](https://github.com/jdx/aube/pull/810))

### Other

- *(resolver)* cover alias peer cycle ([#813](https://github.com/jdx/aube/pull/813))
- *(ci)* switch to github-hosted runners ([#814](https://github.com/jdx/aube/pull/814))
- refresh benchmarks for v1.16.1 ([#808](https://github.com/jdx/aube/pull/808))

## [1.16.1](https://github.com/jdx/aube/compare/aube-resolver-v1.16.0...aube-resolver-v1.16.1) - 2026-05-29

### Other

- refresh benchmarks for v1.16.0 ([#787](https://github.com/jdx/aube/pull/787))

## [1.16.0](https://github.com/jdx/aube/compare/aube-resolver-v1.15.0...aube-resolver-v1.16.0) - 2026-05-25

### Added

- *(pnpm)* catch up with pnpm 11 parity ([#761](https://github.com/jdx/aube/pull/761))

### Fixed

- *(resolver)* pin hosted git tarball integrity ([#783](https://github.com/jdx/aube/pull/783))
- *(resolver)* skip fast fail for optional deps ([#756](https://github.com/jdx/aube/pull/756))

### Other

- refresh benchmarks for v1.15.0 ([#750](https://github.com/jdx/aube/pull/750))
- *(resolver)* hoist BFS state into ResolveDriver, split process_task ([#745](https://github.com/jdx/aube/pull/745))
- *(resolver)* split resolve.rs into submodules ([#738](https://github.com/jdx/aube/pull/738))

## [1.15.0](https://github.com/jdx/aube/compare/aube-resolver-v1.14.1...aube-resolver-v1.15.0) - 2026-05-17

### Added

- *(yarn)* support berry portal and exec protocols ([#729](https://github.com/jdx/aube/pull/729))

### Fixed

- *(yarn)* support esm exec generators ([#736](https://github.com/jdx/aube/pull/736))

### Other

- refresh benchmarks for v1.14.1 ([#721](https://github.com/jdx/aube/pull/721))

## [1.14.0](https://github.com/jdx/aube/compare/aube-resolver-v1.13.1...aube-resolver-v1.14.0) - 2026-05-14

### Other

- refresh benchmarks for v1.13.1 ([#687](https://github.com/jdx/aube/pull/687))

## [1.13.0](https://github.com/jdx/aube/compare/aube-resolver-v1.12.0...aube-resolver-v1.13.0) - 2026-05-13

### Fixed

- *(install)* skip prerelease dist-tag latest in postinstall summary ([#668](https://github.com/jdx/aube/pull/668))

### Other

- *(resolver)* correct build.rs version_cap comment to match measured numbers ([#676](https://github.com/jdx/aube/pull/676))
- *(resolver)* drop primer version_cap from 1000 to 100 ([#674](https://github.com/jdx/aube/pull/674))
- *(resolver)* shrink primer by dropping deterministic tarball URLs and shasum ([#664](https://github.com/jdx/aube/pull/664))
- refresh benchmarks for v1.12.0 ([#625](https://github.com/jdx/aube/pull/625))

## [1.12.0](https://github.com/jdx/aube/compare/aube-resolver-v1.11.0...aube-resolver-v1.12.0) - 2026-05-12

### Fixed

- *(update)* preserve cross-platform optionals and time entries ([#637](https://github.com/jdx/aube/pull/637))

### Other

- *(resolver)* pre-size name_index + trim CLAUDE.md perf wishlists ([#638](https://github.com/jdx/aube/pull/638))
- refresh benchmarks for v1.11.0 ([#622](https://github.com/jdx/aube/pull/622))

## [1.11.0](https://github.com/jdx/aube/compare/aube-resolver-v1.10.4...aube-resolver-v1.11.0) - 2026-05-11

### Added

- *(install)* fill resolving bar against a real denominator ([#611](https://github.com/jdx/aube/pull/611))

### Other

- refresh benchmarks for v1.10.4 ([#600](https://github.com/jdx/aube/pull/600))

## [1.10.1](https://github.com/jdx/aube/compare/aube-resolver-v1.10.0...aube-resolver-v1.10.1) - 2026-05-10

### Other

- refresh benchmarks for v1.10.0 ([#571](https://github.com/jdx/aube/pull/571))
- *(registry)* swap simd-json for sonic-rs on packument hot path ([#569](https://github.com/jdx/aube/pull/569))
- refresh benchmarks for v1.10.0 ([#566](https://github.com/jdx/aube/pull/566))

## [1.10.0](https://github.com/jdx/aube/compare/aube-resolver-v1.9.1...aube-resolver-v1.10.0) - 2026-05-10

### Added

- *(resolver)* propagate peer suffix through non-peer-declaring ancestors ([#563](https://github.com/jdx/aube/pull/563))
- *(diag)* instrument install and add aube diag subcommand ([#547](https://github.com/jdx/aube/pull/547))

### Other

- refresh benchmarks for v1.9.1 ([#555](https://github.com/jdx/aube/pull/555))
- lead hero with auto-install promise over speed ([#557](https://github.com/jdx/aube/pull/557))
- *(install)* adaptive limiter + tarball http1 split ([#548](https://github.com/jdx/aube/pull/548))
- refresh benchmarks for v1.9.1 ([#534](https://github.com/jdx/aube/pull/534))
- refresh benchmarks for v1.9.0 ([#532](https://github.com/jdx/aube/pull/532))

## [1.9.1](https://github.com/jdx/aube/compare/aube-resolver-v1.9.0...aube-resolver-v1.9.1) - 2026-05-06

### Fixed

- *(resolver)* fetch registry on primer range miss ([#531](https://github.com/jdx/aube/pull/531))
- *(ci)* harden primer generation ([#528](https://github.com/jdx/aube/pull/528))

### Other

- refresh benchmarks for v1.9.0 ([#525](https://github.com/jdx/aube/pull/525))
- cold install pipeline overhaul ([#522](https://github.com/jdx/aube/pull/522))

## [1.9.0](https://github.com/jdx/aube/compare/aube-resolver-v1.8.0...aube-resolver-v1.9.0) - 2026-05-05

### Other

- refresh benchmarks for v1.8.0 ([#508](https://github.com/jdx/aube/pull/508))

## [1.8.0](https://github.com/jdx/aube/compare/aube-resolver-v1.7.0...aube-resolver-v1.8.0) - 2026-05-03

### Added

- *(progress)* redesign install progress UI ([#501](https://github.com/jdx/aube/pull/501))
- *(run)* prefer local bins for run and dlx ([#502](https://github.com/jdx/aube/pull/502))
- *(codes)* introduce ERR_AUBE_/WARN_AUBE_ codes, exit codes, dep chains ([#492](https://github.com/jdx/aube/pull/492))

### Fixed

- *(resolver)* prefer closest ancestor for unmet peers over distant matches ([#503](https://github.com/jdx/aube/pull/503))
- *(release)* embed primer in linux tarballs ([#493](https://github.com/jdx/aube/pull/493))
- *(lockfile)* honor bun workspace-scoped direct deps ([#489](https://github.com/jdx/aube/pull/489))

### Other

- refresh benchmarks for v1.7.0 ([#490](https://github.com/jdx/aube/pull/490))

## [1.7.0](https://github.com/jdx/aube/compare/aube-resolver-v1.6.2...aube-resolver-v1.7.0) - 2026-05-03

### Fixed

- *(resolver)* resolve nested link:/file: deps from local parents and overrides ([#470](https://github.com/jdx/aube/pull/470))

### Other

- refresh benchmarks for v1.6.2 ([#474](https://github.com/jdx/aube/pull/474))
- refresh benchmarks for v1.6.2 ([#467](https://github.com/jdx/aube/pull/467))

## [1.6.2](https://github.com/jdx/aube/compare/aube-resolver-v1.6.1...aube-resolver-v1.6.2) - 2026-05-01

### Added

- *(cli)* check engines.{aube,pnpm} and workspace per-project engines ([#458](https://github.com/jdx/aube/pull/458))

## [1.6.1](https://github.com/jdx/aube/compare/aube-resolver-v1.6.0...aube-resolver-v1.6.1) - 2026-05-01

### Fixed

- *(ci)* unblock v1.6.0 release publishing path ([#460](https://github.com/jdx/aube/pull/460))

### Other

- refresh benchmarks for v1.5.2 ([#459](https://github.com/jdx/aube/pull/459))

## [1.6.0](https://github.com/jdx/aube/compare/aube-resolver-v1.5.2...aube-resolver-v1.6.0) - 2026-05-01

### Other

- cache hot-path work across install, resolver, and registry ([#453](https://github.com/jdx/aube/pull/453))
- refresh benchmarks for v1.5.2 ([#452](https://github.com/jdx/aube/pull/452))
- refresh benchmarks for v1.5.2 ([#448](https://github.com/jdx/aube/pull/448))

## [1.5.2](https://github.com/jdx/aube/compare/aube-resolver-v1.5.1...aube-resolver-v1.5.2) - 2026-04-30

### Fixed

- *(resolver)* detect host libc via /proc/self/maps ([#398](https://github.com/jdx/aube/pull/398))
- *(install)* fetch hosted git deps over https, not ssh ([#394](https://github.com/jdx/aube/pull/394))

### Other

- *(resolver)* add bundled metadata primer ([#397](https://github.com/jdx/aube/pull/397))
- thank Namespace for GitHub Actions runner support ([#412](https://github.com/jdx/aube/pull/412))
- *(resolver)* fetch full metadata for age-gated resolves ([#391](https://github.com/jdx/aube/pull/391))
- refresh benchmarks for v1.5.1 ([#392](https://github.com/jdx/aube/pull/392))

## [1.5.0](https://github.com/jdx/aube/compare/aube-resolver-v1.4.0...aube-resolver-v1.5.0) - 2026-04-29

### Fixed

- *(resolver)* require structured trust evidence ([#379](https://github.com/jdx/aube/pull/379))
- *(resolver)* bound resolved package stream ([#377](https://github.com/jdx/aube/pull/377))

## [1.4.0](https://github.com/jdx/aube/compare/aube-resolver-v1.3.0...aube-resolver-v1.4.0) - 2026-04-28

### Added

- *(audit)* support update fix mode ([#363](https://github.com/jdx/aube/pull/363))

### Fixed

- *(resolver)* trust benchmark fixture churn packages ([#370](https://github.com/jdx/aube/pull/370))
- roundup of critical/high audit findings ([#361](https://github.com/jdx/aube/pull/361))
- *(resolver)* exclude provenance churn packages ([#360](https://github.com/jdx/aube/pull/360))
- *(packaging)* include README on published aube crate ([#349](https://github.com/jdx/aube/pull/349))

### Other

- warn about npm install caveats ([#368](https://github.com/jdx/aube/pull/368))

## [1.3.0](https://github.com/jdx/aube/compare/aube-resolver-v1.2.1...aube-resolver-v1.3.0) - 2026-04-27

### Added

- *(security)* enforce trustPolicy by default, add paranoid bundle, security docs ([#333](https://github.com/jdx/aube/pull/333))

### Fixed

- *(resolver)* accept abbreviated git commit SHAs in user specs ([#346](https://github.com/jdx/aube/pull/346))
- *(lockfile)* preserve npm platform optional metadata ([#329](https://github.com/jdx/aube/pull/329))
- bun.lock parity for workspaces, platforms, and locked versions ([#327](https://github.com/jdx/aube/pull/327))

## [1.2.1](https://github.com/jdx/aube/compare/aube-resolver-v1.2.0...aube-resolver-v1.2.1) - 2026-04-26

### Fixed

- *(install)* keep transitive peers out of root modules ([#316](https://github.com/jdx/aube/pull/316))
- pnpm snapshot round-trip + workspace negation patterns ([#312](https://github.com/jdx/aube/pull/312))

### Other

- *(resolver)* avoid full packuments for aged metadata ([#314](https://github.com/jdx/aube/pull/314))

## [1.2.0](https://github.com/jdx/aube/compare/aube-resolver-v1.1.0...aube-resolver-v1.2.0) - 2026-04-25

### Fixed

- lockfile and resolver correctness pass ([#291](https://github.com/jdx/aube/pull/291))

### Security

- cve-class hardening across linker, registry, resolver, install ([#296](https://github.com/jdx/aube/pull/296))

## [1.1.0](https://github.com/jdx/aube/compare/aube-resolver-v1.0.0...aube-resolver-v1.1.0) - 2026-04-24

### Added

- *(resolver)* support pnpm `&path:/<sub>` git dep selector ([#273](https://github.com/jdx/aube/pull/273))

### Fixed

- *(resolver)* wire transitive url/git subdeps into parent snapshot ([#276](https://github.com/jdx/aube/pull/276))

### Other

- *(bun)* preserve top-level + per-entry metadata on roundtrip ([#250](https://github.com/jdx/aube/pull/250))
- dedup pass + registry/store perf wave ([#254](https://github.com/jdx/aube/pull/254))
- shared helpers + migrate hardcoded sites ([#245](https://github.com/jdx/aube/pull/245))

## [1.0.0](https://github.com/jdx/aube/compare/aube-resolver-v1.0.0-beta.12...aube-resolver-v1.0.0) - 2026-04-23

### Other

- split lib.rs into focused modules ([#235](https://github.com/jdx/aube/pull/235))

## [1.0.0-beta.12](https://github.com/jdx/aube/compare/aube-resolver-v1.0.0-beta.11...aube-resolver-v1.0.0-beta.12) - 2026-04-22

### Other

- cross-crate dedup pass ([#208](https://github.com/jdx/aube/pull/208))
- enrich NoMatch error with importer, chain, available versions ([#205](https://github.com/jdx/aube/pull/205))
- treat empty version range as `*` ([#206](https://github.com/jdx/aube/pull/206))
- allow exotic subdeps from local parents ([#201](https://github.com/jdx/aube/pull/201))
- cross-crate correctness and security fixes ([#196](https://github.com/jdx/aube/pull/196))

## [1.0.0-beta.11](https://github.com/jdx/aube/compare/aube-resolver-v1.0.0-beta.10...aube-resolver-v1.0.0-beta.11) - 2026-04-21

### Other

- warm-install speedup ([#177](https://github.com/jdx/aube/pull/177))

## [1.0.0-beta.10](https://github.com/jdx/aube/compare/aube-resolver-v1.0.0-beta.9...aube-resolver-v1.0.0-beta.10) - 2026-04-21

### Fixed

- pnpm-workspace.yaml overrides/patches, npm: alias overrides, cross-platform pnpm-lock ([#175](https://github.com/jdx/aube/pull/175))

### Other

- avoid sorting packument versions during picks ([#176](https://github.com/jdx/aube/pull/176))
- scope deprecation warnings + add `aube deprecations` ([#170](https://github.com/jdx/aube/pull/170))
- collapse install bool bags into enums, FxHashMap in resolver ([#165](https://github.com/jdx/aube/pull/165))

## [1.0.0-beta.9](https://github.com/jdx/aube/compare/aube-resolver-v1.0.0-beta.8...aube-resolver-v1.0.0-beta.9) - 2026-04-20

### Other

- silence peer-dep mismatches by default (bun parity) ([#158](https://github.com/jdx/aube/pull/158))

## [1.0.0-beta.7](https://github.com/jdx/aube/compare/aube-resolver-v1.0.0-beta.6...aube-resolver-v1.0.0-beta.7) - 2026-04-19

### Other

- pnpm compat: multi-document lockfile + override over npm-alias ([#116](https://github.com/jdx/aube/pull/116))
- link bare-semver deps to workspace packages (yarn/npm/bun style) ([#118](https://github.com/jdx/aube/pull/118))
- byte-identical pnpm-lock.yaml / bun.lock on re-emit ([#107](https://github.com/jdx/aube/pull/107))
- classify bare http(s) URLs as tarballs ([#114](https://github.com/jdx/aube/pull/114))

## [1.0.0-beta.6](https://github.com/jdx/aube/compare/aube-resolver-v1.0.0-beta.5...aube-resolver-v1.0.0-beta.6) - 2026-04-19

### Other

- dedupe root deps declared in multiple sections ([#102](https://github.com/jdx/aube/pull/102))
- widen aube-lock.yaml to every common platform ([#94](https://github.com/jdx/aube/pull/94))
- honor pnpm overrides "-" removal marker ([#98](https://github.com/jdx/aube/pull/98))
- extract peer-context pass into its own module ([#91](https://github.com/jdx/aube/pull/91))
- resolve catalog: indirection on override targets ([#78](https://github.com/jdx/aube/pull/78))

## [1.0.0-beta.3](https://github.com/jdx/aube/compare/aube-resolver-v1.0.0-beta.2...aube-resolver-v1.0.0-beta.3) - 2026-04-19

### Added

- *(cli)* support jsr: specifier protocol ([#19](https://github.com/jdx/aube/pull/19))

### Other

- discover from workspace root + package.json sources ([#44](https://github.com/jdx/aube/pull/44))
- preserve npm-alias as folder name on fresh resolve ([#37](https://github.com/jdx/aube/pull/37))
- *(npm)* resolve peer deps when installing from package-lock.json ([#35](https://github.com/jdx/aube/pull/35))
- *(npm)* support npm:<real>@<ver> aliases + fix dep_path tail ([#30](https://github.com/jdx/aube/pull/30))
- Parse pnpm snapshot optional dependencies ([#18](https://github.com/jdx/aube/pull/18))

## [1.0.0-beta.2](https://github.com/jdx/aube/compare/aube-resolver-v1.0.0-beta.1...aube-resolver-v1.0.0-beta.2) - 2026-04-18

### Other

- aube-cli crate -> aube ([#7](https://github.com/jdx/aube/pull/7))
