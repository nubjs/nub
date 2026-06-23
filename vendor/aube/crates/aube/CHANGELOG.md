# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.22.0](https://github.com/jdx/aube/compare/v1.21.0...v1.22.0) - 2026-06-17

### Added

- *(registry)* support scope-specific auth tokens ([#899](https://github.com/jdx/aube/pull/899))

### Fixed

- *(install)* verify tarball urls against packuments ([#905](https://github.com/jdx/aube/pull/905))
- *(embedder)* honor the embedder profile in the install banner and cache/name sites ([#888](https://github.com/jdx/aube/pull/888))
- *(install)* close pnpm-lock.yaml parity and re-resolution gaps ([#896](https://github.com/jdx/aube/pull/896))
- *(install)* keep nested pnpm-workspace.yaml as a hard workspace boundary ([#889](https://github.com/jdx/aube/pull/889))
- *(lockfile)* close pnpm-lock.yaml formatting and field parity gaps ([#893](https://github.com/jdx/aube/pull/893))
- *(install)* repair member installs under sharedWorkspaceLockfile=false ([#891](https://github.com/jdx/aube/pull/891))

### Other

- *(commands)* return exit codes instead of process::exit (lib-embed safety) ([#897](https://github.com/jdx/aube/pull/897))
- refresh benchmarks for v1.21.0 ([#890](https://github.com/jdx/aube/pull/890))

## [1.21.0](https://github.com/jdx/aube/compare/v1.20.0...v1.21.0) - 2026-06-13

### Added

- *(lockfile)* emit packageExtensionsChecksum and pnpmfileChecksum for pnpm parity ([#883](https://github.com/jdx/aube/pull/883))

### Fixed

- *(install)* map peer-suffixed source deps to their canonical store index ([#885](https://github.com/jdx/aube/pull/885))
- *(install)* write the root workspace lockfile under sharedWorkspaceLockfile=false ([#882](https://github.com/jdx/aube/pull/882))
- *(packaging)* restore endevco npm scope ([#887](https://github.com/jdx/aube/pull/887))
- *(install)* stop double-counting link deps in the progress bar ([#884](https://github.com/jdx/aube/pull/884))

## [1.20.0](https://github.com/jdx/aube/compare/v1.19.0...v1.20.0) - 2026-06-13

### Added

- *(scripts)* match pnpm's full npm_* env for lifecycle & run scripts ([#879](https://github.com/jdx/aube/pull/879))
- embeddable Embedder profile (compile-time pluggability) ([#862](https://github.com/jdx/aube/pull/862))

### Fixed

- *(install)* preserve each member's lockfile format under sharedWorkspaceLockfile=false ([#880](https://github.com/jdx/aube/pull/880))
- *(linker)* resolve git deps in global virtual store ([#857](https://github.com/jdx/aube/pull/857))

### Other

- link to all sponsors ([#876](https://github.com/jdx/aube/pull/876))
- refresh benchmarks for v1.19.0 ([#866](https://github.com/jdx/aube/pull/866))

## [1.19.0](https://github.com/jdx/aube/compare/v1.18.2...v1.19.0) - 2026-06-11

### Added

- *(runtime)* node version switching and aube self-version management ([#861](https://github.com/jdx/aube/pull/861))

### Fixed

- *(scripts)* match URL source build approvals ([#860](https://github.com/jdx/aube/pull/860))
- *(scripts)* require source keys for build approvals ([#858](https://github.com/jdx/aube/pull/858))
- *(install)* warn on deprecated override refs ([#859](https://github.com/jdx/aube/pull/859))

### Other

- refresh benchmarks for v1.18.2 ([#851](https://github.com/jdx/aube/pull/851))

## [1.18.2](https://github.com/jdx/aube/compare/v1.18.1...v1.18.2) - 2026-06-08

### Other

- migrate project links to jdx ([#845](https://github.com/jdx/aube/pull/845))

## [1.18.1](https://github.com/jdx/aube/compare/v1.18.0...v1.18.1) - 2026-06-07

### Fixed

- *(install)* regenerate conflicted lockfiles ([#843](https://github.com/jdx/aube/pull/843))
- *(update)* support global updates ([#840](https://github.com/jdx/aube/pull/840))

### Other

- refresh benchmarks for v1.18.0 ([#841](https://github.com/jdx/aube/pull/841))

### Security

- *(install)* verify lockfile tarball URLs ([#842](https://github.com/jdx/aube/pull/842))

## [1.18.0](https://github.com/jdx/aube/compare/v1.17.1...v1.18.0) - 2026-06-04

### Added

- add sponsors command ([#824](https://github.com/jdx/aube/pull/824))

### Fixed

- *(publish)* normalize repository field ([#826](https://github.com/jdx/aube/pull/826))

### Other

- refresh benchmarks for v1.17.1 ([#820](https://github.com/jdx/aube/pull/820))

## [1.17.1](https://github.com/jdx/aube/compare/v1.17.0...v1.17.1) - 2026-05-31

### Other

- *(ci)* switch back to namespace runners ([#819](https://github.com/jdx/aube/pull/819))

## [1.17.0](https://github.com/jdx/aube/compare/v1.16.1...v1.17.0) - 2026-05-31

### Added

- *(linker)* add hoisting limits ([#809](https://github.com/jdx/aube/pull/809))
- *(resolver)* trust staged publishes ([#810](https://github.com/jdx/aube/pull/810))

### Fixed

- *(dist-tag)* support otp writes ([#811](https://github.com/jdx/aube/pull/811))

### Other

- *(deps)* bump sha2 from 0.10.9 to 0.11.0 ([#790](https://github.com/jdx/aube/pull/790))
- *(ci)* switch to github-hosted runners ([#814](https://github.com/jdx/aube/pull/814))
- refresh benchmarks for v1.16.1 ([#808](https://github.com/jdx/aube/pull/808))
- *(deps)* bump toml from 0.8.23 to 1.1.2+spec-1.1.0 ([#796](https://github.com/jdx/aube/pull/796))

## [1.16.1](https://github.com/jdx/aube/compare/v1.16.0...v1.16.1) - 2026-05-29

### Fixed

- *(publish)* normalize semver metadata ([#806](https://github.com/jdx/aube/pull/806))
- *(add)* accept linkWorkspacePackages deep ([#799](https://github.com/jdx/aube/pull/799))

### Other

- *(deps)* bump sha1 from 0.10.6 to 0.11.0 ([#797](https://github.com/jdx/aube/pull/797))
- refresh benchmarks for v1.16.0 ([#787](https://github.com/jdx/aube/pull/787))

### Security

- *(linker)* reject unsafe package aliases ([#800](https://github.com/jdx/aube/pull/800))

## [1.16.0](https://github.com/jdx/aube/compare/v1.15.0...v1.16.0) - 2026-05-25

### Added

- *(pnpm)* catch up with pnpm 11 parity ([#761](https://github.com/jdx/aube/pull/761))
- *(publish)* add npm stage fallback ([#762](https://github.com/jdx/aube/pull/762))

### Fixed

- *(resolver)* pin hosted git tarball integrity ([#783](https://github.com/jdx/aube/pull/783))
- *(publish)* prompt for OTP on registry challenge ([#767](https://github.com/jdx/aube/pull/767))
- *(publish)* support npm trusted publishing auth ([#763](https://github.com/jdx/aube/pull/763))
- *(update)* include root package in workspace version map ([#757](https://github.com/jdx/aube/pull/757))
- *(lockfile)* avoid lossy npm metadata drift rewrites ([#753](https://github.com/jdx/aube/pull/753))

### Other

- *(deps)* bump hickory dns stack ([#780](https://github.com/jdx/aube/pull/780))
- clarify prune commands ([#758](https://github.com/jdx/aube/pull/758))
- refresh benchmarks for v1.15.0 ([#750](https://github.com/jdx/aube/pull/750))
- *(commands)* split command-wide helpers ([#749](https://github.com/jdx/aube/pull/749))
- *(deploy)* split deploy.rs into staging/injection/rewrite/filtering submodules ([#748](https://github.com/jdx/aube/pull/748))
- *(add)* split command flow helpers ([#746](https://github.com/jdx/aube/pull/746))
- split main startup helpers ([#744](https://github.com/jdx/aube/pull/744))
- *(add)* split add.rs into spec/manifest/global submodules ([#740](https://github.com/jdx/aube/pull/740))
- *(install)* split orchestration setup ([#739](https://github.com/jdx/aube/pull/739))

## [1.15.0](https://github.com/jdx/aube/compare/v1.14.1...v1.15.0) - 2026-05-17

### Added

- *(yarn)* support berry portal and exec protocols ([#729](https://github.com/jdx/aube/pull/729))
- *(add)* add deny-build flag ([#730](https://github.com/jdx/aube/pull/730))
- *(yarn)* support berry patch protocol ([#728](https://github.com/jdx/aube/pull/728))

### Fixed

- *(update)* keep member updates on workspace lockfile ([#732](https://github.com/jdx/aube/pull/732))
- *(bun)* apply top-level patchedDependencies ([#724](https://github.com/jdx/aube/pull/724))

### Other

- refresh benchmarks for v1.14.1 ([#721](https://github.com/jdx/aube/pull/721))
- *(install)* split resolve phase ([#715](https://github.com/jdx/aube/pull/715))
- *(install)* split finalize phase ([#712](https://github.com/jdx/aube/pull/712))
- *(install)* split link phase ([#711](https://github.com/jdx/aube/pull/711))
- *(install)* split layout resolution ([#708](https://github.com/jdx/aube/pull/708))

## [1.14.1](https://github.com/jdx/aube/compare/v1.14.0...v1.14.1) - 2026-05-15

### Other

- *(install)* split fetch pipeline ([#704](https://github.com/jdx/aube/pull/704))
- *(install)* split pipeline modules ([#702](https://github.com/jdx/aube/pull/702))
- *(install)* split helper modules ([#698](https://github.com/jdx/aube/pull/698))

## [1.14.0](https://github.com/jdx/aube/compare/v1.13.1...v1.14.0) - 2026-05-14

### Added

- *(install)* add OSV bloom-filter prefilter for lockfile installs ([#680](https://github.com/jdx/aube/pull/680))
- *(install)* content-sniff dep lifecycle scripts before approve-builds ([#685](https://github.com/jdx/aube/pull/685))

### Other

- refresh benchmarks for v1.13.1 ([#687](https://github.com/jdx/aube/pull/687))

## [1.13.1](https://github.com/jdx/aube/compare/v1.13.0...v1.13.1) - 2026-05-14

### Fixed

- *(install)* pass version to OSV in transitive MAL-* check ([#682](https://github.com/jdx/aube/pull/682))

## [1.13.0](https://github.com/jdx/aube/compare/v1.12.0...v1.13.0) - 2026-05-13

### Added

- *(install)* route OSV checks live-API vs local mirror by fresh-resolution ([#678](https://github.com/jdx/aube/pull/678))
- *(add)* skip supply-chain gates on private packages + allowlist globs ([#673](https://github.com/jdx/aube/pull/673))
- *(install)* bun-compatible security scanner ([#657](https://github.com/jdx/aube/pull/657))
- *(add)* block malicious packages via OSV + prompt on low downloads ([#656](https://github.com/jdx/aube/pull/656))
- *(install)* stop auto-seeding allowBuilds placeholders in package.json ([#662](https://github.com/jdx/aube/pull/662))

### Fixed

- *(add)* clarify --allow-build help and let it flip an existing deny ([#660](https://github.com/jdx/aube/pull/660))
- *(linker)* anchor windows bin-shim relative path on the surface tree ([#659](https://github.com/jdx/aube/pull/659))
- *(global)* unlink hash pointer via remove_dir fallback on Windows ([#658](https://github.com/jdx/aube/pull/658))

### Other

- *(install)* delete pre-resolver direct-dep packument prefetch ([#672](https://github.com/jdx/aube/pull/672))
- refresh benchmarks for v1.12.0 ([#625](https://github.com/jdx/aube/pull/625))

## [1.12.0](https://github.com/jdx/aube/compare/v1.11.0...v1.12.0) - 2026-05-12

### Added

- *(config)* scope .npmrc to npm-shared keys, route aube settings to config.toml, support dotted map writes ([#634](https://github.com/jdx/aube/pull/634))
- *(install)* polish install progress display ([#616](https://github.com/jdx/aube/pull/616))

### Fixed

- *(install)* hoist bun.lock peer-only packages before GC ([#639](https://github.com/jdx/aube/pull/639))
- *(update)* correct version lookup for direct deps shadowed by transitives ([#636](https://github.com/jdx/aube/pull/636))
- *(install)* co-locate cached indexes with CAS + verified probe self-heal ([#635](https://github.com/jdx/aube/pull/635))
- *(update)* preserve cross-platform optionals and time entries ([#637](https://github.com/jdx/aube/pull/637))
- *(install)* ignore engines.pnpm to stop spurious version warnings ([#633](https://github.com/jdx/aube/pull/633))
- *(install)* run peer-context pass on bun.lock imports ([#619](https://github.com/jdx/aube/pull/619))
- *(add)* forward --allow-build to global installs ([#620](https://github.com/jdx/aube/pull/620))

### Other

- refresh benchmarks for v1.11.0 ([#622](https://github.com/jdx/aube/pull/622))

## [1.11.0](https://github.com/jdx/aube/compare/v1.10.4...v1.11.0) - 2026-05-11

### Added

- *(outdated, update)* wire `-w/--workspace-root` to retarget cwd at workspace root ([#614](https://github.com/jdx/aube/pull/614))
- *(install)* fill resolving bar against a real denominator ([#611](https://github.com/jdx/aube/pull/611))
- *(config)* scope-split settings precedence; project config.toml support ([#608](https://github.com/jdx/aube/pull/608))
- *(deploy)* accept --offline and --prefer-offline ([#606](https://github.com/jdx/aube/pull/606))

### Fixed

- *(linker)* point bin shim NODE_PATH at the hidden modules dir ([#613](https://github.com/jdx/aube/pull/613))
- address several bugs reported in #602 ([#610](https://github.com/jdx/aube/pull/610))
- *(install)* surface materializer error instead of generic channel-closed message ([#607](https://github.com/jdx/aube/pull/607))
- *(progress)* clamp reused on downward set_total rebase ([#609](https://github.com/jdx/aube/pull/609))
- *(config)* preserve symlinked ~/.config/aube/config.toml on write ([#605](https://github.com/jdx/aube/pull/605))
- *(install)* probe link strategy against the actual destination dir ([#604](https://github.com/jdx/aube/pull/604))
- *(registry)* coalesce slow-metadata warnings into one resolve summary ([#592](https://github.com/jdx/aube/pull/592))

### Other

- *(store)* direct-write CAS fast path on macOS under exclusive install lock ([#615](https://github.com/jdx/aube/pull/615))
- refresh benchmarks for v1.10.4 ([#600](https://github.com/jdx/aube/pull/600))

## [1.10.3](https://github.com/jdx/aube/compare/v1.10.2...v1.10.3) - 2026-05-10

### Other

- update Cargo.lock dependencies

## [1.10.1](https://github.com/jdx/aube/compare/v1.10.0...v1.10.1) - 2026-05-10

### Fixed

- *(deploy)* resolve catalog: refs and accept versionless packages ([#574](https://github.com/jdx/aube/pull/574))
- *(install)* pad pkg counts and drop ETA placeholder in progress UI ([#570](https://github.com/jdx/aube/pull/570))

### Other

- refresh benchmarks for v1.10.0 ([#571](https://github.com/jdx/aube/pull/571))
- refresh benchmarks for v1.10.0 ([#566](https://github.com/jdx/aube/pull/566))

## [1.10.0](https://github.com/jdx/aube/compare/v1.9.1...v1.10.0) - 2026-05-10

### Added

- *(install)* show post-install dependency summary ([#559](https://github.com/jdx/aube/pull/559))
- *(update)* add --lockfile-only flag ([#560](https://github.com/jdx/aube/pull/560))
- *(cli)* finish recursive-run flags and parallel output ([#545](https://github.com/jdx/aube/pull/545))
- *(diag)* instrument install and add aube diag subcommand ([#547](https://github.com/jdx/aube/pull/547))
- *(add)* linkWorkspacePackages + saveWorkspaceProtocol ([#539](https://github.com/jdx/aube/pull/539))

### Fixed

- *(workspace)* three workspace install correctness fixes from pnpm test port ([#564](https://github.com/jdx/aube/pull/564))
- *(update)* keep filtered workspace lockfile at root ([#558](https://github.com/jdx/aube/pull/558))
- *(pnpmfile)* fail install when readPackage returns non-object ([#562](https://github.com/jdx/aube/pull/562))
- *(workspace)* include root in filtered runs ([#556](https://github.com/jdx/aube/pull/556))
- *(update)* add interactive picker ([#552](https://github.com/jdx/aube/pull/552))
- *(deploy)* preserve filtered packages missing version ([#549](https://github.com/jdx/aube/pull/549))
- *(install)* inherit build approvals for git prepare ([#546](https://github.com/jdx/aube/pull/546))
- *(cli)* skip verify-deps inside lifecycle scripts ([#538](https://github.com/jdx/aube/pull/538))
- *(install)* require approve-builds selection ([#537](https://github.com/jdx/aube/pull/537))

### Other

- refresh benchmarks for v1.9.1 ([#555](https://github.com/jdx/aube/pull/555))
- lead hero with auto-install promise over speed ([#557](https://github.com/jdx/aube/pull/557))
- add global virtual store page ([#550](https://github.com/jdx/aube/pull/550))
- *(install)* adaptive limiter + tarball http1 split ([#548](https://github.com/jdx/aube/pull/548))
- refresh benchmarks for v1.9.1 ([#534](https://github.com/jdx/aube/pull/534))
- refresh benchmarks for v1.9.0 ([#532](https://github.com/jdx/aube/pull/532))

## [1.9.1](https://github.com/jdx/aube/compare/v1.9.0...v1.9.1) - 2026-05-06

### Added

- *(install)* aube-util::http module + pre-resolver prefetch + cold-path optimizations ([#529](https://github.com/jdx/aube/pull/529))

### Fixed

- *(cli)* skip registry for workspace deps ([#523](https://github.com/jdx/aube/pull/523))
- *(run)* add node-gyp bootstrap to script PATH ([#518](https://github.com/jdx/aube/pull/518))

### Other

- *(install)* pipeline per-project materialize into fetch phase ([#527](https://github.com/jdx/aube/pull/527))
- refresh benchmarks for v1.9.0 ([#525](https://github.com/jdx/aube/pull/525))
- cold install pipeline overhaul ([#522](https://github.com/jdx/aube/pull/522))

## [1.9.0](https://github.com/jdx/aube/compare/v1.8.0...v1.9.0) - 2026-05-05

### Added

- *(config)* store aube settings outside npmrc ([#517](https://github.com/jdx/aube/pull/517))
- *(run)* forward inspect flags to node targets ([#515](https://github.com/jdx/aube/pull/515))
- *(workspace)* preserve comments in workspace yaml edits via yamlpatch ([#511](https://github.com/jdx/aube/pull/511))

### Fixed

- *(deploy)* bundle workspace siblings and file: deps; add --no-prod ([#507](https://github.com/jdx/aube/pull/507))

### Other

- refresh benchmarks for v1.8.0 ([#508](https://github.com/jdx/aube/pull/508))

## [1.8.0](https://github.com/jdx/aube/compare/v1.7.0...v1.8.0) - 2026-05-03

### Added

- *(progress)* redesign install progress UI ([#501](https://github.com/jdx/aube/pull/501))
- *(run)* prefer local bins for run and dlx ([#502](https://github.com/jdx/aube/pull/502))
- *(codes)* introduce ERR_AUBE_/WARN_AUBE_ codes, exit codes, dep chains ([#492](https://github.com/jdx/aube/pull/492))

### Fixed

- *(cli)* why/list/query work from a workspace subpackage ([#504](https://github.com/jdx/aube/pull/504))
- *(install)* handle workspace scripts and pnpm aliases ([#500](https://github.com/jdx/aube/pull/500))
- *(add)* auto-detect local-path specs instead of hitting the registry ([#499](https://github.com/jdx/aube/pull/499))

### Other

- *(cli)* bucket per-command --help by moving cross-cutting flags off global ([#505](https://github.com/jdx/aube/pull/505))
- refresh benchmarks for v1.7.0 ([#490](https://github.com/jdx/aube/pull/490))

## [1.7.0](https://github.com/jdx/aube/compare/v1.6.2...v1.7.0) - 2026-05-03

### Added

- *(cli)* support link: and file: specs in aube add ([#487](https://github.com/jdx/aube/pull/487))
- *(cli)* support yaml-only workspace roots in list/run/install/query/why ([#486](https://github.com/jdx/aube/pull/486))
- *(cli)* support git specs in aube add ([#483](https://github.com/jdx/aube/pull/483))
- *(cli)* rewrite manifest specifier on update without --latest ([#479](https://github.com/jdx/aube/pull/479))
- *(cli)* aube rebuild <pkg> targets a specific package ([#477](https://github.com/jdx/aube/pull/477))
- *(install)* persist unreviewed-builds warning across repeat installs ([#476](https://github.com/jdx/aube/pull/476))
- *(cli)* warn when aube update --depth is set ([#473](https://github.com/jdx/aube/pull/473))

### Fixed

- *(cli)* wrap doc comments so -h help stays one line per flag ([#478](https://github.com/jdx/aube/pull/478))
- *(install)* allow workspace members without `version` field ([#480](https://github.com/jdx/aube/pull/480))
- *(resolver)* resolve nested link:/file: deps from local parents and overrides ([#470](https://github.com/jdx/aube/pull/470))
- *(lockfile)* parse bare user/repo as github shorthand ([#472](https://github.com/jdx/aube/pull/472))

### Other

- refresh benchmarks for v1.6.2 ([#474](https://github.com/jdx/aube/pull/474))
- streaming sha512, parallel cas, tls prewarm, fetch reorder ([#469](https://github.com/jdx/aube/pull/469))
- refresh benchmarks for v1.6.2 ([#467](https://github.com/jdx/aube/pull/467))

## [1.6.2](https://github.com/jdx/aube/compare/v1.6.1...v1.6.2) - 2026-05-01

### Added

- *(cli)* check engines.{aube,pnpm} and workspace per-project engines ([#458](https://github.com/jdx/aube/pull/458))

## [1.6.1](https://github.com/jdx/aube/compare/v1.6.0...v1.6.1) - 2026-05-01

### Other

- refresh benchmarks for v1.5.2 ([#459](https://github.com/jdx/aube/pull/459))

## [1.6.0](https://github.com/jdx/aube/compare/v1.5.1...v1.6.0) - 2026-05-01

### Added

- *(cli)* aube update parses <pkg>@<spec> args + accepts indirect deps ([#446](https://github.com/jdx/aube/pull/446))
- *(cli)* add generic --config.<key>=<value> flags ([#447](https://github.com/jdx/aube/pull/447))
- *(cli)* emit pnpm's verbatim error for empty --allow-build values ([#444](https://github.com/jdx/aube/pull/444))
- *(pnpmfile)* emit ctx.log records as pnpm:hook ndjson on stdout ([#440](https://github.com/jdx/aube/pull/440))
- *(cli)* add --pnpmfile and --global-pnpmfile flags ([#439](https://github.com/jdx/aube/pull/439))
- *(cli)* add --lockfile-dir / lockfileDir setting ([#431](https://github.com/jdx/aube/pull/431))
- *(cli)* add --fetch-timeout / --fetch-retries / retry backoff flags ([#436](https://github.com/jdx/aube/pull/436))
- *(pnpmfile)* wire hooks into update; add preResolution hook ([#423](https://github.com/jdx/aube/pull/423))
- --save-catalog, workspace:* parsing, and sharedWorkspaceLockfile=false ([#418](https://github.com/jdx/aube/pull/418))
- *(cli)* aube add bootstraps package.json + 10 misc.ts ports ([#417](https://github.com/jdx/aube/pull/417))

### Fixed

- *(cli)* honor AUBE_VIRTUAL_STORE_DIR env var + port 5 more pnpm/misc tests ([#456](https://github.com/jdx/aube/pull/456))
- *(cli)* aube update --latest preserves higher-than-latest prerelease pins ([#445](https://github.com/jdx/aube/pull/445))
- *(cli)* reject `.` as a foreign --lockfile-dir importer; correct docs ([#442](https://github.com/jdx/aube/pull/442))
- *(scripts)* close 3 lifecycle parity gaps with pnpm ([#421](https://github.com/jdx/aube/pull/421))
- *(cli)* honor full gitignore semantics in pack/publish ([#411](https://github.com/jdx/aube/pull/411))
- *(dlx)* pick .cmd shim on Windows so bin runs without --shell-mode ([#401](https://github.com/jdx/aube/pull/401))
- *(install)* fetch hosted git deps over https, not ssh ([#394](https://github.com/jdx/aube/pull/394))

### Other

- *(cli)* port pnpm monorepo filter tests + wire --fail-if-no-match ([#457](https://github.com/jdx/aube/pull/457))
- cache hot-path work across install, resolver, and registry ([#453](https://github.com/jdx/aube/pull/453))
- refresh benchmarks for v1.5.2 ([#452](https://github.com/jdx/aube/pull/452))
- dedupe and cache hot-path work in install and resolver ([#449](https://github.com/jdx/aube/pull/449))
- refresh benchmarks for v1.5.2 ([#448](https://github.com/jdx/aube/pull/448))
- *(install)* port four allowBuilds review tests from pnpm lifecycleScripts.ts ([#441](https://github.com/jdx/aube/pull/441))
- *(install)* port pnpm/test/update.ts (13/22) ([#438](https://github.com/jdx/aube/pull/438))
- refresh benchmarks for v1.5.1 ([#426](https://github.com/jdx/aube/pull/426))
- release v1.5.2 ([#389](https://github.com/jdx/aube/pull/389))
- *(resolver)* add bundled metadata primer ([#397](https://github.com/jdx/aube/pull/397))
- thank Namespace for GitHub Actions runner support ([#412](https://github.com/jdx/aube/pull/412))
- refresh benchmarks for v1.5.1 ([#392](https://github.com/jdx/aube/pull/392))

## [1.5.2](https://github.com/jdx/aube/compare/v1.5.1...v1.5.2) - 2026-04-30

### Fixed

- *(cli)* honor full gitignore semantics in pack/publish ([#411](https://github.com/jdx/aube/pull/411))
- *(dlx)* pick .cmd shim on Windows so bin runs without --shell-mode ([#401](https://github.com/jdx/aube/pull/401))
- *(install)* fetch hosted git deps over https, not ssh ([#394](https://github.com/jdx/aube/pull/394))

### Other

- *(resolver)* add bundled metadata primer ([#397](https://github.com/jdx/aube/pull/397))
- thank Namespace for GitHub Actions runner support ([#412](https://github.com/jdx/aube/pull/412))
- refresh benchmarks for v1.5.1 ([#392](https://github.com/jdx/aube/pull/392))

## [1.5.0](https://github.com/jdx/aube/compare/v1.4.0...v1.5.0) - 2026-04-29

### Added

- *(cli)* add dependency graph query command ([#380](https://github.com/jdx/aube/pull/380))

### Fixed

- *(cli,linker,lockfile)* patch-commit destination, CRLF patches, npm-alias catalog ([#384](https://github.com/jdx/aube/pull/384))
- *(workspace)* default-write aube-workspace.yaml instead of pnpm-workspace.yaml ([#382](https://github.com/jdx/aube/pull/382))
- *(resolver)* bound resolved package stream ([#377](https://github.com/jdx/aube/pull/377))

### Other

- *(bench)* add install phase timings ([#381](https://github.com/jdx/aube/pull/381))

## [1.4.0](https://github.com/jdx/aube/compare/v1.3.0...v1.4.0) - 2026-04-28

### Added

- *(audit)* support update fix mode ([#363](https://github.com/jdx/aube/pull/363))
- *(install)* adopt pnpm 11 allowBuilds reviews ([#364](https://github.com/jdx/aube/pull/364))
- *(pnpmfile)* support esm pnpmfiles ([#362](https://github.com/jdx/aube/pull/362))
- *(scripts)* enforce build jails on linux ([#350](https://github.com/jdx/aube/pull/350))

### Fixed

- *(npm)* preserve extensionless bin shims ([#369](https://github.com/jdx/aube/pull/369))
- roundup of critical/high audit findings ([#361](https://github.com/jdx/aube/pull/361))
- *(resolver)* exclude provenance churn packages ([#360](https://github.com/jdx/aube/pull/360))
- *(linker)* link workspace bins into dependent packages ([#353](https://github.com/jdx/aube/pull/353))
- *(packaging)* include README on published aube crate ([#349](https://github.com/jdx/aube/pull/349))

### Other

- warn about npm install caveats ([#368](https://github.com/jdx/aube/pull/368))

## [1.3.0](https://github.com/jdx/aube/compare/v1.2.1...v1.3.0) - 2026-04-27

### Added

- *(config)* add settings discovery ([#347](https://github.com/jdx/aube/pull/347))
- *(security)* enforce trustPolicy by default, add paranoid bundle, security docs ([#333](https://github.com/jdx/aube/pull/333))
- *(scripts)* add jailed dependency builds ([#306](https://github.com/jdx/aube/pull/306))

### Fixed

- *(resolver)* accept abbreviated git commit SHAs in user specs ([#346](https://github.com/jdx/aube/pull/346))
- *(lockfile)* preserve package and bun lock compatibility ([#339](https://github.com/jdx/aube/pull/339))
- *(registry)* surface retry warnings and cap timeout retries at 1 ([#331](https://github.com/jdx/aube/pull/331))
- bun.lock parity for workspaces, platforms, and locked versions ([#327](https://github.com/jdx/aube/pull/327))

### Other

- *(add)* drop redundant pre-install resolve, use FrozenMode::Fix ([#348](https://github.com/jdx/aube/pull/348))
- *(install)* skip unused dep bin links ([#343](https://github.com/jdx/aube/pull/343))
- *(deps)* replace serde_yaml with yaml_serde ([#340](https://github.com/jdx/aube/pull/340))

## [1.2.1](https://github.com/jdx/aube/compare/v1.2.0...v1.2.1) - 2026-04-26

### Fixed

- *(add)* preserve package manifest field order ([#315](https://github.com/jdx/aube/pull/315))

### Other

- *(resolver)* avoid full packuments for aged metadata ([#314](https://github.com/jdx/aube/pull/314))

## [1.2.0](https://github.com/jdx/aube/compare/v1.1.0...v1.2.0) - 2026-04-25

### Added

- *(cli)* mise-style --version + scope update notifier to version commands ([#301](https://github.com/jdx/aube/pull/301))
- *(cli)* add short command aliases ([#299](https://github.com/jdx/aube/pull/299))

### Fixed

- support git url specs in dlx and parser ([#295](https://github.com/jdx/aube/pull/295))
- *(install)* link bins with mixed metadata ([#300](https://github.com/jdx/aube/pull/300))
- cross-platform install correctness pass ([#293](https://github.com/jdx/aube/pull/293))
- *(install)* restore missing lockfile from install state ([#289](https://github.com/jdx/aube/pull/289))

### Security

- cve-class hardening across linker, registry, resolver, install ([#296](https://github.com/jdx/aube/pull/296))

## [1.1.0](https://github.com/jdx/aube/compare/v1.0.0...v1.1.0) - 2026-04-24

### Added

- *(resolver)* support pnpm `&path:/<sub>` git dep selector ([#273](https://github.com/jdx/aube/pull/273))
- *(install)* support global approve-builds ([#274](https://github.com/jdx/aube/pull/274))
- *(scripts)* run pack/publish/version lifecycle hooks ([#262](https://github.com/jdx/aube/pull/262))

### Fixed

- *(store)* speed up cold installs ([#267](https://github.com/jdx/aube/pull/267))
- *(linker)* strip windows verbatim prefix before diffing bin-shim paths ([#275](https://github.com/jdx/aube/pull/275))
- *(publish)* report post-hook name/version in PublishOutcome ([#272](https://github.com/jdx/aube/pull/272))
- *(global)* strip Windows \\?\\ verbatim prefix from canonicalized install dir ([#243](https://github.com/jdx/aube/pull/243))

### Other

- *(install)* split warm freshness state ([#271](https://github.com/jdx/aube/pull/271))
- avoid duplicate warm state reads ([#266](https://github.com/jdx/aube/pull/266))
- use warm path in frozen mode ([#264](https://github.com/jdx/aube/pull/264))
- always shim self-bin so CI artifact round-trips work ([#259](https://github.com/jdx/aube/pull/259))
- dedup pass + registry/store perf wave ([#254](https://github.com/jdx/aube/pull/254))
- resolve catalog: in overrides + honor override-rewritten importer specs ([#249](https://github.com/jdx/aube/pull/249))
- shared helpers + migrate hardcoded sites ([#245](https://github.com/jdx/aube/pull/245))

## [1.0.0](https://github.com/jdx/aube/compare/v1.0.0-beta.12...v1.0.0) - 2026-04-23

### Other

- split lib.rs into focused modules ([#235](https://github.com/jdx/aube/pull/235))
- split mod.rs into bin_linking/git_prepare/lifecycle submodules ([#237](https://github.com/jdx/aube/pull/237))
- *(delta)* invalidate changed no-gvs subtrees ([#233](https://github.com/jdx/aube/pull/233))
- link importer's own bin into node_modules/.bin ([#230](https://github.com/jdx/aube/pull/230))
- windows install correctness + workspace filter fixes ([#229](https://github.com/jdx/aube/pull/229))
- *(pack)* drop CHANGELOG from always-included files ([#227](https://github.com/jdx/aube/pull/227))
- *(install)* show transfer rate + elapsed timer in progress bars ([#225](https://github.com/jdx/aube/pull/225))
- speed up babylon warm reinstalls ([#224](https://github.com/jdx/aube/pull/224))
- *(install)* fix node-gyp bootstrap walkup causing bats parallel hang ([#220](https://github.com/jdx/aube/pull/220))

## [1.0.0-beta.12](https://github.com/jdx/aube/compare/v1.0.0-beta.11...v1.0.0-beta.12) - 2026-04-22

### Other

- anchor aube install at workspace root from member subdir ([#217](https://github.com/jdx/aube/pull/217))
- apply dependency policy in add/remove/update/dedupe resolver ([#218](https://github.com/jdx/aube/pull/218))
- compare packageManagerStrictVersion against user-facing version ([#216](https://github.com/jdx/aube/pull/216))
- anchor auto-install freshness check at workspace root ([#215](https://github.com/jdx/aube/pull/215))
- make packageManagerStrict a tri-state, default warn ([#213](https://github.com/jdx/aube/pull/213))
- append -DEBUG to version on non-release builds ([#212](https://github.com/jdx/aube/pull/212))
- include integrity in package index cache key ([#209](https://github.com/jdx/aube/pull/209))
- bootstrap node-gyp when absent from PATH ([#210](https://github.com/jdx/aube/pull/210))
- cross-crate dedup pass ([#208](https://github.com/jdx/aube/pull/208))
- enrich NoMatch error with importer, chain, available versions ([#205](https://github.com/jdx/aube/pull/205))
- raise RLIMIT_NOFILE soft limit at startup ([#207](https://github.com/jdx/aube/pull/207))
- cross-crate security hardening ([#202](https://github.com/jdx/aube/pull/202))
- *(filter)* keep root importer deps in workspace selects ([#199](https://github.com/jdx/aube/pull/199))
- cross-crate correctness and security fixes ([#196](https://github.com/jdx/aube/pull/196))

## [1.0.0-beta.11](https://github.com/jdx/aube/compare/v1.0.0-beta.10...v1.0.0-beta.11) - 2026-04-21

### Other

- recognize package.json#workspaces as a workspace-root marker ([#194](https://github.com/jdx/aube/pull/194))
- verify warm-path deps from install state ([#188](https://github.com/jdx/aube/pull/188))
- warm-install speedup ([#177](https://github.com/jdx/aube/pull/177))
- short-circuit bin linking on packages with no bin metadata ([#192](https://github.com/jdx/aube/pull/192))
- warn instead of erroring on packageManager mismatch for run ([#191](https://github.com/jdx/aube/pull/191))
- skip pnpm v9 virtual importers in workspace link passes ([#190](https://github.com/jdx/aube/pull/190))

## [1.0.0-beta.10](https://github.com/jdx/aube/compare/v1.0.0-beta.9...v1.0.0-beta.10) - 2026-04-21

### Fixed

- pnpm-workspace.yaml overrides/patches, npm: alias overrides, cross-platform pnpm-lock ([#175](https://github.com/jdx/aube/pull/175))
- close remaining audit findings across registry, store, and linker ([#164](https://github.com/jdx/aube/pull/164))

### Other

- honor pnpm-workspace.yaml supportedArchitectures, ignoredOptionalDependencies, pnpmfilePath ([#181](https://github.com/jdx/aube/pull/181))
- hint at `aube deprecations --transitive` when transitives exist ([#183](https://github.com/jdx/aube/pull/183))
- support $name references in overrides ([#180](https://github.com/jdx/aube/pull/180))
- scope deprecation warnings + add `aube deprecations` ([#170](https://github.com/jdx/aube/pull/170))
- read top-level trustedDependencies as allow-source ([#172](https://github.com/jdx/aube/pull/172))
- collapse install bool bags into enums, FxHashMap in resolver ([#165](https://github.com/jdx/aube/pull/165))
- render parse errors with miette source span ([#166](https://github.com/jdx/aube/pull/166))

## [1.0.0-beta.9](https://github.com/jdx/aube/compare/v1.0.0-beta.8...v1.0.0-beta.9) - 2026-04-20

### Other

- reject path-traversing bin names and targets ([#162](https://github.com/jdx/aube/pull/162))
- wipe node_modules when global virtual store toggles ([#160](https://github.com/jdx/aube/pull/160))
- render package.json parse errors with miette source span ([#157](https://github.com/jdx/aube/pull/157))
- *(config)* add --local shortcut for --location project ([#161](https://github.com/jdx/aube/pull/161))
- silence peer-dep mismatches by default (bun parity) ([#158](https://github.com/jdx/aube/pull/158))
- *(troubleshooting)* lead with disable-gvs as first step ([#156](https://github.com/jdx/aube/pull/156))
- short-circuit warm path when install-state matches ([#127](https://github.com/jdx/aube/pull/127))
- create scoped bin shim parents ([#149](https://github.com/jdx/aube/pull/149))
- emit colored stderr under CI even when not a TTY ([#146](https://github.com/jdx/aube/pull/146))

## [1.0.0-beta.8](https://github.com/jdx/aube/compare/v1.0.0-beta.7...v1.0.0-beta.8) - 2026-04-20

### Other

- rewrite gvs auto-disable warning in plain English ([#140](https://github.com/jdx/aube/pull/140))
- default to ~/.local/share/aube/store per XDG spec ([#129](https://github.com/jdx/aube/pull/129))

## [1.0.0-beta.7](https://github.com/jdx/aube/compare/v1.0.0-beta.6...v1.0.0-beta.7) - 2026-04-19

### Other

- write per-dep .bin for transitive lifecycle-script bins ([#122](https://github.com/jdx/aube/pull/122))
- make workspace warm installs incremental ([#110](https://github.com/jdx/aube/pull/110))
- byte-identical pnpm-lock.yaml / bun.lock on re-emit ([#107](https://github.com/jdx/aube/pull/107))
- drop webpack and rollup from gvs auto-disable defaults ([#117](https://github.com/jdx/aube/pull/117))
- registry + install: tolerate napi-rs packuments and warn on ignored builds ([#113](https://github.com/jdx/aube/pull/113))
- include bun.lock in --lockfile removal set ([#105](https://github.com/jdx/aube/pull/105))
- fix --version / -V on aubr and aubx multicall shims ([#106](https://github.com/jdx/aube/pull/106))

## [1.0.0-beta.6](https://github.com/jdx/aube/compare/v1.0.0-beta.5...v1.0.0-beta.6) - 2026-04-19

### Other

- widen disableGlobalVirtualStoreForPackages default list ([#101](https://github.com/jdx/aube/pull/101))
- widen aube-lock.yaml to every common platform ([#94](https://github.com/jdx/aube/pull/94))
- split into frozen/settings/side_effects_cache submodules ([#88](https://github.com/jdx/aube/pull/88))
- *(progress)* split ci-mode state into own module ([#87](https://github.com/jdx/aube/pull/87))
- move install state to node_modules/.aube-state ([#80](https://github.com/jdx/aube/pull/80))
- Fix two aube install issues on real RN monorepos ([#82](https://github.com/jdx/aube/pull/82))
- exit silently on ctrl-c at script picker ([#81](https://github.com/jdx/aube/pull/81))

## [1.0.0-beta.5](https://github.com/jdx/aube/compare/v1.0.0-beta.4...v1.0.0-beta.5) - 2026-04-19

### Other

- pluralize counted nouns in CLI output ([#70](https://github.com/jdx/aube/pull/70))
- use strum derives for Severity and NodeLinker ([#69](https://github.com/jdx/aube/pull/69))
- keep filtered workspace installs rooted ([#67](https://github.com/jdx/aube/pull/67))
- accept registry flag on install ([#63](https://github.com/jdx/aube/pull/63))
- add global gvs override ([#61](https://github.com/jdx/aube/pull/61))

## [1.0.0-beta.4](https://github.com/jdx/aube/compare/v1.0.0-beta.3...v1.0.0-beta.4) - 2026-04-19

### Other

- discover root catalogs via package.json workspaces field ([#56](https://github.com/jdx/aube/pull/56))

## [1.0.0-beta.3](https://github.com/jdx/aube/compare/v1.0.0-beta.2...v1.0.0-beta.3) - 2026-04-19

### Added

- *(cli)* support jsr: specifier protocol ([#19](https://github.com/jdx/aube/pull/19))

### Fixed

- *(dlx)* resolve bin from installed package when names differ ([#25](https://github.com/jdx/aube/pull/25))
- verifyDepsBeforeRun fires when node_modules is removed ([#23](https://github.com/jdx/aube/pull/23))

### Other

- discover from workspace root + package.json sources ([#44](https://github.com/jdx/aube/pull/44))
- AUBE_DEBUG/AUBE_LOG replace RUST_LOG for log control ([#43](https://github.com/jdx/aube/pull/43))
- preserve npm-alias as folder name on fresh resolve ([#37](https://github.com/jdx/aube/pull/37))
- *(npm)* resolve peer deps when installing from package-lock.json ([#35](https://github.com/jdx/aube/pull/35))
- clarify packageManagerStrict rejection message ([#40](https://github.com/jdx/aube/pull/40))
- swap CAS hash from SHA-512 to BLAKE3 ([#36](https://github.com/jdx/aube/pull/36))
- auto-disable global virtual store for packages known to break on it ([#32](https://github.com/jdx/aube/pull/32))
- *(npm)* support npm:<real>@<ver> aliases + fix dep_path tail ([#30](https://github.com/jdx/aube/pull/30))
- print "Already up to date" on a no-op install ([#17](https://github.com/jdx/aube/pull/17))

## [1.0.0-beta.2](https://github.com/jdx/aube/compare/v1.0.0-beta.1...v1.0.0-beta.2) - 2026-04-18

### Other

- update Cargo.toml dependencies
