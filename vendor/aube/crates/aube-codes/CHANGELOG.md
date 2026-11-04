# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [2.2.8](https://github.com/aubepkg/aube/compare/aube-codes-v2.2.7...aube-codes-v2.2.8) - 2026-09-04

### Other

- update Cargo.lock dependencies

## [2.2.5](https://github.com/aubepkg/aube/compare/aube-codes-v2.2.4...aube-codes-v2.2.5) - 2026-09-03

### Fixed

- *(lockfile)* reject pre-v9 pnpm lockfiles instead of installing nothing ([#1456](https://github.com/aubepkg/aube/pull/1456))

### Other

- move routine workflows to GitHub-hosted runners ([#1469](https://github.com/aubepkg/aube/pull/1469))
- move project to aubepkg and aube.sh ([#1460](https://github.com/aubepkg/aube/pull/1460))
- fix prose and content in docs and cli help ([#1455](https://github.com/aubepkg/aube/pull/1455))
- refresh benchmarks for v2.2.4 ([#1434](https://github.com/aubepkg/aube/pull/1434))

## [2.2.4](https://github.com/jdx/aube/compare/aube-codes-v2.2.3...aube-codes-v2.2.4) - 2026-08-31

### Other

- refresh benchmarks for v2.2.3 ([#1417](https://github.com/jdx/aube/pull/1417))

## [2.2.1](https://github.com/jdx/aube/compare/aube-codes-v2.2.0...aube-codes-v2.2.1) - 2026-08-29

### Other

- refresh benchmarks for v2.2.0 ([#1384](https://github.com/jdx/aube/pull/1384))
- *(sponsors)* replace 37signals with omacom foundation ([#1380](https://github.com/jdx/aube/pull/1380))

## [2.2.0](https://github.com/jdx/aube/compare/aube-codes-v2.1.0...aube-codes-v2.2.0) - 2026-08-25

### Fixed

- *(install)* bundle curated package extensions ([#1369](https://github.com/jdx/aube/pull/1369))

### Other

- refresh benchmarks for v2.1.0 ([#1372](https://github.com/jdx/aube/pull/1372))

## [2.1.0](https://github.com/jdx/aube/compare/aube-codes-v2.0.1...aube-codes-v2.1.0) - 2026-08-23

### Other

- refresh benchmarks for v2.0.1 ([#1350](https://github.com/jdx/aube/pull/1350))

## [2.0.1](https://github.com/jdx/aube/compare/aube-codes-v2.0.0...aube-codes-v2.0.1) - 2026-08-23

### Added

- *(resolver)* [**breaking**] expose lowest-direct resolution mode ([#1345](https://github.com/jdx/aube/pull/1345))
- *(global)* [**breaking**] keep aube's global dirs under its own data root ([#1231](https://github.com/jdx/aube/pull/1231))

## [1.41.0](https://github.com/jdx/aube/compare/aube-codes-v1.40.0...aube-codes-v1.41.0) - 2026-08-16

### Fixed

- *(settings)* validate package extensions ([#1304](https://github.com/jdx/aube/pull/1304))

### Other

- refresh benchmarks for v1.40.0 ([#1290](https://github.com/jdx/aube/pull/1290))

## [1.40.0](https://github.com/jdx/aube/compare/aube-codes-v1.39.0...aube-codes-v1.40.0) - 2026-08-13

### Other

- refresh benchmarks for v1.39.0 ([#1285](https://github.com/jdx/aube/pull/1285))
- Update Star History chart links with sealed tokens

## [1.39.0](https://github.com/jdx/aube/compare/aube-codes-v1.38.1...aube-codes-v1.39.0) - 2026-08-12

### Fixed

- *(store)* prune unused global virtual store entries ([#1273](https://github.com/jdx/aube/pull/1273))

### Other

- refresh benchmarks for v1.38.1 ([#1257](https://github.com/jdx/aube/pull/1257))

## [1.38.1](https://github.com/jdx/aube/compare/aube-codes-v1.38.0...aube-codes-v1.38.1) - 2026-08-10

### Fixed

- *(linker)* protect project root from modules cleanup ([#1246](https://github.com/jdx/aube/pull/1246))

### Other

- refresh benchmarks for v1.38.0 ([#1244](https://github.com/jdx/aube/pull/1244))

## [1.38.0](https://github.com/jdx/aube/compare/aube-codes-v1.37.0...aube-codes-v1.38.0) - 2026-08-07

### Fixed

- *(store)* fail closed on incomplete index scans ([#1237](https://github.com/jdx/aube/pull/1237))
- *(install)* route deprecation warnings through embedder output ([#1236](https://github.com/jdx/aube/pull/1236))
- *(lockfile)* reject unsupported named registry identities ([#1215](https://github.com/jdx/aube/pull/1215))

### Other

- refresh benchmarks for v1.37.0 ([#1211](https://github.com/jdx/aube/pull/1211))
- refresh benchmarks for v1.37.0 ([#1206](https://github.com/jdx/aube/pull/1206))

## [1.37.0](https://github.com/jdx/aube/compare/aube-codes-v1.36.0...aube-codes-v1.37.0) - 2026-07-31

### Added

- *(scripts)* use pnpm trusted dependency list ([#1199](https://github.com/jdx/aube/pull/1199))

### Fixed

- *(patch)* preserve existing patches and refresh hashes ([#1196](https://github.com/jdx/aube/pull/1196))
- *(update)* warn when release age hides upgrades ([#1193](https://github.com/jdx/aube/pull/1193))

### Other

- refresh benchmarks for v1.36.0 ([#1185](https://github.com/jdx/aube/pull/1185))

## [1.36.0](https://github.com/jdx/aube/compare/aube-codes-v1.35.0...aube-codes-v1.36.0) - 2026-07-29

### Other

- refresh benchmarks for v1.35.0 ([#1172](https://github.com/jdx/aube/pull/1172))

## [1.35.0](https://github.com/jdx/aube/compare/aube-codes-v1.34.0...aube-codes-v1.35.0) - 2026-07-28

### Added

- *(add)* block new and lookalike package names ([#1157](https://github.com/jdx/aube/pull/1157))
- *(settings)* let cacheDir relocate the global virtual store ([#1146](https://github.com/jdx/aube/pull/1146))

### Other

- refresh benchmarks for v1.34.0 ([#1124](https://github.com/jdx/aube/pull/1124))

## [1.33.0](https://github.com/jdx/aube/compare/aube-codes-v1.32.0...aube-codes-v1.33.0) - 2026-07-25

### Added

- *(run)* complete package.json scripts in shell completions ([#1108](https://github.com/jdx/aube/pull/1108))
- *(runtime)* detect FreeBSD as a supported host platform ([#1084](https://github.com/jdx/aube/pull/1084))

### Other

- refresh benchmarks for v1.32.0 ([#1080](https://github.com/jdx/aube/pull/1080))

### Security

- *(login)* cap web token responses ([#1103](https://github.com/jdx/aube/pull/1103))

## [1.32.0](https://github.com/jdx/aube/compare/aube-codes-v1.31.0...aube-codes-v1.32.0) - 2026-07-22

### Other

- refresh benchmarks for v1.31.0 ([#1072](https://github.com/jdx/aube/pull/1072))

## [1.30.0](https://github.com/jdx/aube/compare/aube-codes-v1.29.1...aube-codes-v1.30.0) - 2026-07-20

### Other

- refresh benchmarks for v1.29.1 ([#1060](https://github.com/jdx/aube/pull/1060))

## [1.29.0](https://github.com/jdx/aube/compare/aube-codes-v1.28.0...aube-codes-v1.29.0) - 2026-07-16

### Added

- *(node)* add configure() for embedder setting defaults ([#1053](https://github.com/jdx/aube/pull/1053))

### Other

- refresh benchmarks for v1.28.0 ([#1050](https://github.com/jdx/aube/pull/1050))

## [1.28.0](https://github.com/jdx/aube/compare/aube-codes-v1.27.0...aube-codes-v1.28.0) - 2026-07-16

### Added

- *(packaging)* add C ABI embedding distribution ([#1046](https://github.com/jdx/aube/pull/1046))
- *(packaging)* add production Node-API embedding ([#1025](https://github.com/jdx/aube/pull/1025))
- *(install)* add embedder control hooks ([#1036](https://github.com/jdx/aube/pull/1036))

### Other

- refresh benchmarks for v1.27.0 ([#1041](https://github.com/jdx/aube/pull/1041))

## [1.27.0](https://github.com/jdx/aube/compare/aube-codes-v1.26.0...aube-codes-v1.27.0) - 2026-07-13

### Added

- *(access)* manage registry package access ([#1012](https://github.com/jdx/aube/pull/1012))

### Other

- refresh benchmarks for v1.26.0 ([#1002](https://github.com/jdx/aube/pull/1002))

## [1.26.0](https://github.com/jdx/aube/compare/aube-codes-v1.25.2...aube-codes-v1.26.0) - 2026-07-06

### Added

- *(resolver)* allow trust exclude version ranges ([#989](https://github.com/jdx/aube/pull/989))

### Other

- Update sponsor references for jdx.dev ([#978](https://github.com/jdx/aube/pull/978))
- refresh benchmarks for v1.25.2 ([#975](https://github.com/jdx/aube/pull/975))

## [1.25.2](https://github.com/jdx/aube/compare/aube-codes-v1.25.1...aube-codes-v1.25.2) - 2026-07-01

### Other

- refresh benchmarks for v1.25.1 ([#962](https://github.com/jdx/aube/pull/962))

## [1.25.1](https://github.com/jdx/aube/compare/aube-codes-v1.25.0...aube-codes-v1.25.1) - 2026-06-25

### Other

- refresh benchmarks for v1.25.0 ([#947](https://github.com/jdx/aube/pull/947))

## [1.25.0](https://github.com/jdx/aube/compare/aube-codes-v1.24.0...aube-codes-v1.25.0) - 2026-06-25

### Added

- *(runtime)* add shell-activated tool shims ([#945](https://github.com/jdx/aube/pull/945))
- *(resolver)* support globs and version unions in minimumReleaseAgeExclude ([#941](https://github.com/jdx/aube/pull/941))

### Other

- refresh benchmarks for v1.24.0 ([#937](https://github.com/jdx/aube/pull/937))

## [1.24.0](https://github.com/jdx/aube/compare/aube-codes-v1.23.0...aube-codes-v1.24.0) - 2026-06-23

### Added

- *(config)* add managed hardening config ([#935](https://github.com/jdx/aube/pull/935))

### Other

- refresh benchmarks for v1.23.0 ([#922](https://github.com/jdx/aube/pull/922))

## [1.23.0](https://github.com/jdx/aube/compare/aube-codes-v1.22.0...aube-codes-v1.23.0) - 2026-06-21

### Fixed

- *(outdated)* support global packages ([#910](https://github.com/jdx/aube/pull/910))

### Other

- refresh benchmarks for v1.22.0 ([#907](https://github.com/jdx/aube/pull/907))

## [1.22.0](https://github.com/jdx/aube/compare/aube-codes-v1.21.0...aube-codes-v1.22.0) - 2026-06-17

### Fixed

- *(install)* close pnpm-lock.yaml parity and re-resolution gaps ([#896](https://github.com/jdx/aube/pull/896))

### Other

- refresh benchmarks for v1.21.0 ([#890](https://github.com/jdx/aube/pull/890))

## [1.21.0](https://github.com/jdx/aube/compare/aube-codes-v1.20.0...aube-codes-v1.21.0) - 2026-06-13

### Added

- *(lockfile)* emit packageExtensionsChecksum and pnpmfileChecksum for pnpm parity ([#883](https://github.com/jdx/aube/pull/883))

### Fixed

- *(packaging)* restore endevco npm scope ([#887](https://github.com/jdx/aube/pull/887))

## [1.20.0](https://github.com/jdx/aube/compare/aube-codes-v1.19.0...aube-codes-v1.20.0) - 2026-06-13

### Other

- link to all sponsors ([#876](https://github.com/jdx/aube/pull/876))
- refresh benchmarks for v1.19.0 ([#866](https://github.com/jdx/aube/pull/866))

## [1.19.0](https://github.com/jdx/aube/compare/aube-codes-v1.18.2...aube-codes-v1.19.0) - 2026-06-11

### Added

- *(runtime)* node version switching and aube self-version management ([#861](https://github.com/jdx/aube/pull/861))

### Fixed

- *(install)* warn on deprecated override refs ([#859](https://github.com/jdx/aube/pull/859))
- *(registry)* keep project npmrc env refs literal ([#856](https://github.com/jdx/aube/pull/856))
- *(lockfile)* reject mismatched resolution shapes ([#855](https://github.com/jdx/aube/pull/855))

### Other

- refresh benchmarks for v1.18.2 ([#851](https://github.com/jdx/aube/pull/851))

## [1.18.2](https://github.com/jdx/aube/compare/aube-codes-v1.18.1...aube-codes-v1.18.2) - 2026-06-08

### Other

- migrate project links to jdx ([#845](https://github.com/jdx/aube/pull/845))

## [1.18.1](https://github.com/jdx/aube/compare/aube-codes-v1.18.0...aube-codes-v1.18.1) - 2026-06-07

### Fixed

- *(install)* regenerate conflicted lockfiles ([#843](https://github.com/jdx/aube/pull/843))

### Other

- refresh benchmarks for v1.18.0 ([#841](https://github.com/jdx/aube/pull/841))

### Security

- *(install)* verify lockfile tarball URLs ([#842](https://github.com/jdx/aube/pull/842))

## [1.18.0](https://github.com/jdx/aube/compare/aube-codes-v1.17.1...aube-codes-v1.18.0) - 2026-06-04

### Added

- add sponsors command ([#824](https://github.com/jdx/aube/pull/824))

### Other

- refresh benchmarks for v1.17.1 ([#820](https://github.com/jdx/aube/pull/820))

## [1.17.1](https://github.com/jdx/aube/compare/aube-codes-v1.17.0...aube-codes-v1.17.1) - 2026-05-31

### Other

- *(ci)* switch back to namespace runners ([#819](https://github.com/jdx/aube/pull/819))

## [1.17.0](https://github.com/jdx/aube/compare/aube-codes-v1.16.1...aube-codes-v1.17.0) - 2026-05-31

### Other

- *(ci)* switch to github-hosted runners ([#814](https://github.com/jdx/aube/pull/814))
- refresh benchmarks for v1.16.1 ([#808](https://github.com/jdx/aube/pull/808))

## [1.16.1](https://github.com/jdx/aube/compare/aube-codes-v1.16.0...aube-codes-v1.16.1) - 2026-05-29

### Other

- refresh benchmarks for v1.16.0 ([#787](https://github.com/jdx/aube/pull/787))

### Security

- *(registry)* scope unqualified credentials ([#801](https://github.com/jdx/aube/pull/801))
- *(linker)* reject unsafe package aliases ([#800](https://github.com/jdx/aube/pull/800))

## [1.16.0](https://github.com/jdx/aube/compare/aube-codes-v1.15.0...aube-codes-v1.16.0) - 2026-05-25

### Other

- refresh benchmarks for v1.15.0 ([#750](https://github.com/jdx/aube/pull/750))

## [1.15.0](https://github.com/jdx/aube/compare/aube-codes-v1.14.1...aube-codes-v1.15.0) - 2026-05-17

### Added

- *(add)* add deny-build flag ([#730](https://github.com/jdx/aube/pull/730))

### Other

- refresh benchmarks for v1.14.1 ([#721](https://github.com/jdx/aube/pull/721))

## [1.14.0](https://github.com/jdx/aube/compare/aube-codes-v1.13.1...aube-codes-v1.14.0) - 2026-05-14

### Added

- *(install)* add OSV bloom-filter prefilter for lockfile installs ([#680](https://github.com/jdx/aube/pull/680))
- *(install)* content-sniff dep lifecycle scripts before approve-builds ([#685](https://github.com/jdx/aube/pull/685))

### Other

- refresh benchmarks for v1.13.1 ([#687](https://github.com/jdx/aube/pull/687))

## [1.13.0](https://github.com/jdx/aube/compare/aube-codes-v1.12.0...aube-codes-v1.13.0) - 2026-05-13

### Added

- *(install)* route OSV checks live-API vs local mirror by fresh-resolution ([#678](https://github.com/jdx/aube/pull/678))
- *(install)* bun-compatible security scanner ([#657](https://github.com/jdx/aube/pull/657))
- *(add)* block malicious packages via OSV + prompt on low downloads ([#656](https://github.com/jdx/aube/pull/656))

### Fixed

- *(scripts)* reap orphaned grandchildren on Windows when a lifecycle script aborts ([#661](https://github.com/jdx/aube/pull/661))

### Other

- refresh benchmarks for v1.12.0 ([#625](https://github.com/jdx/aube/pull/625))

## [1.12.0](https://github.com/jdx/aube/compare/aube-codes-v1.11.0...aube-codes-v1.12.0) - 2026-05-12

### Added

- *(config)* scope .npmrc to npm-shared keys, route aube settings to config.toml, support dotted map writes ([#634](https://github.com/jdx/aube/pull/634))

### Other

- refresh benchmarks for v1.11.0 ([#622](https://github.com/jdx/aube/pull/622))

## [1.11.0](https://github.com/jdx/aube/compare/aube-codes-v1.10.4...aube-codes-v1.11.0) - 2026-05-11

### Fixed

- *(registry)* coalesce slow-metadata warnings into one resolve summary ([#592](https://github.com/jdx/aube/pull/592))

### Other

- refresh benchmarks for v1.10.4 ([#600](https://github.com/jdx/aube/pull/600))

## [1.10.3](https://github.com/jdx/aube/compare/aube-codes-v1.10.2...aube-codes-v1.10.3) - 2026-05-10

### Other

- update Cargo.lock dependencies

## [1.10.1](https://github.com/jdx/aube/compare/aube-codes-v1.10.0...aube-codes-v1.10.1) - 2026-05-10

### Other

- refresh benchmarks for v1.10.0 ([#571](https://github.com/jdx/aube/pull/571))
- refresh benchmarks for v1.10.0 ([#566](https://github.com/jdx/aube/pull/566))

## [1.10.0](https://github.com/jdx/aube/compare/aube-codes-v1.9.1...aube-codes-v1.10.0) - 2026-05-10

### Added

- *(cli)* finish recursive-run flags and parallel output ([#545](https://github.com/jdx/aube/pull/545))

### Other

- refresh benchmarks for v1.9.1 ([#555](https://github.com/jdx/aube/pull/555))
- lead hero with auto-install promise over speed ([#557](https://github.com/jdx/aube/pull/557))
- refresh benchmarks for v1.9.1 ([#534](https://github.com/jdx/aube/pull/534))
- refresh benchmarks for v1.9.0 ([#532](https://github.com/jdx/aube/pull/532))

## [1.9.1](https://github.com/jdx/aube/compare/aube-codes-v1.9.0...aube-codes-v1.9.1) - 2026-05-06

### Fixed

- *(cli)* skip registry for workspace deps ([#523](https://github.com/jdx/aube/pull/523))

### Other

- refresh benchmarks for v1.9.0 ([#525](https://github.com/jdx/aube/pull/525))

## [1.9.0](https://github.com/jdx/aube/compare/aube-codes-v1.8.0...aube-codes-v1.9.0) - 2026-05-05

### Other

- refresh benchmarks for v1.8.0 ([#508](https://github.com/jdx/aube/pull/508))

## [1.8.0](https://github.com/jdx/aube/compare/aube-codes-v1.7.0...aube-codes-v1.8.0) - 2026-05-03

### Added

- *(progress)* redesign install progress UI ([#501](https://github.com/jdx/aube/pull/501))
- *(run)* prefer local bins for run and dlx ([#502](https://github.com/jdx/aube/pull/502))
