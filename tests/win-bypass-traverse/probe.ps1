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
    // Sampled on TIMEOUT only, and it is what separates the two failure shapes a hang can have:
    // cpu ~= wall means a busy retry loop (libuv's `uv__pipe_server` incrementing a name and
    // calling `CreateNamedPipeA` again), cpu ~= 0 means a blocking wait on something.
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool GetProcessTimes(IntPtr h, out long creation, out long exit,
        out long kernel, out long user);

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
                long c, x, k, u;
                if (GetProcessTimes(pi.hProcess, out c, out x, out k, out u))
                    extra = " cpu_ms=" + ((k + u) / 10000L);
                TerminateProcess(pi.hProcess, 0xDEAD);
                extra = " TIMED-OUT" + extra;
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

# ──────────────────── the DEVICE-OBJECT security surface (separate type) ────────────────────
#
# A SECOND `Add-Type` on purpose. This block answers a question the launcher does not — whether an
# unprivileged process can rewrite `\Device\Null`'s and the NPFS root's DACL — and a compile error
# in it must degrade to "the device section is unavailable" rather than take the validated
# filesystem table down with it. Hence its own class, its own try/catch, and `$script:HaveSec`.
#
# WHY A HANDLE AND NOT A NAME. `GetNamedSecurityInfoW` on `\\.\pipe\` returns
# ERROR_INVALID_PARAMETER (87) — measured, run 30473523088 — so the name-based API cannot read a
# device object's descriptor at all. The handle route (`CreateFileW` -> `GetSecurityInfo`
# SE_KERNEL_OBJECT) is what Codex's `allow_null_device` uses, and it is the only one that works.
$secSrc = @'
using System;
using System.Runtime.InteropServices;

public static class BtSec
{
    [StructLayout(LayoutKind.Sequential)]
    struct TRUSTEE_W
    {
        public IntPtr pMultipleTrustee;
        public int MultipleTrusteeOperation;
        public int TrusteeForm;
        public int TrusteeType;
        public IntPtr ptstrName;
    }

    [StructLayout(LayoutKind.Sequential)]
    struct EXPLICIT_ACCESS_W
    {
        public uint grfAccessPermissions;
        public uint grfAccessMode;
        public uint grfInheritance;
        public TRUSTEE_W Trustee;
    }

