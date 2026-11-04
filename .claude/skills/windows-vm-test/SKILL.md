---
name: windows-vm-test
description: >-
  Run ad-hoc Nub tests and debugging probes in the maintainer's existing local
  Windows 11 ARM64 QEMU/HVF VM when its external disk and runbook are present.
  Use for interactive win32-arm64 or Windows-on-ARM x64-emulation checks; use
  ci-adhoc-test for true x64-native behavior or when the local VM is unavailable.
metadata:
  internal: true
---

# Windows VM testing

Operate the existing VM; do not provision one. The known-good guest was Windows 11 ARM64 under QEMU/HVF at `/Volumes/Plex/nub-winvm`, with host port 2222 forwarded to guest SSH. The VM tests win32-arm64 natively and win32-x64 under emulation. True win32-x64-native behavior requires `windows-latest` through `ci-adhoc-test`.

## Fail-closed preflight

Run read-only checks first:

```sh
VM_ROOT=/Volumes/Plex/nub-winvm
test -d "$VM_ROOT" || {
  echo 'Windows VM disk unavailable; use ci-adhoc-test or restore the external volume.' >&2
  exit 1
}
test -r "$HOME/.ssh/nub-winvm" || {
  echo 'Windows VM SSH key unavailable.' >&2
  exit 1
}
command -v qemu-system-aarch64 qemu-img >/dev/null || {
  echo 'QEMU tooling unavailable.' >&2
  exit 1
}
find "$VM_ROOT" -maxdepth 1 -type f -print
pgrep -alf 'qemu-system-aarch64|install-loop|win11' || true
lsof -nP -iTCP:2222 -sTCP:LISTEN || true
```

Stop if the disk, its runbook, or its own lifecycle helpers are absent. Do not mount a missing volume, reconstruct QEMU arguments, create a disk, or reprovision Windows as part of this skill.

## VM lifecycle

Read the restored runbook and invoke its exact start, stop, and snapshot commands. Before launch, prove that no other QEMU process owns `win11.qcow2`. Never run competing QEMU or installer loops against one disk.

The historical clean-snapshot command is evidence, not a default action:

```sh
cd /Volumes/Plex/nub-winvm
qemu-img snapshot -c clean win11.qcow2
```

Create or apply a snapshot only while the VM state matches the restored runbook. Never guess a snapshot-restore sequence.

## SSH readiness

Gate readiness on a guest command, not an open forwarded port:

```sh
SSH_OPTS=(
  -i "$HOME/.ssh/nub-winvm"
  -p 2222
  -o StrictHostKeyChecking=no
  -o UserKnownHostsFile=/dev/null
)
SCP_OPTS=(
  -i "$HOME/.ssh/nub-winvm"
  -P 2222
  -o StrictHostKeyChecking=no
  -o UserKnownHostsFile=/dev/null
)

ssh "${SSH_OPTS[@]}" nub@localhost \
  'powershell -NoProfile -Command "$PSVersionTable.OS; $env:PROCESSOR_ARCHITECTURE"'
```

Record the reported OS and architecture with every result.

## Transfer and run a probe

Copy only the binary, script, and fixtures the probe needs into the remote user's home:

```sh
NUB_EXE=/absolute/path/to/nub.exe
test -f "$NUB_EXE"
file "$NUB_EXE"
shasum -a 256 "$NUB_EXE"

scp "${SCP_OPTS[@]}" "$NUB_EXE" nub@localhost:nub.exe
scp "${SCP_OPTS[@]}" ./probe.ps1 nub@localhost:probe.ps1
ssh "${SSH_OPTS[@]}" nub@localhost \
  'powershell -NoProfile -ExecutionPolicy Bypass -File "$HOME\probe.ps1"'
```

For a Rust failure, preserve the exit code while capturing a backtrace and log:

```powershell
$ErrorActionPreference = 'Stop'
$env:RUST_BACKTRACE = 'full'
$env:RUST_LOG = 'debug'

& "$HOME\nub.exe" <args> *>&1 | Tee-Object "$HOME\nub-probe.log"
$code = $LASTEXITCODE
Write-Host "EXIT_CODE=$code"
exit $code
```

Use `Get-Command nub -All`, `where.exe nub`, `Get-Item`, and `Get-Acl` for resolution or filesystem diagnostics. Kill only processes created by the probe.

## Run the Windows survey

After the runbook's clean-snapshot restore, copy and run `tests/windows/papercut-survey.ps1`:

```powershell
& "$HOME\papercut-survey.ps1" `
  -NubBin "$HOME\nub.exe" `
  -WorkDir "$HOME\nub-papercut" `
  -OutputJson "$HOME\nub-papercut\results.json"
exit $LASTEXITCODE
```

Retrieve `nub-papercut/results.json` and any focused log before stopping or reverting the guest.

## Evidence and CI boundary

Record the host commit, guest build/architecture, binary architecture, exact PowerShell invocation, exit status, stdout/stderr, and retrieved artifacts. The local ARM VM does not prove x64-native behavior and is not a clean hosted runner. Invoke `ci-adhoc-test` for a fresh Windows environment, x64-native behavior, unavailable local storage, or release-platform evidence.
