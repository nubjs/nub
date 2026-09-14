# winget packaging

Manifests and automation that make nub installable with `winget install` on Windows.

winget installs from manifests published in the community repo
[`microsoft/winget-pkgs`](https://github.com/microsoft/winget-pkgs). This directory
holds:

- `manifests/n/Nubjs/Nub/<version>/` — the three-file manifest set (version,
  installer, en-US locale) for a published nub release. It is both the **first-submission
  payload** for winget-pkgs and the **fixture** the test workflow installs from.
- Automation: `.github/workflows/winget-validate.yml` (the per-change install test)
  and a gated `submit-winget` job in `.github/workflows/release.yml` (release-time
  submission of new versions).

## Package identity

| Field | Value |
| --- | --- |
| PackageIdentifier | `Nubjs.Nub` |
| Moniker | `nub` (so `winget install nub` resolves once published) |
| Installer | per-arch `.zip` (x64, arm64), one portable `nub.exe` |

The publisher/casing (`Nubjs.Nub`, `nub contributors`) follows winget convention and
can be adjusted on review before the first submission.

Only `nub` is registered as a command alias. `nubx` and `nubr` are the same binary dispatched
on argv[0], and winget refuses a duplicate `RelativeFilePath`, so each alias needs its own
file in the zip before it can be listed.

## Confidence chain — why a broken manifest cannot reach users

1. **Local / CI install from the manifest** (`winget install --manifest ...`) performs
   the *exact* operation the winget-pkgs validation bot performs: download the release
   zip, verify its SHA256, extract, and register the portable binaries. Green here means
   the manifest is installable.
2. **Submission to `microsoft/winget-pkgs`** opens a PR. Microsoft's validation bot
   **re-runs that same install-in-sandbox check** and **blocks merge on failure**.
3. Therefore a manifest that fails cannot be merged — the worst case of a bad submission
   is "the PR doesn't merge," never "users get a broken install." The local/CI install is
   the high-confidence pre-check that makes a submission near-certain to pass.

## Testing

### Automated (CI) — `winget install --manifest`

`.github/workflows/winget-validate.yml` runs on `windows-latest` whenever the manifest
or the workflow changes, and on manual `workflow_dispatch`. It `winget validate`s the
manifest, then `winget install --manifest`s it and asserts `nub --version` succeeds. This needs **no** winget-pkgs publication — it tests the
manifest directly.

### Manual local test (a Windows machine)

```powershell
winget validate --manifest .\winget\manifests\n\Nubjs\Nub\0.9.2
winget install --manifest .\winget\manifests\n\Nubjs\Nub\0.9.2 `
  --accept-package-agreements --accept-source-agreements
nub --version
```

### Highest-fidelity local test — Windows Sandbox

winget-pkgs ships `Tools/SandboxTest.ps1`, which spins up Windows Sandbox and runs the
manifest through the same flow the validation bot uses — a 1:1 mirror of the merge gate.
On a Windows host with Windows Sandbox enabled, from a `microsoft/winget-pkgs` checkout:

```powershell
.\Tools\SandboxTest.ps1 <path-to>\winget\manifests\n\Nubjs\Nub\0.9.2
```

This is the most faithful pre-submission check; it is not run in CI (it requires
Windows Sandbox / nested virtualization).

### Post-publish smoke (only after the package is live in winget-pkgs)

Once `Nubjs.Nub` exists in winget-pkgs, a scheduled/manual job can verify the *published*
package end-to-end:

```powershell
winget install --id Nubjs.Nub --silent --accept-package-agreements --accept-source-agreements
nub --version
```

This is secondary — it only works after submission; the per-change `--manifest` install
above is the gate that matters.

## Maintainer enablement (one-time)

The release workflow's `submit-winget` job is a **no-op until enabled** — it auto-submits
nothing without a secret.

1. **Bootstrap the first version.** winget-releaser *updates* an existing package; the
   first `Nubjs.Nub` version is submitted manually from the committed manifest. Either
   open a PR to `microsoft/winget-pkgs` adding `manifests/n/Nubjs/Nub/0.9.2/` (the files
   here), or run [`wingetcreate`](https://github.com/microsoft/winget-create):
   `wingetcreate submit --token <PAT> .\winget\manifests\n\Nubjs\Nub\0.9.2`.
   Verify CI/Sandbox green first.
2. **Create the PAT.** Under the account that will own the winget-pkgs fork, create a
   classic PAT with the `public_repo` scope (or a fine-grained token with
   contents + pull-requests: write on the fork).
3. **Add the secret.** Add it as the `WINGET_PAT` repository secret on `nubjs/nub`.

After that, every tagged release auto-opens a winget-pkgs PR for the new version via the
`submit-winget` job; Microsoft's bot validates and merges it.

## Refreshing the committed manifest fixture

The committed manifest pins a specific release (URLs + SHA256s). To re-point the CI test
at a newer release, copy the version directory, bump `PackageVersion`, the two
`InstallerUrl`s, and the `ReleaseNotesUrl`, and replace each `InstallerSha256` with the
value from that release's `nub-win32-<arch>.zip.sha256` sidecar asset (uppercased).
Ongoing winget-pkgs submissions are automated by `submit-winget`, so this fixture only
needs refreshing when you want the test to track a newer release.