    [StructLayout(LayoutKind.Sequential)]
    struct SID_AND_ATTRIBUTES { public IntPtr Sid; public uint Attributes; }

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true, EntryPoint = "CreateFileW")]
    static extern IntPtr OpenObj(string name, uint access, uint share, IntPtr sa, uint disp,
        uint flags, IntPtr templ);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool CloseHandle(IntPtr h);
    [DllImport("kernel32.dll")] static extern IntPtr LocalFree(IntPtr p);
    [DllImport("kernel32.dll")] static extern IntPtr GetCurrentProcess();

    [DllImport("advapi32.dll", SetLastError = true)]
    static extern uint GetSecurityInfo(IntPtr h, int objType, uint secInfo, IntPtr owner,
        IntPtr group, out IntPtr dacl, IntPtr sacl, out IntPtr sd);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern uint SetSecurityInfo(IntPtr h, int objType, uint secInfo, IntPtr owner,
        IntPtr group, IntPtr dacl, IntPtr sacl);
    [DllImport("advapi32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern uint SetEntriesInAclW(uint count, ref EXPLICIT_ACCESS_W list, IntPtr oldAcl,
        out IntPtr newAcl);
    [DllImport("advapi32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool ConvertSecurityDescriptorToStringSecurityDescriptorW(IntPtr sd, uint rev,
        uint secInfo, out IntPtr str, out uint len);
    [DllImport("advapi32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool ConvertStringSidToSidW(string s, out IntPtr sid);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool OpenProcessToken(IntPtr proc, uint access, out IntPtr tok);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool CreateRestrictedToken(IntPtr existing, uint flags, uint disableCount,
        IntPtr sidsToDisable, uint delPrivCount, IntPtr privsToDelete, uint restrictCount,
        IntPtr sidsToRestrict, out IntPtr newTok);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool DuplicateTokenEx(IntPtr tok, uint access, IntPtr sa, int impLevel,
        int tokType, out IntPtr newTok);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool SetTokenInformation(IntPtr tok, int cls, IntPtr info, uint len);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool ImpersonateLoggedOnUser(IntPtr tok);
    [DllImport("advapi32.dll", SetLastError = true)] static extern bool RevertToSelf();

    // THE FALLBACK ROUTE, added after run 30512950258. `GetSecurityInfo` and `SetSecurityInfo` both
    // return 87 ERROR_INVALID_PARAMETER on the NPFS root — on BOTH images — while the very same
    // calls succeed on `\Device\Null`, so the refusal is that object's and not the API's. Win32's
    // wrappers are thin shims over these, so going direct distinguishes "the wrapper rejected the
    // shape" from "the driver refuses to serve a security query at all". Without this the npfsfix
    // arm measures nothing: its grant never ran, which is a DIFFERENT result from the grant failing.
    [DllImport("ntdll.dll")]
    static extern int NtQuerySecurityObject(IntPtr h, uint secInfo, IntPtr sd, uint len,
        out uint needed);
    [DllImport("ntdll.dll")]
    static extern int NtSetSecurityObject(IntPtr h, uint secInfo, IntPtr sd);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool GetSecurityDescriptorDacl(IntPtr sd, out bool present, out IntPtr dacl,
        out bool defaulted);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool InitializeSecurityDescriptor(IntPtr sd, uint rev);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool SetSecurityDescriptorDacl(IntPtr sd, bool present, IntPtr dacl,
        bool defaulted);

    const int SE_KERNEL_OBJECT = 6;
    const uint OWNER_SI = 1, GROUP_SI = 2, DACL_SI = 4;
    const uint OPEN_EXISTING = 3;
    const uint FILE_SHARE_RW = 3;
    // FILE_FLAG_BACKUP_SEMANTICS is what lets `CreateFileW` open a DIRECTORY object, which is what
    // the NPFS root (`\\.\pipe\`) is. Without it the open fails and reads as "no such device".
    const uint FILE_FLAG_BACKUP_SEMANTICS = 0x02000000;
    const uint TRUSTEE_IS_SID = 0, TRUSTEE_IS_UNKNOWN = 0;
    const uint SET_ACCESS = 2, REVOKE_ACCESS = 4;
    const int TokenIntegrityLevel = 25;
    const uint SE_GROUP_INTEGRITY = 0x20;

    public const uint READ_CONTROL = 0x00020000, WRITE_DAC = 0x00040000;
    // Codex's `allow_null_device` mask, reproduced exactly so this measures THEIR remedy rather
    // than a looser one of ours: FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE.
    public const uint NUL_MASK = 0x00120089 | 0x00120116 | 0x001200A0;
    // The NPFS root is a directory and the operation to re-open is CREATING a pipe in it, so the
    // loose mask is the right FIRST cell: if FILE_ALL_ACCESS does not work, nothing narrower will.
    public const uint FILE_ALL_ACCESS = 0x001F01FF;

    static IntPtr Open(string path, uint access)
    {
        uint flags = path.EndsWith("\\") ? FILE_FLAG_BACKUP_SEMANTICS : 0;
        return OpenObj(path, access, FILE_SHARE_RW, IntPtr.Zero, OPEN_EXISTING, flags, IntPtr.Zero);
    }

    /// The object's owner, group and DACL as SDDL. SACL is deliberately NOT requested: reading it
    /// needs ACCESS_SYSTEM_SECURITY, i.e. SeSecurityPrivilege, which a standard user lacks — asking
    /// for it would make this read fail for a reason unrelated to what it measures.
    public static string Sddl(string path)
    {
        IntPtr h = Open(path, READ_CONTROL);
        if (h == new IntPtr(-1)) return "ERR open err=" + Marshal.GetLastWin32Error();
        IntPtr dacl, sd;
        try
        {
            uint rc = GetSecurityInfo(h, SE_KERNEL_OBJECT, OWNER_SI | GROUP_SI | DACL_SI,
                IntPtr.Zero, IntPtr.Zero, out dacl, IntPtr.Zero, out sd);
            if (rc != 0) return "ERR getsecurityinfo rc=" + rc;
            IntPtr str; uint len;
            if (!ConvertSecurityDescriptorToStringSecurityDescriptorW(sd, 1,
                    OWNER_SI | GROUP_SI | DACL_SI, out str, out len))
                return "ERR tosddl err=" + Marshal.GetLastWin32Error();
            string s = Marshal.PtrToStringUni(str);
            LocalFree(str);
            LocalFree(sd);
            return "win32: " + s;
        }
        finally { CloseHandle(h); }
    }

    /// The same read through `NtQuerySecurityObject`. Two-call form: length 0 to learn the size,
    /// then again into a buffer.
    public static string SddlNt(string path)
    {
        IntPtr h = Open(path, READ_CONTROL);
        if (h == new IntPtr(-1)) return "ERR open err=" + Marshal.GetLastWin32Error();
        IntPtr buf = IntPtr.Zero;
        try
        {
            uint needed;
            int st = NtQuerySecurityObject(h, OWNER_SI | GROUP_SI | DACL_SI, IntPtr.Zero, 0, out needed);
            // STATUS_BUFFER_TOO_SMALL / STATUS_BUFFER_OVERFLOW are the expected sizing answers.
            if (st != unchecked((int)0xC0000023) && st != unchecked((int)0x80000005) && st != 0)
                return "ERR ntquery-size status=0x" + st.ToString("x8");
            if (needed == 0) return "ERR ntquery-size needed=0";
            buf = Marshal.AllocHGlobal((int)needed);
            st = NtQuerySecurityObject(h, OWNER_SI | GROUP_SI | DACL_SI, buf, needed, out needed);
            if (st != 0) return "ERR ntquery status=0x" + st.ToString("x8");
            IntPtr str; uint len;
            if (!ConvertSecurityDescriptorToStringSecurityDescriptorW(buf, 1,
                    OWNER_SI | GROUP_SI | DACL_SI, out str, out len))
                return "ERR tosddl err=" + Marshal.GetLastWin32Error();
            string s = Marshal.PtrToStringUni(str);
            LocalFree(str);
            return "nt: " + s;
        }
        finally
        {
            if (buf != IntPtr.Zero) Marshal.FreeHGlobal(buf);
            CloseHandle(h);
        }
    }

    /// Read whichever route the object serves. Reported with its route prefix, because "the win32
    /// wrapper refused but the native call worked" is a materially different fact from either alone.
    public static string SddlAny(string path)
    {
        string a = Sddl(path);
        if (!a.StartsWith("ERR")) return a;
        string b = SddlNt(path);
        if (!b.StartsWith("ERR")) return b;
        return a + " | " + b;
    }

    /// Just the OPEN, so "can this context obtain WRITE_DAC" is measured independently of whether
    /// the subsequent rewrite succeeds. A refusal here is the whole answer to the privilege
    /// question; a refusal later would be something else entirely.
    public static string CanOpen(string path, uint access)
    {
        IntPtr h = Open(path, access);
        if (h == new IntPtr(-1)) return "ERR err=" + Marshal.GetLastWin32Error();
        CloseHandle(h);
        return "OK";
    }

    static string Edit(string path, string sidStr, uint mask, uint mode)
    {
        IntPtr sid;
        if (!ConvertStringSidToSidW(sidStr, out sid))
            return "ERR sid err=" + Marshal.GetLastWin32Error();
        IntPtr h = Open(path, READ_CONTROL | WRITE_DAC);
        if (h == new IntPtr(-1))
        {
            LocalFree(sid);
            return "ERR open err=" + Marshal.GetLastWin32Error();
        }
        IntPtr newDacl = IntPtr.Zero;
        try
        {
            IntPtr dacl, sd;
            uint rc = GetSecurityInfo(h, SE_KERNEL_OBJECT, DACL_SI, IntPtr.Zero, IntPtr.Zero,
                out dacl, IntPtr.Zero, out sd);
            if (rc != 0) return "ERR getsecurityinfo rc=" + rc;
            EXPLICIT_ACCESS_W ea = new EXPLICIT_ACCESS_W();
            ea.grfAccessPermissions = mask;
            ea.grfAccessMode = mode;
            ea.grfInheritance = 0;
            ea.Trustee.TrusteeForm = (int)TRUSTEE_IS_SID;
            ea.Trustee.TrusteeType = (int)TRUSTEE_IS_UNKNOWN;
            ea.Trustee.ptstrName = sid;
            uint rc2 = SetEntriesInAclW(1, ref ea, dacl, out newDacl);
            if (rc2 != 0) return "ERR setentriesinacl rc=" + rc2;
            uint rc3 = SetSecurityInfo(h, SE_KERNEL_OBJECT, DACL_SI, IntPtr.Zero, IntPtr.Zero,
                newDacl, IntPtr.Zero);
            LocalFree(sd);
            if (rc3 != 0) return "ERR setsecurityinfo rc=" + rc3;
            return "OK";
        }
        finally
        {
            if (newDacl != IntPtr.Zero) LocalFree(newDacl);
            CloseHandle(h);
            LocalFree(sid);
        }
    }

    /// The same edit through `NtQuerySecurityObject` / `NtSetSecurityObject`, assembling the new
    /// descriptor by hand: query self-relative, pull its DACL out, merge the ace, wrap the result in
    /// a fresh ABSOLUTE descriptor (which is what NtSetSecurityObject wants), set.
    static string EditNt(string path, string sidStr, uint mask, uint mode)
    {
        IntPtr sid;
        if (!ConvertStringSidToSidW(sidStr, out sid))
            return "ERR sid err=" + Marshal.GetLastWin32Error();
        IntPtr h = Open(path, READ_CONTROL | WRITE_DAC);
        if (h == new IntPtr(-1))
        {
            LocalFree(sid);
            return "ERR open err=" + Marshal.GetLastWin32Error();
        }
        IntPtr buf = IntPtr.Zero, newDacl = IntPtr.Zero, newSd = IntPtr.Zero;
        try
        {
            uint needed;
            int st = NtQuerySecurityObject(h, DACL_SI, IntPtr.Zero, 0, out needed);
            if (st != unchecked((int)0xC0000023) && st != unchecked((int)0x80000005) && st != 0)
                return "ERR ntquery-size status=0x" + st.ToString("x8");
            if (needed == 0) return "ERR ntquery-size needed=0";
            buf = Marshal.AllocHGlobal((int)needed);
            st = NtQuerySecurityObject(h, DACL_SI, buf, needed, out needed);
            if (st != 0) return "ERR ntquery status=0x" + st.ToString("x8");
            bool present, defaulted;
            IntPtr oldDacl;
            if (!GetSecurityDescriptorDacl(buf, out present, out oldDacl, out defaulted))
                return "ERR getdacl err=" + Marshal.GetLastWin32Error();
            EXPLICIT_ACCESS_W ea = new EXPLICIT_ACCESS_W();
            ea.grfAccessPermissions = mask;
            ea.grfAccessMode = mode;
            ea.grfInheritance = 0;
            ea.Trustee.TrusteeForm = (int)TRUSTEE_IS_SID;
            ea.Trustee.TrusteeType = (int)TRUSTEE_IS_UNKNOWN;
            ea.Trustee.ptstrName = sid;
            uint rc = SetEntriesInAclW(1, ref ea, present ? oldDacl : IntPtr.Zero, out newDacl);
            if (rc != 0) return "ERR setentriesinacl rc=" + rc;
            // SECURITY_DESCRIPTOR is 20 bytes on x64 / 40 with alignment slack; over-allocating is
            // harmless and avoids depending on the struct layout.
            newSd = Marshal.AllocHGlobal(64);
            if (!InitializeSecurityDescriptor(newSd, 1))
                return "ERR initsd err=" + Marshal.GetLastWin32Error();
            if (!SetSecurityDescriptorDacl(newSd, true, newDacl, false))
                return "ERR setsddacl err=" + Marshal.GetLastWin32Error();
            st = NtSetSecurityObject(h, DACL_SI, newSd);
            if (st != 0) return "ERR ntset status=0x" + st.ToString("x8");
            return "OK";
        }
        finally
        {
            if (newSd != IntPtr.Zero) Marshal.FreeHGlobal(newSd);
            if (newDacl != IntPtr.Zero) LocalFree(newDacl);
            if (buf != IntPtr.Zero) Marshal.FreeHGlobal(buf);
            CloseHandle(h);
            LocalFree(sid);
        }
    }

    static string EditAny(string path, string sidStr, uint mask, uint mode)
    {
        string a = Edit(path, sidStr, mask, mode);
        if (a == "OK") return "OK win32";
        string b = EditNt(path, sidStr, mask, mode);
        if (b == "OK") return "OK nt";
        return a + " | nt: " + b;
    }

    public static string Grant(string path, string sidStr, uint mask) { return EditAny(path, sidStr, mask, SET_ACCESS); }
    public static string Revoke(string path, string sidStr) { return EditAny(path, sidStr, 0, REVOKE_ACCESS); }

    /// Run `CanOpen` under a DE-ELEVATED impersonation of our own token: Administrators disabled
    /// (deny-only), every removable privilege dropped, integrity lowered to Medium — the access
    /// check a standard user gets. The GitHub runner is `runneradmin` and elevated, so an
    /// elevated-only success would be worthless as evidence for nub's shipping case; this is the
    /// arm that makes the privilege answer real rather than assumed.
    ///
    /// The failure modes are reported DISTINCTLY (`setup-` prefix) because "we could not build the
    /// de-elevated context" and "the de-elevated context was refused" are opposite conclusions.
    public static string CanOpenDeElevated(string path, uint access)
    {
        IntPtr own = IntPtr.Zero, restricted = IntPtr.Zero, imp = IntPtr.Zero;
        IntPtr adminSid = IntPtr.Zero, disable = IntPtr.Zero, ilSid = IntPtr.Zero, label = IntPtr.Zero;
        try
        {
            if (!OpenProcessToken(GetCurrentProcess(), 0x0002 | 0x0008, out own))
                return "ERR setup-openprocesstoken err=" + Marshal.GetLastWin32Error();
            if (!ConvertStringSidToSidW("BA", out adminSid))
                return "ERR setup-adminsid err=" + Marshal.GetLastWin32Error();
            SID_AND_ATTRIBUTES sa = new SID_AND_ATTRIBUTES();
            sa.Sid = adminSid;
            sa.Attributes = 0;
            disable = Marshal.AllocHGlobal(Marshal.SizeOf(typeof(SID_AND_ATTRIBUTES)));
            Marshal.StructureToPtr(sa, disable, false);
            // DISABLE_MAX_PRIVILEGE(0x1) additionally deletes every privilege but SeChangeNotify.
            if (!CreateRestrictedToken(own, 0x1, 1, disable, 0, IntPtr.Zero, 0, IntPtr.Zero,
                    out restricted))
                return "ERR setup-createrestrictedtoken err=" + Marshal.GetLastWin32Error();
            if (!DuplicateTokenEx(restricted, 0x000F01FF, IntPtr.Zero, 2, 2, out imp))
                return "ERR setup-duplicatetokenex err=" + Marshal.GetLastWin32Error();
            if (!ConvertStringSidToSidW("S-1-16-8192", out ilSid))
                return "ERR setup-ilsid err=" + Marshal.GetLastWin32Error();
            SID_AND_ATTRIBUTES il = new SID_AND_ATTRIBUTES();
            il.Sid = ilSid;
            il.Attributes = SE_GROUP_INTEGRITY;
            label = Marshal.AllocHGlobal(Marshal.SizeOf(typeof(SID_AND_ATTRIBUTES)));
            Marshal.StructureToPtr(il, label, false);
            if (!SetTokenInformation(imp, TokenIntegrityLevel, label,
                    (uint)Marshal.SizeOf(typeof(SID_AND_ATTRIBUTES))))
                return "ERR setup-setintegrity err=" + Marshal.GetLastWin32Error();
            // A token derived from our OWN is impersonable without SeImpersonatePrivilege, which a
            // standard user does not hold (§5e measured `CreateProcessWithTokenW` failing 1314 for
            // exactly that reason). If this call is what fails, the arm reports `setup-` and the
            // privilege question stays open rather than being answered wrongly.
            if (!ImpersonateLoggedOnUser(imp))
                return "ERR setup-impersonate err=" + Marshal.GetLastWin32Error();
            try { return CanOpen(path, access); }
            finally { RevertToSelf(); }
        }
        finally
        {
            if (label != IntPtr.Zero) Marshal.FreeHGlobal(label);
            if (disable != IntPtr.Zero) Marshal.FreeHGlobal(disable);
            if (ilSid != IntPtr.Zero) LocalFree(ilSid);
            if (adminSid != IntPtr.Zero) LocalFree(adminSid);
            if (imp != IntPtr.Zero) CloseHandle(imp);
            if (restricted != IntPtr.Zero) CloseHandle(restricted);
            if (own != IntPtr.Zero) CloseHandle(own);
        }
    }
}
'@

$script:HaveSec = $false
try {
  Add-Type -TypeDefinition $secSrc -Language CSharp -ErrorAction Stop
  $script:HaveSec = $true
} catch {
  W "  fact:device-security-type-compile = ERR $_"
}

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

# ──────────────── DEVICE OBJECTS: `\Device\Null` and the NPFS root ────────────────
#
# THE CANDIDATE. Microsoft's mxc documents `\Device\Null` as a hard blocker for its own
# AppContainer backends (`docs/host-prep.md`, `prepare-null-device`): "the Windows kernel resets the
# SD to a default value at every boot; for the AppContainer-based backends the default does not
# include the well-known AppContainer SIDs, and processes that open `NUL` for stdin/stdout/stderr
# redirection fail with ERROR_ACCESS_DENIED partway through startup." Their remedy runs ELEVATED,
# once per boot — disqualifying for nub. Codex performs the same repair UNPRIVILEGED and
# best-effort (`windows-sandbox-rs/src/acl.rs` `allow_null_device`), so the question is which of
# those two is right about the privilege it takes.
#
# THE EXTENSION, and the bigger prize. The refused named-pipe namespace is the same SHAPE of
# problem on a different device: every `\\.\pipe\…` `CreateNamedPipeW` is ACCESS_DENIED to a LowBox
# token while `\\.\pipe\LOCAL\…` is created (measured, run 30473523088), and libuv's stdio path
# spells the global form. If the NPFS root's DACL is unprivileged-writable the piped-spawn hang is
# fixable with no libuv change at all, so both devices are measured with the same three cells.
#
# THE ONE-VARIABLE PAIR that makes the privilege answer real: READ_CONTROL and WRITE_DAC requested
# from the SAME de-elevated impersonation context. Without the READ_CONTROL control a WRITE_DAC
# refusal cannot be told from an impersonation that never took effect.
W ''
W '== device object security =='
Prop 'device-security-section-available' $script:HaveSec `
  'the device-object P/Invoke type must compile, or every cell below is absent rather than negative'

$devNull = '\\.\NUL'
$npfsRoot = '\\.\pipe\'
$nullSddl = ''
$npfsSddl = ''
if ($script:HaveSec) {
  $nullSddl = [BtSec]::SddlAny($devNull)
  $npfsSddl = [BtSec]::SddlAny($npfsRoot)
  Fact 'sddl[\Device\Null]' $nullSddl
  Fact 'sddl[NPFS root]' $npfsSddl
  Prop 'device-sd-read-works' (($nullSddl -notlike 'ERR*') -and ($npfsSddl -notlike 'ERR*')) `
    "both descriptors must be readable or nothing below is interpretable: null=$nullSddl npfs=$npfsSddl"

  # THE PRECONDITION mxc states. `AC` is the SDDL abbreviation for S-1-15-2-1 (ALL APPLICATION
  # PACKAGES) and `S-1-15-2-2` is ALL RESTRICTED APPLICATION PACKAGES; either, or any `S-1-15-*`
  # trustee, would mean the default already admits an AppContainer and the candidate's premise is
  # false on this image. Matched on both spellings because SDDL emits the abbreviation when one
  # exists and the raw sid otherwise.
  # `RC` (RESTRICTED, S-1-5-12) is deliberately NOT counted: mxc's target SDDL grants it, but it is
  # a restricted-token trustee, not an AppContainer one, and counting it would make the precondition
  # read as already-satisfied for a reason that has nothing to do with a LowBox token.
  function Has-AcTrustee([string]$sddl) {
    return ($sddl -match ';AC\)') -or ($sddl -match ';S-1-15-')
  }
  Fact 'null-device-names-an-appcontainer-trustee' (Has-AcTrustee $nullSddl)
  Fact 'npfs-root-names-an-appcontainer-trustee' (Has-AcTrustee $npfsSddl)
  Prop 'null-device-default-sd-excludes-appcontainer-sids' (-not (Has-AcTrustee $nullSddl)) `
    "mxc's stated precondition: the boot default must name no AppContainer trustee. FAIL here means the premise does not hold on this image, which is itself the finding: $nullSddl"

  # The NPFS root has no single canonical Win32 spelling and `GetNamedSecurityInfoW` on it returns
  # ERROR_INVALID_PARAMETER (run 30473523088), so the alternatives are reported rather than assumed:
  # a failed open on ONE name must not be read as "the device has no descriptor".
  foreach ($alt in @('\\.\pipe\', '\\?\pipe\', '\\.\PIPE\')) {
    Fact "npfs-open-spelling[$alt]" ([BtSec]::CanOpen($alt, [BtSec]::READ_CONTROL))
  }

  foreach ($pair in @(@('null', $devNull), @('npfs', $npfsRoot))) {
    $tag = $pair[0]; $obj = $pair[1]
    Fact "$tag/open-read-control-elevated-context" ([BtSec]::CanOpen($obj, [BtSec]::READ_CONTROL))
    Fact "$tag/open-write-dac-elevated-context" ([BtSec]::CanOpen($obj, [BtSec]::READ_CONTROL -bor [BtSec]::WRITE_DAC))
    Fact "$tag/open-read-control-deelevated" ([BtSec]::CanOpenDeElevated($obj, [BtSec]::READ_CONTROL))
    Fact "$tag/open-write-dac-deelevated" ([BtSec]::CanOpenDeElevated($obj, [BtSec]::READ_CONTROL -bor [BtSec]::WRITE_DAC))
  }
  $nullRcDe = [BtSec]::CanOpenDeElevated($devNull, [BtSec]::READ_CONTROL)
  $nullWdDe = [BtSec]::CanOpenDeElevated($devNull, [BtSec]::READ_CONTROL -bor [BtSec]::WRITE_DAC)
  $npfsRcDe = [BtSec]::CanOpenDeElevated($npfsRoot, [BtSec]::READ_CONTROL)
  $npfsWdDe = [BtSec]::CanOpenDeElevated($npfsRoot, [BtSec]::READ_CONTROL -bor [BtSec]::WRITE_DAC)
  Prop 'deelevated-context-is-live' (($nullRcDe -eq 'OK') -and ($npfsRcDe -eq 'OK')) `
    "the de-elevated impersonation must still obtain READ_CONTROL on both devices, or a WRITE_DAC refusal below is about the harness rather than about privilege: null=$nullRcDe npfs=$npfsRcDe"
  Prop 'unprivileged-write-dac-on-null-device' ($nullWdDe -eq 'OK') `
    "THE question mxc and Codex disagree about — can a standard user rewrite \Device\Null's DACL: $nullWdDe"
  # CORRECTED after run 30513433808, because the bare open is MISLEADING. The de-elevated
  # `WRITE_DAC` open on `\\.\pipe\` SUCCEEDS on both images — and the object serves no security
  # descriptor at all: `GetSecurityInfo` returns 87 and `NtQuerySecurityObject` returns
  # STATUS_INVALID_PARAMETER (0xC000000D). An object whose descriptor cannot be queried is one whose
  # DACL was never consulted on open either (the `\Device\Afd` lesson, §5d/§5f), so reporting that
  # open alone as "a standard user can rewrite the NPFS root" would be flatly wrong. The property
  # therefore requires the descriptor to be REACHABLE as well as the access to be granted.
  Prop 'unprivileged-write-dac-on-npfs-root' (($npfsWdDe -eq 'OK') -and ($npfsSddl -notlike 'ERR*')) `
    "a yes would fix the piped-spawn hang with no libuv change, and it needs BOTH halves: de-elevated WRITE_DAC open=$npfsWdDe, descriptor reachable=$npfsSddl. An open that succeeds against an object with no queryable descriptor is not a writable DACL, it is a DACL that is never consulted."

  # A THROWAWAY AppContainer sid, so the grant/revoke round trip is exercised against the same kind
  # of trustee the arms use — and revoked immediately, since a device DACL is machine-global.
  $devProbeName = "nubbt_dev_$([Guid]::NewGuid().ToString('N').Substring(0,8))"
  $devProbeSid = [Bt]::CreateProfile($devProbeName)
  Fact 'device-probe-sid' $devProbeSid
  if ($devProbeSid -notlike 'ERR*') {
    Fact 'null/grant-roundtrip' ([BtSec]::Grant($devNull, $devProbeSid, [BtSec]::NUL_MASK))
    $afterGrant = [BtSec]::SddlAny($devNull)
    Fact 'null/sddl-names-probe-sid-after-grant' ($afterGrant -match [Regex]::Escape($devProbeSid))
    Fact 'null/revoke-roundtrip' ([BtSec]::Revoke($devNull, $devProbeSid))
    $afterRevoke = [BtSec]::SddlAny($devNull)
    Fact 'null/sddl-names-probe-sid-after-revoke' ($afterRevoke -match [Regex]::Escape($devProbeSid))
    Prop 'null-device-grant-is-reversible' (($afterGrant -match [Regex]::Escape($devProbeSid)) -and
      (-not ($afterRevoke -match [Regex]::Escape($devProbeSid)))) `
      "a machine-global device DACL edit must be provably revertible before any arm relies on it: present-after-grant=$($afterGrant -match [Regex]::Escape($devProbeSid)) present-after-revoke=$($afterRevoke -match [Regex]::Escape($devProbeSid))"
    Fact 'device-probe-profile-deleted' ([Bt]::DeleteProfile($devProbeName))
  }
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
    [switch]$DeriveOnly,
    # 120s is right for the fs table, whose only hanging op is now opt-in. The object arms carry an
    # op that spins FOREVER, and a 120s bound there would cost the run twelve minutes to learn
    # something a 45s bound establishes just as well.
    [int]$TimeoutMs = 120000,
    # Grant this arm's per-run AC sid on `\Device\Null` / the NPFS root for the duration of the
    # launch. Per-run sids are what make these arms inherently one-variable: each arm's grant names
    # a trustee no other arm has, so a revoke that failed cannot silently treat a later arm.
    [switch]$RepairNull,
    [switch]$RepairNpfs,
    # `child.js`'s piped spawn is opt-in because it does not fail, it spins — leaving it on cost
    # every AppContainer arm the full launch timeout while reporting MISSING-OP either way. The
    # object arms measure the same thing with a repair differential and a bound.
    [switch]$Piped,
    # `fork` opens its IPC pipe inside the `fork` call, so it needs an arm of its own: a cell placed
    # after an already-hanging cell never runs.
    [string]$ObjMode = ''
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
  $devGranted = @()
  try {
    foreach ($p in $GrantRX) { Grant-Ace $p $sid 'ReadAndExecute'; $granted += $p }
    foreach ($p in $GrantModify) { Grant-Ace $p $sid 'Modify'; $granted += $p }
    Fact "$Name/grants" $(if ($granted.Count) { ($granted -join ' ; ') } else { '(none)' })

    # THE DEVICE REPAIRS, and their read-back. Same discipline as the deep file's DACL below: a
    # grant that never landed would make the treatment arm fail for a reason with nothing to do
    # with the device, which reads exactly like "the repair does not help".
    $nullAce = 'none'
    $npfsAce = 'none'
    if ($script:HaveSec -and $AppContainer) {
      if ($RepairNull) {
        Fact "$Name/repair-null-device" ([BtSec]::Grant('\\.\NUL', $sid, [BtSec]::NUL_MASK))
        $devGranted += '\\.\NUL'
      }
      if ($RepairNpfs) {
        Fact "$Name/repair-npfs-root" ([BtSec]::Grant('\\.\pipe\', $sid, [BtSec]::FILE_ALL_ACCESS))
        $devGranted += '\\.\pipe\'
      }
      if ([BtSec]::SddlAny('\\.\NUL') -match [Regex]::Escape($sid)) { $nullAce = 'present' }
      if ([BtSec]::SddlAny('\\.\pipe\') -match [Regex]::Escape($sid)) { $npfsAce = 'present' }
    }
    Fact "$Name/null-device-dacl-for-ac-sid" $nullAce
    Fact "$Name/npfs-root-dacl-for-ac-sid" $npfsAce

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
    $env:BT_PIPED = $(if ($Piped) { '1' } else { '' })
    $env:BT_OBJ_MODE = $ObjMode
    $log = Join-Path $logDir "$Name.log"
    $entry = if ($EntryFile) { $EntryFile } else { $jailChild }
    $flagPart = if ($NodeFlags.Count) { ' ' + ($NodeFlags -join ' ') } else { '' }
    $cmdline = '"' + $jailNode + '"' + $flagPart + ' "' + $entry + '"'
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $status = [Bt]::Launch($sid, $jailNode, $cmdline, $Cwd, $log, $TimeoutMs)
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
    $arm['null-dacl-grants-ac-sid'] = @($(if ($nullAce -eq 'present') { 'OK' } else { 'ERR' }), "dacl:$nullAce")
    $arm['npfs-dacl-grants-ac-sid'] = @($(if ($npfsAce -eq 'present') { 'OK' } else { 'ERR' }), "dacl:$npfsAce")
    # Did the child die in Node's own realpath walk on the volume root, before user code? This is
    # a property of the LOG, not of an op line — an unflagged confined node emits no op lines at
    # all, so without this cell that arm is indistinguishable from a launch that never happened.
    $raw = ($lines -join "`n")
    $diedRealpath = ($raw -match "EPERM") -and ($raw -match "lstat") -and ($raw -match "realpathSync")
    $arm['node-died-realpath-c-root'] = @($(if ($diedRealpath) { 'OK' } else { 'ERR' }), 'log:derived')
    # `*dacl*` and `node-died-*` are harness-side cells, not child observations, so they must not
    # inflate the op count the flag differential asserts on (`ac-noflags` must report EXACTLY 0).
    $arm['__opcount'] = @(@($arm.Keys | Where-Object { $_ -notlike '__*' -and $_ -notlike '*dacl*' -and $_ -ne 'node-died-realpath-c-root' }).Count, '')
    $arm['__launch'] = @($status, '')
    $arm['__lines'] = @($lines.Count, '')
    $cells[$Name] = $arm
  }
  finally {
    foreach ($p in $granted) { try { Revoke-Ace $p $sid } catch { W "    revoke failed on $p : $_" } }
    # A device DACL is MACHINE-GLOBAL, so its revoke is reported rather than done silently: a
    # residual ace on `\Device\Null` would be state this probe left on the host.
    foreach ($d in $devGranted) {
      Fact "$Name/device-revoke[$d]" ([BtSec]::Revoke($d, $sid))
    }
    if ($profileName) { Fact "$Name/profile-deleted" ([Bt]::DeleteProfile($profileName)) }
  }
}

# 1. The control that makes every other row readable: identical child, identical paths, no
#    SECURITY_CAPABILITIES, no ACEs (the user already owns its own profile). `-Piped` only here:
#    the unconfined arm is where the piped spawn RETURNS, so it is the one arm where measuring it
#    costs nothing and yields the allow half of the differential.
Invoke-Arm -Name 'plain' -AppContainer $false -Cwd $runtimeDir -Piped

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

# ────────────────── THE OBJECT ARMS: NUL, the pipe namespaces, the stdio shapes ──────────────────
#
# Five arms, each one variable from its neighbour, all running `child-objects.js` with the SAME
# grants as arm 3 so the filesystem side is held constant and only the device treatment moves:
#
#   obj-plain             UNCONFINED. Every cell must pass. Without it a table of failures in every
#                         confined arm is indistinguishable from a broken child — the exact false
#                         negative that has burned two lanes on this effort.
#   obj-ac-baseline       confined, devices AS SHIPPED. The as-is state of the blocker.
#   obj-ac-nulfix         confined, `\Device\Null` granted to this arm's sid. ONE variable.
#   obj-ac-npfsfix        confined, NPFS root granted to this arm's sid. ONE variable.
#   obj-ac-baseline-again confined, devices as shipped again, run LAST. §5e's `core-js` false
#                         positive was a persistent marker outside the fixture faking a
#                         regression; a device DACL is machine-global, so a repair whose revoke
#                         silently failed would make this arm look repaired. Re-running the
#                         baseline last is what catches that.
$objSink = Join-Path $dataDir 'obj-sink.txt'
$env:BT_OBJ_SINK = $objSink
$objChild = Join-Path $runtimeDir 'child-objects.js'
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'child-objects.js') -Destination $objChild -Force

