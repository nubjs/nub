# winget packaging

Nub is installable on Windows with `winget install Nub.Nub`. This directory holds the manifest fixture that proves a release's zip installs the way winget installs it, and the automation around it.

## Package identity

| Field | Value |
| --- | --- |
| PackageIdentifier | `Nub.Nub` |
| Moniker | `nub` (`winget install nub` resolves to it) |
| Installer | per-arch `.zip` (x64, arm64), one portable `nub.exe` |
| Command aliases | `nub`, `nubx`, `nubr` |

`Nub.Nub` lives in the community repo [`microsoft/winget-pkgs`](https://github.com/microsoft/winget-pkgs) under `manifests/n/Nub/Nub/`. It was first published from outside this repo and has been bumped by community automation (UnownBot, using komac) within minutes of every release since 0.1.0. Nothing here publishes it; the release workflow's `submit-winget` job is a dormant fallback (below).

### One executable, three aliases

The zip carries one `bin\nub.exe`; `nubx` and `nubr` are that binary dispatched on argv[0]. winget creates one command alias per `NestedInstallerFiles` entry and refuses a repeated `RelativeFilePath`, comparing the literal string — so each alias names the same file in a different spelling (`bin/nub.exe`, `./bin/nub.exe`, `bin\nub.exe`). The published manifests have carried `nub` + `nubx` this way since 0.1.0; the fixture adds `nubr` the same way, and the validate workflow proves all three run.

## Confidence chain — why a broken manifest cannot reach users

1. **CI install from the manifest** (`winget install --manifest ...`) performs the *exact* operation the winget-pkgs validation bot performs: download the release zip, verify its SHA256, extract, and register the portable aliases. Green here means the manifest is installable.
2. **A submission to `microsoft/winget-pkgs`** opens a PR. Microsoft's validation bot **re-runs that same install-in-sandbox check** and **blocks merge on failure**.
3. The worst case of a bad submission is "the PR doesn't merge," never "users get a broken install."

## Testing

### Automated (CI) — `winget install --manifest`

`.github/workflows/winget-validate.yml` runs on `windows-latest` whenever the manifest or the workflow changes (PR runs are opt-in via the `ci` label), and on manual `workflow_dispatch`. It `winget validate`s the manifest, then `winget install --manifest`s it and asserts `nub --version`, `nubx --version` and `nubr --version` each succeed. This needs **no** winget-pkgs publication — it tests the manifest directly.

### Manual local test (a Windows machine)

```powershell
winget validate --manifest .\winget\manifests\n\Nub\Nub\0.9.2
winget install --manifest .\winget\manifests\n\Nub\Nub\0.9.2 `
  --accept-package-agreements --accept-source-agreements
nub --version
nubx --version
nubr --version
```

### Highest-fidelity local test — Windows Sandbox

winget-pkgs ships `Tools/SandboxTest.ps1`, which spins up Windows Sandbox and runs the manifest through the same flow the validation bot uses. On a Windows host with Windows Sandbox enabled, from a `microsoft/winget-pkgs` checkout:

```powershell
.\Tools\SandboxTest.ps1 <path-to>\winget\manifests\n\Nub\Nub\0.9.2
```

### Published-package smoke

```powershell
winget install --id Nub.Nub --silent --accept-package-agreements --accept-source-agreements
nub --version
```

## The fallback publisher — `submit-winget`

The release workflow's `submit-winget` job refreshes `Nub.Nub` through [winget-releaser](https://github.com/vedantmgoyal9/winget-releaser). It is a **no-op until a `WINGET_PAT` secret exists**, and it should stay that way while the community automation keeps landing each version: two submissions for one release race each other and the second is flagged as a duplicate. Enable it only if that automation stops.

1. Under the GitHub account that owns a fork of `microsoft/winget-pkgs`, create a classic PAT with the `public_repo` scope (or a fine-grained token with Contents + Pull requests write on the fork).
2. Add it as the `WINGET_PAT` repository secret on `nubjs/nub`.

## Refreshing the committed manifest fixture

The fixture pins one release (URLs + SHA256s). To track a newer one, rename the version directory, bump `PackageVersion`, `ReleaseDate`, the two `InstallerUrl`s and `ReleaseNotesUrl`, and replace each `InstallerSha256` with the uppercased value from that release's `nub-win32-<arch>.zip.sha256` sidecar asset. Keep the three `NestedInstallerFiles` entries: a published version that drops one loses that alias for every winget user on the next upgrade.
