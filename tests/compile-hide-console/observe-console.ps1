# Does a console window actually appear? — the one question CI cannot answer.
#
# run.sh asserts everything measurable without a desktop: the subsystem byte, that
# the artifact runs, and that the launcher took the suppressing path. None of that
# is the user-visible claim. This script makes the claim directly, by launching the
# artifact the way Explorer does and counting the console windows that appear.
#
# NOT wired into CI. A hosted runner has no interactive desktop, so a console
# window may be un-creatable there and every assertion below would pass without
# meaning anything — the exact false green the harness is built to avoid. Run it on
# a real Windows desktop or the local Windows VM.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File observe-console.ps1 `
#     -Hidden .\hidden.exe -Shown .\shown.exe
param(
  [Parameter(Mandatory = $true)][string]$Hidden,
  [Parameter(Mandatory = $true)][string]$Shown,
  [int]$SettleMs = 1500
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
  // and the Windows 11 / Windows Terminal one. Counted rather than matched by
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

function Measure-Launch([string]$exe, [string]$label) {
  $before = [Win]::Consoles()
  $hosts0 = @(Get-Process -Name conhost, OpenConsole -ErrorAction SilentlyContinue).Count
  # ShellExecute, which is what a double-click in Explorer does — and unlike a
  # direct spawn it hands the child no console of ours to inherit, so a
  # console-subsystem program gets a brand new window.
  $proc = Start-Process -FilePath $exe -PassThru
  Start-Sleep -Milliseconds $SettleMs
  $after = [Win]::Consoles()
  $hosts1 = @(Get-Process -Name conhost, OpenConsole -ErrorAction SilentlyContinue).Count
  try { if (-not $proc.HasExited) { $proc.Kill() } } catch { }
  $new = @($after | Where-Object { $before -notcontains $_ }).Count
  Write-Host ("  {0}: {1} new console window(s), conhost delta {2}" -f $label, $new, ($hosts1 - $hosts0))
  return $new
}

$fail = 0

# The control runs FIRST and on purpose. If an ordinary compiled binary shows no
# console either, then this desktop cannot create one and the hidden result below
# proves nothing — which is a different answer from "the feature works".
Write-Host "== control: a binary built WITHOUT --hide-console =="
$shownWindows = Measure-Launch $Shown 'shown.exe'
if ($shownWindows -lt 1) {
  Write-Host "  INCONCLUSIVE: no console appeared for the control either, so this"
  Write-Host "                environment cannot show one and the arm below is vacuous."
  Write-Host "RESULT: inconclusive on this host"
  exit 0
}
Write-Host "  ok: the control opens a console, so this desktop can show one"

Write-Host "== the claim: --hide-console opens none =="
$hiddenWindows = Measure-Launch $Hidden 'hidden.exe'
if ($hiddenWindows -eq 0) {
  Write-Host "  ok: no console window appeared"
} else {
  Write-Host ("  FAIL: {0} console window(s) appeared" -f $hiddenWindows)
  $fail = 1
}

Write-Host ""
if ($fail -eq 0) { Write-Host "RESULT: all checks passed" } else { Write-Host "RESULT: $fail check(s) failed" }
exit $fail