Invoke-Arm -Name 'obj-plain' -AppContainer $false -Cwd $runtimeDir -EntryFile $objChild -TimeoutMs 45000
Invoke-Arm -Name 'obj-ac-baseline' -AppContainer $true -GrantRX @($runtimeDir) `
  -GrantModify @($dataDir) -Cwd $runtimeDir -EntryFile $objChild -TimeoutMs 45000
Invoke-Arm -Name 'obj-ac-nulfix' -AppContainer $true -GrantRX @($runtimeDir) `
  -GrantModify @($dataDir) -Cwd $runtimeDir -EntryFile $objChild -TimeoutMs 45000 -RepairNull
Invoke-Arm -Name 'obj-ac-npfsfix' -AppContainer $true -GrantRX @($runtimeDir) `
  -GrantModify @($dataDir) -Cwd $runtimeDir -EntryFile $objChild -TimeoutMs 45000 -RepairNpfs
Invoke-Arm -Name 'obj-ac-baseline-again' -AppContainer $true -GrantRX @($runtimeDir) `
  -GrantModify @($dataDir) -Cwd $runtimeDir -EntryFile $objChild -TimeoutMs 45000

# THE RESIDUAL the file-descriptor mitigation cannot cover. `child_process.fork` opens an IPC channel
# — a `uv_pipe` with `ipc=1`, through the same `uv__create_pipe_pair` -> `uv__pipe_server` path in the
# same global namespace — and no `stdio` option removes it. Measured rather than inferred, with the
# unconfined half in the same run, and in its own arm because it is expected to hang.
Invoke-Arm -Name 'obj-plain-fork' -AppContainer $false -Cwd $runtimeDir -EntryFile $objChild `
  -TimeoutMs 30000 -ObjMode 'fork'
Invoke-Arm -Name 'obj-ac-fork' -AppContainer $true -GrantRX @($runtimeDir) `
  -GrantModify @($dataDir) -Cwd $runtimeDir -EntryFile $objChild -TimeoutMs 30000 -ObjMode 'fork'

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
