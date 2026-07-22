# Security

## Reporting a vulnerability

Please report security issues privately via GitHub's security advisory form:
**[github.com/jdx/aube/security/advisories/new](https://github.com/jdx/aube/security/advisories/new)**

Do not file a public discussion for vulnerabilities. We will acknowledge
receipt within a few business days, work with you on a fix, and credit you in
the release notes unless you prefer otherwise.

## Security features at a glance

aube ships with several supply-chain protections enabled by default and
several more available as one-line opt-ins. Full reference:
**[aube.jdx.dev/security](https://aube.jdx.dev/security)**.

| Setting | Default | What it protects against |
| --- | --- | --- |
| `blockExoticSubdeps` | `true` | Transitive deps from `git+`, `file:`, or raw tarball URLs |
| `allowBuilds` | deny all dep scripts | Lifecycle scripts running without explicit approval |
| `trustPolicy` | `"no-downgrade"` | Versions that lost provenance or trusted-publisher evidence |
| `minimumReleaseAge` | `1440` (24h) | Newly published versions before they have aged in the registry |
| `advisoryCheck` | `"on"` | Known-malicious packages (OSV `MAL-*` advisories) on `aube add` and fresh-resolution installs |
| `lowDownloadThreshold` | `1000` | Typosquats — `aube add` prompts on packages with near-zero weekly downloads |
| `jailBuilds` | `false` (planned `true` in the next major) | Approved scripts getting full filesystem / network / env |
| `paranoid` | `false` | Master switch — forces `jailBuilds`, `trustPolicy=no-downgrade`, `minimumReleaseAgeStrict`, `strictStoreIntegrity`, `strictDepBuilds`, `advisoryCheck=required` |
| Tarball integrity (SHA-512) | always on | Tampered tarballs in the registry or proxy cache |
| Content-addressed store (BLAKE3) | always on | Drift between the store and the linked `node_modules` |
| `aube audit` | n/a | Known CVEs against the resolved dependency tree |

## Supported versions

Security fixes target the latest minor of the current major release. Older
majors do not receive backports.
