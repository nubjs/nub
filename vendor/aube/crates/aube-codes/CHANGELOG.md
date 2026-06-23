# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
