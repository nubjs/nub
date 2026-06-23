# Lifecycle scripts

Packages can define lifecycle scripts such as `preinstall`, `install`,
`postinstall`, and `prepare`. aube treats root scripts and dependency scripts
differently.

## Root scripts

Root package scripts run during install unless scripts are ignored:

```sh
aube install --ignore-scripts
```

## Dependency scripts

Dependency lifecycle scripts follow the pnpm v11 build approval model. Packages
must be explicitly allowlisted before their install-time scripts run.

```sh
aube ignored-builds
aube approve-builds
aube rebuild
```

Supported policy fields — aube reads all of these at install time:

In `aube-workspace.yaml` or `pnpm-workspace.yaml` (pnpm v11's build-review
map, and what aube writes to — aube creates `aube-workspace.yaml` from
scratch, but mutates an existing `pnpm-workspace.yaml` in place):

```yaml
allowBuilds:
  esbuild: true
  sharp: true
  untrusted-package: false
```

The old `onlyBuiltDependencies` and `neverBuiltDependencies` list keys are
still honored as read-only compatibility inputs, but new approvals go into
`allowBuilds`. When install sees an unreviewed dependency build, it adds that
package to `allowBuilds` with `false`; `aube approve-builds` flips reviewed
entries to `true`.

In `package.json` (legacy — still honored as a read source).
Every key under `pnpm.*` is also accepted under `aube.*`; when both are
present for the same key, `aube.*` wins. Disjoint entries from either
namespace merge.

```json
{
  "aube": {
    "allowBuilds": {
      "esbuild": true,
      "untrusted-package": false
    }
  }
}
```

Deny rules win over allow rules. Workspace-yaml entries and
`package.json` entries merge; you don't have to migrate a legacy
`pnpm.allowBuilds` to start using `aube approve-builds`.

Entry keys support a bare package name (matches every version), an
exact version pin (`esbuild@0.19.0`), an exact version union
(`esbuild@0.19.0 || 0.20.0`), or a `*` wildcard name (`@babel/*`,
`*-loader`, or bare `*` for everything). Exact non-registry source
keys from pnpm are also honored, such as
`dep@https://codeload.github.com/example/dep/tar.gz/<sha>`.
Wildcards can't be combined
with a version pin — the point of a version pin is to assert a
specific build was audited, and a wildcard defeats that. Semver
ranges aren't supported for the same reason.

## Jailed dependency builds

Build approval controls whether a dependency script may run at all. Jailed
builds add a second boundary for approved packages:

```yaml
jailBuilds: true
```

With `jailBuilds` enabled, approved dependency `preinstall`, `install`, and
`postinstall` scripts run with a scrubbed environment and a temporary `HOME`.
On macOS and Linux, aube also applies a native jail that denies network access
and restricts filesystem writes to the package directory and temporary
directories.

`jailBuilds` defaults to `false` today and is planned to default to `true` in
the next major version.

For packages that need a narrow exception, grant only that privilege:

```yaml
jailBuildPermissions:
  "@vendor/*":
    env:
      - SHARP_DIST_BASE_URL
    write:
      - ~/.cache/sharp
```

For packages that cannot run in the jail yet, disable the jail for a package
glob while keeping the build approval requirement:

```yaml
jailBuildExclusions:
  - "@legacy-native/*"
```

See [Jailed builds](/package-manager/jailed-builds) for the full profile,
supported permissions, and platform behavior.

## Git dependencies

Git dependencies with `prepare` scripts get a nested install in the clone
before aube snapshots the package. The final linked package uses the packed
result, not the raw checkout.

## Side effects cache

Allowlisted dependency builds can cache their post-build package tree and reuse
it on future installs with the same input hash.

## Bun comparison

Bun also treats dependency scripts as a security boundary and uses an allowlist
model through `trustedDependencies`. aube reads the top-level
`trustedDependencies` array as an additional allow-source alongside
`pnpm.onlyBuiltDependencies`, so bun projects work without rewriting the
manifest. `pnpm.neverBuiltDependencies` still wins when both sides list the
same package.
