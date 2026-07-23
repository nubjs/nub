# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.32.0](https://github.com/jdx/aube/compare/aube-store-v1.31.0...aube-store-v1.32.0) - 2026-07-22

### Other

- refresh benchmarks for v1.31.0 ([#1072](https://github.com/jdx/aube/pull/1072))

## [1.30.0](https://github.com/jdx/aube/compare/aube-store-v1.29.1...aube-store-v1.30.0) - 2026-07-20

### Other

- refresh benchmarks for v1.29.1 ([#1060](https://github.com/jdx/aube/pull/1060))

## [1.29.0](https://github.com/jdx/aube/compare/aube-store-v1.28.0...aube-store-v1.29.0) - 2026-07-16

### Other

- refresh benchmarks for v1.28.0 ([#1050](https://github.com/jdx/aube/pull/1050))

## [1.28.0](https://github.com/jdx/aube/compare/aube-store-v1.27.0...aube-store-v1.28.0) - 2026-07-16

### Fixed

- *(install)* refresh changed file directory dependencies ([#1034](https://github.com/jdx/aube/pull/1034))

### Other

- refresh benchmarks for v1.27.0 ([#1041](https://github.com/jdx/aube/pull/1041))
- undo 7-day soak toolchain changes ([#1039](https://github.com/jdx/aube/pull/1039))

## [1.27.0](https://github.com/jdx/aube/compare/aube-store-v1.26.0...aube-store-v1.27.0) - 2026-07-13

### Other

- 7-day soak across toolchain and deps, cold-path hints, pinned tooling ([#1020](https://github.com/jdx/aube/pull/1020))
- refresh benchmarks for v1.26.0 ([#1002](https://github.com/jdx/aube/pull/1002))

## [1.26.0](https://github.com/jdx/aube/compare/aube-store-v1.25.2...aube-store-v1.26.0) - 2026-07-06

### Added

- *(store)* opt-in transparent FS-compression of native addons on import ([#985](https://github.com/jdx/aube/pull/985))

### Fixed

- *(resolver)* derive integrity from dist.shasum when dist.integrity is absent ([#994](https://github.com/jdx/aube/pull/994))

### Other

- Update sponsor references for jdx.dev ([#978](https://github.com/jdx/aube/pull/978))
- refresh benchmarks for v1.25.2 ([#975](https://github.com/jdx/aube/pull/975))

## [1.25.2](https://github.com/jdx/aube/compare/aube-store-v1.25.1...aube-store-v1.25.2) - 2026-07-01

### Other

- refresh benchmarks for v1.25.1 ([#962](https://github.com/jdx/aube/pull/962))

## [1.25.1](https://github.com/jdx/aube/compare/aube-store-v1.25.0...aube-store-v1.25.1) - 2026-06-25

### Fixed

- *(store)* tolerate tarball build metadata ([#951](https://github.com/jdx/aube/pull/951))

### Other

- refresh benchmarks for v1.25.0 ([#947](https://github.com/jdx/aube/pull/947))

## [1.25.0](https://github.com/jdx/aube/compare/aube-store-v1.24.0...aube-store-v1.25.0) - 2026-06-25

### Added

- *(runtime)* add shell-activated tool shims ([#945](https://github.com/jdx/aube/pull/945))

### Other

- refresh benchmarks for v1.24.0 ([#937](https://github.com/jdx/aube/pull/937))

## [1.24.0](https://github.com/jdx/aube/compare/aube-store-v1.23.0...aube-store-v1.24.0) - 2026-06-23

### Fixed

- *(install)* persist computed tarball integrity ([#933](https://github.com/jdx/aube/pull/933))

### Other

- refresh benchmarks for v1.23.0 ([#922](https://github.com/jdx/aube/pull/922))

## [1.23.0](https://github.com/jdx/aube/compare/aube-store-v1.22.0...aube-store-v1.23.0) - 2026-06-21

### Other

- refresh benchmarks for v1.22.0 ([#907](https://github.com/jdx/aube/pull/907))

## [1.22.0](https://github.com/jdx/aube/compare/aube-store-v1.21.0...aube-store-v1.22.0) - 2026-06-17

### Fixed

- *(embedder)* honor the embedder profile in the install banner and cache/name sites ([#888](https://github.com/jdx/aube/pull/888))
- *(install)* repair member installs under sharedWorkspaceLockfile=false ([#891](https://github.com/jdx/aube/pull/891))

### Other

- refresh benchmarks for v1.21.0 ([#890](https://github.com/jdx/aube/pull/890))

## [1.21.0](https://github.com/jdx/aube/compare/aube-store-v1.20.0...aube-store-v1.21.0) - 2026-06-13

### Fixed

- *(packaging)* restore endevco npm scope ([#887](https://github.com/jdx/aube/pull/887))

## [1.20.0](https://github.com/jdx/aube/compare/aube-store-v1.19.0...aube-store-v1.20.0) - 2026-06-13

### Added

- embeddable Embedder profile (compile-time pluggability) ([#862](https://github.com/jdx/aube/pull/862))

### Fixed

- *(linker)* resolve git deps in global virtual store ([#857](https://github.com/jdx/aube/pull/857))

### Other

- link to all sponsors ([#876](https://github.com/jdx/aube/pull/876))
- refresh benchmarks for v1.19.0 ([#866](https://github.com/jdx/aube/pull/866))

## [1.19.0](https://github.com/jdx/aube/compare/aube-store-v1.18.2...aube-store-v1.19.0) - 2026-06-11

### Added

- *(runtime)* node version switching and aube self-version management ([#861](https://github.com/jdx/aube/pull/861))

### Other

- refresh benchmarks for v1.18.2 ([#851](https://github.com/jdx/aube/pull/851))

## [1.18.2](https://github.com/jdx/aube/compare/aube-store-v1.18.1...aube-store-v1.18.2) - 2026-06-08

### Other

- migrate project links to jdx ([#845](https://github.com/jdx/aube/pull/845))

## [1.18.1](https://github.com/jdx/aube/compare/aube-store-v1.18.0...aube-store-v1.18.1) - 2026-06-07

### Other

- refresh benchmarks for v1.18.0 ([#841](https://github.com/jdx/aube/pull/841))

## [1.18.0](https://github.com/jdx/aube/compare/aube-store-v1.17.1...aube-store-v1.18.0) - 2026-06-04

### Added

- add sponsors command ([#824](https://github.com/jdx/aube/pull/824))

### Other

- refresh benchmarks for v1.17.1 ([#820](https://github.com/jdx/aube/pull/820))

## [1.17.1](https://github.com/jdx/aube/compare/aube-store-v1.17.0...aube-store-v1.17.1) - 2026-05-31

### Other

- *(ci)* switch back to namespace runners ([#819](https://github.com/jdx/aube/pull/819))

## [1.17.0](https://github.com/jdx/aube/compare/aube-store-v1.16.1...aube-store-v1.17.0) - 2026-05-31

### Other

- *(deps)* bump sha2 from 0.10.9 to 0.11.0 ([#790](https://github.com/jdx/aube/pull/790))
- *(ci)* switch to github-hosted runners ([#814](https://github.com/jdx/aube/pull/814))
- refresh benchmarks for v1.16.1 ([#808](https://github.com/jdx/aube/pull/808))

## [1.16.1](https://github.com/jdx/aube/compare/aube-store-v1.16.0...aube-store-v1.16.1) - 2026-05-29

### Other

- *(deps)* bump sha1 from 0.10.6 to 0.11.0 ([#797](https://github.com/jdx/aube/pull/797))
- refresh benchmarks for v1.16.0 ([#787](https://github.com/jdx/aube/pull/787))

## [1.16.0](https://github.com/jdx/aube/compare/aube-store-v1.15.0...aube-store-v1.16.0) - 2026-05-25

### Added

- *(pnpm)* catch up with pnpm 11 parity ([#761](https://github.com/jdx/aube/pull/761))

### Fixed

- *(resolver)* pin hosted git tarball integrity ([#783](https://github.com/jdx/aube/pull/783))

### Other

- refresh benchmarks for v1.15.0 ([#750](https://github.com/jdx/aube/pull/750))

## [1.15.0](https://github.com/jdx/aube/compare/aube-store-v1.14.1...aube-store-v1.15.0) - 2026-05-17

### Other

- refresh benchmarks for v1.14.1 ([#721](https://github.com/jdx/aube/pull/721))
- *(store)* split store modules ([#716](https://github.com/jdx/aube/pull/716))

## [1.14.1](https://github.com/jdx/aube/compare/aube-store-v1.14.0...aube-store-v1.14.1) - 2026-05-15

### Other

- *(store)* split git helpers ([#699](https://github.com/jdx/aube/pull/699))

## [1.14.0](https://github.com/jdx/aube/compare/aube-store-v1.13.1...aube-store-v1.14.0) - 2026-05-14

### Other

- refresh benchmarks for v1.13.1 ([#687](https://github.com/jdx/aube/pull/687))

## [1.13.0](https://github.com/jdx/aube/compare/aube-store-v1.12.0...aube-store-v1.13.0) - 2026-05-13

### Other

- refresh benchmarks for v1.12.0 ([#625](https://github.com/jdx/aube/pull/625))

## [1.12.0](https://github.com/jdx/aube/compare/aube-store-v1.11.0...aube-store-v1.12.0) - 2026-05-12

### Fixed

- *(install)* co-locate cached indexes with CAS + verified probe self-heal ([#635](https://github.com/jdx/aube/pull/635))

### Other

- *(store)* gate posix_fallocate and posix_fadvise by file size ([#643](https://github.com/jdx/aube/pull/643))
- refresh benchmarks for v1.11.0 ([#622](https://github.com/jdx/aube/pull/622))

## [1.11.0](https://github.com/jdx/aube/compare/aube-store-v1.10.4...aube-store-v1.11.0) - 2026-05-11

### Other

- *(store)* direct-write CAS fast path on macOS under exclusive install lock ([#615](https://github.com/jdx/aube/pull/615))
- refresh benchmarks for v1.10.4 ([#600](https://github.com/jdx/aube/pull/600))

## [1.10.4](https://github.com/jdx/aube/compare/aube-store-v1.10.3...aube-store-v1.10.4) - 2026-05-11

### Fixed

- *(store)* take libc::off_t in posix_fallocate wrapper for 32-bit targets ([#587](https://github.com/jdx/aube/pull/587))

## [1.10.1](https://github.com/jdx/aube/compare/aube-store-v1.10.0...aube-store-v1.10.1) - 2026-05-10

### Other

- refresh benchmarks for v1.10.0 ([#571](https://github.com/jdx/aube/pull/571))
- *(registry)* swap simd-json for sonic-rs on packument hot path ([#569](https://github.com/jdx/aube/pull/569))
- refresh benchmarks for v1.10.0 ([#566](https://github.com/jdx/aube/pull/566))

## [1.10.0](https://github.com/jdx/aube/compare/aube-store-v1.9.1...aube-store-v1.10.0) - 2026-05-10

### Added

- *(diag)* instrument install and add aube diag subcommand ([#547](https://github.com/jdx/aube/pull/547))

### Other

- refresh benchmarks for v1.9.1 ([#555](https://github.com/jdx/aube/pull/555))
- lead hero with auto-install promise over speed ([#557](https://github.com/jdx/aube/pull/557))
- *(install)* adaptive limiter + tarball http1 split ([#548](https://github.com/jdx/aube/pull/548))
- refresh benchmarks for v1.9.1 ([#534](https://github.com/jdx/aube/pull/534))
- refresh benchmarks for v1.9.0 ([#532](https://github.com/jdx/aube/pull/532))

## [1.9.1](https://github.com/jdx/aube/compare/aube-store-v1.9.0...aube-store-v1.9.1) - 2026-05-06

### Other

- refresh benchmarks for v1.9.0 ([#525](https://github.com/jdx/aube/pull/525))
- cold install pipeline overhaul ([#522](https://github.com/jdx/aube/pull/522))

## [1.9.0](https://github.com/jdx/aube/compare/aube-store-v1.8.0...aube-store-v1.9.0) - 2026-05-05

### Other

- refresh benchmarks for v1.8.0 ([#508](https://github.com/jdx/aube/pull/508))

## [1.8.0](https://github.com/jdx/aube/compare/aube-store-v1.7.0...aube-store-v1.8.0) - 2026-05-03

### Added

- *(run)* prefer local bins for run and dlx ([#502](https://github.com/jdx/aube/pull/502))
- *(codes)* introduce ERR_AUBE_/WARN_AUBE_ codes, exit codes, dep chains ([#492](https://github.com/jdx/aube/pull/492))

### Other

- refresh benchmarks for v1.7.0 ([#490](https://github.com/jdx/aube/pull/490))

## [1.7.0](https://github.com/jdx/aube/compare/aube-store-v1.6.2...aube-store-v1.7.0) - 2026-05-03

### Other

- refresh benchmarks for v1.6.2 ([#474](https://github.com/jdx/aube/pull/474))
- streaming sha512, parallel cas, tls prewarm, fetch reorder ([#469](https://github.com/jdx/aube/pull/469))
- refresh benchmarks for v1.6.2 ([#467](https://github.com/jdx/aube/pull/467))

## [1.6.1](https://github.com/jdx/aube/compare/aube-store-v1.6.0...aube-store-v1.6.1) - 2026-05-01

### Other

- refresh benchmarks for v1.5.2 ([#459](https://github.com/jdx/aube/pull/459))

## [1.6.0](https://github.com/jdx/aube/compare/aube-store-v1.5.2...aube-store-v1.6.0) - 2026-05-01

### Other

- cache hot-path work across install, resolver, and registry ([#453](https://github.com/jdx/aube/pull/453))
- refresh benchmarks for v1.5.2 ([#452](https://github.com/jdx/aube/pull/452))
- refresh benchmarks for v1.5.2 ([#448](https://github.com/jdx/aube/pull/448))
- refresh benchmarks for v1.5.1 ([#426](https://github.com/jdx/aube/pull/426))

## [1.5.2](https://github.com/jdx/aube/compare/aube-store-v1.5.1...aube-store-v1.5.2) - 2026-04-30

### Fixed

- *(install)* fetch hosted git deps over https, not ssh ([#394](https://github.com/jdx/aube/pull/394))
- *(linker,store)* self-heal install on missing CAS shard ([#395](https://github.com/jdx/aube/pull/395))

### Other

- thank Namespace for GitHub Actions runner support ([#412](https://github.com/jdx/aube/pull/412))
- refresh benchmarks for v1.5.1 ([#392](https://github.com/jdx/aube/pull/392))

## [1.5.1](https://github.com/jdx/aube/compare/aube-store-v1.5.0...aube-store-v1.5.1) - 2026-04-29

### Fixed

- *(install)* allow POSIX colon tarball filenames ([#386](https://github.com/jdx/aube/pull/386))

## [1.4.0](https://github.com/jdx/aube/compare/aube-store-v1.3.0...aube-store-v1.4.0) - 2026-04-28

### Fixed

- *(store)* repair truncated CAS entries ([#357](https://github.com/jdx/aube/pull/357))
- *(packaging)* include README on published aube crate ([#349](https://github.com/jdx/aube/pull/349))

### Other

- warn about npm install caveats ([#368](https://github.com/jdx/aube/pull/368))

## [1.3.0](https://github.com/jdx/aube/compare/aube-store-v1.2.1...aube-store-v1.3.0) - 2026-04-27

### Fixed

- *(resolver)* accept abbreviated git commit SHAs in user specs ([#346](https://github.com/jdx/aube/pull/346))
- *(lockfile)* preserve package and bun lock compatibility ([#339](https://github.com/jdx/aube/pull/339))

## [1.2.0](https://github.com/jdx/aube/compare/aube-store-v1.1.0...aube-store-v1.2.0) - 2026-04-25

### Security

- cve-class hardening across linker, registry, resolver, install ([#296](https://github.com/jdx/aube/pull/296))

## [1.1.0](https://github.com/jdx/aube/compare/aube-store-v1.0.0...aube-store-v1.1.0) - 2026-04-24

### Fixed

- *(store)* speed up cold installs ([#267](https://github.com/jdx/aube/pull/267))

### Other

- accept legacy sha1/sha256/sha384 integrity in verify_integrity ([#263](https://github.com/jdx/aube/pull/263))
- dedup pass + registry/store perf wave ([#254](https://github.com/jdx/aube/pull/254))
- copy small files instead of reflinking ([#251](https://github.com/jdx/aube/pull/251))
- shared helpers + migrate hardcoded sites ([#245](https://github.com/jdx/aube/pull/245))

## [1.0.0-beta.12](https://github.com/jdx/aube/compare/aube-store-v1.0.0-beta.11...aube-store-v1.0.0-beta.12) - 2026-04-22

### Other

- include integrity in package index cache key ([#209](https://github.com/jdx/aube/pull/209))
- cross-crate dedup pass ([#208](https://github.com/jdx/aube/pull/208))
- skip pkg-content version check for URL-shaped lockfile entries ([#203](https://github.com/jdx/aube/pull/203))
- cross-crate security hardening ([#202](https://github.com/jdx/aube/pull/202))
- cross-crate correctness and security fixes ([#196](https://github.com/jdx/aube/pull/196))

## [1.0.0-beta.10](https://github.com/jdx/aube/compare/aube-store-v1.0.0-beta.9...aube-store-v1.0.0-beta.10) - 2026-04-21

### Fixed

- close remaining audit findings across registry, store, and linker ([#164](https://github.com/jdx/aube/pull/164))

## [1.0.0-beta.8](https://github.com/jdx/aube/compare/aube-store-v1.0.0-beta.7...aube-store-v1.0.0-beta.8) - 2026-04-20

### Other

- default to ~/.local/share/aube/store per XDG spec ([#129](https://github.com/jdx/aube/pull/129))

## [1.0.0-beta.6](https://github.com/jdx/aube/compare/aube-store-v1.0.0-beta.5...aube-store-v1.0.0-beta.6) - 2026-04-19

### Other

- skip PAX global/extension tar headers ([#100](https://github.com/jdx/aube/pull/100))
- tolerate leading `v` in tarball package.json version ([#95](https://github.com/jdx/aube/pull/95))
- reject traversing and non-regular tar entries on import ([#85](https://github.com/jdx/aube/pull/85))
- cap tarball decompression to prevent gzip-bomb dos ([#79](https://github.com/jdx/aube/pull/79))
- reject dash-prefixed urls and commits passed to git ([#75](https://github.com/jdx/aube/pull/75))

## [1.0.0-beta.3](https://github.com/jdx/aube/compare/aube-store-v1.0.0-beta.2...aube-store-v1.0.0-beta.3) - 2026-04-19

### Other

- swap CAS hash from SHA-512 to BLAKE3 ([#36](https://github.com/jdx/aube/pull/36))

## [1.0.0-beta.2](https://github.com/jdx/aube/compare/aube-store-v1.0.0-beta.1...aube-store-v1.0.0-beta.2) - 2026-04-18

### Other

- aube-cli crate -> aube ([#7](https://github.com/jdx/aube/pull/7))
