# Security

This page lists every security-relevant feature in aube, its default, and the
one-line config to turn it on or off.

To report a vulnerability, see the [security policy](https://github.com/jdx/aube/security/policy).

## The `paranoid` switch

The fastest way to enable the strict bundle is one line:

```yaml
paranoid: true
```

This forces every setting in the strict bundle on, regardless of how each is
configured individually:

- [`jailBuilds = true`](#jailed-lifecycle-scripts)
- [`trustPolicy = no-downgrade`](#trust-policy) (overrides explicit `off`)
- [`minimumReleaseAgeStrict = true`](#minimum-release-age) — turns the age
  gate into a hard fail instead of "fall back to the lowest satisfying
  version"
- [`strictStoreIntegrity = true`](/settings/#setting-strictstoreintegrity) —
  fail when a tarball ships without `dist.integrity` instead of warning
- [`strictDepBuilds = true`](/settings/#setting-strictdepbuilds) — fail the
  install when a dep has unreviewed build scripts instead of silently
  skipping them
- [`advisoryCheck = required`](#typosquat-and-impersonation-protection) —
  fail any live-API OSV check when OSV can't be reached, instead of warning
  and continuing

Use it when you want maximum protection without listing each setting.

## Default-deny lifecycle scripts

Lifecycle scripts (`preinstall`, `install`, `postinstall`) run arbitrary code
when a package is installed, which makes them a common attack vector. aube
doesn't run dependency lifecycle scripts unless you've approved them
explicitly:

```yaml
# aube-workspace.yaml
allowBuilds:
  esbuild: true
  sharp: true
```

Or interactively:

```sh
aube approve-builds
```

Root-package lifecycle scripts (your own project's) still run normally; only
dependency scripts need approval.

For a Git dependency that tracks a branch, approve the package and repository
URL rather than a resolved commit. The rule remains limited to that repository;
a package-name-only rule never approves Git-sourced code:

```yaml
allowBuilds:
  native-addon@git+https://github.com/acme/native-addon.git: true
```

Settings: [`allowBuilds`](/settings/#setting-allowbuilds). Install adds
unreviewed build packages to `aube-workspace.yaml` (or `pnpm-workspace.yaml`
if one already exists) as `false`; approving them flips the entry to `true`.

### Suspicious-script content sniff

Before nudging you to run `aube approve-builds`, aube runs a small pattern
matcher against each unreviewed dep's `preinstall` / `install` /
`postinstall` script bodies and surfaces a
`WARN_AUBE_SUSPICIOUS_LIFECYCLE_SCRIPT` for any that match a known-dangerous
shape:

- `curl … | sh` / `wget … | bash` — fetch-and-pipe-to-shell.
- `eval(atob(…))` / `Function(atob(…))` / `eval(Buffer.from(…))` —
  base64-decode-then-evaluate, a common dropper shape.
- Reads of `~/.ssh`, `~/.aws`, `~/.npmrc`, `~/.config/gh` — credential files
  a lifecycle script has no business touching.
- `process.env.*TOKEN`, `*SECRET`, `*API_KEY`, etc. — secret-shaped env vars
  exfiltrated from CI.
- Discord webhooks, Telegram bot API, OAST collaborator hosts — known exfil
  channels.
- `http://1.2.3.4/…` bare-IP HTTP targets.

The sniff is **advisory** — it never blocks an install or write. The
`allowBuilds` allowlist remains the only gate on whether scripts actually
execute; the sniff just gives you more than `name@version` to judge by when
deciding whether to approve a build. `aube approve-builds` repeats the same
warnings inline next to each picker entry, and `aube ignored-builds` lists
them under each `name@version` line.

False positives are possible (an SDK that legitimately hits a Discord
webhook from a `postinstall` would flag), but lifecycle script bodies are
short and almost never contain a bare `curl … | sh` legitimately. Once
you've inspected a flagged script and decided it's fine, add the package to
`allowBuilds: true` — the warning has done its job.

## Jailed lifecycle scripts

When a dependency is approved to build, jailing keeps it from getting your
full filesystem, network, and environment. On macOS aube wraps the script with
a Seatbelt profile; on Linux it applies Landlock and seccomp before exec. Both
deny network access and limit writes to package and jail-owned temporary
directories. On Windows the env is scrubbed and `HOME` is redirected to a
temporary directory.

```yaml
jailBuilds: true
```

Grant narrow exceptions per-package instead of disabling the jail wholesale:

```yaml
jailBuilds: true
jailBuildPermissions:
  sharp:
    env: [SHARP_DIST_BASE_URL]
    write: ["~/.cache/sharp"]
    network: true
```

Default: `false` today, planned to flip to `true` in the next major.

Full reference: [Jailed builds](/package-manager/jailed-builds).

## Trust policy

`trustPolicy = no-downgrade` blocks installs of a version that carries weaker
trust evidence than any earlier-published version of the same package. aube
recognizes three tiers of evidence, strongest first, and only counts the
structured metadata shapes npm emits after registry-side checks:

1. **npm staged publish approval** — package metadata carries an `approver`
   field from the registry-side approval flow.
2. **npm trusted-publisher** — package was published via OIDC from a trusted
   CI provider (`_npmUser.trustedPublisher.id`).
3. **Sigstore provenance** — package was published with `npm publish
   --provenance` (`dist.attestations.provenance.predicateType` with an SLSA
   provenance URI).

This install-time policy validates the registry metadata shape; it does not
cryptographically verify the attached attestation bundle.

A trust downgrade may indicate a supply-chain incident: publisher account
takeover, repository tampering, or a malicious co-maintainer publishing
without the original CI flow.

```yaml
trustPolicy: no-downgrade
```

Exempt specific packages or versions when needed:

```yaml
trustPolicyExclude:
  - "@vendor/legacy-pkg"       # every version of one package
  - "old-thing@1.0.0"          # one exact version
  - "things@^1.0.0 || >=2 <3"  # union of semver ranges
  - "is-*"                     # name glob (globs take no version)
```

Version selectors are npm-style semver ranges.

Default: `no-downgrade`. Set `trustPolicy: off` to disable, or use
`trustPolicyExclude` for per-package opt-outs.

Settings: [`trustPolicy`](/settings/#setting-trustpolicy),
[`trustPolicyExclude`](/settings/#setting-trustpolicyexclude),
[`trustPolicyIgnoreAfter`](/settings/#setting-trustpolicyignoreafter).

## Minimum release age

Wait a configurable period before installing newly published versions. Catches
typo-squat and dependency-confusion attacks that get unpublished within hours.

```yaml
minimumReleaseAge: 4320  # 3 days
```

`minimumReleaseAgeStrict: true` fails the install when no version satisfies
the range; otherwise the resolver falls back to the lowest satisfying version
ignoring the cutoff for that pick only.

Default: `1440` (24 hours). Set `minimumReleaseAge: 0` to disable.

Settings: [`minimumReleaseAge`](/settings/#setting-minimumreleaseage),
[`minimumReleaseAgeExclude`](/settings/#setting-minimumreleaseageexclude),
[`minimumReleaseAgeStrict`](/settings/#setting-minimumreleaseagestrict).

## Typosquat and impersonation protection

`aube add` checks every package you name on the command line — and, after
resolution, the full transitive closure — against [OSV](https://osv.dev) for
`MAL-*` malicious-package advisories. The same check runs on `aube update`
and on any install where the resolver picks a version the lockfile didn't
already pin. Plain reinstalls (where the lockfile was authoritative) skip
the live API for latency; two opt-in local backends cover that path — see
[Install-time OSV check](#install-time-osv-check) below.

Two signals, with different response levels:

**Known-malicious advisories.** aube batch-queries [OSV](https://osv.dev) for
`MAL-*` advisories on every name about to be added. A hit fails the install
with `ERR_AUBE_MALICIOUS_PACKAGE` and a link to the advisory. If
the OSV API can't be reached, the default (`advisoryCheck: on`) warns and
continues; `advisoryCheck: required` upgrades that to a fail-closed
`ERR_AUBE_ADVISORY_CHECK_FAILED` so CI can tell a network outage from a
confirmed-malicious advisory.

**Low download count.** A typosquat or impersonation has approximately zero
installs on day one regardless of how cleverly it's named, so a
download-count floor catches the long tail of squats that haven't been
reported yet. Below the threshold, aube prompts for confirmation:

```
aube add supabase-javascript

  ⚠ supabase-javascript looks suspicious:
    • 3 downloads last week (threshold: 1000)
  Continue adding supabase-javascript? [y/N]
```

In non-interactive contexts the prompt becomes a hard refusal with
`ERR_AUBE_LOW_DOWNLOAD_PACKAGE` unless `--allow-low-downloads` is passed.

**Private packages skip both gates automatically.** Any package routed
through a non-`registry.npmjs.org` registry — whether by a scoped
override (`@myorg:registry=https://npm.internal.example/`) or by
replacing the default `registry=` URL outright — is exempted from
the OSV check and the downloads gate, because npmjs has no signal on
it. Workspace deps and git/local specs are also skipped.

For names that *do* route through public npmjs but are known-internal
(e.g. you publish a low-traffic helper under your own brand), list
them in `allowedUnpopularPackages` to skip the downloads gate alone:

```yaml
advisoryCheck: on            # default; fail open on network error
lowDownloadThreshold: 1000   # weekly downloads, 0 disables
allowedUnpopularPackages:    # glob patterns; OSV check still runs
  - "@mycompany/*"
  - "internal-*"
```

Set `advisoryCheck: required` to fail closed when OSV can't be reached —
appropriate for hardened CI, included in `paranoid: true`. Set
`advisoryCheck: off` or `lowDownloadThreshold: 0` to disable either check
independently.

Settings: [`advisoryCheck`](/settings/#setting-advisorycheck),
[`lowDownloadThreshold`](/settings/#setting-lowdownloadthreshold),
[`allowedUnpopularPackages`](/settings/#setting-allowedunpopularpackages).

## Install-time OSV check

After resolution, every install picks exactly one OSV `MAL-*` backend, so
the freshest signal lands when it matters most without paying a per-install
network round-trip when it doesn't:

| Install path                                   | Backend         | Setting                                   |
| ---------------------------------------------- | --------------- | ----------------------------------------- |
| `aube add`, `aube update`                      | Live API        | `advisoryCheck` (default `on`)            |
| Missing lockfile / resolver picked new version | Live API        | `advisoryCheck` (default `on`)            |
| `advisoryCheckEveryInstall = true`             | Live API        | `advisoryCheck` (default `on`)            |
| Plain reinstall (lockfile authoritative)       | Bloom prefilter | `advisoryBloomCheck` (default `off`)      |
| Plain reinstall, bloom disabled                | Local mirror    | `advisoryCheckOnInstall` (default `off`)  |
| Plain reinstall, both disabled                 | No check        | —                                         |

The two local backends cover plain reinstalls without a live round-trip:

**Bloom prefilter** (`advisoryBloomCheck`) downloads a ~380 KB bloom filter
built from OSV's malicious-package archive (regenerated upstream every 10
minutes), probes the resolved graph against it, and escalates only the hits
(~0.1% false-positive rate) to the live API for exact `(name, version)`
confirmation. A typical lockfile costs zero or one extra live-API round
trip per install. When both local backends are enabled, the bloom wins —
it's far cheaper on the wire and its confirmed hits go through the same
live-API oracle.

**Local mirror** (`advisoryCheckOnInstall`) keeps the bulk zip from
`osv-vulnerabilities.storage.googleapis.com/npm/all.zip` (roughly tens of
MB) at `$XDG_CACHE_HOME/aube/osv/npm/`, lazily refreshed with an
ETag-conditional GET every 24 hours. Lookups are sub-millisecond, but the
index lags reality by up to ~24h — an advisory published in the last day
won't be in it unless a refresh happens to fall after it. Fresh-resolution
installs always go through the live API, so that lag never affects new
picks.

Confirmed hits from any backend fail the install with the same
`ERR_AUBE_MALICIOUS_PACKAGE` exit.

```yaml
# Default: live API on aube add / update / fresh-resolution.
# Plain reinstalls skip OSV entirely.
advisoryCheck: on
advisoryBloomCheck: off
advisoryCheckOnInstall: off
advisoryCheckEveryInstall: false
```

```yaml
# Hardened CI: live API on every install, fail-closed on fetch errors.
advisoryCheck: required
advisoryCheckEveryInstall: true
```

```yaml
# Cheap always-on coverage: bloom prefilter covers plain reinstalls
# with a ~380 KB download; hits escalate to the live API.
advisoryCheck: on
advisoryBloomCheck: on
```

Both local backends share the same refresh-failure semantics:

- `on`: warn (`WARN_AUBE_OSV_BLOOM_REFRESH_FAILED` /
  `WARN_AUBE_OSV_MIRROR_REFRESH_FAILED`) and continue against the prior
  on-disk copy (or empty on first sync).
- `required`: refresh failures map to `ERR_AUBE_ADVISORY_CHECK_FAILED`. Use
  in hardened CI where a stale or unreachable index should block.

Settings:
[`advisoryCheck`](/settings/#setting-advisorycheck),
[`advisoryBloomCheck`](/settings/#setting-advisorybloomcheck),
[`advisoryCheckOnInstall`](/settings/#setting-advisorycheckoninstall),
[`advisoryCheckEveryInstall`](/settings/#setting-advisorycheckeveryinstall).

## Block exotic transitive dependencies

Reject transitive dependencies that resolve to `git+`, `file:`, or direct
tarball URLs — those skip the registry and its integrity verification. Direct
deps you pin yourself in `package.json` are still allowed.

```yaml
blockExoticSubdeps: true   # default
```

Settings: [`blockExoticSubdeps`](/settings/#setting-blockexoticsubdeps).

## Tarball integrity

Every registry tarball is verified against the SHA-512 hash recorded in the
packument's `dist.integrity` field before it is added to the store. Mismatches
fail the install. The hash is preserved in the lockfile, so subsequent
installs reverify on every fetch.

The content-addressable store itself uses BLAKE3 for the on-disk index — fast
to compute and immune to length-extension. Linked `node_modules` files are
reflinks (APFS/btrfs), hardlinks (ext4), or copies; none of those paths can
modify the canonical store entry.

## Auth tokens

Registry tokens are read from `.npmrc` (the npm convention) or environment
variables (`NPM_TOKEN`, `AUBE_AUTH_TOKEN`, etc.) and **never written to the
lockfile, tarball cache, or logs**. `aube login` and `aube logout` manage
tokens via the standard npm config file.

Inside jailed lifecycle scripts, common token env vars (`NPM_TOKEN`,
`NODE_AUTH_TOKEN`, `GITHUB_TOKEN`, `SSH_AUTH_SOCK`, `AWS_*`, etc.) are
scrubbed from the script environment unless explicitly granted via
`jailBuildPermissions`.

## Pluggable security scanner

`securityScanner` runs a
[Bun-compatible security scanner](https://bun.sh/docs/pm/security-scanner-api)
against the resolved install graph. Point the setting at the same npm
package you'd put in Bun's `bunfig.toml#install.security.scanner` and aube
loads it through a `node` bridge — the
[oven-sh template](https://github.com/oven-sh/security-scanner-template)
and [`@socketsecurity/bun-security-scanner`](https://github.com/SocketDev/bun-security-scanner)
both run unchanged.

```yaml
# aube-workspace.yaml
securityScanner: "@acme/bun-security-scanner"
```

The scanner fires post-resolve, sees the full transitive graph with
resolved versions, and **fails closed** on any scanner failure (missing
`node`, unresolvable module, timeout, etc.). Requires Node 22.6+. Set
`securityScanner: ""` to disable when bootstrapping.

Full reference: [Security scanner](/package-manager/security-scanner).

## Auditing installed dependencies

```sh
aube audit                # list known CVEs at moderate+ severity
aube audit --audit-level high
aube audit --fix          # write package.json overrides to patched versions
aube audit --json | jq    # machine-readable for CI
```

Same advisory data source as `npm audit` and `pnpm audit`; same response
schema.

## Recommended baseline

For most projects, the following is a good starting point:

```yaml
# aube-workspace.yaml
paranoid: true             # bundles jailBuilds, no-downgrade, strict gates
allowBuilds:
  esbuild: true
  sharp: true
  # ...whatever your project actually needs to build
```

`trustPolicy=no-downgrade` and `minimumReleaseAge: 1440` (24h) are already
default-on; `paranoid: true` adds the rest of the bundle on top. Pair this
with `aube audit` in CI so a newly disclosed CVE fails the build instead of
silently shipping.
