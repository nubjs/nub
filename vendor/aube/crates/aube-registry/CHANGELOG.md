# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.22.0](https://github.com/jdx/aube/compare/aube-registry-v1.21.0...aube-registry-v1.22.0) - 2026-06-17

### Added

- *(registry)* support scope-specific auth tokens ([#899](https://github.com/jdx/aube/pull/899))

### Fixed

- *(install)* verify tarball urls against packuments ([#905](https://github.com/jdx/aube/pull/905))
- *(embedder)* honor the embedder profile in the install banner and cache/name sites ([#888](https://github.com/jdx/aube/pull/888))

### Other

- refresh benchmarks for v1.21.0 ([#890](https://github.com/jdx/aube/pull/890))

## [1.21.0](https://github.com/jdx/aube/compare/aube-registry-v1.20.0...aube-registry-v1.21.0) - 2026-06-13

### Fixed

- *(packaging)* restore endevco npm scope ([#887](https://github.com/jdx/aube/pull/887))

## [1.20.0](https://github.com/jdx/aube/compare/aube-registry-v1.19.0...aube-registry-v1.20.0) - 2026-06-13

### Added

- embeddable Embedder profile (compile-time pluggability) ([#862](https://github.com/jdx/aube/pull/862))

### Fixed

- *(registry)* honor pnpm url-scoped auth env ([#863](https://github.com/jdx/aube/pull/863))

### Other

- link to all sponsors ([#876](https://github.com/jdx/aube/pull/876))
- refresh benchmarks for v1.19.0 ([#866](https://github.com/jdx/aube/pull/866))

## [1.19.0](https://github.com/jdx/aube/compare/aube-registry-v1.18.2...aube-registry-v1.19.0) - 2026-06-11

### Added

- *(runtime)* node version switching and aube self-version management ([#861](https://github.com/jdx/aube/pull/861))

### Fixed

- *(registry)* keep project npmrc env refs literal ([#856](https://github.com/jdx/aube/pull/856))

### Other

- refresh benchmarks for v1.18.2 ([#851](https://github.com/jdx/aube/pull/851))

## [1.18.2](https://github.com/jdx/aube/compare/aube-registry-v1.18.1...aube-registry-v1.18.2) - 2026-06-08

### Other

- migrate project links to jdx ([#845](https://github.com/jdx/aube/pull/845))

## [1.18.1](https://github.com/jdx/aube/compare/aube-registry-v1.18.0...aube-registry-v1.18.1) - 2026-06-07

### Other

- refresh benchmarks for v1.18.0 ([#841](https://github.com/jdx/aube/pull/841))

## [1.18.0](https://github.com/jdx/aube/compare/aube-registry-v1.17.1...aube-registry-v1.18.0) - 2026-06-04

### Added

- add sponsors command ([#824](https://github.com/jdx/aube/pull/824))

### Other

- refresh benchmarks for v1.17.1 ([#820](https://github.com/jdx/aube/pull/820))

## [1.17.1](https://github.com/jdx/aube/compare/aube-registry-v1.17.0...aube-registry-v1.17.1) - 2026-05-31

### Other

- *(ci)* switch back to namespace runners ([#819](https://github.com/jdx/aube/pull/819))

## [1.17.0](https://github.com/jdx/aube/compare/aube-registry-v1.16.1...aube-registry-v1.17.0) - 2026-05-31

### Added

- *(resolver)* trust staged publishes ([#810](https://github.com/jdx/aube/pull/810))

### Fixed

- *(dist-tag)* support otp writes ([#811](https://github.com/jdx/aube/pull/811))

### Other

- *(ci)* switch to github-hosted runners ([#814](https://github.com/jdx/aube/pull/814))
- refresh benchmarks for v1.16.1 ([#808](https://github.com/jdx/aube/pull/808))

## [1.16.1](https://github.com/jdx/aube/compare/aube-registry-v1.16.0...aube-registry-v1.16.1) - 2026-05-29

### Other

- refresh benchmarks for v1.16.0 ([#787](https://github.com/jdx/aube/pull/787))

### Security

- *(registry)* scope unqualified credentials ([#801](https://github.com/jdx/aube/pull/801))

## [1.16.0](https://github.com/jdx/aube/compare/aube-registry-v1.15.0...aube-registry-v1.16.0) - 2026-05-25

### Fixed

- *(publish)* support npm trusted publishing auth ([#763](https://github.com/jdx/aube/pull/763))

### Other

- *(deps)* bump hickory dns stack ([#780](https://github.com/jdx/aube/pull/780))
- refresh benchmarks for v1.15.0 ([#750](https://github.com/jdx/aube/pull/750))
- *(registry)* split client module ([#742](https://github.com/jdx/aube/pull/742))
- *(registry)* split config module ([#737](https://github.com/jdx/aube/pull/737))

## [1.15.0](https://github.com/jdx/aube/compare/aube-registry-v1.14.1...aube-registry-v1.15.0) - 2026-05-17

### Other

- refresh benchmarks for v1.14.1 ([#721](https://github.com/jdx/aube/pull/721))
- *(registry)* split registry client modules ([#706](https://github.com/jdx/aube/pull/706))

## [1.14.0](https://github.com/jdx/aube/compare/aube-registry-v1.13.1...aube-registry-v1.14.0) - 2026-05-14

### Added

- *(install)* add OSV bloom-filter prefilter for lockfile installs ([#680](https://github.com/jdx/aube/pull/680))

### Fixed

- *(registry)* confirm OSV hits against affected versions ([#689](https://github.com/jdx/aube/pull/689))

### Other

- refresh benchmarks for v1.13.1 ([#687](https://github.com/jdx/aube/pull/687))

## [1.13.1](https://github.com/jdx/aube/compare/aube-registry-v1.13.0...aube-registry-v1.13.1) - 2026-05-14

### Fixed

- *(install)* pass version to OSV in transitive MAL-* check ([#682](https://github.com/jdx/aube/pull/682))

## [1.13.0](https://github.com/jdx/aube/compare/aube-registry-v1.12.0...aube-registry-v1.13.0) - 2026-05-13

### Added

- *(install)* route OSV checks live-API vs local mirror by fresh-resolution ([#678](https://github.com/jdx/aube/pull/678))
- *(add)* skip supply-chain gates on private packages + allowlist globs ([#673](https://github.com/jdx/aube/pull/673))
- *(add)* block malicious packages via OSV + prompt on low downloads ([#656](https://github.com/jdx/aube/pull/656))

### Other

- *(registry)* single-flight concurrent packument fetches per name ([#651](https://github.com/jdx/aube/pull/651))
- refresh benchmarks for v1.12.0 ([#625](https://github.com/jdx/aube/pull/625))

## [1.12.0](https://github.com/jdx/aube/compare/aube-registry-v1.11.0...aube-registry-v1.12.0) - 2026-05-12

### Added

- *(install)* polish install progress display ([#616](https://github.com/jdx/aube/pull/616))

### Other

- *(registry)* rewrite tolerant packument deserializers as Visitors ([#641](https://github.com/jdx/aube/pull/641))
- refresh benchmarks for v1.11.0 ([#622](https://github.com/jdx/aube/pull/622))

## [1.11.0](https://github.com/jdx/aube/compare/aube-registry-v1.10.4...aube-registry-v1.11.0) - 2026-05-11

### Added

- *(config)* scope-split settings precedence; project config.toml support ([#608](https://github.com/jdx/aube/pull/608))

### Fixed

- *(registry)* coalesce slow-metadata warnings into one resolve summary ([#592](https://github.com/jdx/aube/pull/592))

### Other

- refresh benchmarks for v1.10.4 ([#600](https://github.com/jdx/aube/pull/600))

## [1.10.4](https://github.com/jdx/aube/compare/aube-registry-v1.10.3...aube-registry-v1.10.4) - 2026-05-11

### Fixed

- *(registry)* retry initial request in start_tarball_stream ([#591](https://github.com/jdx/aube/pull/591))

## [1.10.1](https://github.com/jdx/aube/compare/aube-registry-v1.10.0...aube-registry-v1.10.1) - 2026-05-10

### Other

- refresh benchmarks for v1.10.0 ([#571](https://github.com/jdx/aube/pull/571))
- *(registry)* swap simd-json for sonic-rs on packument hot path ([#569](https://github.com/jdx/aube/pull/569))
- *(registry)* drop deep clone and fsync from packument cache writes ([#568](https://github.com/jdx/aube/pull/568))
- refresh benchmarks for v1.10.0 ([#566](https://github.com/jdx/aube/pull/566))

## [1.10.0](https://github.com/jdx/aube/compare/aube-registry-v1.9.1...aube-registry-v1.10.0) - 2026-05-10

### Added

- *(diag)* instrument install and add aube diag subcommand ([#547](https://github.com/jdx/aube/pull/547))

### Fixed

- *(registry)* accept duplicate bundle/bundledDependencies in payloads ([#544](https://github.com/jdx/aube/pull/544))
- *(registry)* honor top-level cafile/ca in .npmrc ([#542](https://github.com/jdx/aube/pull/542))

### Other

- refresh benchmarks for v1.9.1 ([#555](https://github.com/jdx/aube/pull/555))
- lead hero with auto-install promise over speed ([#557](https://github.com/jdx/aube/pull/557))
- *(install)* adaptive limiter + tarball http1 split ([#548](https://github.com/jdx/aube/pull/548))
- refresh benchmarks for v1.9.1 ([#534](https://github.com/jdx/aube/pull/534))
- refresh benchmarks for v1.9.0 ([#532](https://github.com/jdx/aube/pull/532))

## [1.9.1](https://github.com/jdx/aube/compare/aube-registry-v1.9.0...aube-registry-v1.9.1) - 2026-05-06

### Added

- *(install)* aube-util::http module + pre-resolver prefetch + cold-path optimizations ([#529](https://github.com/jdx/aube/pull/529))

### Fixed

- *(resolver)* fetch registry on primer range miss ([#531](https://github.com/jdx/aube/pull/531))
- *(registry)* expand env vars in npmrc keys ([#521](https://github.com/jdx/aube/pull/521))

### Other

- refresh benchmarks for v1.9.0 ([#525](https://github.com/jdx/aube/pull/525))
- cold install pipeline overhaul ([#522](https://github.com/jdx/aube/pull/522))

## [1.9.0](https://github.com/jdx/aube/compare/aube-registry-v1.8.0...aube-registry-v1.9.0) - 2026-05-05

### Other

- refresh benchmarks for v1.8.0 ([#508](https://github.com/jdx/aube/pull/508))

## [1.8.0](https://github.com/jdx/aube/compare/aube-registry-v1.7.0...aube-registry-v1.8.0) - 2026-05-03

### Added

- *(progress)* redesign install progress UI ([#501](https://github.com/jdx/aube/pull/501))
- *(run)* prefer local bins for run and dlx ([#502](https://github.com/jdx/aube/pull/502))
- *(codes)* introduce ERR_AUBE_/WARN_AUBE_ codes, exit codes, dep chains ([#492](https://github.com/jdx/aube/pull/492))

### Other

- refresh benchmarks for v1.7.0 ([#490](https://github.com/jdx/aube/pull/490))

## [1.7.0](https://github.com/jdx/aube/compare/aube-registry-v1.6.2...aube-registry-v1.7.0) - 2026-05-03

### Other

- refresh benchmarks for v1.6.2 ([#474](https://github.com/jdx/aube/pull/474))
- streaming sha512, parallel cas, tls prewarm, fetch reorder ([#469](https://github.com/jdx/aube/pull/469))
- refresh benchmarks for v1.6.2 ([#467](https://github.com/jdx/aube/pull/467))

## [1.6.1](https://github.com/jdx/aube/compare/aube-registry-v1.6.0...aube-registry-v1.6.1) - 2026-05-01

### Other

- refresh benchmarks for v1.5.2 ([#459](https://github.com/jdx/aube/pull/459))

## [1.6.0](https://github.com/jdx/aube/compare/aube-registry-v1.5.2...aube-registry-v1.6.0) - 2026-05-01

### Other

- cache hot-path work across install, resolver, and registry ([#453](https://github.com/jdx/aube/pull/453))
- refresh benchmarks for v1.5.2 ([#452](https://github.com/jdx/aube/pull/452))
- dedupe and cache hot-path work in install and resolver ([#449](https://github.com/jdx/aube/pull/449))
- refresh benchmarks for v1.5.2 ([#448](https://github.com/jdx/aube/pull/448))
- refresh benchmarks for v1.5.1 ([#426](https://github.com/jdx/aube/pull/426))

## [1.5.2](https://github.com/jdx/aube/compare/aube-registry-v1.5.1...aube-registry-v1.5.2) - 2026-04-30

### Other

- *(resolver)* add bundled metadata primer ([#397](https://github.com/jdx/aube/pull/397))
- thank Namespace for GitHub Actions runner support ([#412](https://github.com/jdx/aube/pull/412))
- refresh benchmarks for v1.5.1 ([#392](https://github.com/jdx/aube/pull/392))

## [1.5.0](https://github.com/jdx/aube/compare/aube-registry-v1.4.0...aube-registry-v1.5.0) - 2026-04-29

### Fixed

- *(resolver)* require structured trust evidence ([#379](https://github.com/jdx/aube/pull/379))

## [1.4.0](https://github.com/jdx/aube/compare/aube-registry-v1.3.0...aube-registry-v1.4.0) - 2026-04-28

### Fixed

- *(registry)* request identity encoding for tarballs ([#356](https://github.com/jdx/aube/pull/356))
- *(packaging)* include README on published aube crate ([#349](https://github.com/jdx/aube/pull/349))

### Other

- warn about npm install caveats ([#368](https://github.com/jdx/aube/pull/368))

## [1.3.0](https://github.com/jdx/aube/compare/aube-registry-v1.2.1...aube-registry-v1.3.0) - 2026-04-27

### Added

- *(security)* enforce trustPolicy by default, add paranoid bundle, security docs ([#333](https://github.com/jdx/aube/pull/333))

### Fixed

- *(lockfile)* parse scalar pnpm platform fields ([#337](https://github.com/jdx/aube/pull/337))
- *(registry)* surface retry warnings and cap timeout retries at 1 ([#331](https://github.com/jdx/aube/pull/331))

### Other

- *(deps)* replace serde_yaml with yaml_serde ([#340](https://github.com/jdx/aube/pull/340))

## [1.2.1](https://github.com/jdx/aube/compare/aube-registry-v1.2.0...aube-registry-v1.2.1) - 2026-04-26

### Fixed

- *(registry)* raise fetch timeout default ([#323](https://github.com/jdx/aube/pull/323))

### Other

- *(resolver)* avoid full packuments for aged metadata ([#314](https://github.com/jdx/aube/pull/314))

## [1.2.0](https://github.com/jdx/aube/compare/aube-registry-v1.1.0...aube-registry-v1.2.0) - 2026-04-25

### Added

- *(registry)* make packument + tarball body caps configurable, raise packument default to 200 MiB ([#282](https://github.com/jdx/aube/pull/282))

### Fixed

- cross-platform install correctness pass ([#293](https://github.com/jdx/aube/pull/293))

### Security

- cve-class hardening across linker, registry, resolver, install ([#296](https://github.com/jdx/aube/pull/296))

## [1.1.0](https://github.com/jdx/aube/compare/aube-registry-v1.0.0...aube-registry-v1.1.0) - 2026-04-24

### Other

- dedup pass + registry/store perf wave ([#254](https://github.com/jdx/aube/pull/254))
- shared helpers + migrate hardcoded sites ([#245](https://github.com/jdx/aube/pull/245))

## [1.0.0-beta.12](https://github.com/jdx/aube/compare/aube-registry-v1.0.0-beta.11...aube-registry-v1.0.0-beta.12) - 2026-04-22

### Other

- cross-crate dedup pass ([#208](https://github.com/jdx/aube/pull/208))
- cross-crate security hardening ([#202](https://github.com/jdx/aube/pull/202))
- cross-crate correctness and security fixes ([#196](https://github.com/jdx/aube/pull/196))

## [1.0.0-beta.11](https://github.com/jdx/aube/compare/aube-registry-v1.0.0-beta.10...aube-registry-v1.0.0-beta.11) - 2026-04-21

### Other

- retry cold fetch body decode errors ([#189](https://github.com/jdx/aube/pull/189))

## [1.0.0-beta.10](https://github.com/jdx/aube/compare/aube-registry-v1.0.0-beta.9...aube-registry-v1.0.0-beta.10) - 2026-04-21

### Fixed

- close remaining audit findings across registry, store, and linker ([#164](https://github.com/jdx/aube/pull/164))

### Other

- strip matched surrounding quotes from .npmrc values ([#182](https://github.com/jdx/aube/pull/182))
- parse cached full packuments directly ([#184](https://github.com/jdx/aube/pull/184))
- increase packument cache ttl for repeat installs ([#173](https://github.com/jdx/aube/pull/173))

## [1.0.0-beta.9](https://github.com/jdx/aube/compare/aube-registry-v1.0.0-beta.8...aube-registry-v1.0.0-beta.9) - 2026-04-20

### Other

- short-circuit warm path when install-state matches ([#127](https://github.com/jdx/aube/pull/127))
- tolerate string engines metadata ([#150](https://github.com/jdx/aube/pull/150))

## [1.0.0-beta.8](https://github.com/jdx/aube/compare/aube-registry-v1.0.0-beta.7...aube-registry-v1.0.0-beta.8) - 2026-04-20

### Other

- quiet retry warnings; settings: kebab-case gvs npmrc aliases ([#139](https://github.com/jdx/aube/pull/139))
- tolerate legacy array engines shape in packuments ([#138](https://github.com/jdx/aube/pull/138))
- *(auth)* longest-prefix .npmrc lookup with default-port stripping ([#131](https://github.com/jdx/aube/pull/131))
- honor NPM_CONFIG_USERCONFIG for user-level .npmrc path ([#130](https://github.com/jdx/aube/pull/130))

## [1.0.0-beta.7](https://github.com/jdx/aube/compare/aube-registry-v1.0.0-beta.6...aube-registry-v1.0.0-beta.7) - 2026-04-19

### Other

- byte-identical pnpm-lock.yaml / bun.lock on re-emit ([#107](https://github.com/jdx/aube/pull/107))
- registry + install: tolerate napi-rs packuments and warn on ignored builds ([#113](https://github.com/jdx/aube/pull/113))

## [1.0.0-beta.6](https://github.com/jdx/aube/compare/aube-registry-v1.0.0-beta.5...aube-registry-v1.0.0-beta.6) - 2026-04-19

### Other

- gate slow-tarball warning on elapsed > 1s to match pnpm ([#93](https://github.com/jdx/aube/pull/93))
- gate tokenHelper to user scope and sanitize the value ([#89](https://github.com/jdx/aube/pull/89))
- tolerate object-valued dep-map entries in packuments ([#92](https://github.com/jdx/aube/pull/92))
- url-encode scoped names and expand packument accept header ([#83](https://github.com/jdx/aube/pull/83))
- tolerate null values in packument string maps ([#76](https://github.com/jdx/aube/pull/76))

## [1.0.0-beta.3](https://github.com/jdx/aube/compare/aube-registry-v1.0.0-beta.2...aube-registry-v1.0.0-beta.3) - 2026-04-19

### Added

- *(cli)* support jsr: specifier protocol ([#19](https://github.com/jdx/aube/pull/19))

### Other

- honor npm_config_* env vars in NpmConfig::load ([#47](https://github.com/jdx/aube/pull/47))

## [1.0.0-beta.2](https://github.com/jdx/aube/compare/aube-registry-v1.0.0-beta.1...aube-registry-v1.0.0-beta.2) - 2026-04-18

### Other

- use cross + rustls-tls for linux targets ([#15](https://github.com/jdx/aube/pull/15))
- aube-cli crate -> aube ([#7](https://github.com/jdx/aube/pull/7))
