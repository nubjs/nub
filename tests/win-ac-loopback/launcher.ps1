# AppContainer launcher + security helpers for the same-package loopback probe (W1).
#
# Derived from `tests/win-fsnet-ceiling/launcher.ps1` (measured, runs 30688900451 /
# 30689267117 / 30689583039). Three things differ, each because this question needs two
# children ALIVE AT THE SAME TIME rather than one child run to completion:
#
#   1. `Start` / `Wait` / `Kill` replace the synchronous `LaunchEx`. A listener and a
#      connector must overlap, so the launch primitive returns a handle instead of an
#      exit code. Everything else about the launch is unchanged, deliberately: the arms
#      differ by package sid and capability set, never by program.
#   2. `IsAdmin` -- `CheckTokenMembership(NULL, Administrators)`, evaluated against the
#      EFFECTIVE token. MECHANISM-FACTS 5h: `CreateRestrictedToken` COPIES `TokenIsElevated`,
#      so that flag still reads 1 on a de-elevated token and gating on it is worthless.
#      An access check is the only honest de-elevation assertion.
#   3. The raw Win32 filesystem probes are gone -- nothing here asks a filesystem question.
#
# Retained without change because they are what makes an arm an arm:
#   - `QueryChildToken` off the CHILD's own primary token through its process handle
#     (`TokenIsAppContainer` / `TokenAppContainerSid` / `TokenCapabilities` /
#     `TokenIntegrityLevel`). `tests/win-bypass-traverse/launcher.ps1` declared a
#     `capabilitySids` parameter and wrote `CapabilityCount = 0` unconditionally, so every
#     arm it ever ran was a zero-capability arm. Only a read-back makes that unrepeatable.
#   - The de-elevated base token is lowered to MEDIUM integrity before any launch.
#   - `CharSet.Unicode` on every declaration. The ANSI default marshals a name into a
#     UTF-16 API and the failure reads as "mechanism unavailable" (MECHANISM-FACTS 5e).

$src = @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;

public static class Fx
{
    // ----------------------------- interop declarations -----------------------------

    [StructLayout(LayoutKind.Sequential)]
    struct SECURITY_CAPABILITIES
    {
        public IntPtr AppContainerSid;
        public IntPtr Capabilities;
        public uint CapabilityCount;
        public uint Reserved;
    }

