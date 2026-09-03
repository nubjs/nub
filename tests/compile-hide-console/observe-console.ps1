# Does a console actually appear? - the one question CI cannot answer.
#
# ASCII ONLY, deliberately. PowerShell 5.1 reads a BOM-less script in the ANSI
# codepage, so a single em-dash in a comment fails the whole file with "The string
# is missing the terminator", pointing at the last line rather than the offending
# one. Keep every character in this file plain ASCII.
#
# run.sh asserts everything measurable without a desktop: the subsystem byte, that
# the artifact runs, and that the launcher took the suppressing path. None of that
# is the user-visible claim. This script makes the claim directly, by launching the
# artifact the way Explorer does and watching for a console being allocated.
#
# NOT wired into CI. A hosted runner may not be able to allocate a console at all,
# in which case every assertion below passes without meaning anything - the exact
# false green this harness exists to avoid. The control arm detects that and says
# so instead of reporting a pass.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File observe-console.ps1 `
#     -Hidden .\hidden.exe -Shown .\shown.exe
param(
  [Parameter(Mandatory = $true)][string]$Hidden,
  [Parameter(Mandatory = $true)][string]$Shown,
  # The fixture must write this file and then exit 7. Standard output has nowhere
  # to go when there is no console, so a file is the only channel that proves the
  # program actually ran rather than dying on its own invalid handles.
  [string]$Marker = "$env:TEMP\nub-hidden-probe.txt",
  [int]$SettleMs = 2000
)
$ErrorActionPreference = 'Stop'

Add-Type @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;
public static class Win {
  delegate bool EnumProc(IntPtr hWnd, IntPtr lParam);
  [DllImport("user32.dll")] static extern bool EnumWindows(EnumProc cb, IntPtr p);
  [DllImport("user32.dll", CharSet = CharSet.Unicode)]
  static extern int GetClassName(IntPtr hWnd, StringBuilder buf, int max);
  [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr hWnd);
  // Every console window is one of these two classes: the classic conhost window
  // and the Windows 11 / Windows Terminal one. Counted by class rather than by
  // title, which a program can change.
  public static List<IntPtr> Consoles() {
    var found = new List<IntPtr>();
    EnumWindows((h, p) => {
      var sb = new StringBuilder(256);
      if (GetClassName(h, sb, sb.Capacity) > 0) {
        var cls = sb.ToString();
        if ((cls == "ConsoleWindowClass" || cls == "PseudoConsoleWindow") && IsWindowVisible(h))
          found.Add(h);
      }
      return true;
    }, IntPtr.Zero);
    return found;
  }
}
'@

# Two independent signals, because each one is blind somewhere.
#
# CONSOLE HOST COUNT is the primary. Windows starts one console host process per
# console it allocates, and it does so in every window station - including the
# non-interactive one an SSH session runs in, where window enumeration sees
# nothing at all. This is the signal that survives being driven remotely.
#
# WINDOW COUNT is the confirmation, and it is what literally answers "did
# something appear on screen". It only means anything on an interactive desktop,
# so it is reported rather than asserted on.
function Measure-Launch([string]$exe, [string]$label) {
  $before = [Win]::Consoles()
  $hosts0 = @(Get-Process -Name conhost, OpenConsole -ErrorAction SilentlyContinue).Count
  # ShellExecute, which is what a double-click in Explorer does. Unlike a direct
  # spawn it hands the child no console of ours to inherit, so a console-subsystem
  # program gets a brand new one.
  Remove-Item -Force -ErrorAction SilentlyContinue $script:Marker
  $proc = Start-Process -FilePath $exe -PassThru
  Start-Sleep -Milliseconds $SettleMs
  $after  = [Win]::Consoles()
  $hosts1 = @(Get-Process -Name conhost, OpenConsole -ErrorAction SilentlyContinue).Count
  # Let it finish rather than killing it. The exit code and the marker are the only
  # evidence that a GUI-subsystem launcher, whose standard handles are invalid,
  # actually ran the program instead of dying on them.
  $exited = $proc.WaitForExit(20000)
  $code = if ($exited) { $proc.ExitCode } else { try { $proc.Kill() } catch { }; 'TIMEOUT' }
  $ran = Test-Path $script:Marker
  $windows = @($after | Where-Object { $before -notcontains $_ }).Count
  $hosts = $hosts1 - $hosts0
  Write-Host ("  {0}: {1} new console host(s), {2} new console window(s), exit {3}, marker {4}" `
    -f $label, $hosts, $windows, $code, $ran)
  return [pscustomobject]@{ Hosts = $hosts; Windows = $windows; Code = $code; Ran = $ran }
}

$fail = 0
$script:Marker = $Marker

# The control runs FIRST and on purpose. If an ordinary compiled binary allocates
# no console either, this host cannot show one and the hidden result below proves
# nothing - which is a different answer from "the feature works".
Write-Host "== control: a binary built WITHOUT --hide-console =="
$control = Measure-Launch $Shown 'shown.exe'
if (-not $control.Ran) {
  Write-Host "  FAIL: the control never wrote its marker, so the fixture is wrong and"
  Write-Host "        nothing below can be believed."
  Write-Host "RESULT: 1 check(s) failed"
  exit 1
}
if ($control.Hosts -lt 1) {
  Write-Host "  INCONCLUSIVE: the control allocated no console either, so this host"
  Write-Host "                cannot show one and the arm below would be vacuous."
  Write-Host "RESULT: inconclusive on this host"
  exit 0
}
Write-Host "  ok: the control allocates a console, so this host can"

Write-Host "== the claim: --hide-console allocates none =="
$hidden = Measure-Launch $Hidden 'hidden.exe'
if ($hidden.Hosts -eq 0) {
  Write-Host "  ok: no console was allocated"
} else {
  Write-Host ("  FAIL: {0} console host(s) started, so a window would appear" -f $hidden.Hosts)
  $fail = 1
}

# Hiding a console the program needed is not a success. With no console its
# standard handles are invalid, and this is where a launcher that mishandles them
# shows up: no marker, or a nonzero exit.
Write-Host "== and it still runs, with no valid standard handles =="
if ($hidden.Ran -and $hidden.Code -eq 7) {
  Write-Host "  ok: the program ran to completion and returned its own exit code"
} else {
  Write-Host ("  FAIL: marker {0}, exit {1} (expected marker True, exit 7)" -f $hidden.Ran, $hidden.Code)
  $fail = 1
}

Write-Host ""
if ($fail -eq 0) { Write-Host "RESULT: all checks passed" } else { Write-Host "RESULT: $fail check(s) failed" }
exit $fail
