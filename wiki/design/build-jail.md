# Dependency build jail

Nub runs approved dependency lifecycle scripts through a shared unprivileged sandbox engine. Root-authored scripts remain ordinary user commands. The jail reduces exposure without requiring machine setup.

## Policy and approval

Build approval and confinement are separate decisions. Approval permits a lifecycle script to run; the catalog supplies its filesystem, environment and network policy.

The install configuration can disable confinement without changing approval:

```json
{
  "install": {
    "buildJail": false
  }
}
```

This setting belongs in project or user `nub.jsonc`. Project configuration takes precedence over user configuration. The default is enabled. Root-authored `allowScripts` entries can opt an individual dependency out with `"no-jail"`; a dependency cannot disable its own confinement. Fetched Git preparation and its nested dependency builds retain dependency provenance.

The integration lives in [[crates/nub-cli/src/pm_engine/build_jail.rs#NubBuildJail]]. It owns lifecycle confinement instead of applying a second sandbox around Aube's standalone implementation. Package-manager approvals and script scheduling remain in the vendored engine.

## Shared engine

The engine compiles policy separately from launching a process. The package manager supplies paths and configuration provenance; enforcement does not discover configuration itself.

The public library operations are [[crates/nub-sandbox/src/compiler/preset.rs#compile_build_jail]] and [[crates/nub-sandbox/src/backend/mod.rs#apply]]. A prepared launch owns its proxies, temporary directories and process-tree cleanup until completion or cancellation.

| Platform | Filesystem and process enforcement | Network enforcement |
| --- | --- | --- |
| Linux | Landlock, seccomp and owned process groups | Socket restrictions and supervised per-host egress |
| macOS | Seatbelt and owned process groups | Seatbelt restrictions and a policy proxy |
| Windows | AppContainer, temporary ACL grants and Job Objects | Capability restrictions and a co-package egress helper |

None of these paths creates an account, requests elevation or installs a privileged helper. Windows per-host egress uses a helper in the same AppContainer package rather than a machine-wide loopback exemption. Unsupported Windows TLS-inspection and credential-broker policies fail closed.

## Compatibility limits

The build jail is defense in depth, not a boundary suitable for arbitrary hostile workloads. Catalog grants deliberately admit capabilities required by dependency builds.

Windows full-disk catalog grants omit AppContainer. Those launches keep environment filtering and process-tree ownership but have no OS filesystem or network boundary. This compatibility tier covers packages whose build tools cannot operate under a LowBox token. Other policies may reject a launch when a required guarantee is unavailable; callers must surface reported degradation.

Windows Node preloads are compressed inline modules. They need no writable preload file, and their combined environment value stays below the limit used by downstream tools such as MSBuild. Successful process creation alone does not establish that those tools can copy the environment.

The engine does not infer whether a repository is trustworthy. Configuration capabilities follow their source: dependency-controlled policy cannot acquire root-authored dynamic environment or credential-broker privileges.

## Tools and cache ownership

Native-build tooling is prepared before entering the jail. Private registry credentials stay outside published tool trees, and cache reuse distinguishes confined from unconfined builds.

Tool installation uses private staging and publishes only the usable dependency tree. A legacy credential-bearing configuration that cannot be removed prevents launch. Windows confined installs use physical copies to avoid granting shared store files through hardlinks.

The side-effects cache key includes confinement mode, the resolved shell and Node identity. Rebuild uses the same policy and cache identity as installation. Failed jailed lifecycle scripts retain a confinement-specific diagnostic and the applicable opt-out remedy.

## Runtime checks

The lifecycle contract exercises the real CLI, not a stub hook. Package compatibility checks additionally load native addons or invoke the installed tool's API.

The CLI integration target runs `tests/build-jail-corpus/contract.mjs`, `private-registry.mjs`, `cache-modes.mjs`, `descendants.mjs`, `layouts.mjs` and `fetched-native.mjs`. They cover policy precedence, root scripts, fetched native builds, private registry credentials, cache transitions, rebuild diagnostics, isolated/hoisted dependency resolution and descendant cleanup after success, failure or cancellation.

The explicit corpus runner adds real-package functional checks. Its separate pinned-population sweep records both jail-on and jail-off outcomes, engagement and native-artifact inventory. A lifecycle-only pass is not a claim that every package API was exercised; an install where no lifecycle ran is recorded separately.

The [framework application suite](../../tests/build-jail-frameworks/README.md) checks production builds, SSR/API responses, native image operations and frozen reinstalls. Each installation includes a dependency-side confinement sentinel and a root-script control. Registry lifecycle launches are recorded separately from the sentinel; prebuilt packages are not counted as lifecycle coverage.