    [StructLayout(LayoutKind.Sequential)]
    struct SID_AND_ATTRIBUTES { public IntPtr Sid; public uint Attributes; }

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
    [DllImport("userenv.dll", CharSet = CharSet.Unicode)]
    static extern int DeriveAppContainerSidFromAppContainerName(string name, out IntPtr sid);
    [DllImport("advapi32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool ConvertSidToStringSidW(IntPtr sid, out IntPtr str);
    [DllImport("advapi32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool ConvertStringSidToSidW(string s, out IntPtr sid);
    [DllImport("advapi32.dll")] static extern uint GetLengthSid(IntPtr sid);
    [DllImport("kernel32.dll")] static extern IntPtr LocalFree(IntPtr p);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool CloseHandle(IntPtr h);

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern IntPtr CreateFileW(string name, uint access, uint share,
        ref SECURITY_ATTRIBUTES sa, uint disp, uint flags, IntPtr templ);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool InitializeProcThreadAttributeList(IntPtr list, int count, int flags, ref IntPtr size);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool UpdateProcThreadAttribute(IntPtr list, uint flags, IntPtr attr, IntPtr value,
        IntPtr size, IntPtr prev, IntPtr ret);
    [DllImport("kernel32.dll")] static extern void DeleteProcThreadAttributeList(IntPtr list);
    [DllImport("advapi32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool CreateProcessAsUserW(IntPtr token, string app, StringBuilder cmdline,
        IntPtr pa, IntPtr ta, bool inherit, uint flags, IntPtr env, string cwd,
        ref STARTUPINFOEXW si, out PROCESS_INFORMATION pi);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool CreateProcessW(string app, StringBuilder cmdline, IntPtr pa, IntPtr ta,
        bool inherit, uint flags, IntPtr env, string cwd, ref STARTUPINFOEXW si,
        out PROCESS_INFORMATION pi);
    [DllImport("kernel32.dll", SetLastError = true)] static extern uint WaitForSingleObject(IntPtr h, uint ms);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool GetExitCodeProcess(IntPtr h, out uint code);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool TerminateProcess(IntPtr h, uint code);

    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool CreateRestrictedToken(IntPtr existing, uint flags,
        uint disableCount, IntPtr disableSids, uint deleteCount, IntPtr deletePrivs,
        uint restrictCount, IntPtr restrictSids, out IntPtr newToken);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool DuplicateTokenEx(IntPtr existing, uint access, IntPtr attrs,
        int impLevel, int tokenType, out IntPtr newToken);
    [DllImport("advapi32.dll", SetLastError = true)] static extern bool ImpersonateLoggedOnUser(IntPtr token);
    [DllImport("advapi32.dll", SetLastError = true)] static extern bool RevertToSelf();
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool OpenProcessToken(IntPtr proc, uint access, out IntPtr token);
    [DllImport("kernel32.dll")] static extern IntPtr GetCurrentProcess();
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool AllocateAndInitializeSid(byte[] auth, byte count,
        uint a0, uint a1, uint a2, uint a3, uint a4, uint a5, uint a6, uint a7, out IntPtr sid);
    [DllImport("advapi32.dll")] static extern IntPtr FreeSid(IntPtr sid);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool SetTokenInformation(IntPtr token, int cls, IntPtr info, uint len);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool GetTokenInformation(IntPtr token, int cls, IntPtr info, uint len, out uint ret);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool CheckTokenMembership(IntPtr token, IntPtr sid, out bool isMember);

    const uint GENERIC_READ = 0x80000000, GENERIC_WRITE = 0x40000000;
    const uint FILE_SHARE_READ = 1, FILE_SHARE_WRITE = 2;
    const uint CREATE_ALWAYS = 2, OPEN_EXISTING = 3, FILE_ATTRIBUTE_NORMAL = 0x80;
    const uint EXTENDED_STARTUPINFO_PRESENT = 0x00080000;
    const uint STARTF_USESTDHANDLES = 0x00000100;
    static readonly IntPtr PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES = new IntPtr(0x00020009);
    const uint WAIT_TIMEOUT = 0x00000102;
    const uint SE_GROUP_ENABLED = 0x00000004;
    const uint SE_GROUP_INTEGRITY = 0x00000020;
    const uint DISABLE_MAX_PRIVILEGE = 1;
    const uint TOKEN_ALL_ACCESS = 0xF01FF;
    const uint TOKEN_QUERY = 8;
    // TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY | TOKEN_IMPERSONATE | TOKEN_ADJUST_DEFAULT
    const uint TOKEN_OPEN_ACCESS = 2 | 8 | 1 | 4 | 0x80;
    const int TokenIntegrityLevel = 25, TokenIsAppContainer = 29, TokenCapabilities = 30,
              TokenAppContainerSid = 31;

    static string SidStr(IntPtr sid)
    {
        IntPtr s;
        if (sid == IntPtr.Zero || !ConvertSidToStringSidW(sid, out s)) return "?";
        string r = Marshal.PtrToStringUni(s);
        LocalFree(s);
        return r;
    }

    // ----------------------------- AppContainer profile -----------------------------

    public static string CreateProfile(string name)
    {
        IntPtr sid;
        int hr = CreateAppContainerProfile(name, name, name, IntPtr.Zero, 0, out sid);
        if (hr != 0) return "ERR hr=0x" + hr.ToString("x8");
        string s = SidStr(sid);
        LocalFree(sid);
        return s;
    }

    public static string DeleteProfile(string name)
    {
        int hr = DeleteAppContainerProfile(name);
        return hr == 0 ? "OK" : "ERR hr=0x" + hr.ToString("x8");
    }

    /// The hash-derived sid for a name, written nowhere. Printed beside `CreateProfile`'s
    /// answer so the run re-confirms MECHANICS 5h's "the sid is a pure hash of a name nub
    /// chooses" on this image rather than importing it.
    public static string DeriveSid(string name)
    {
        IntPtr sid;
        int hr = DeriveAppContainerSidFromAppContainerName(name, out sid);
        if (hr != 0) return "ERR hr=0x" + hr.ToString("x8");
        string s = SidStr(sid);
        FreeSid(sid);
        return s;
    }

    // --------------------- de-elevated, MEDIUM-IL token construction ---------------------

    static IntPtr MakeRestricted(int tokenType, out string err)
    {
        err = "";
        IntPtr self = IntPtr.Zero, restricted = IntPtr.Zero, adminSid = IntPtr.Zero,
               disable = IntPtr.Zero, dup = IntPtr.Zero, medSid = IntPtr.Zero, label = IntPtr.Zero;
        try
        {
            if (!OpenProcessToken(GetCurrentProcess(), TOKEN_OPEN_ACCESS, out self))
            { err = "OpenProcessToken=" + Marshal.GetLastWin32Error(); return IntPtr.Zero; }
            byte[] ntAuth = new byte[] { 0, 0, 0, 0, 0, 5 };
            if (!AllocateAndInitializeSid(ntAuth, 2, 32, 544, 0, 0, 0, 0, 0, 0, out adminSid))
            { err = "AllocateAndInitializeSid=" + Marshal.GetLastWin32Error(); return IntPtr.Zero; }
            disable = Marshal.AllocHGlobal(Marshal.SizeOf(typeof(SID_AND_ATTRIBUTES)));
            SID_AND_ATTRIBUTES sd = new SID_AND_ATTRIBUTES();
            sd.Sid = adminSid; sd.Attributes = 0;
            Marshal.StructureToPtr(sd, disable, false);
            if (!CreateRestrictedToken(self, DISABLE_MAX_PRIVILEGE, 1, disable, 0, IntPtr.Zero,
                    0, IntPtr.Zero, out restricted))
            { err = "CreateRestrictedToken=" + Marshal.GetLastWin32Error(); return IntPtr.Zero; }

            // tokenType 1 = TokenPrimary, 2 = TokenImpersonation; impLevel 2 = SecurityImpersonation.
            if (!DuplicateTokenEx(restricted, TOKEN_ALL_ACCESS, IntPtr.Zero, 2, tokenType, out dup))
            { err = "DuplicateTokenEx=" + Marshal.GetLastWin32Error(); return IntPtr.Zero; }

            // S-1-16-8192 = SECURITY_MANDATORY_MEDIUM_RID, a standard user's integrity level.
            if (!ConvertStringSidToSidW("S-1-16-8192", out medSid))
            { err = "ConvertStringSidToSid(medium)=" + Marshal.GetLastWin32Error(); CloseHandle(dup); return IntPtr.Zero; }
            int stride = Marshal.SizeOf(typeof(SID_AND_ATTRIBUTES));
            label = Marshal.AllocHGlobal(stride);
            SID_AND_ATTRIBUTES ml = new SID_AND_ATTRIBUTES();
            ml.Sid = medSid; ml.Attributes = SE_GROUP_INTEGRITY;
            Marshal.StructureToPtr(ml, label, false);
            if (!SetTokenInformation(dup, TokenIntegrityLevel, label, (uint)(stride + GetLengthSid(medSid))))
            { err = "SetTokenInformation(IL)=" + Marshal.GetLastWin32Error(); CloseHandle(dup); return IntPtr.Zero; }
            IntPtr ret = dup;
            dup = IntPtr.Zero;
            return ret;
        }
        finally
        {
            if (dup != IntPtr.Zero) CloseHandle(dup);
            if (label != IntPtr.Zero) Marshal.FreeHGlobal(label);
            if (medSid != IntPtr.Zero) LocalFree(medSid);
            if (restricted != IntPtr.Zero) CloseHandle(restricted);
            if (disable != IntPtr.Zero) Marshal.FreeHGlobal(disable);
            if (adminSid != IntPtr.Zero) FreeSid(adminSid);
            if (self != IntPtr.Zero) CloseHandle(self);
        }
    }

    static IntPtr impersonating = IntPtr.Zero;

    /// Impersonate the de-elevated medium-IL token, so `CreateAppContainerProfile`,
    /// `SetNamedSecurityInfoW` and the launches below all access-check against a token
    /// holding no administrative authority -- with no second account and no second process.
    public static string BeginDeelevated()
    {
        string err;
        IntPtr t = MakeRestricted(2, out err);
        if (t == IntPtr.Zero) return "ERR " + err;
        if (!ImpersonateLoggedOnUser(t)) { CloseHandle(t); return "ERR impersonate=" + Marshal.GetLastWin32Error(); }
        impersonating = t;
        return "OK";
    }

    public static string EndDeelevated()
    {
        if (!RevertToSelf()) return "ERR revert=" + Marshal.GetLastWin32Error();
        if (impersonating != IntPtr.Zero) { CloseHandle(impersonating); impersonating = IntPtr.Zero; }
        return "OK";
    }

    /// THE DE-ELEVATION ASSERTION. `CheckTokenMembership(NULL, ...)` evaluates against the
    /// calling THREAD's impersonation token when one is in force, else the process token --
    /// so this is the effective answer either way. It is an ACCESS CHECK, deliberately:
    /// `TokenIsElevated` is COPIED by `CreateRestrictedToken` and still reads 1 here
    /// (MECHANISM-FACTS 5h, run 30423750288). Never gate on the flag.
    public static string IsAdmin()
    {
        IntPtr adminSid = IntPtr.Zero;
        try
        {
            byte[] ntAuth = new byte[] { 0, 0, 0, 0, 0, 5 };
            if (!AllocateAndInitializeSid(ntAuth, 2, 32, 544, 0, 0, 0, 0, 0, 0, out adminSid))
                return "ERR alloc=" + Marshal.GetLastWin32Error();
            bool member;
            if (!CheckTokenMembership(IntPtr.Zero, adminSid, out member))
                return "ERR check=" + Marshal.GetLastWin32Error();
            return member ? "1" : "0";
        }
        finally { if (adminSid != IntPtr.Zero) FreeSid(adminSid); }
    }

    /// The effective token's integrity level and AppContainer flag, so "de-elevated" is a
    /// measured property of the token in force rather than an assertion about what was asked for.
    public static string WhoAmI()
    {
        IntPtr tok = impersonating;
        if (tok != IntPtr.Zero) return "il=" + QueryIl(tok) + " appcontainer=" + QueryIsAc(tok);
        if (!OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, out tok))
            return "ERR open=" + Marshal.GetLastWin32Error();
        string r = "il=" + QueryIl(tok) + " appcontainer=" + QueryIsAc(tok);
        CloseHandle(tok);
        return r;
    }

    static string QueryIl(IntPtr token)
    {
        uint need = 0;
        GetTokenInformation(token, TokenIntegrityLevel, IntPtr.Zero, 0, out need);
        if (need == 0) return "?";
        IntPtr buf = Marshal.AllocHGlobal((int)need);
        try
        {
            if (!GetTokenInformation(token, TokenIntegrityLevel, buf, need, out need))
                return "ERR" + Marshal.GetLastWin32Error();
            SID_AND_ATTRIBUTES sa = (SID_AND_ATTRIBUTES)Marshal.PtrToStructure(buf, typeof(SID_AND_ATTRIBUTES));
            return SidStr(sa.Sid);
        }
        finally { Marshal.FreeHGlobal(buf); }
    }

    static string QueryIsAc(IntPtr token)
    {
        uint need = 0;
        IntPtr buf = Marshal.AllocHGlobal(4);
        try
        {
            if (!GetTokenInformation(token, TokenIsAppContainer, buf, 4, out need))
                return "ERR" + Marshal.GetLastWin32Error();
            return Marshal.ReadInt32(buf).ToString();
        }
        finally { Marshal.FreeHGlobal(buf); }
    }

    static string QueryAcSid(IntPtr token)
    {
        uint need = 0;
        GetTokenInformation(token, TokenAppContainerSid, IntPtr.Zero, 0, out need);
        if (need == 0) return "none";
        IntPtr buf = Marshal.AllocHGlobal((int)need);
        try
        {
            if (!GetTokenInformation(token, TokenAppContainerSid, buf, need, out need))
                return "ERR" + Marshal.GetLastWin32Error();
            IntPtr sid = Marshal.ReadIntPtr(buf);
            return sid == IntPtr.Zero ? "none" : SidStr(sid);
        }
        finally { Marshal.FreeHGlobal(buf); }
    }

    /// The capability sids the token actually HOLDS -- the read-back that makes the
    /// `win-bypass-traverse` declared-but-dropped-capability bug impossible to repeat.
    static string QueryCaps(IntPtr token)
    {
        uint need = 0;
        GetTokenInformation(token, TokenCapabilities, IntPtr.Zero, 0, out need);
        if (need == 0) return "";
        IntPtr buf = Marshal.AllocHGlobal((int)need);
        try
        {
            if (!GetTokenInformation(token, TokenCapabilities, buf, need, out need))
                return "ERR" + Marshal.GetLastWin32Error();
            int count = Marshal.ReadInt32(buf);
            // TOKEN_GROUPS is { DWORD GroupCount; SID_AND_ATTRIBUTES Groups[]; }; the array starts
            // at the platform's pointer alignment, not at offset 4, on 64-bit.
            int off = IntPtr.Size == 8 ? 8 : 4;
            int stride = Marshal.SizeOf(typeof(SID_AND_ATTRIBUTES));
            StringBuilder sb = new StringBuilder();
            for (int i = 0; i < count; i++)
            {
                SID_AND_ATTRIBUTES sa = (SID_AND_ATTRIBUTES)Marshal.PtrToStructure(
                    new IntPtr(buf.ToInt64() + off + i * stride), typeof(SID_AND_ATTRIBUTES));
                if (sb.Length > 0) sb.Append(',');
                sb.Append(SidStr(sa.Sid));
            }
            return sb.ToString();
        }
        finally { Marshal.FreeHGlobal(buf); }
    }

    // ----------------------------- launch -----------------------------

    class Live { public IntPtr hProcess, hThread; public uint pid; }
    static readonly List<Live> live = new List<Live>();

    /// One launch primitive for every arm, so an arm difference is never a program difference.
    ///
    ///   `acSidStr` empty  => no AppContainer attribute at all (the plain arms).
    ///   `capabilitySids`  => REALLY passed (count AND array), and read back off the child.
    ///   `deelevate`       => `CreateProcessAsUserW` on the restricted medium-IL admin-deny-only
    ///                       token, so "AppContainer vs not" is never confounded with elevation.
    ///
    /// ASYNCHRONOUS, unlike the fs probe's `LaunchEx`: this question needs a listener and a
    /// connector alive together. Returns `h=<idx>` plus the `childtoken:` block read off the
    /// CHILD's own primary token through its process handle, before any wait.
    public static string Start(string acSidStr, string exe, string cmdline, string cwd,
        string logPath, string[] capabilitySids, bool deelevate)
    {
        IntPtr acSid = IntPtr.Zero, attrList = IntPtr.Zero, capsBuf = IntPtr.Zero, capArray = IntPtr.Zero;
        IntPtr hOut = new IntPtr(-1), hIn = new IntPtr(-1), baseTok = IntPtr.Zero;
        List<IntPtr> capSids = new List<IntPtr>();
        try
        {
            SECURITY_ATTRIBUTES sa = new SECURITY_ATTRIBUTES();
            sa.nLength = Marshal.SizeOf(typeof(SECURITY_ATTRIBUTES));
            sa.bInheritHandle = 1;
            // Opened by the UNCONFINED parent and inherited already-open. Access is checked at
            // open and cached in the handle, so the child writes its report even when it can read
            // nothing at all -- the difference between a negative result and no result.
            hOut = CreateFileW(logPath, GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE,
                ref sa, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, IntPtr.Zero);
            if (hOut == new IntPtr(-1)) return "launch-error CreateFileW(log) err=" + Marshal.GetLastWin32Error();
            hIn = CreateFileW("NUL", GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE,
                ref sa, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, IntPtr.Zero);
            if (hIn == new IntPtr(-1)) return "launch-error CreateFileW(NUL) err=" + Marshal.GetLastWin32Error();

            bool confined = !string.IsNullOrEmpty(acSidStr);
            STARTUPINFOEXW si = new STARTUPINFOEXW();
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
                caps.Capabilities = IntPtr.Zero;
                caps.CapabilityCount = 0;
                if (capabilitySids != null && capabilitySids.Length > 0)
                {
                    int stride = Marshal.SizeOf(typeof(SID_AND_ATTRIBUTES));
                    capArray = Marshal.AllocHGlobal(stride * capabilitySids.Length);
                    for (int i = 0; i < capabilitySids.Length; i++)
                    {
                        IntPtr csid;
                        if (!ConvertStringSidToSidW(capabilitySids[i], out csid))
                            return "launch-error ConvertStringSidToSid(cap " + capabilitySids[i] + ") err="
                                + Marshal.GetLastWin32Error();
                        capSids.Add(csid);
                        SID_AND_ATTRIBUTES sa2 = new SID_AND_ATTRIBUTES();
                        sa2.Sid = csid;
                        // A capability sid must be ENABLED or it is present-and-inert, which reads
                        // as "the capability did not help" when in fact it was never held.
                        sa2.Attributes = SE_GROUP_ENABLED;
                        Marshal.StructureToPtr(sa2, new IntPtr(capArray.ToInt64() + i * stride), false);
                    }
                    caps.Capabilities = capArray;
                    caps.CapabilityCount = (uint)capabilitySids.Length;
                }
                caps.Reserved = 0;
                capsBuf = Marshal.AllocHGlobal(Marshal.SizeOf(typeof(SECURITY_CAPABILITIES)));
                Marshal.StructureToPtr(caps, capsBuf, false);
                if (!UpdateProcThreadAttribute(attrList, 0,
                        PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, capsBuf,
                        new IntPtr(Marshal.SizeOf(typeof(SECURITY_CAPABILITIES))), IntPtr.Zero, IntPtr.Zero))
                    return "launch-error UpdateProcThreadAttribute err=" + Marshal.GetLastWin32Error();
                si.lpAttributeList = attrList;
                flags |= EXTENDED_STARTUPINFO_PRESENT;
            }

            PROCESS_INFORMATION pi;
            StringBuilder cl = new StringBuilder(cmdline, cmdline.Length + 64);
            bool ok;
            if (deelevate)
            {
                string err;
                baseTok = MakeRestricted(1, out err);
                if (baseTok == IntPtr.Zero) return "launch-error deelevated-token " + err;
                ok = CreateProcessAsUserW(baseTok, exe, cl, IntPtr.Zero, IntPtr.Zero, true, flags,
                    IntPtr.Zero, cwd, ref si, out pi);
            }
            else
            {
                ok = CreateProcessW(exe, cl, IntPtr.Zero, IntPtr.Zero, true, flags,
                    IntPtr.Zero, cwd, ref si, out pi);
            }
            if (!ok) return "launch-error CreateProcess" + (deelevate ? "AsUserW" : "W")
                + " err=" + Marshal.GetLastWin32Error();

            // THE ENGAGEMENT PROOF, taken from the CHILD's own token through its process handle.
            // Reading the parent's intent instead would prove nothing. The child ALSO reports
            // the same four values from inside itself; two independent readings of one fact.
            string tokLines;
            IntPtr ctok;
            if (OpenProcessToken(pi.hProcess, TOKEN_QUERY, out ctok))
            {
                tokLines = "\nchildtoken:isAppContainer=" + QueryIsAc(ctok)
                    + "\nchildtoken:packageSid=" + QueryAcSid(ctok)
                    + "\nchildtoken:capabilities=[" + QueryCaps(ctok) + "]"
                    + "\nchildtoken:integrity=" + QueryIl(ctok);
                CloseHandle(ctok);
            }
            else tokLines = "\nchildtoken:ERR open=" + Marshal.GetLastWin32Error();

            Live l = new Live();
            l.hProcess = pi.hProcess; l.hThread = pi.hThread; l.pid = pi.dwProcessId;
            live.Add(l);
            return "h=" + (live.Count - 1) + "\nlaunch=OK pid=" + pi.dwProcessId + tokLines;
        }
        finally
        {
            if (baseTok != IntPtr.Zero) CloseHandle(baseTok);
            if (hOut != new IntPtr(-1)) CloseHandle(hOut);
            if (hIn != new IntPtr(-1)) CloseHandle(hIn);
            if (attrList != IntPtr.Zero) { DeleteProcThreadAttributeList(attrList); Marshal.FreeHGlobal(attrList); }
            if (capsBuf != IntPtr.Zero) Marshal.FreeHGlobal(capsBuf);
            foreach (IntPtr p in capSids) LocalFree(p);
            if (capArray != IntPtr.Zero) Marshal.FreeHGlobal(capArray);
            if (acSid != IntPtr.Zero) LocalFree(acSid);
        }
    }

    public static string Wait(int h, uint timeoutMs)
    {
        if (h < 0 || h >= live.Count || live[h] == null) return "ERR no-handle";
        Live l = live[h];
        uint wr = WaitForSingleObject(l.hProcess, timeoutMs);
        if (wr == WAIT_TIMEOUT) return "TIMED-OUT";
        uint code;
        GetExitCodeProcess(l.hProcess, out code);
        return "rc=" + code + " (0x" + code.ToString("x8") + ")";
    }

    /// Terminate and release. Called for every handle at arm end, so a hung listener can
    /// never leak into the next arm and answer a connect it was not part of.
    public static string Kill(int h)
    {
        if (h < 0 || h >= live.Count || live[h] == null) return "ERR no-handle";
        Live l = live[h];
        uint code;
        if (GetExitCodeProcess(l.hProcess, out code) && code == 259) TerminateProcess(l.hProcess, 0xDEAD);
        CloseHandle(l.hThread);
        CloseHandle(l.hProcess);
        live[h] = null;
        return "OK";
    }

    // ----------------------------- ACE write / read-back -----------------------------

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
        public int grfAccessMode;
        public uint grfInheritance;
        public TRUSTEE_W Trustee;
    }

    [DllImport("advapi32.dll", CharSet = CharSet.Unicode)]
    static extern uint GetNamedSecurityInfoW(string obj, int type, uint si,
        IntPtr owner, IntPtr group, out IntPtr dacl, IntPtr sacl, out IntPtr sd);
    [DllImport("advapi32.dll", CharSet = CharSet.Unicode)]
    static extern uint SetNamedSecurityInfoW(string obj, int type, uint si,
        IntPtr owner, IntPtr group, IntPtr dacl, IntPtr sacl);
    [DllImport("advapi32.dll")]
    static extern uint SetEntriesInAclW(uint count, ref EXPLICIT_ACCESS_W list, IntPtr old, out IntPtr newAcl);
    [DllImport("advapi32.dll", CharSet = CharSet.Unicode)]
    static extern uint GetEffectiveRightsFromAclW(IntPtr acl, ref TRUSTEE_W trustee, out uint rights);

    const int SE_FILE_OBJECT = 1;
    const uint DACL_SECURITY_INFORMATION = 4;
    const int TRUSTEE_IS_SID = 0, TRUSTEE_IS_UNKNOWN = 0;
    const uint SUB_CONTAINERS_AND_OBJECTS_INHERIT = 3; // CONTAINER_INHERIT | OBJECT_INHERIT
    const uint NO_INHERITANCE = 0;

    /// Add (`mode` = 1 GRANT) or remove (`mode` = 4 REVOKE) an ACE for `sidStr` on `path`.
    public static string SetAce(string path, string sidStr, uint access, int mode, bool inherit)
    {
        IntPtr sid = IntPtr.Zero, oldDacl = IntPtr.Zero, sd = IntPtr.Zero, newDacl = IntPtr.Zero;
        try
        {
            if (!ConvertStringSidToSidW(sidStr, out sid)) return "ERR sid=" + Marshal.GetLastWin32Error();
            uint rc = GetNamedSecurityInfoW(path, SE_FILE_OBJECT, DACL_SECURITY_INFORMATION,
                IntPtr.Zero, IntPtr.Zero, out oldDacl, IntPtr.Zero, out sd);
            if (rc != 0) return "ERR get=" + rc;
            EXPLICIT_ACCESS_W ea = new EXPLICIT_ACCESS_W();
            ea.grfAccessPermissions = access;
            ea.grfAccessMode = mode;
            ea.grfInheritance = inherit ? SUB_CONTAINERS_AND_OBJECTS_INHERIT : NO_INHERITANCE;
            ea.Trustee.TrusteeForm = TRUSTEE_IS_SID;
            ea.Trustee.TrusteeType = TRUSTEE_IS_UNKNOWN;
            ea.Trustee.ptstrName = sid;
            rc = SetEntriesInAclW(1, ref ea, oldDacl, out newDacl);
            if (rc != 0) return "ERR setentries=" + rc;
            rc = SetNamedSecurityInfoW(path, SE_FILE_OBJECT, DACL_SECURITY_INFORMATION,
                IntPtr.Zero, IntPtr.Zero, newDacl, IntPtr.Zero);
            return rc == 0 ? "OK" : "ERR set=" + rc;
        }
        finally
        {
            if (newDacl != IntPtr.Zero) LocalFree(newDacl);
            if (sd != IntPtr.Zero) LocalFree(sd);
            if (sid != IntPtr.Zero) LocalFree(sid);
        }
    }

    /// The effective rights `sidStr` holds on `path`'s OWN dacl. Without this read-back a
    /// propagation slip and a kernel denial are indistinguishable in a child's output.
    public static string ReadAceMask(string path, string sidStr)
    {
        IntPtr sid = IntPtr.Zero, dacl = IntPtr.Zero, sd = IntPtr.Zero;
        try
        {
            if (!ConvertStringSidToSidW(sidStr, out sid)) return "ERR sid=" + Marshal.GetLastWin32Error();
            uint rc = GetNamedSecurityInfoW(path, SE_FILE_OBJECT, DACL_SECURITY_INFORMATION,
                IntPtr.Zero, IntPtr.Zero, out dacl, IntPtr.Zero, out sd);
            if (rc != 0) return "ERR get=" + rc;
            TRUSTEE_W t = new TRUSTEE_W();
            t.TrusteeForm = TRUSTEE_IS_SID;
            t.TrusteeType = TRUSTEE_IS_UNKNOWN;
            t.ptstrName = sid;
            uint rights;
            rc = GetEffectiveRightsFromAclW(dacl, ref t, out rights);
            if (rc != 0) return "ERR eff=" + rc;
            return "0x" + rights.ToString("x8");
        }
        finally
        {
            if (sd != IntPtr.Zero) LocalFree(sd);
            if (sid != IntPtr.Zero) LocalFree(sid);
        }
    }
}
'@

Add-Type -TypeDefinition $src -Language CSharp -ErrorAction Stop
