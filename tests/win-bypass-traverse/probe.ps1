# Does BYPASS-TRAVERSE let an AppContainer child read DEEP into %USERPROFILE% when only a
# directory BENEATH the profile carries an ACE, with C:\ and C:\Users left exactly as shipped?
#
# WHY THIS IS NOT ALREADY ANSWERED. `.fray/sandbox-MECHANISM-FACTS.md` §5/§5d/§5f measured the
# AppContainer route with `AccessCheck` and found `C:\` and `C:\Users` DENIED, and concluded the
# route is dead because those two paths cannot be ACE'd unprivileged. But a LowBox token retains
# `SeChangeNotifyPrivilege` ENABLED (measured, §5f), and that privilege makes the object manager
# skip the access check on every INTERMEDIATE path component. `AccessCheck` evaluates ONE
# descriptor and cannot model that by construction — so those DENIEDs establish only that
# `lstat`/`readdir`/`chdir` ON `C:\` and `C:\Users` fail, NOT that a deep open THROUGH them
# fails. This probe is the real launch that settles it.
#
# WHY CI AND NOT SSH. An AppContainer cannot be launched over OpenSSH: sshd lands you in
# services session 0, which has no window station a LowBox token can attach to, and every
# launch returns 0xC0000142 STATUS_DLL_INIT_FAILED (§5e — established environmentally, via
# nub's own CI-proven harness failing identically there). A restricted token is exempt; a LowBox
# token is not. So the venue is a branch-scoped GitHub Actions workflow, no PR
# (`.claude/skills/ci-adhoc-test/SKILL.md`).
#
# THE LAUNCH MIRRORS THE SHIPPING PATH, not a convenient approximation:
# `CreateAppContainerProfile` -> per-run AC SID -> inheritable GRANT_ACCESS ACEs on the allowed
# leaves -> `CreateProcessW` with `EXTENDED_STARTUPINFO_PRESENT` and
# `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`, capability count ZERO (so `internetClient` is
# withheld and egress is denied by construction). That is `crates/nub-sandbox/src/backend/
# windows.rs`'s `run()`, step for step.
#
# WHAT IS NEVER TOUCHED: no ACE, label, or DACL write of any kind on `C:\` or `C:\Users`. They
# are read (icacls, reported as facts) and otherwise left exactly as the image ships them. If
# the deep read passes, it passed through descriptors this probe did not write.
#
# THE CONTROLS, and why a result without them is worthless. This effort has been burned twice by
# tables where every arm failed identically for a HARNESS reason — six launch arms all failing
# `CreateFileW err=2` because a P/Invoke lacked `CharSet.Unicode`, and an `AccessCheck` sweep
# whose DENIEDs could not be told from a gate that denies unconditionally. So:
#
#   plain                 the SAME child, SAME paths, SAME code path minus SECURITY_CAPABILITIES.
#                         Must pass EVERYTHING. A red cell here is a harness or host defect and
#                         invalidates the whole run.
#   gate-is-live          every AppContainer arm must be DENIED on `C:\`. GRANTED would mean the
#                         token is not actually confined and every "pass" below is vacuous.
#   positive              an AppContainer arm must READ `C:\Windows\System32` (it carries an ALL
#                         APPLICATION PACKAGES ace). Without this, a table of denials cannot be
#                         told from a child that fails at everything.
#   ace-absent            the decisive deep read, with the data grant WITHHELD, must FAIL. If it
#                         passes, the ACE is doing nothing and the treatment arm proves nothing.
#   ungranted-sibling     a sibling path under %USERPROFILE% that never got a grant must FAIL, or
#                         the grant is not SCOPING anything.
#   egress-differential   egress must be denied in every AppContainer arm AND permitted in the
#                         plain arm. A deny with no matching allow is not evidence.
#
# Unprivileged by construction on the paths that matter: every ACE is written on a directory the
# invoking user OWNS (its own profile), which needs no privilege. The runner's elevation is
# reported as a fact so an elevated baseline is never mistaken for the shipping case — and since
# no write is attempted above %USERPROFILE%, elevation cannot be what makes any cell pass.

$ErrorActionPreference = 'Continue'
Set-StrictMode -Off

# W / Fact / Prop, the cell accessors, and the whole verdict live in `verdict.ps1` so that
# `selftest.ps1` can drive the verdict with synthetic cells and prove it discriminates in both
# directions. Dot-sourced, so its functions read `$cells` and write `$script:fails` here.
. (Join-Path $PSScriptRoot 'verdict.ps1')

# ─────────────────────────────── the launcher ───────────────────────────────

$src = @'
using System;
using System.Runtime.InteropServices;
using System.Text;

public static class Bt
{
    [StructLayout(LayoutKind.Sequential)]
    struct SECURITY_CAPABILITIES
    {
        public IntPtr AppContainerSid;
        public IntPtr Capabilities;
        public uint CapabilityCount;
        public uint Reserved;
    }

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    struct STARTUPINFOW
    {
        public uint cb;
        public IntPtr lpReserved, lpDesktop, lpTitle;
        public uint dwX, dwY, dwXSize, dwYSize, dwXCountChars, dwYCountChars, dwFillAttribute, dwFlags;
        public ushort wShowWindow, cbReserved2;
        public IntPtr lpReserved2, hStdInput, hStdOutput, hStdError;
    }

