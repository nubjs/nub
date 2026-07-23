# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.32.0](https://github.com/jdx/aube/compare/aube-lockfile-v1.31.0...aube-lockfile-v1.32.0) - 2026-07-22

### Other

- refresh benchmarks for v1.31.0 ([#1072](https://github.com/jdx/aube/pull/1072))

## [1.30.0](https://github.com/jdx/aube/compare/aube-lockfile-v1.29.1...aube-lockfile-v1.30.0) - 2026-07-20

### Other

- satisfy nightly clippy (question_mark, fetch_update) ([#1066](https://github.com/jdx/aube/pull/1066))
- refresh benchmarks for v1.29.1 ([#1060](https://github.com/jdx/aube/pull/1060))

## [1.29.0](https://github.com/jdx/aube/compare/aube-lockfile-v1.28.0...aube-lockfile-v1.29.0) - 2026-07-16

### Other

- refresh benchmarks for v1.28.0 ([#1050](https://github.com/jdx/aube/pull/1050))

## [1.28.0](https://github.com/jdx/aube/compare/aube-lockfile-v1.27.0...aube-lockfile-v1.28.0) - 2026-07-16

### Fixed

- *(deploy)* dedupe injected workspace dependencies ([#1038](https://github.com/jdx/aube/pull/1038))
- *(lockfile)* preserve pnpm patch hashes on re-resolve ([#1035](https://github.com/jdx/aube/pull/1035))

### Other

- refresh benchmarks for v1.27.0 ([#1041](https://github.com/jdx/aube/pull/1041))
- undo 7-day soak toolchain changes ([#1039](https://github.com/jdx/aube/pull/1039))

## [1.27.0](https://github.com/jdx/aube/compare/aube-lockfile-v1.26.0...aube-lockfile-v1.27.0) - 2026-07-13

### Added

- *(scripts)* allow git repository build approvals ([#1010](https://github.com/jdx/aube/pull/1010))

### Other

- 7-day soak across toolchain and deps, cold-path hints, pinned tooling ([#1020](https://github.com/jdx/aube/pull/1020))
- refresh benchmarks for v1.26.0 ([#1002](https://github.com/jdx/aube/pull/1002))

## [1.26.0](https://github.com/jdx/aube/compare/aube-lockfile-v1.25.2...aube-lockfile-v1.26.0) - 2026-07-06

### Fixed

- *(import)* preserve custom registry tarball urls ([#997](https://github.com/jdx/aube/pull/997))
- *(lockfile)* gate tarball integrity parse errors on strict mode ([#995](https://github.com/jdx/aube/pull/995))

### Other

- Update sponsor references for jdx.dev ([#978](https://github.com/jdx/aube/pull/978))
- refresh benchmarks for v1.25.2 ([#975](https://github.com/jdx/aube/pull/975))

## [1.25.2](https://github.com/jdx/aube/compare/aube-lockfile-v1.25.1...aube-lockfile-v1.25.2) - 2026-07-01

### Fixed

- *(lockfile)* handle yarn berry builtin compat flags & ~/ patch paths ([#963](https://github.com/jdx/aube/pull/963))
- *(lockfile)* detect npm package-lock root drift ([#974](https://github.com/jdx/aube/pull/974))

### Other

- refresh benchmarks for v1.25.1 ([#962](https://github.com/jdx/aube/pull/962))
- *(install)* overlap cold-install lockfile write with the link tail ([#961](https://github.com/jdx/aube/pull/961))
- *(deps)* update rust crate criterion to 0.8 ([#955](https://github.com/jdx/aube/pull/955))

## [1.25.1](https://github.com/jdx/aube/compare/aube-lockfile-v1.25.0...aube-lockfile-v1.25.1) - 2026-06-25

### Other

- refresh benchmarks for v1.25.0 ([#947](https://github.com/jdx/aube/pull/947))

## [1.25.0](https://github.com/jdx/aube/compare/aube-lockfile-v1.24.0...aube-lockfile-v1.25.0) - 2026-06-25

### Added

- *(runtime)* add shell-activated tool shims ([#945](https://github.com/jdx/aube/pull/945))

### Other

- *(linker)* hoist redundant per-package dep-path encode ([#943](https://github.com/jdx/aube/pull/943))
- *(lockfile)* byte-cursor subset parser for pnpm-lock.yaml ([#912](https://github.com/jdx/aube/pull/912))
- refresh benchmarks for v1.24.0 ([#937](https://github.com/jdx/aube/pull/937))

## [1.24.0](https://github.com/jdx/aube/compare/aube-lockfile-v1.23.0...aube-lockfile-v1.24.0) - 2026-06-23

### Other

- refresh benchmarks for v1.23.0 ([#922](https://github.com/jdx/aube/pull/922))

## [1.23.0](https://github.com/jdx/aube/compare/aube-lockfile-v1.22.0...aube-lockfile-v1.23.0) - 2026-06-21

### Other

- refresh benchmarks for v1.22.0 ([#907](https://github.com/jdx/aube/pull/907))

## [1.22.0](https://github.com/jdx/aube/compare/aube-lockfile-v1.21.0...aube-lockfile-v1.22.0) - 2026-06-17

### Fixed

- *(embedder)* honor the embedder profile in the install banner and cache/name sites ([#888](https://github.com/jdx/aube/pull/888))
- *(install)* close pnpm-lock.yaml parity and re-resolution gaps ([#896](https://github.com/jdx/aube/pull/896))
- *(lockfile)* reject unsafe dependency aliases ([#898](https://github.com/jdx/aube/pull/898))
- *(lockfile)* close pnpm-lock.yaml formatting and field parity gaps ([#893](https://github.com/jdx/aube/pull/893))
- *(install)* repair member installs under sharedWorkspaceLockfile=false ([#891](https://github.com/jdx/aube/pull/891))

### Other

- refresh benchmarks for v1.21.0 ([#890](https://github.com/jdx/aube/pull/890))

## [1.21.0](https://github.com/jdx/aube/compare/aube-lockfile-v1.20.0...aube-lockfile-v1.21.0) - 2026-06-13

### Added

- *(lockfile)* emit packageExtensionsChecksum and pnpmfileChecksum for pnpm parity ([#883](https://github.com/jdx/aube/pull/883))

### Fixed

- *(install)* map peer-suffixed source deps to their canonical store index ([#885](https://github.com/jdx/aube/pull/885))
- *(packaging)* restore endevco npm scope ([#887](https://github.com/jdx/aube/pull/887))

## [1.20.0](https://github.com/jdx/aube/compare/aube-lockfile-v1.19.0...aube-lockfile-v1.20.0) - 2026-06-13

### Added

- embeddable Embedder profile (compile-time pluggability) ([#862](https://github.com/jdx/aube/pull/862))

### Fixed

- *(linker)* resolve git deps in global virtual store ([#857](https://github.com/jdx/aube/pull/857))

### Other

- link to all sponsors ([#876](https://github.com/jdx/aube/pull/876))
- refresh benchmarks for v1.19.0 ([#866](https://github.com/jdx/aube/pull/866))

## [1.19.0](https://github.com/jdx/aube/compare/aube-lockfile-v1.18.2...aube-lockfile-v1.19.0) - 2026-06-11

### Added

- *(runtime)* node version switching and aube self-version management ([#861](https://github.com/jdx/aube/pull/861))

### Fixed

- *(scripts)* match URL source build approvals ([#860](https://github.com/jdx/aube/pull/860))
- *(scripts)* require source keys for build approvals ([#858](https://github.com/jdx/aube/pull/858))
- *(lockfile)* reject mismatched resolution shapes ([#855](https://github.com/jdx/aube/pull/855))

### Other

- refresh benchmarks for v1.18.2 ([#851](https://github.com/jdx/aube/pull/851))

## [1.18.2](https://github.com/jdx/aube/compare/aube-lockfile-v1.18.1...aube-lockfile-v1.18.2) - 2026-06-08

### Other

- migrate project links to jdx ([#845](https://github.com/jdx/aube/pull/845))

## [1.18.1](https://github.com/jdx/aube/compare/aube-lockfile-v1.18.0...aube-lockfile-v1.18.1) - 2026-06-07

### Fixed

- *(install)* regenerate conflicted lockfiles ([#843](https://github.com/jdx/aube/pull/843))

### Other

- refresh benchmarks for v1.18.0 ([#841](https://github.com/jdx/aube/pull/841))

## [1.18.0](https://github.com/jdx/aube/compare/aube-lockfile-v1.17.1...aube-lockfile-v1.18.0) - 2026-06-04

### Added

- add sponsors command ([#824](https://github.com/jdx/aube/pull/824))

### Fixed

- *(lockfile)* rebase pnpm workspace link paths ([#827](https://github.com/jdx/aube/pull/827))

### Other

- refresh benchmarks for v1.17.1 ([#820](https://github.com/jdx/aube/pull/820))

## [1.17.1](https://github.com/jdx/aube/compare/aube-lockfile-v1.17.0...aube-lockfile-v1.17.1) - 2026-05-31

### Other

- *(ci)* switch back to namespace runners ([#819](https://github.com/jdx/aube/pull/819))

## [1.17.0](https://github.com/jdx/aube/compare/aube-lockfile-v1.16.1...aube-lockfile-v1.17.0) - 2026-05-31

### Fixed

- *(lockfile)* preserve remote tarball integrity ([#812](https://github.com/jdx/aube/pull/812))

### Other

- *(lockfile)* cover remote tarball fallback lookup ([#815](https://github.com/jdx/aube/pull/815))
- *(ci)* switch to github-hosted runners ([#814](https://github.com/jdx/aube/pull/814))
- refresh benchmarks for v1.16.1 ([#808](https://github.com/jdx/aube/pull/808))

## [1.16.1](https://github.com/jdx/aube/compare/aube-lockfile-v1.16.0...aube-lockfile-v1.16.1) - 2026-05-29

### Other

- refresh benchmarks for v1.16.0 ([#787](https://github.com/jdx/aube/pull/787))

### Security

- *(lockfile)* require remote tarball integrity ([#802](https://github.com/jdx/aube/pull/802))

## [1.16.0](https://github.com/jdx/aube/compare/aube-lockfile-v1.15.0...aube-lockfile-v1.16.0) - 2026-05-25

### Added

- *(pnpm)* catch up with pnpm 11 parity ([#761](https://github.com/jdx/aube/pull/761))

### Fixed

- *(resolver)* pin hosted git tarball integrity ([#783](https://github.com/jdx/aube/pull/783))
- *(lockfile)* avoid lossy npm metadata drift rewrites ([#753](https://github.com/jdx/aube/pull/753))

### Other

- refresh benchmarks for v1.15.0 ([#750](https://github.com/jdx/aube/pull/750))

## [1.15.0](https://github.com/jdx/aube/compare/aube-lockfile-v1.14.1...aube-lockfile-v1.15.0) - 2026-05-17

### Added

- *(yarn)* support berry portal and exec protocols ([#729](https://github.com/jdx/aube/pull/729))
- *(yarn)* support berry patch protocol ([#728](https://github.com/jdx/aube/pull/728))

### Fixed

- *(lockfile)* prune pnpm time entries to direct deps ([#735](https://github.com/jdx/aube/pull/735))
- *(lockfile)* parse yarn classic dependency tails ([#733](https://github.com/jdx/aube/pull/733))

### Other

- *(lockfile)* clean up pnpm split feedback ([#720](https://github.com/jdx/aube/pull/720))
- refresh benchmarks for v1.14.1 ([#721](https://github.com/jdx/aube/pull/721))
- *(lockfile)* split bun modules ([#718](https://github.com/jdx/aube/pull/718))
- *(lockfile)* split npm and yarn modules ([#717](https://github.com/jdx/aube/pull/717))
- *(lockfile)* split pnpm modules ([#719](https://github.com/jdx/aube/pull/719))
- *(lockfile)* split drift checks ([#714](https://github.com/jdx/aube/pull/714))
- *(lockfile)* split format io ([#713](https://github.com/jdx/aube/pull/713))
- *(lockfile)* split package source parsing ([#710](https://github.com/jdx/aube/pull/710))

## [1.14.1](https://github.com/jdx/aube/compare/aube-lockfile-v1.14.0...aube-lockfile-v1.14.1) - 2026-05-15

### Fixed

- *(lockfile)* preserve hashed npm peer roots ([#697](https://github.com/jdx/aube/pull/697))
- *(lockfile)* parse bun workspace paths as links ([#696](https://github.com/jdx/aube/pull/696))

## [1.14.0](https://github.com/jdx/aube/compare/aube-lockfile-v1.13.1...aube-lockfile-v1.14.0) - 2026-05-14

### Fixed

- *(lockfile)* preserve yarn npm-protocol alias real name ([#686](https://github.com/jdx/aube/pull/686))

### Other

- refresh benchmarks for v1.13.1 ([#687](https://github.com/jdx/aube/pull/687))

## [1.13.0](https://github.com/jdx/aube/compare/aube-lockfile-v1.12.0...aube-lockfile-v1.13.0) - 2026-05-13

### Other

- refresh benchmarks for v1.12.0 ([#625](https://github.com/jdx/aube/pull/625))

## [1.12.0](https://github.com/jdx/aube/compare/aube-lockfile-v1.11.0...aube-lockfile-v1.12.0) - 2026-05-12

### Other

- refresh benchmarks for v1.11.0 ([#622](https://github.com/jdx/aube/pull/622))

## [1.11.0](https://github.com/jdx/aube/compare/aube-lockfile-v1.10.4...aube-lockfile-v1.11.0) - 2026-05-11

### Fixed

- address several bugs reported in #602 ([#610](https://github.com/jdx/aube/pull/610))

### Other

- refresh benchmarks for v1.10.4 ([#600](https://github.com/jdx/aube/pull/600))

## [1.10.1](https://github.com/jdx/aube/compare/aube-lockfile-v1.10.0...aube-lockfile-v1.10.1) - 2026-05-10

### Other

- refresh benchmarks for v1.10.0 ([#571](https://github.com/jdx/aube/pull/571))
- *(registry)* swap simd-json for sonic-rs on packument hot path ([#569](https://github.com/jdx/aube/pull/569))
- refresh benchmarks for v1.10.0 ([#566](https://github.com/jdx/aube/pull/566))

## [1.10.0](https://github.com/jdx/aube/compare/aube-lockfile-v1.9.1...aube-lockfile-v1.10.0) - 2026-05-10

### Added

- *(diag)* instrument install and add aube diag subcommand ([#547](https://github.com/jdx/aube/pull/547))

### Fixed

- *(workspace)* three workspace install correctness fixes from pnpm test port ([#564](https://github.com/jdx/aube/pull/564))
- *(lockfile)* recognize file: resolved field in npm package-lock ([#553](https://github.com/jdx/aube/pull/553))
- *(lockfile)* preserve imported workspace links ([#535](https://github.com/jdx/aube/pull/535))

### Other

- refresh benchmarks for v1.9.1 ([#555](https://github.com/jdx/aube/pull/555))
- lead hero with auto-install promise over speed ([#557](https://github.com/jdx/aube/pull/557))
- refresh benchmarks for v1.9.1 ([#534](https://github.com/jdx/aube/pull/534))
- refresh benchmarks for v1.9.0 ([#532](https://github.com/jdx/aube/pull/532))

## [1.9.1](https://github.com/jdx/aube/compare/aube-lockfile-v1.9.0...aube-lockfile-v1.9.1) - 2026-05-06

### Other

- refresh benchmarks for v1.9.0 ([#525](https://github.com/jdx/aube/pull/525))
- cold install pipeline overhaul ([#522](https://github.com/jdx/aube/pull/522))

## [1.9.0](https://github.com/jdx/aube/compare/aube-lockfile-v1.8.0...aube-lockfile-v1.9.0) - 2026-05-05

### Fixed

- *(lockfile)* tolerate legacy license shapes in package-lock.json ([#512](https://github.com/jdx/aube/pull/512))

### Other

- refresh benchmarks for v1.8.0 ([#508](https://github.com/jdx/aube/pull/508))

## [1.8.0](https://github.com/jdx/aube/compare/aube-lockfile-v1.7.0...aube-lockfile-v1.8.0) - 2026-05-03

### Added

- *(run)* prefer local bins for run and dlx ([#502](https://github.com/jdx/aube/pull/502))
- *(codes)* introduce ERR_AUBE_/WARN_AUBE_ codes, exit codes, dep chains ([#492](https://github.com/jdx/aube/pull/492))

### Fixed

- *(install)* handle workspace scripts and pnpm aliases ([#500](https://github.com/jdx/aube/pull/500))
- *(lockfile)* honor bun workspace-scoped direct deps ([#489](https://github.com/jdx/aube/pull/489))

### Other

- refresh benchmarks for v1.7.0 ([#490](https://github.com/jdx/aube/pull/490))

## [1.7.0](https://github.com/jdx/aube/compare/aube-lockfile-v1.6.2...aube-lockfile-v1.7.0) - 2026-05-03

### Fixed

- *(lockfile)* parse bare user/repo as github shorthand ([#472](https://github.com/jdx/aube/pull/472))

### Other

- refresh benchmarks for v1.6.2 ([#474](https://github.com/jdx/aube/pull/474))
- refresh benchmarks for v1.6.2 ([#467](https://github.com/jdx/aube/pull/467))

## [1.6.1](https://github.com/jdx/aube/compare/aube-lockfile-v1.6.0...aube-lockfile-v1.6.1) - 2026-05-01

### Other

- refresh benchmarks for v1.5.2 ([#459](https://github.com/jdx/aube/pull/459))

## [1.6.0](https://github.com/jdx/aube/compare/aube-lockfile-v1.5.2...aube-lockfile-v1.6.0) - 2026-05-01

### Fixed

- Preserve npm workspace importers ([#443](https://github.com/jdx/aube/pull/443))

### Other

- cache hot-path work across install, resolver, and registry ([#453](https://github.com/jdx/aube/pull/453))
- refresh benchmarks for v1.5.2 ([#452](https://github.com/jdx/aube/pull/452))
- dedupe and cache hot-path work in install and resolver ([#449](https://github.com/jdx/aube/pull/449))
- refresh benchmarks for v1.5.2 ([#448](https://github.com/jdx/aube/pull/448))
- refresh benchmarks for v1.5.1 ([#426](https://github.com/jdx/aube/pull/426))

## [1.5.2](https://github.com/jdx/aube/compare/aube-lockfile-v1.5.1...aube-lockfile-v1.5.2) - 2026-04-30

### Fixed

- *(lockfile)* accept scalar os/cpu/libc in npm package-lock.json ([#405](https://github.com/jdx/aube/pull/405))
- *(lockfile)* synthesize npm-alias entries for transitive deps in pnpm lockfiles ([#403](https://github.com/jdx/aube/pull/403))
- *(install)* fetch hosted git deps over https, not ssh ([#394](https://github.com/jdx/aube/pull/394))

### Other

- thank Namespace for GitHub Actions runner support ([#412](https://github.com/jdx/aube/pull/412))
- refresh benchmarks for v1.5.1 ([#392](https://github.com/jdx/aube/pull/392))

## [1.5.0](https://github.com/jdx/aube/compare/aube-lockfile-v1.4.0...aube-lockfile-v1.5.0) - 2026-04-29

### Fixed

- *(cli,linker,lockfile)* patch-commit destination, CRLF patches, npm-alias catalog ([#384](https://github.com/jdx/aube/pull/384))
- *(lockfile)* preserve pnpm registry tarball urls ([#378](https://github.com/jdx/aube/pull/378))
- *(lockfile)* hoist npm workspace links to root importer deps ([#374](https://github.com/jdx/aube/pull/374))

### Other

- *(lockfile)* add property roundtrip coverage ([#376](https://github.com/jdx/aube/pull/376))

## [1.4.0](https://github.com/jdx/aube/compare/aube-lockfile-v1.3.0...aube-lockfile-v1.4.0) - 2026-04-28

### Fixed

- *(lockfile)* store bun dependency tails ([#355](https://github.com/jdx/aube/pull/355))
- *(lockfile)* apply overrides before frozen-lockfile spec comparison ([#354](https://github.com/jdx/aube/pull/354))
- *(packaging)* include README on published aube crate ([#349](https://github.com/jdx/aube/pull/349))

### Other

- warn about npm install caveats ([#368](https://github.com/jdx/aube/pull/368))

## [1.3.0](https://github.com/jdx/aube/compare/aube-lockfile-v1.2.1...aube-lockfile-v1.3.0) - 2026-04-27

### Fixed

- *(lockfile)* preserve non-registry and bun platform entries ([#338](https://github.com/jdx/aube/pull/338))
- *(lockfile)* preserve package and bun lock compatibility ([#339](https://github.com/jdx/aube/pull/339))
- *(lockfile)* parse scalar pnpm platform fields ([#337](https://github.com/jdx/aube/pull/337))
- *(lockfile)* preserve npm platform optional metadata ([#329](https://github.com/jdx/aube/pull/329))
- bun.lock parity for workspaces, platforms, and locked versions ([#327](https://github.com/jdx/aube/pull/327))

### Other

- *(deps)* replace serde_yaml with yaml_serde ([#340](https://github.com/jdx/aube/pull/340))

## [1.2.1](https://github.com/jdx/aube/compare/aube-lockfile-v1.2.0...aube-lockfile-v1.2.1) - 2026-04-26

### Fixed

- pnpm snapshot round-trip + workspace negation patterns ([#312](https://github.com/jdx/aube/pull/312))

## [1.2.0](https://github.com/jdx/aube/compare/aube-lockfile-v1.1.0...aube-lockfile-v1.2.0) - 2026-04-25

### Fixed

- support git url specs in dlx and parser ([#295](https://github.com/jdx/aube/pull/295))
- *(install)* link bins with mixed metadata ([#300](https://github.com/jdx/aube/pull/300))
- lockfile and resolver correctness pass ([#291](https://github.com/jdx/aube/pull/291))

## [1.1.0](https://github.com/jdx/aube/compare/aube-lockfile-v1.0.0...aube-lockfile-v1.1.0) - 2026-04-24

### Added

- *(resolver)* support pnpm `&path:/<sub>` git dep selector ([#273](https://github.com/jdx/aube/pull/273))

### Fixed

- *(resolver)* wire transitive url/git subdeps into parent snapshot ([#276](https://github.com/jdx/aube/pull/276))

### Other

- *(bun)* preserve top-level + per-entry metadata on roundtrip ([#250](https://github.com/jdx/aube/pull/250))
- *(pnpm)* preserve workspace importer specifiers ([#260](https://github.com/jdx/aube/pull/260))
- dedup pass + registry/store perf wave ([#254](https://github.com/jdx/aube/pull/254))
- resolve catalog: in overrides + honor override-rewritten importer specs ([#249](https://github.com/jdx/aube/pull/249))
- shared helpers + migrate hardcoded sites ([#245](https://github.com/jdx/aube/pull/245))

## [1.0.0](https://github.com/jdx/aube/compare/aube-lockfile-v1.0.0-beta.12...aube-lockfile-v1.0.0) - 2026-04-23

### Other

- *(yarn)* drop per-lookup String allocs in berry parser ([#234](https://github.com/jdx/aube/pull/234))
- extract read_lockfile helper to dedupe parser I/O ([#232](https://github.com/jdx/aube/pull/232))

## [1.0.0-beta.12](https://github.com/jdx/aube/compare/aube-lockfile-v1.0.0-beta.11...aube-lockfile-v1.0.0-beta.12) - 2026-04-22

### Other

- *(pnpm)* strip peer-context suffix from URL importer versions ([#214](https://github.com/jdx/aube/pull/214))
- cross-crate dedup pass ([#208](https://github.com/jdx/aube/pull/208))
- *(pnpm)* prefer pnpm version field for url-keyed transitives ([#204](https://github.com/jdx/aube/pull/204))
- cross-crate security hardening ([#202](https://github.com/jdx/aube/pull/202))
- *(npm)* parse workspace link entries ([#198](https://github.com/jdx/aube/pull/198))
- cross-crate correctness and security fixes ([#196](https://github.com/jdx/aube/pull/196))

## [1.0.0-beta.11](https://github.com/jdx/aube/compare/aube-lockfile-v1.0.0-beta.10...aube-lockfile-v1.0.0-beta.11) - 2026-04-21

### Other

- warm-install speedup ([#177](https://github.com/jdx/aube/pull/177))
- short-circuit bin linking on packages with no bin metadata ([#192](https://github.com/jdx/aube/pull/192))

## [1.0.0-beta.10](https://github.com/jdx/aube/compare/aube-lockfile-v1.0.0-beta.9...aube-lockfile-v1.0.0-beta.10) - 2026-04-21

### Fixed

- pnpm-workspace.yaml overrides/patches, npm: alias overrides, cross-platform pnpm-lock ([#175](https://github.com/jdx/aube/pull/175))

### Other

- honor pnpm-workspace.yaml supportedArchitectures, ignoredOptionalDependencies, pnpmfilePath ([#181](https://github.com/jdx/aube/pull/181))
- render parse errors with miette source span ([#166](https://github.com/jdx/aube/pull/166))
- *(bun)* emit version, bin, optionalPeers on non-root workspaces ([#169](https://github.com/jdx/aube/pull/169))

## [1.0.0-beta.8](https://github.com/jdx/aube/compare/aube-lockfile-v1.0.0-beta.7...aube-lockfile-v1.0.0-beta.8) - 2026-04-20

### Other

- default to ~/.local/share/aube/store per XDG spec ([#129](https://github.com/jdx/aube/pull/129))
- *(npm)* tolerate legacy array engines field ([#132](https://github.com/jdx/aube/pull/132))
- *(npm)* accept string and array funding shapes ([#133](https://github.com/jdx/aube/pull/133))

## [1.0.0-beta.7](https://github.com/jdx/aube/compare/aube-lockfile-v1.0.0-beta.6...aube-lockfile-v1.0.0-beta.7) - 2026-04-19

### Other

- pnpm compat: multi-document lockfile + override over npm-alias ([#116](https://github.com/jdx/aube/pull/116))
- *(pnpm)* normalize empty-string root importer key ([#121](https://github.com/jdx/aube/pull/121))
- byte-identical pnpm-lock.yaml / bun.lock on re-emit ([#107](https://github.com/jdx/aube/pull/107))
- classify bare http(s) URLs as tarballs ([#114](https://github.com/jdx/aube/pull/114))
- *(bun)* emit and parse non-root workspaces ([#104](https://github.com/jdx/aube/pull/104))

## [1.0.0-beta.6](https://github.com/jdx/aube/compare/aube-lockfile-v1.0.0-beta.5...aube-lockfile-v1.0.0-beta.6) - 2026-04-19

### Other

- match pnpm ignored optionals order ([#90](https://github.com/jdx/aube/pull/90))

## [1.0.0-beta.5](https://github.com/jdx/aube/compare/aube-lockfile-v1.0.0-beta.4...aube-lockfile-v1.0.0-beta.5) - 2026-04-19

### Other

- normalize git selector fragments ([#62](https://github.com/jdx/aube/pull/62))

## [1.0.0-beta.3](https://github.com/jdx/aube/compare/aube-lockfile-v1.0.0-beta.2...aube-lockfile-v1.0.0-beta.3) - 2026-04-19

### Other

- *(bun)* handle github/git 3-tuple package entries ([#42](https://github.com/jdx/aube/pull/42))
- preserve npm-alias as folder name on fresh resolve ([#37](https://github.com/jdx/aube/pull/37))
- *(npm)* resolve peer deps when installing from package-lock.json ([#35](https://github.com/jdx/aube/pull/35))
- *(npm)* support npm:<real>@<ver> aliases + fix dep_path tail ([#30](https://github.com/jdx/aube/pull/30))
- Parse pnpm snapshot optional dependencies ([#18](https://github.com/jdx/aube/pull/18))

## [1.0.0-beta.2](https://github.com/jdx/aube/compare/aube-lockfile-v1.0.0-beta.1...aube-lockfile-v1.0.0-beta.2) - 2026-04-18

### Other

- aube-cli crate -> aube ([#7](https://github.com/jdx/aube/pull/7))
