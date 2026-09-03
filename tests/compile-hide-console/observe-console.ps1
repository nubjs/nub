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
  $proc = Start-Process -FilePath $exe -PassThru
  Start-Sleep -Milliseconds $SettleMs
  $after  = [Win]::Consoles()
  $hosts1 = @(Get-Process -Name conhost, OpenConsole -ErrorAction SilentlyContinue).Count
  try { if (-not $proc.HasExited) { $proc.Kill() } } catch { }
  $windows = @($after | Where-Object { $before -notcontains $_ }).Count
  $hosts = $hosts1 - $hosts0
  Write-Host ("  {0}: {1} new console host(s), {2} new console window(s)" -f $label, $hosts, $windows)
  return $hosts
}

$fail = 0

# The control runs FIRST and on purpose. If an ordinary compiled binary allocates
# no console either, this host cannot show one and the hidden result below proves
# nothing - which is a different answer from "the feature works".
Write-Host "== control: a binary built WITHOUT --hide-console =="
$shownHosts = Measure-Launch $Shown 'shown.exe'
if ($shownHosts -lt 1) {
  Write-Host "  INCONCLUSIVE: the control allocated no console either, so this host"
  Write-Host "                cannot show one and the arm below would be vacuous."
  Write-Host "RESULT: inconclusive on this host"
  exit 0
}
Write-Host "  ok: the control allocates a console, so this host can"

Write-Host "== the claim: --hide-console allocates none =="
$hiddenHosts = Measure-Launch $Hidden 'hidden.exe'
if ($hiddenHosts -eq 0) {
  Write-Host "  ok: no console was allocated"
} else {
  Write-Host ("  FAIL: {0} console host(s) started, so a window would appear" -f $hiddenHosts)
  $fail = 1
}

Write-Host ""
if ($fail -eq 0) { Write-Host "RESULT: all checks passed" } else { Write-Host "RESULT: $fail check(s) failed" }
exit $fail
