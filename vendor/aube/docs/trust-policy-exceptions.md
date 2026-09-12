# Trust policy downgrades

<script setup lang="ts">
import { data as packages } from './trust-policy-exceptions.data.ts'

function packageUrl(packageSpec: string): string {
  const versionSeparator = packageSpec.lastIndexOf('@')
  if (versionSeparator > 0) {
    const name = packageSpec.slice(0, versionSeparator)
    const version = packageSpec.slice(versionSeparator + 1)
    return `https://www.npmjs.com/package/${name}/v/${version}`
  }
  return `https://www.npmjs.com/package/${packageSpec}`
}
</script>

`trustPolicy = no-downgrade` stops an install when the selected package
version has weaker publishing evidence than an earlier release. This is a
signal to investigate, not just another version-resolution error.

## What the failure means

An earlier-published version had npm staged-publish approval, trusted-publisher
identity, or provenance metadata that the selected version no longer has. That
can indicate a compromised publisher, stolen token, malicious co-maintainer, or
a release built outside the repository's expected CI workflow.

There are also less dramatic explanations:

- a maintainer manually published a release or backport;
- a release shortcut skipped the trusted-publisher or provenance-enabled job;
- parallel major-version lines use inconsistent release automation;
- a registry proxy or mirror omitted trust metadata.

Release-process drift may be benign, but it is still a packaging failure. Once
a project establishes a trusted publishing path, every release should preserve
it. Metadata lost only by a proxy or mirror is instead a registry-operator
failure.

## What to do before adding an exception

1. Confirm the package name and selected version are the ones you expected.
2. Compare the npm publish time, publisher identity, source tag, commit, and
   release notes with the last trusted release.
3. Inspect the tarball contents and integrity metadata. Look for unexpected
   files, generated code, install scripts, dependency changes, or other signs
   that the release was tampered with.
4. Check the upstream repository and advisories for a compromised account,
   workflow outage, or intentional manual publish.
5. Report inconsistent evidence to the relevant upstream owner. Ask the package
   maintainer to restore a drifted publishing workflow, or ask the registry
   operator to restore metadata that exists on npmjs.org but is missing from its
   proxy or mirror.

Do not treat an allowlist entry as proof that a package is safe. It only records
that someone chose to bypass this particular signal.

Inspect an exact version without installing it:

```sh
aube trust check package-name@1.2.3
```

The report shows its publish time and trust evidence, the strongest evidence
on an earlier release, and whether a built-in exception applies. Add
`--ignore-default-excludes` to enforce the underlying policy even when aube
normally exempts that version, or `--json` for machine-readable output.

## Choosing the narrowest workaround

Prefer these options in order:

1. Pin a release that still carries trust evidence.
2. After reviewing the release, exempt only the affected version:

   ```yaml
   trustPolicyExclude:
     - "package-name@1.2.3"
   ```

3. Exempt every version only when the package's release model is inherently
   inconsistent and you are willing to review future releases without this
   protection:

   ```yaml
   trustPolicyExclude:
     - "package-name"
   ```

Setting `trustPolicy = off` disables the check for the entire install and
should be a last resort.

## Wall of shame

The following {{ packages.length }} packages are built into aube's default
exception list because their published metadata has triggered legitimate
`no-downgrade` failures:

<ul>
  <li v-for="packageName in packages" :key="packageName">
    <a :href="packageUrl(packageName)">{{ packageName }}</a>
  </li>
</ul>

This list is generated directly from
[`DEFAULT_TRUST_POLICY_EXCLUDES`](https://github.com/aubepkg/aube/blob/main/crates/aube-resolver/src/trust.rs)
at documentation build time, so it cannot drift from the resolver. Inclusion
is not an accusation of malware; it records inconsistent publishing evidence
that weakens the protection for every aube user.

If a package restores consistent trusted publishing, remove its built-in
exception so future regressions are blocked again.