    [StructLayout(LayoutKind.Sequential)]
    struct STARTUPINFOEXW { public STARTUPINFOW StartupInfo; public IntPtr lpAttributeList; }

    [StructLayout(LayoutKind.Sequential)]
    struct PROCESS_INFORMATION { public IntPtr hProcess, hThread; public uint dwProcessId, dwThreadId; }

    [StructLayout(LayoutKind.Sequential)]
    struct SECURITY_ATTRIBUTES { public int nLength; public IntPtr lpSecurityDescriptor; public int bInheritHandle; }

    [DllImport("userenv.dll", CharSet = CharSet.Unicode)]
    static extern int CreateAppContainerProfile(string name, string display, string desc,
        IntPtr caps, uint capCount, out IntPtr sid);
    [DllImport("userenv.dll", CharSet = CharSet.Unicode)]
    static extern int DeleteAppContainerProfile(string name);
    // DERIVE, not CREATE: this computes the package sid from the name by hashing, touching no
    // registry and no disk. It is the zero-persistent-state form of getting an AC sid, and the
    // `ac-derive-only` arm exists to find out whether a launch works without ever registering a
    // profile — which is the difference between "cleans up after itself" and "never writes".
    [DllImport("userenv.dll", CharSet = CharSet.Unicode)]
    static extern int DeriveAppContainerSidFromAppContainerName(string name, out IntPtr sid);
    [DllImport("advapi32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool ConvertSidToStringSidW(IntPtr sid, out IntPtr str);
    [DllImport("advapi32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool ConvertStringSidToSidW(string s, out IntPtr sid);
    [DllImport("kernel32.dll")] static extern IntPtr LocalFree(IntPtr p);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool CloseHandle(IntPtr h);

    // CharSet.Unicode is LOAD-BEARING, not decoration: the ANSI default marshals the name into a
    // UTF-16 API and every open fails `err=2`, which reads exactly like "mechanism unavailable".
    // That bug cost this effort a whole run (MECHANISM-FACTS §5e).
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern IntPtr CreateFileW(string name, uint access, uint share,
        ref SECURITY_ATTRIBUTES sa, uint disp, uint flags, IntPtr templ);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool InitializeProcThreadAttributeList(IntPtr list, int count, int flags, ref IntPtr size);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool UpdateProcThreadAttribute(IntPtr list, uint flags, IntPtr attr, IntPtr value,
        IntPtr size, IntPtr prev, IntPtr ret);
    [DllImport("kernel32.dll")] static extern void DeleteProcThreadAttributeList(IntPtr list);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool CreateProcessW(string app, StringBuilder cmdline, IntPtr pa, IntPtr ta,
        bool inherit, uint flags, IntPtr env, string cwd, ref STARTUPINFOEXW si,
        out PROCESS_INFORMATION pi);
    [DllImport("kernel32.dll", SetLastError = true)] static extern uint WaitForSingleObject(IntPtr h, uint ms);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool GetExitCodeProcess(IntPtr h, out uint code);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool TerminateProcess(IntPtr h, uint code);

    const uint GENERIC_READ = 0x80000000, GENERIC_WRITE = 0x40000000;
    const uint FILE_SHARE_READ = 1, FILE_SHARE_WRITE = 2;
    const uint CREATE_ALWAYS = 2, OPEN_EXISTING = 3, FILE_ATTRIBUTE_NORMAL = 0x80;
    const uint EXTENDED_STARTUPINFO_PRESENT = 0x00080000;
    const uint STARTF_USESTDHANDLES = 0x00000100;
    // ProcThreadAttributeSecurityCapabilities(9) | PROC_THREAD_ATTRIBUTE_INPUT(0x20000).
    static readonly IntPtr PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES = new IntPtr(0x00020009);
    const uint WAIT_TIMEOUT = 0x00000102;

    public static string CreateProfile(string name)
    {
        IntPtr sid;
        int hr = CreateAppContainerProfile(name, name, name, IntPtr.Zero, 0, out sid);
        if (hr != 0) return "ERR hr=0x" + hr.ToString("x8");
        IntPtr str;
        if (!ConvertSidToStringSidW(sid, out str)) return "ERR sidstring=" + Marshal.GetLastWin32Error();
        string s = Marshal.PtrToStringUni(str);
        LocalFree(str);
        LocalFree(sid);
        return s;
    }

    public static string DeleteProfile(string name)
    {
        int hr = DeleteAppContainerProfile(name);
        return hr == 0 ? "OK" : "ERR hr=0x" + hr.ToString("x8");
    }

    public static string DeriveSid(string name)
    {
        IntPtr sid;
        int hr = DeriveAppContainerSidFromAppContainerName(name, out sid);
        if (hr != 0) return "ERR hr=0x" + hr.ToString("x8");
        IntPtr str;
        if (!ConvertSidToStringSidW(sid, out str)) return "ERR sidstring=" + Marshal.GetLastWin32Error();
        string s = Marshal.PtrToStringUni(str);
        LocalFree(str);
        LocalFree(sid);
        return s;
    }

    /// Launch `exe` with `cmdline` in `cwd`, stdout+stderr to `logPath`. When `acSidStr` is
    /// non-empty the child is a real AppContainer with ZERO capabilities (internetClient
    /// withheld); when empty it is an ordinary child — the same code path minus the one
    /// attribute, which is what makes the plain arm a control rather than a different program.
    public static string Launch(string acSidStr, string exe, string cmdline, string cwd,
        string logPath, uint timeoutMs)
    {
        IntPtr acSid = IntPtr.Zero;
        IntPtr attrList = IntPtr.Zero;
        IntPtr capsBuf = IntPtr.Zero;
        IntPtr hOut = new IntPtr(-1), hIn = new IntPtr(-1);
        try
        {
            SECURITY_ATTRIBUTES sa = new SECURITY_ATTRIBUTES();
            sa.nLength = Marshal.SizeOf(typeof(SECURITY_ATTRIBUTES));
            sa.bInheritHandle = 1;
            // The log handle is opened by the UNCONFINED parent and inherited already-open.
            // Access is checked at open, so the child writes its table even when it can read
            // nothing at all — which is the difference between a negative result and no result.
            hOut = CreateFileW(logPath, GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE,
                ref sa, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, IntPtr.Zero);
            if (hOut == new IntPtr(-1)) return "launch-error CreateFileW(log) err=" + Marshal.GetLastWin32Error();
            hIn = CreateFileW("NUL", GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE,
                ref sa, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, IntPtr.Zero);
            if (hIn == new IntPtr(-1)) return "launch-error CreateFileW(NUL) err=" + Marshal.GetLastWin32Error();

            bool confined = !string.IsNullOrEmpty(acSidStr);
            STARTUPINFOEXW si = new STARTUPINFOEXW();
            // `cb` must match the struct actually passed: STARTUPINFOEXW only with
            // EXTENDED_STARTUPINFO_PRESENT, plain STARTUPINFOW without it. Passing the extended
            // size unflagged risks ERROR_INVALID_PARAMETER on the PLAIN arm — i.e. it would break
            // the one control that makes every other row attributable.
            si.StartupInfo.cb = (uint)Marshal.SizeOf(confined ? typeof(STARTUPINFOEXW) : typeof(STARTUPINFOW));
            si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
            si.StartupInfo.hStdInput = hIn;
            si.StartupInfo.hStdOutput = hOut;
            si.StartupInfo.hStdError = hOut;

            uint flags = 0;
            if (confined)
            {
                if (!ConvertStringSidToSidW(acSidStr, out acSid))
                    return "launch-error ConvertStringSidToSid err=" + Marshal.GetLastWin32Error();

                IntPtr size = IntPtr.Zero;
                InitializeProcThreadAttributeList(IntPtr.Zero, 1, 0, ref size);
                attrList = Marshal.AllocHGlobal(size);
                if (!InitializeProcThreadAttributeList(attrList, 1, 0, ref size))
                    return "launch-error InitializeProcThreadAttributeList err=" + Marshal.GetLastWin32Error();

                SECURITY_CAPABILITIES caps = new SECURITY_CAPABILITIES();
                caps.AppContainerSid = acSid;
                caps.Capabilities = IntPtr.Zero;   // ZERO capabilities: no internetClient.
                caps.CapabilityCount = 0;
                caps.Reserved = 0;
                capsBuf = Marshal.AllocHGlobal(Marshal.SizeOf(typeof(SECURITY_CAPABILITIES)));
                Marshal.StructureToPtr(caps, capsBuf, false);
                bool upd = UpdateProcThreadAttribute(attrList, 0,
                    PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, capsBuf,
                    new IntPtr(Marshal.SizeOf(typeof(SECURITY_CAPABILITIES))), IntPtr.Zero, IntPtr.Zero);
                if (!upd) return "launch-error UpdateProcThreadAttribute err=" + Marshal.GetLastWin32Error();
                si.lpAttributeList = attrList;
                flags |= EXTENDED_STARTUPINFO_PRESENT;
            }

            PROCESS_INFORMATION pi;
            StringBuilder cl = new StringBuilder(cmdline, cmdline.Length + 64);
            // env = NULL: the child inherits THIS process's environment, which is where the
            // BT_* path variables the child reads were set.
            bool ok = CreateProcessW(exe, cl, IntPtr.Zero, IntPtr.Zero, true, flags,
                IntPtr.Zero, cwd, ref si, out pi);
            if (!ok) return "launch-error CreateProcessW err=" + Marshal.GetLastWin32Error();

            uint wr = WaitForSingleObject(pi.hProcess, timeoutMs);
            uint code = 0xFFFFFFFF;
            string extra = "";
            if (wr == WAIT_TIMEOUT)
            {
                TerminateProcess(pi.hProcess, 0xDEAD);
                extra = " TIMED-OUT";
            }
            else
            {
                GetExitCodeProcess(pi.hProcess, out code);
            }
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
            return "rc=" + code + " (0x" + code.ToString("x8") + ")" + extra;
        }
        finally
        {
            if (hOut != new IntPtr(-1)) CloseHandle(hOut);
            if (hIn != new IntPtr(-1)) CloseHandle(hIn);
            if (attrList != IntPtr.Zero) { DeleteProcThreadAttributeList(attrList); Marshal.FreeHGlobal(attrList); }
            if (capsBuf != IntPtr.Zero) Marshal.FreeHGlobal(capsBuf);
            if (acSid != IntPtr.Zero) LocalFree(acSid);
        }
    }
}
'@

Add-Type -TypeDefinition $src -Language CSharp -ErrorAction Stop

# ─────────────────────────────── host facts ───────────────────────────────

W ''
W 'PROBE windows appcontainer bypass-traverse'
W ''
W '== host =='
Fact 'os' ((Get-CimInstance Win32_OperatingSystem).Caption + ' build ' + [Environment]::OSVersion.Version.ToString())
Fact 'arch' $env:PROCESSOR_ARCHITECTURE
Fact 'whoami' (whoami)
Fact 'userprofile' $env:USERPROFILE
$id = [Security.Principal.WindowsIdentity]::GetCurrent()
$pr = New-Object Security.Principal.WindowsPrincipal($id)
Fact 'elevated' $pr.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
Fact 'session-id' (Get-Process -Id $PID).SessionId
Fact 'powershell' $PSVersionTable.PSVersion.ToString()
$nodeExe = (Get-Command node -ErrorAction SilentlyContinue).Source
Fact 'node-exe' $nodeExe
Fact 'node-version' (& node -p 'process.versions.node')

# The two descriptors this probe must NOT write, read so the reader can see they are as shipped.
foreach ($p in @('C:\', 'C:\Users', $env:USERPROFILE)) {
  W "  fact:icacls[$p] ="
  (icacls $p 2>&1) | ForEach-Object { W ("    " + $_) }
}

# ─────────────────────────────── fixture ───────────────────────────────

$nonce = [Guid]::NewGuid().ToString('N').Substring(0, 8)
$root = Join-Path $env:USERPROFILE "nubbt-$nonce"
$runtimeDir = Join-Path $root 'runtime'
$dataDir = Join-Path $root 'data'
$otherDir = Join-Path $root 'other'
$projDir = Join-Path $dataDir 'proj'
$deepDir = Join-Path $projDir 'node_modules\dep'
$deep = Join-Path $deepDir 'index.js'
$sibProfile = Join-Path $env:USERPROFILE "nubbt-ungranted-$nonce\secret.txt"
$sibInside = Join-Path $otherDir 'secret.txt'
$logDir = Join-Path $PWD 'bt-logs'

foreach ($d in @($runtimeDir, $deepDir, $otherDir, (Split-Path $sibProfile), $logDir,
                 (Join-Path $projDir 'node_modules\dep2'))) {
  New-Item -ItemType Directory -Force -Path $d | Out-Null
}

# The deep dep doubles as an ENTRY POINT. `require.main === module` is what separates the two
# uses: as an entry it proves `resolveMainPath`'s realpath — which opens every prefix as a
# TARGET, not as an intermediate component — survived the walk down from C:\.
@'
module.exports = { marker: 'deep-dep-ok' };
if (require.main === module) {
  const out = (s) => process.stdout.write(s + '\n');
  out('op:entry-as-deep-file=OK ' + __filename);
  try { out('op:entry-cwd=OK ' + process.cwd()); } catch (e) { out('op:entry-cwd=ERR ' + (e.code || e)); }
  try { out('op:entry-realpath=OK ' + require('fs').realpathSync(__filename)); }
  catch (e) { out('op:entry-realpath=ERR ' + (e.code || e)); }
  try { out('op:entry-require-bare=OK ' + JSON.stringify(require('dep2'))); }
  catch (e) { out('op:entry-require-bare=ERR ' + (e.code || e) + ' ' + String(e.message).split('\n')[0]); }
  try { out('op:entry-read-c-root=OK ' + require('fs').readdirSync('C:\\').length); }
  catch (e) { out('op:entry-read-c-root=ERR ' + (e.code || e)); }
  out('child:done arm=entry');
}
'@ | Set-Content -LiteralPath $deep -Encoding ASCII

# A BARE specifier: resolving it makes `_nodeModulePaths` probe `<dir>\node_modules` at every
# ancestor up to the drive root, so it exercises the ungranted chain as a series of real opens.
@'
module.exports = { marker: 'bare-resolve-ok' };
'@ | Set-Content -LiteralPath (Join-Path $projDir 'node_modules\dep2\index.js') -Encoding ASCII

'top-secret-inside' | Set-Content -LiteralPath $sibInside -Encoding ASCII
'top-secret-profile-sibling' | Set-Content -LiteralPath $sibProfile -Encoding ASCII
'{"name":"proj"}' | Set-Content -LiteralPath (Join-Path $projDir 'package.json') -Encoding ASCII

# node.exe is COPIED into the granted runtime dir rather than granted where it lives. Granting
# `C:\Program Files\nodejs` would need WRITE_DAC on a path the user does not own, i.e. exactly
# the elevation this design must not require — and it would leave the result unattributable.
Copy-Item -LiteralPath $nodeExe -Destination (Join-Path $runtimeDir 'node.exe') -Force
$childSrc = Join-Path $PSScriptRoot 'child.js'
Copy-Item -LiteralPath $childSrc -Destination (Join-Path $runtimeDir 'child.js') -Force
$jailNode = Join-Path $runtimeDir 'node.exe'
$jailChild = Join-Path $runtimeDir 'child.js'

Fact 'fixture-root' $root
Fact 'deep-file' $deep

# The secrets the whole exercise exists to keep unreachable. Real paths in the real profile, not a
# stand-in: the claim is about `%USERPROFILE%\.ssh\id_rsa` and `.npmrc`, so those are the paths
# measured. Only files this probe CREATED are removed at teardown — an `.npmrc` the runner image or
# setup-node already wrote is read as-is and left alone.
$sshDir = Join-Path $env:USERPROFILE '.ssh'
$sshKey = Join-Path $sshDir 'id_rsa'
$npmrc = Join-Path $env:USERPROFILE '.npmrc'
New-Item -ItemType Directory -Force -Path $sshDir | Out-Null
$createdSshKey = -not (Test-Path -LiteralPath $sshKey)
if ($createdSshKey) {
  "-----BEGIN OPENSSH PRIVATE KEY-----`nSECRET-CANARY-DO-NOT-LEAK`n-----END OPENSSH PRIVATE KEY-----" |
    Set-Content -LiteralPath $sshKey -Encoding ASCII
}
$createdNpmrc = -not (Test-Path -LiteralPath $npmrc)
if ($createdNpmrc) { '//registry.npmjs.org/:_authToken=SECRET-CANARY-TOKEN' | Set-Content -LiteralPath $npmrc -Encoding ASCII }
Fact 'ssh-key-path' "$sshKey (created-by-probe=$createdSshKey)"
Fact 'npmrc-path' "$npmrc (created-by-probe=$createdNpmrc)"
$env:BT_SSH_KEY = $sshKey
$env:BT_NPMRC = $npmrc

$env:BT_DEEP = $deep
$env:BT_DEEPDIR = $deepDir
$env:BT_DATA = $dataDir
$env:BT_RUNTIME = $runtimeDir
$env:BT_SIB_INSIDE = $sibInside
$env:BT_SIB_PROFILE = $sibProfile

# ── no pre-existing AppContainer grant may reach the tree ──
# An inherited `ALL APPLICATION PACKAGES` (or any `S-1-15-*`) ace would make the ace-absent
# control pass for a reason that has nothing to do with the grant under test. Detected, and
# stripped by disabling inheritance on the test root — an ordinary owner operation on a path in
# the user's own profile, and it touches no ancestor.
function Get-AcAces([string]$path) {
  return @((Get-Acl -LiteralPath $path).Access |
    Where-Object { $_.IdentityReference.Value -like 'S-1-15-*' })
}
function Strip-AppContainerAces([string]$path) {
  if ((Get-AcAces $path).Count -eq 0) { return 0 }
  $acl = Get-Acl -LiteralPath $path
  $acl.SetAccessRuleProtection($true, $true)   # stop inheriting, keep the copies, then remove them
  Set-Acl -LiteralPath $path -AclObject $acl
  $acl = Get-Acl -LiteralPath $path
  $n = 0
  foreach ($r in @($acl.Access | Where-Object { $_.IdentityReference.Value -like 'S-1-15-*' })) {
    $null = $acl.RemoveAccessRuleSpecific($r); $n++
  }
  Set-Acl -LiteralPath $path -AclObject $acl
  return $n
}
# RECURSIVE, and that is the point: an inheritable ace on an ancestor is physically COPIED into
# every child at creation time, so stripping only the test root would leave live copies on the
# decisive deep file — which would make the ace-absent control pass and the whole run a lie.
function Strip-AppContainerAcesTree([string]$path) {
  $n = 0
  $targets = @($path) + @(Get-ChildItem -LiteralPath $path -Recurse -Force |
    ForEach-Object { $_.FullName })
  foreach ($t in $targets) { $n += (Strip-AppContainerAces $t) }
  return $n
}
function Count-AcAcesTree([string]$path) {
  $n = 0
  $targets = @($path) + @(Get-ChildItem -LiteralPath $path -Recurse -Force |
    ForEach-Object { $_.FullName })
  foreach ($t in $targets) { $n += (Get-AcAces $t).Count }
  return $n
}
$sibRoot = Split-Path $sibProfile
Fact 'appcontainer-aces-stripped-from-test-tree' (Strip-AppContainerAcesTree $root)
Fact 'appcontainer-aces-stripped-from-profile-sibling' (Strip-AppContainerAcesTree $sibRoot)
$residual = Count-AcAcesTree $root
$residualSib = Count-AcAcesTree $sibRoot
Prop 'no-appcontainer-ace-on-test-tree' (($residual -eq 0) -and ($residualSib -eq 0)) `
  "root-tree=$residual profile-sibling-tree=$residualSib S-1-15-* aces remain; a residual one would make the ace-absent control pass for the wrong reason"

# ─────────────────────────────── ACE plumbing ───────────────────────────────

function Grant-Ace([string]$path, [string]$sid, [string]$rights) {
  $acl = Get-Acl -LiteralPath $path
  $trustee = New-Object System.Security.Principal.SecurityIdentifier($sid)
  $inherit = [System.Security.AccessControl.InheritanceFlags]::ContainerInherit -bor `
             [System.Security.AccessControl.InheritanceFlags]::ObjectInherit
  $rule = New-Object System.Security.AccessControl.FileSystemAccessRule(
    $trustee, [System.Security.AccessControl.FileSystemRights]$rights, $inherit,
    [System.Security.AccessControl.PropagationFlags]::None,
    [System.Security.AccessControl.AccessControlType]::Allow)
  $acl.AddAccessRule($rule)
  Set-Acl -LiteralPath $path -AclObject $acl
}

function Revoke-Ace([string]$path, [string]$sid) {
  $acl = Get-Acl -LiteralPath $path
  $trustee = New-Object System.Security.Principal.SecurityIdentifier($sid)
  $null = $acl.PurgeAccessRules($trustee)
  Set-Acl -LiteralPath $path -AclObject $acl
}

# ────────────────── THE ZERO-SETUP GATE (report this before any capability) ──────────────────
# A mechanism is only acceptable if the VERY FIRST `nub install` on a fresh machine works as a
# standard user with nothing registered and no prior command run. `CreateAppContainerProfile` is
# the one call on this path that plausibly writes persistent machine state, so measure what it
# leaves — before, after create, and after delete — rather than inferring ephemerality from the
# fact that the shipping code calls Delete on drop.
#
# Two registries of AppContainer state: the profile MAPPING under HKCU (name <-> sid) and the
# profile DIRECTORY under %LOCALAPPDATA%\Packages.
$mapKey = 'HKCU:\Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppContainer\Mappings'
function Map-Count { if (Test-Path $mapKey) { return @(Get-ChildItem $mapKey -ErrorAction SilentlyContinue).Count } else { return -1 } }
function Map-Has([string]$sid) { if (-not $sid -or -not (Test-Path $mapKey)) { return $false } return (Test-Path (Join-Path $mapKey $sid)) }

W ''
W '== zero-setup gate =='
$zsName = "nubbt_zs_$nonce"
Fact 'zs/mappings-before' (Map-Count)
$zsDerived = [Bt]::DeriveSid($zsName)
Fact 'zs/derive-without-create' $zsDerived
Fact 'zs/mapping-exists-after-derive-only' (Map-Has $zsDerived)
$zsPkgDir = Join-Path $env:LOCALAPPDATA "Packages\$zsName"
Fact 'zs/package-dir-after-derive-only' (Test-Path -LiteralPath $zsPkgDir)
$zsCreated = [Bt]::CreateProfile($zsName)
Fact 'zs/create-profile' $zsCreated
Fact 'zs/derive-equals-create' ($zsCreated -eq $zsDerived)
Fact 'zs/mappings-after-create' (Map-Count)
Fact 'zs/mapping-exists-after-create' (Map-Has $zsCreated)
Fact 'zs/package-dir-after-create' (Test-Path -LiteralPath $zsPkgDir)
Fact 'zs/delete-profile' ([Bt]::DeleteProfile($zsName))
$zsMapAfterDelete = Map-Has $zsCreated
$zsDirAfterDelete = Test-Path -LiteralPath $zsPkgDir
Fact 'zs/mappings-after-delete' (Map-Count)
Fact 'zs/mapping-exists-after-delete' $zsMapAfterDelete
Fact 'zs/package-dir-after-delete' $zsDirAfterDelete
Prop 'zero-setup-profile-leaves-no-residue' ((-not $zsMapAfterDelete) -and (-not $zsDirAfterDelete)) `
  "after DeleteAppContainerProfile: HKCU mapping present=$zsMapAfterDelete, %LOCALAPPDATA%\Packages dir present=$zsDirAfterDelete — both must be gone or the mechanism accumulates machine state"

# ─────────────────────────────── the arms ───────────────────────────────

$cells = @{}

function Invoke-Arm {
  param(
    [string]$Name,
    [bool]$AppContainer,
    [string[]]$GrantRX = @(),
    [string[]]$GrantModify = @(),
    [string]$Cwd,
    [string]$EntryFile,   # $null => the child.js operations table
    # RUN 1 FINDING (30506129146): an unflagged confined `node` dies in `resolveMainPath` with
    # `EPERM lstat 'C:\'` before a single user statement runs — bypass-traverse exempts
    # INTERMEDIATE path components, but Node's JS `realpathSync` opens the volume root as a
    # TARGET. These two flags are the seams that skip `toRealPath`, and they are what lets the
    # operations table run at all. Uniform across EVERY arm (the plain baseline included) so the
    # arms stay one variable apart; the `ac-noflags` arm withholds them as the differential.
    [string[]]$NodeFlags = @('--preserve-symlinks-main', '--preserve-symlinks'),
    # Derive the package sid by hashing the name instead of registering a profile. If a launch
    # works this way, the mechanism writes NO persistent state at all — a stronger zero-setup
    # answer than "it cleans up after itself".
    [switch]$DeriveOnly
  )
  W ''
  W "== arm $Name =="
  $sid = ''
  $profileName = ''
  if ($AppContainer) {
    $nm = "nubbt_${nonce}_$($Name -replace '[^A-Za-z0-9]','_')"
    if ($nm.Length -gt 60) { $nm = $nm.Substring(0, 60) }
    if ($DeriveOnly) {
      $sid = [Bt]::DeriveSid($nm)
      Fact "$Name/derived-sid-no-profile" "$nm -> $sid"
    } else {
      $profileName = $nm
      $sid = [Bt]::CreateProfile($profileName)
      Fact "$Name/profile" "$profileName -> $sid"
    }
    # No dynamic `prop:` name here — the workflow's verdict requires a FIXED list of property
    # names, and a name that only appears on failure cannot be required. A missing arm surfaces
    # as MISSING-ARM in the table and fails the decisive properties below, which is correct.
    if ($sid -like 'ERR*') { $script:fails++; return }
  }
  $granted = @()
  try {
    foreach ($p in $GrantRX) { Grant-Ace $p $sid 'ReadAndExecute'; $granted += $p }
    foreach ($p in $GrantModify) { Grant-Ace $p $sid 'Modify'; $granted += $p }
    Fact "$Name/grants" $(if ($granted.Count) { ($granted -join ' ; ') } else { '(none)' })

    # READ THE DECISIVE TARGET'S DACL BACK. The grant is written on an ANCESTOR as an inheritable
    # ace and relies on `SetNamedSecurityInfo` propagating it into the already-existing deep file.
    # If that propagation did not happen, the deep read would fail for a reason with nothing to do
    # with traverse — a false negative that reads exactly like "AppContainer is dead". So it is
    # verified, per arm, rather than assumed; and in the ace-absent arm the SAME check must come
    # back empty, which is what makes that control a control.
    $deepAce = 'none'
    if ($AppContainer) {
      $found = @((Get-Acl -LiteralPath $deep).Access |
        Where-Object { $_.IdentityReference.Value -eq $sid } |
        ForEach-Object { "$($_.FileSystemRights)" })
      if ($found.Count) { $deepAce = ($found -join ',') }
    }
    Fact "$Name/deep-file-dacl-for-ac-sid" $deepAce

    $env:BT_ARM = $Name
    $log = Join-Path $logDir "$Name.log"
    $entry = if ($EntryFile) { $EntryFile } else { $jailChild }
    $flagPart = if ($NodeFlags.Count) { ' ' + ($NodeFlags -join ' ') } else { '' }
    $cmdline = '"' + $jailNode + '"' + $flagPart + ' "' + $entry + '"'
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $status = [Bt]::Launch($sid, $jailNode, $cmdline, $Cwd, $log, 120000)
    $sw.Stop()
    Fact "$Name/launch" "$status in $($sw.ElapsedMilliseconds)ms cwd=$Cwd"
    Fact "$Name/cmdline" $cmdline

    $lines = @()
    if (Test-Path -LiteralPath $log) { $lines = @(Get-Content -LiteralPath $log -ErrorAction SilentlyContinue) }
    Fact "$Name/log-lines" $lines.Count
    $arm = @{}
    foreach ($l in $lines) {
      W ("    | " + $l)
      if ($l -match '^op:([^=]+)=(OK|ERR)\s*(.*)$') { $arm[$Matches[1]] = @($Matches[2], $Matches[3]) }
    }
    # Harness-side cells, not child observations — hence the `dacl:`/`log:` prefixes on the detail
    # so a reader never mistakes them for something the confined process reported.
    $arm['dacl-grants-ac-sid'] = @($(if ($deepAce -eq 'none') { 'ERR' } else { 'OK' }), "dacl:$deepAce")
    # Did the child die in Node's own realpath walk on the volume root, before user code? This is
    # a property of the LOG, not of an op line — an unflagged confined node emits no op lines at
    # all, so without this cell that arm is indistinguishable from a launch that never happened.
    $raw = ($lines -join "`n")
    $diedRealpath = ($raw -match "EPERM") -and ($raw -match "lstat") -and ($raw -match "realpathSync")
    $arm['node-died-realpath-c-root'] = @($(if ($diedRealpath) { 'OK' } else { 'ERR' }), 'log:derived')
    $arm['__opcount'] = @(@($arm.Keys | Where-Object { $_ -notlike '__*' -and $_ -notlike 'dacl-*' -and $_ -ne 'node-died-realpath-c-root' }).Count, '')
    $arm['__launch'] = @($status, '')
    $arm['__lines'] = @($lines.Count, '')
    $cells[$Name] = $arm
  }
  finally {
    foreach ($p in $granted) { try { Revoke-Ace $p $sid } catch { W "    revoke failed on $p : $_" } }
    if ($profileName) { Fact "$Name/profile-deleted" ([Bt]::DeleteProfile($profileName)) }
  }
}

# 1. The control that makes every other row readable: identical child, identical paths, no
#    SECURITY_CAPABILITIES, no ACEs (the user already owns its own profile).
Invoke-Arm -Name 'plain' -AppContainer $false -Cwd $runtimeDir

# 2. The realistic shape — ONE inheritable grant at a project root directly beneath the profile.
#    Ungranted ancestors: %USERPROFILE%, C:\Users, C:\.
Invoke-Arm -Name 'ac-root-grant' -AppContainer $true -GrantModify @($root) -Cwd $runtimeDir

# 3. Leaf-only grants — the shipping backend's actual model. Adds the test root itself to the
#    ungranted chain, so the traverse skip has one more component to cross.
Invoke-Arm -Name 'ac-leaf-grants' -AppContainer $true -GrantRX @($runtimeDir) `
  -GrantModify @($dataDir) -Cwd $runtimeDir

# 4. NEGATIVE CONTROL. Identical to 3 with the data grant WITHHELD — one variable. The deep read
#    must FAIL here, or the ACE in arm 3 was not what let it through.
Invoke-Arm -Name 'ac-data-ungranted' -AppContainer $true -GrantRX @($runtimeDir) -Cwd $runtimeDir

# 5. cwd set at LAUNCH to the deep dir: `CreateProcessW` must itself open a path five components
#    below the last granted ancestor before any user code exists.
Invoke-Arm -Name 'ac-cwd-deep' -AppContainer $true -GrantRX @($runtimeDir) `
  -GrantModify @($dataDir) -Cwd $deepDir

# 6. `node <deep file>` as the ENTRY POINT — `resolveMainPath`'s realpath runs before user code.
Invoke-Arm -Name 'ac-entry-deep' -AppContainer $true -GrantRX @($runtimeDir) `
  -GrantModify @($dataDir) -Cwd $runtimeDir -EntryFile $deep

# 7. THE FLAG DIFFERENTIAL. Byte-identical to arm 3 with the two realpath-skipping flags WITHHELD
#    — one variable. Run 1 (30506129146) measured this shape dying at `EPERM lstat 'C:\'` in
#    `resolveMainPath` on both images, and this arm keeps that defect a first-class measured cell
#    rather than a fact recalled from a previous run.
Invoke-Arm -Name 'ac-noflags' -AppContainer $true -GrantRX @($runtimeDir) `
  -GrantModify @($dataDir) -Cwd $runtimeDir -NodeFlags @()

# 8. THE ZERO-SETUP ARM. Same as arm 3, but the package sid is DERIVED by hashing the name and no
#    profile is ever registered. If this launches and reads, the mechanism writes no persistent
#    machine state whatsoever — which is a stronger answer to "does the first `nub install` on a
#    fresh machine work" than measuring that Delete cleans up after Create.
Invoke-Arm -Name 'ac-derive-only' -AppContainer $true -DeriveOnly -GrantRX @($runtimeDir) `
  -GrantModify @($dataDir) -Cwd $runtimeDir

# ── ACE RESIDUE: what does a run leave behind on the project tree? ──
# Every grant is revoked in each arm's `finally`, so a residual ace naming a now-dead sid would
# mean the teardown is incomplete and repeated installs accumulate cruft on the user's project.
$residueAfter = Count-AcAcesTree $root
Fact 'ace-residue-after-all-arms' "$residueAfter S-1-15-* aces remain on the test tree"
Prop 'ace-residue-none-after-revoke' ($residueAfter -eq 0) `
  "revoke must leave no ace naming a dead per-run sid on the project tree: $residueAfter remain"

# ─────────────────────────────── ACE cost ───────────────────────────────
# The known hazard: grants are inheritable ACEs written per launch and revoked after, and
# `SetNamedSecurityInfo` propagates them to every existing child. A broad grant previously blew a
# 25-minute CI step. Sized to match the restricted-token lane's 3,878-entry fixture so the two
# mechanisms' per-launch costs are directly comparable.
W ''
W '== ace cost =='
$costRoot = Join-Path $env:USERPROFILE "nubbt-cost-$nonce"
$sw = [Diagnostics.Stopwatch]::StartNew()
$made = 0
for ($d = 0; $d -lt 40; $d++) {
  $dir = Join-Path $costRoot ("d$d\node_modules\pkg$d")
  New-Item -ItemType Directory -Force -Path $dir | Out-Null
  for ($f = 0; $f -lt 97; $f++) {
    [IO.File]::WriteAllText((Join-Path $dir "f$f.js"), "module.exports=$f;")
    $made++
  }
}
$sw.Stop()
$entries = @(Get-ChildItem -LiteralPath $costRoot -Recurse -Force).Count
Fact 'cost-fixture' "$entries entries ($made files) built in $($sw.ElapsedMilliseconds)ms"
$costProfile = "nubbt_cost_$nonce"
$costSid = [Bt]::CreateProfile($costProfile)
Fact 'cost-profile' $costSid
if ($costSid -notlike 'ERR*') {
  $sw = [Diagnostics.Stopwatch]::StartNew(); Grant-Ace $costRoot $costSid 'Modify'; $sw.Stop()
  $grantMs = $sw.ElapsedMilliseconds
  $sw = [Diagnostics.Stopwatch]::StartNew(); Revoke-Ace $costRoot $costSid; $sw.Stop()
  Fact 'cost-grant-ms' "$grantMs (inheritable Modify ace on a $entries-entry tree)"
  Fact 'cost-revoke-ms' $sw.ElapsedMilliseconds
  # A single leaf dir, which is what the shipping backend actually writes per launch.
  $leaf = Join-Path $costRoot 'd0\node_modules\pkg0'
  $sw = [Diagnostics.Stopwatch]::StartNew(); Grant-Ace $leaf $costSid 'Modify'; $sw.Stop()
  $leafMs = $sw.ElapsedMilliseconds
  $sw = [Diagnostics.Stopwatch]::StartNew(); Revoke-Ace $leaf $costSid; $sw.Stop()
  Fact 'cost-leaf-grant-ms' "$leafMs (97-entry leaf dir)"
  Fact 'cost-leaf-revoke-ms' $sw.ElapsedMilliseconds
  Fact 'cost-profile-deleted' ([Bt]::DeleteProfile($costProfile))
}
Remove-Item -LiteralPath $costRoot -Recurse -Force -ErrorAction SilentlyContinue

# ─────────────────────────────── verdict ───────────────────────────────

$null = Invoke-Verdict -Cells $cells

W ''
Fact 'FAILURES' $script:fails
if ($script:fails -eq 0) { W '  RESULT all properties PASS' } else { W "  RESULT $($script:fails) properties FAILED" }

# Teardown. The fixture lives in the runner's own profile; leaving it behind would only matter on
# a reused machine, but a probe that cleans up is a probe that can be re-run in place.
Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue
Remove-Item -LiteralPath (Split-Path $sibProfile) -Recurse -Force -ErrorAction SilentlyContinue
if ($createdSshKey) { Remove-Item -LiteralPath $sshKey -Force -ErrorAction SilentlyContinue }
if ($createdNpmrc) { Remove-Item -LiteralPath $npmrc -Force -ErrorAction SilentlyContinue }
W ''
W 'PROBE END'
