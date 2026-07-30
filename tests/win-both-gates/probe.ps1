# Can ONE unprivileged Windows token get BOTH working filesystem reads AND denied network egress?
#
# THE QUESTION. The build jail has two candidate mechanisms and each is missing half the answer:
#
#   AppContainer (LowBox)   coarse egress-deny for free (withhold `internetClient`), but the
#                           AppContainer SECOND GATE demands a per-path ace, and `C:\` / `C:\Users`
#                           cannot be ACE'd unprivileged (TrustedInstaller / SYSTEM own them and
#                           grant no standard group WRITE_DAC).
#   restricted token, low   reads work everywhere with zero aces written, writes fenced by
#                           integrity, and no known unprivileged egress lever.
#
# The prior composed measurement (`.fray/sandbox-MECHANISM-FACTS.md` 5d) concluded the two do not
# compose. THE FLAW IN IT: every arm ran with ZERO CAPABILITIES. An empty capability array passes
# nothing at the second gate, so those DENIEDs may be measuring the empty array rather than an
# inherent limit — and the disk demonstrably grants a capability, since `C:\` carries an
# `S-1-15-3-65536-…` ace for exactly the read+traverse mask the jail needs.
#
# Four separable questions, each with its own controls:
#
#   G1  Does a LowBox token HOLDING the sids the disk already grants pass the second gate?
#   G2  Is the gate escapable another way — a well-known capability, the user sid as a capability,
#       or the bypass-traverse privilege making the two un-ACE'able roots irrelevant to a deep open?
#   G3  Can `CreateRestrictedToken`'s SidsToRestrict deny egress while leaving reads? That needs the
#       DACL of what a socket actually opens, which is read here off a LIVE socket handle rather
#       than assumed.
#   G4  What IS `S-1-15-3-65536-…`? Answered by deriving every capability name the machine knows and
#       matching, not by guessing.
#
# WHY AccessCheck. It is the OS's own evaluator against a real token and a real descriptor, so it
# answers the DACL-and-integrity question without needing a launch to work first. It is a MODEL of
# the check, not the check in situ — hence three mandatory controls:
#
#   baseline    an unmodified token must be GRANTED everywhere, or the harness is measuring its own
#               mistake (this caught a TOKEN_ADJUST_DEFAULT bug that made two treatment arms fail
#               identically and read exactly like "mechanism unavailable").
#   gate        a LowBox token on an unrestricted base with zero capabilities must be DENIED on
#               `C:\`, reproducing the real confined launch. GRANTED would mean AccessCheck is not
#               applying the AppContainer gate and every composed row is meaningless.
#   positive    that same token must be GRANTED on `C:\Windows\System32`, which carries an ALL
#               APPLICATION PACKAGES ace. Without it, a table of DENIEDs cannot be told from a gate
#               that denies unconditionally.
#
# Unprivileged by construction: every token is derived from THIS process's own token and integrity is
# only ever LOWERED. Elevation is reported as a fact so an elevated baseline is never mistaken for
# the shipping case.

$ErrorActionPreference = 'Continue'
Set-StrictMode -Off

$src = @'
using System;
using System.Collections.Generic;
using System.IO;
using System.Net.Sockets;
using System.Runtime.InteropServices;
using System.Security.AccessControl;
using System.Security.Principal;
using System.Text;
using Microsoft.Win32;

public static class Probe
{
    // ─────────────────────────── interop ───────────────────────────

    [StructLayout(LayoutKind.Sequential)]
    public struct SID_AND_ATTRIBUTES { public IntPtr Sid; public uint Attributes; }

    [StructLayout(LayoutKind.Sequential)]
    public struct GENERIC_MAPPING
    { public uint GenericRead; public uint GenericWrite; public uint GenericExecute; public uint GenericAll; }

    [StructLayout(LayoutKind.Sequential)]
    public struct UNICODE_STRING
    { public ushort Length; public ushort MaximumLength; public IntPtr Buffer; }

    [StructLayout(LayoutKind.Sequential)]
    public struct OBJECT_ATTRIBUTES
    {
        public int Length; public IntPtr RootDirectory; public IntPtr ObjectName;
        public uint Attributes; public IntPtr SecurityDescriptor; public IntPtr SecurityQoS;
    }

    [StructLayout(LayoutKind.Sequential)]
    public struct IO_STATUS_BLOCK { public IntPtr Status; public IntPtr Information; }

    [DllImport("advapi32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern bool ConvertStringSidToSidW(string s, out IntPtr sid);
    [DllImport("advapi32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern bool ConvertSidToStringSidW(IntPtr sid, out IntPtr str);
    [DllImport("advapi32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern bool ConvertSecurityDescriptorToStringSecurityDescriptorW(
        byte[] sd, uint rev, uint info, out IntPtr str, out uint len);
    [DllImport("kernel32.dll")] static extern IntPtr LocalFree(IntPtr p);
    [DllImport("kernel32.dll")] static extern IntPtr GetCurrentProcess();
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool CloseHandle(IntPtr h);

    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool OpenProcessToken(IntPtr proc, uint access, out IntPtr token);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool CreateRestrictedToken(IntPtr existing, uint flags,
        uint disableCount, SID_AND_ATTRIBUTES[] disable,
        uint deleteCount, IntPtr delete,
        uint restrictCount, SID_AND_ATTRIBUTES[] restrict, out IntPtr outTok);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool DuplicateTokenEx(IntPtr t, uint access, IntPtr sa,
        int impLevel, int tokenType, out IntPtr dup);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool SetTokenInformation(IntPtr t, int cls, IntPtr info, uint len);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool GetTokenInformation(IntPtr t, int cls, IntPtr info, uint len, out uint ret);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool AccessCheck(byte[] sd, IntPtr token, uint desired,
        ref GENERIC_MAPPING map, IntPtr privSet, ref uint privLen,
        out uint granted, out int status);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool GetKernelObjectSecurity(IntPtr h, uint info, byte[] sd, uint len, out uint needed);
    [DllImport("advapi32.dll", SetLastError = true)] static extern uint GetLengthSid(IntPtr sid);
    [DllImport("advapi32.dll", SetLastError = true)] static extern IntPtr FreeSid(IntPtr sid);
    [DllImport("advapi32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern bool LookupPrivilegeNameW(string sys, IntPtr luid, StringBuilder name, ref int cch);

    [DllImport("userenv.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern int DeriveAppContainerSidFromAppContainerName(string name, out IntPtr sid);
    [DllImport("userenv.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern bool DeriveCapabilitySidsFromName(string name,
        out IntPtr groupSids, out uint groupCount, out IntPtr capSids, out uint capCount);

    [DllImport("ntdll.dll")]
    static extern int NtCreateLowBoxToken(out IntPtr token, IntPtr existing, uint access,
        ref OBJECT_ATTRIBUTES oa, IntPtr packageSid,
        uint capCount, IntPtr caps, uint handleCount, IntPtr handles);
    [DllImport("ntdll.dll")]
    static extern int NtOpenFile(out IntPtr h, uint access, ref OBJECT_ATTRIBUTES oa,
        out IO_STATUS_BLOCK iosb, uint share, uint options);
    [DllImport("ntdll.dll")]
    static extern int NtQuerySecurityObject(IntPtr h, uint info, byte[] sd, uint len, out uint needed);

    const uint TOKEN_DUPLICATE = 0x0002, TOKEN_QUERY = 0x0008;
    const uint TOKEN_ADJUST_DEFAULT = 0x0080, TOKEN_ASSIGN_PRIMARY = 0x0001;
    const uint TOKEN_ALL_ACCESS = 0xF01FF;
    // TOKEN_ADJUST_DEFAULT is load-bearing: SetTokenInformation(IntegrityLevel) needs it, and
    // CreateRestrictedToken propagates the SOURCE handle's granted access. Opening with only
    // DUPLICATE|QUERY once produced a restricted token that could not be relabelled, so both
    // treatment arms died with ACCESS_DENIED before any access check ran.
    const uint TOKEN_RIGHTS = TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ADJUST_DEFAULT | TOKEN_ASSIGN_PRIMARY;
    const uint DISABLE_MAX_PRIVILEGE = 0x1;
    const int TokenIntegrityLevel = 25, TokenIsAppContainer = 29, TokenCapabilities = 30;
    const int TokenPrivileges = 3, TokenRestrictedSids = 11, TokenAppContainerSid = 31;
    const uint SE_GROUP_INTEGRITY = 0x20, SE_GROUP_ENABLED = 0x4;

    const uint FILE_READ_DATA = 0x1, FILE_WRITE_DATA = 0x2;
    const uint FILE_TRAVERSE = 0x20, FILE_READ_ATTRIBUTES = 0x80, FILE_ADD_FILE = 0x2;
    const uint READ_SET = FILE_READ_DATA | FILE_TRAVERSE | FILE_READ_ATTRIBUTES;
    const uint WRITE_SET = FILE_WRITE_DATA | FILE_ADD_FILE;
    const uint READ_CONTROL = 0x20000, SYNCHRONIZE = 0x100000;

    static int fails = 0;

    static void W(string s) { Console.WriteLine(s); }
    static void Fact(string k, string v) { W("  fact:" + k + " = " + v); }
    static void Prop(string k, bool ok, string why)
    {
        W("  prop:" + k + "=" + (ok ? "PASS" : "FAIL") + "  " + why);
        if (!ok) fails++;
    }

    static string SidStr(IntPtr sid)
    {
        IntPtr s;
        if (!ConvertSidToStringSidW(sid, out s)) return "?";
        string r = Marshal.PtrToStringUni(s); LocalFree(s); return r;
    }
    static IntPtr StrSid(string s)
    {
        IntPtr p;
        if (!ConvertStringSidToSidW(s, out p)) return IntPtr.Zero;
        return p;
    }
    static string SidName(string sddl)
    {
        try { return new SecurityIdentifier(sddl).Translate(typeof(NTAccount)).ToString(); }
        catch (Exception) { return "(unmapped)"; }
    }

    // ─────────────────────────── tokens ───────────────────────────

    static IntPtr OwnToken()
    {
        IntPtr t;
        if (!OpenProcessToken(GetCurrentProcess(), TOKEN_RIGHTS, out t))
            throw new Exception("OpenProcessToken: " + Marshal.GetLastWin32Error());
        return t;
    }

    static IntPtr ForCheck(IntPtr t)
    {
        IntPtr d;
        // AccessCheck requires an IMPERSONATION-level token, not a primary one.
        if (!DuplicateTokenEx(t, TOKEN_ALL_ACCESS, IntPtr.Zero, 2, 2, out d))
            throw new Exception("DuplicateTokenEx: " + Marshal.GetLastWin32Error());
        return d;
    }

    static void SetIntegrity(IntPtr t, string levelSid)
    {
        IntPtr sid = StrSid(levelSid);
        SID_AND_ATTRIBUTES sa = new SID_AND_ATTRIBUTES();
        sa.Sid = sid; sa.Attributes = SE_GROUP_INTEGRITY;
        int sz = Marshal.SizeOf(typeof(SID_AND_ATTRIBUTES));
        IntPtr buf = Marshal.AllocHGlobal(sz);
        Marshal.StructureToPtr(sa, buf, false);
        bool ok = SetTokenInformation(t, TokenIntegrityLevel, buf, (uint)sz + GetLengthSid(sid));
        int err = Marshal.GetLastWin32Error();
        Marshal.FreeHGlobal(buf); LocalFree(sid);
        if (!ok) throw new Exception("SetTokenInformation(integrity): " + err);
    }

    /// A restricted token: Administrators reduced to deny-only, every privilege but
    /// SeChangeNotifyPrivilege stripped (DISABLE_MAX_PRIVILEGE preserves exactly that one), then
    /// integrity lowered. `restrictSids` is the SidsToRestrict set — null means none, which is the
    /// arm every prior measurement used and the gap this probe closes.
    static IntPtr RestrictedToken(string levelSid, string[] restrictSids)
    {
        IntPtr me = OwnToken();
        try
        {
            IntPtr admins = StrSid("S-1-5-32-544");
            SID_AND_ATTRIBUTES[] deny = new SID_AND_ATTRIBUTES[1];
            deny[0].Sid = admins; deny[0].Attributes = 0;

            SID_AND_ATTRIBUTES[] restrict = null;
            List<IntPtr> owned = new List<IntPtr>();
            if (restrictSids != null && restrictSids.Length > 0)
            {
                restrict = new SID_AND_ATTRIBUTES[restrictSids.Length];
                for (int i = 0; i < restrictSids.Length; i++)
                {
                    IntPtr s = StrSid(restrictSids[i]);
                    if (s == IntPtr.Zero) throw new Exception("bad restricting sid " + restrictSids[i]);
                    owned.Add(s);
                    restrict[i].Sid = s; restrict[i].Attributes = 0;
                }
            }
            IntPtr outTok;
            bool ok = CreateRestrictedToken(me, DISABLE_MAX_PRIVILEGE,
                1, deny, 0, IntPtr.Zero,
                restrict == null ? 0u : (uint)restrict.Length, restrict, out outTok);
            int err = Marshal.GetLastWin32Error();
            LocalFree(admins);
            for (int i = 0; i < owned.Count; i++) LocalFree(owned[i]);
            if (!ok) throw new Exception("CreateRestrictedToken: " + err);
            try
            {
                if (levelSid != null) SetIntegrity(outTok, levelSid);
                return ForCheck(outTok);
            }
            finally { CloseHandle(outTok); }
        }
        finally { CloseHandle(me); }
    }

    /// Turn `baseTok` into a LowBox (AppContainer) token carrying `package` and `caps`.
    ///
    /// NtCreateLowBoxToken is the syscall CreateProcessW itself reaches through when handed
    /// SECURITY_CAPABILITIES, and it takes the BASE token as a parameter — which is what makes the
    /// composition testable at all. THE POINT of this probe is that `caps` is no longer empty; a
    /// prior attempt to pass these sids through CreateProcessW's proc-thread attribute was refused
    /// with ERROR_INVALID_PARAMETER, and the Nt layer applies a different (looser) validation.
    static IntPtr LowBoxToken(IntPtr baseTok, IntPtr package, string[] caps)
    {
        OBJECT_ATTRIBUTES oa = new OBJECT_ATTRIBUTES();
        oa.Length = Marshal.SizeOf(typeof(OBJECT_ATTRIBUTES));

        int n = caps == null ? 0 : caps.Length;
        IntPtr arr = IntPtr.Zero;
        List<IntPtr> owned = new List<IntPtr>();
        int stride = Marshal.SizeOf(typeof(SID_AND_ATTRIBUTES));
        if (n > 0)
        {
            arr = Marshal.AllocHGlobal(stride * n);
            for (int i = 0; i < n; i++)
            {
                IntPtr s = StrSid(caps[i]);
                if (s == IntPtr.Zero) throw new Exception("bad capability sid " + caps[i]);
                owned.Add(s);
                SID_AND_ATTRIBUTES sa = new SID_AND_ATTRIBUTES();
                sa.Sid = s; sa.Attributes = SE_GROUP_ENABLED;
                Marshal.StructureToPtr(sa, new IntPtr(arr.ToInt64() + i * stride), false);
            }
        }
        IntPtr outTok;
        int st = NtCreateLowBoxToken(out outTok, baseTok, TOKEN_ALL_ACCESS, ref oa,
            package, (uint)n, arr, 0, IntPtr.Zero);
        if (arr != IntPtr.Zero) Marshal.FreeHGlobal(arr);
        for (int i = 0; i < owned.Count; i++) LocalFree(owned[i]);
        if (st != 0) throw new Exception(string.Format("NtCreateLowBoxToken: NTSTATUS 0x{0:x8}", st));
        try { return ForCheck(outTok); } finally { CloseHandle(outTok); }
    }

    static IntPtr PackageSid(string name)
    {
        IntPtr sid;
        int hr = DeriveAppContainerSidFromAppContainerName(name, out sid);
        if (hr != 0) throw new Exception(string.Format("DeriveAppContainerSid: 0x{0:x8}", hr));
        return sid;
    }

    static int CountGroups(IntPtr t, int cls)
    {
        uint need;
        GetTokenInformation(t, cls, IntPtr.Zero, 0, out need);
        if (need < 4) return -1;
        IntPtr buf = Marshal.AllocHGlobal((int)need);
        int n = GetTokenInformation(t, cls, buf, need, out need) ? Marshal.ReadInt32(buf) : -1;
        Marshal.FreeHGlobal(buf);
        return n;
    }

    /// Privilege names. SeChangeNotifyPrivilege (bypass traverse checking) is reported because it is
    /// the reason a DENIED on `C:\` may not matter: with it held, the object manager skips the
    /// traverse check on every INTERMEDIATE path component and checks only the leaf. AccessCheck
    /// against a single descriptor cannot model that.
    static string Privileges(IntPtr t)
    {
        uint need;
        GetTokenInformation(t, TokenPrivileges, IntPtr.Zero, 0, out need);
        if (need < 4) return "?";
        IntPtr buf = Marshal.AllocHGlobal((int)need);
        StringBuilder sb = new StringBuilder();
        if (GetTokenInformation(t, TokenPrivileges, buf, need, out need))
        {
            int count = Marshal.ReadInt32(buf);
            for (int i = 0; i < count; i++)
            {
                IntPtr rec = new IntPtr(buf.ToInt64() + 4 + i * 12);
                uint attr = (uint)Marshal.ReadInt32(rec, 8);
                StringBuilder nm = new StringBuilder(256); int cch = 256;
                string name = LookupPrivilegeNameW(null, rec, nm, ref cch) ? nm.ToString() : "?";
                sb.Append(name).Append(attr != 0 ? "(on) " : "(off) ");
            }
        }
        Marshal.FreeHGlobal(buf);
        return sb.ToString().Trim();
    }

    static string TokenShape(IntPtr t)
    {
        uint ret;
        IntPtr b4 = Marshal.AllocHGlobal(4);
        bool isAc = GetTokenInformation(t, TokenIsAppContainer, b4, 4, out ret)
                    && Marshal.ReadInt32(b4) != 0;
        Marshal.FreeHGlobal(b4);
        return string.Format("appcontainer={0} capabilities={1} restricting-sids={2}",
            isAc, CountGroups(t, TokenCapabilities), CountGroups(t, TokenRestrictedSids));
    }

    // ─────────────────────────── access checks ───────────────────────────

    static byte[] FileSd(string path)
    {
        // OWNER and GROUP are fetched alongside the DACL deliberately: AccessCheck rejects a
        // descriptor missing either with ERROR_INVALID_SECURITY_DESCR, which would read as a denial.
        AccessControlSections want = AccessControlSections.Access
            | AccessControlSections.Owner | AccessControlSections.Group;
        if (Directory.Exists(path))
            return new DirectoryInfo(path).GetAccessControl(want).GetSecurityDescriptorBinaryForm();
        return new FileInfo(path).GetAccessControl(want).GetSecurityDescriptorBinaryForm();
    }

    /// GRANTED / DENIED, with the granted mask on a partial so a near-miss is diagnosable rather
    /// than collapsing to the same word as a total refusal.
    static string Check(IntPtr token, byte[] sd, uint desired)
    {
        if (sd == null) return "NO-SD";
        GENERIC_MAPPING map = new GENERIC_MAPPING();
        map.GenericRead = READ_SET; map.GenericWrite = WRITE_SET;
        map.GenericExecute = FILE_TRAVERSE; map.GenericAll = READ_SET | WRITE_SET;
        IntPtr privs = Marshal.AllocHGlobal(1024);
        uint privLen = 1024; uint granted; int status;
        bool ok = AccessCheck(sd, token, desired, ref map, privs, ref privLen, out granted, out status);
        int err = Marshal.GetLastWin32Error();
        Marshal.FreeHGlobal(privs);
        if (!ok) return "ERROR:" + err;
        if (status != 0 && (granted & desired) == desired) return "GRANTED";
        if (granted != 0) return string.Format("DENIED(partial 0x{0:x})", granted);
        return "DENIED";
    }

    // ─────────────────────────── device descriptors ───────────────────────────

    /// The descriptor a socket actually opens. Rather than assume what `\Device\Afd`'s DACL is, this
    /// creates a REAL socket and reads the security descriptor off the resulting kernel handle — a
    /// socket handle IS a file handle on an AFD endpoint. That is the object whose DACL the
    /// restricting-sid second check would have to fail for egress to die while reads live.
    static byte[] SocketSd(out string note)
    {
        note = "";
        try
        {
            Socket s = new Socket(AddressFamily.InterNetwork, SocketType.Stream, ProtocolType.Tcp);
            try
            {
                uint need;
                byte[] probe = new byte[4];
                GetKernelObjectSecurity(s.Handle, 7, probe, 4, out need);
                if (need == 0) { note = "needed=0 err=" + Marshal.GetLastWin32Error(); return null; }
                byte[] sd = new byte[need];
                if (!GetKernelObjectSecurity(s.Handle, 7, sd, need, out need))
                { note = "GetKernelObjectSecurity err=" + Marshal.GetLastWin32Error(); return null; }
                return sd;
            }
            finally { s.Close(); }
        }
        catch (Exception e) { note = e.Message; return null; }
    }

    /// A named kernel device's descriptor, via NtOpenFile + NtQuerySecurityObject. The UNICODE_STRING
    /// buffer is allocated explicitly and kept alive across the call — letting the marshaller
    /// produce a temporary would leave OBJECT_ATTRIBUTES pointing at freed memory.
    static byte[] DeviceSd(string ntPath, out string note)
    {
        note = "";
        IntPtr strBuf = Marshal.StringToHGlobalUni(ntPath);
        UNICODE_STRING us = new UNICODE_STRING();
        us.Length = (ushort)(ntPath.Length * 2);
        us.MaximumLength = (ushort)(ntPath.Length * 2 + 2);
        us.Buffer = strBuf;
        IntPtr pus = Marshal.AllocHGlobal(Marshal.SizeOf(typeof(UNICODE_STRING)));
        Marshal.StructureToPtr(us, pus, false);
        OBJECT_ATTRIBUTES oa = new OBJECT_ATTRIBUTES();
        oa.Length = Marshal.SizeOf(typeof(OBJECT_ATTRIBUTES));
        oa.ObjectName = pus;
        oa.Attributes = 0x40;
        IntPtr h = IntPtr.Zero; IO_STATUS_BLOCK iosb;
        int st = NtOpenFile(out h, READ_CONTROL | SYNCHRONIZE, ref oa, out iosb, 7, 0);
        int st2 = 0;
        if (st != 0)
        {
            // Some devices refuse a READ_CONTROL-only open; retry with a use-shaped mask.
            st2 = NtOpenFile(out h, READ_CONTROL | SYNCHRONIZE | FILE_READ_DATA, ref oa, out iosb, 7, 0);
        }
        Marshal.FreeHGlobal(pus); Marshal.FreeHGlobal(strBuf);
        if (st != 0 && st2 != 0)
        { note = string.Format("NtOpenFile 0x{0:x8} / retry 0x{1:x8}", st, st2); return null; }
        try
        {
            uint need;
            byte[] probe = new byte[4];
            NtQuerySecurityObject(h, 7, probe, 4, out need);
            if (need == 0) { note = "needed=0"; return null; }
            byte[] sd = new byte[need];
            int q = NtQuerySecurityObject(h, 7, sd, need, out need);
            if (q != 0) { note = string.Format("NtQuerySecurityObject 0x{0:x8}", q); return null; }
            return sd;
        }
        finally { if (h != IntPtr.Zero) CloseHandle(h); }
    }

    static byte[] RegSd(string hive, out string note)
    {
        note = "";
        try
        {
            RegistryKey k = Registry.LocalMachine.OpenSubKey(hive,
                RegistryKeyPermissionCheck.ReadSubTree, RegistryRights.ReadPermissions);
            if (k == null) { note = "missing"; return null; }
            using (k)
            {
                return k.GetAccessControl(AccessControlSections.Access
                    | AccessControlSections.Owner | AccessControlSections.Group)
                    .GetSecurityDescriptorBinaryForm();
            }
        }
        catch (Exception e) { note = e.Message; return null; }
    }

    static string Sddl(byte[] sd)
    {
        if (sd == null) return "(none)";
        IntPtr str; uint len;
        if (!ConvertSecurityDescriptorToStringSecurityDescriptorW(sd, 1, 7, out str, out len))
            return "(unconvertible:" + Marshal.GetLastWin32Error() + ")";
        string r = Marshal.PtrToStringUni(str); LocalFree(str); return r;
    }

    // ─────────────────────────── entry ───────────────────────────

    static List<string> armNames = new List<string>();
    static List<IntPtr> armTokens = new List<IntPtr>();

    static void Add(string name, Func<IntPtr> mk)
    {
        try { IntPtr t = mk(); armTokens.Add(t); armNames.Add(name); Prop("token-built-" + name, true, ""); }
        catch (Exception e) { Prop("token-built-" + name, false, e.Message); }
    }

    public static int Run()
    {
        W("PROBE win-both-gates: can one unprivileged token read the disk AND be denied egress?");
        Fact("os", Environment.OSVersion.VersionString);
        try
        {
            string k = @"HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Windows NT\CurrentVersion";
            Fact("product", "" + Registry.GetValue(k, "ProductName", "?"));
            Fact("build", "" + Registry.GetValue(k, "CurrentBuildNumber", "?")
                + "." + Registry.GetValue(k, "UBR", "?"));
        }
        catch (Exception e) { Fact("product", "err " + e.Message); }
        Fact("arch", "" + Environment.GetEnvironmentVariable("PROCESSOR_ARCHITECTURE")
            + " 64bit-os=" + Environment.Is64BitOperatingSystem);
        Fact("runner-elevated", new WindowsPrincipal(WindowsIdentity.GetCurrent())
            .IsInRole(WindowsBuiltInRole.Administrator).ToString());
        Fact("user", WindowsIdentity.GetCurrent().Name);
        string meSid = WindowsIdentity.GetCurrent().User.Value;
        Fact("user-sid", meSid);

        string projRoot = Path.Combine(Path.GetTempPath(),
            "nub-bg-" + System.Diagnostics.Process.GetCurrentProcess().Id);
        Directory.CreateDirectory(projRoot);
        string projFile = Path.Combine(projRoot, "leaf.txt");
        File.WriteAllText(projFile, "x");
        string profile = Environment.GetEnvironmentVariable("USERPROFILE");
        if (string.IsNullOrEmpty(profile)) profile = @"C:\Users";

        // ═══ 1 — the DACL survey, and the capability harvest that drives §3 ═══
        W("");
        W("SECTION 1  DACL survey — every ace on the ancestor chain");
        string[] surveyPaths = new string[] {
            @"C:\", @"C:\Users", profile, @"C:\Windows", @"C:\Windows\System32",
            @"C:\Program Files", projRoot
        };
        List<string> harvested = new List<string>();
        Dictionary<string, string> harvestSource = new Dictionary<string, string>();
        for (int pi = 0; pi < surveyPaths.Length; pi++)
        {
            string p = surveyPaths[pi];
            W("  path " + p);
            try
            {
                AccessControlSections want = AccessControlSections.Access
                    | AccessControlSections.Owner | AccessControlSections.Group;
                FileSystemSecurity s = Directory.Exists(p)
                    ? (FileSystemSecurity)new DirectoryInfo(p).GetAccessControl(want)
                    : (FileSystemSecurity)new FileInfo(p).GetAccessControl(want);
                string own = s.GetOwner(typeof(SecurityIdentifier)).ToString();
                W("    owner=" + own + " (" + SidName(own) + ")");
                foreach (FileSystemAccessRule r in s.GetAccessRules(true, true, typeof(SecurityIdentifier)))
                {
                    string sid = r.IdentityReference.Value;
                    W(string.Format("    ace {0,-5} mask=0x{1:x8} inh={2,-22} {3} [{4}]",
                        r.AccessControlType, (uint)r.FileSystemRights, r.InheritanceFlags,
                        sid, SidName(sid)));
                    if (sid.StartsWith("S-1-15-") && !harvested.Contains(sid))
                    {
                        harvested.Add(sid);
                        harvestSource[sid] = p + " mask=0x" + ((uint)r.FileSystemRights).ToString("x8");
                    }
                }
            }
            catch (Exception e) { W("    ERR " + e.Message); }
        }
        W("  harvested app-package sids (S-1-15-*): " + harvested.Count);
        for (int i = 0; i < harvested.Count; i++)
            W("    " + harvested[i] + "   from " + harvestSource[harvested[i]]);
        Prop("harvest-found-appcontainer-aces", harvested.Count > 0,
            "the capability experiment is vacuous if the disk grants no S-1-15-* trustee");

        // ═══ 2 — what IS S-1-15-3-65536-…? Derive every name the machine knows and match. ═══
        W("");
        W("SECTION 2  capability identification — derive, do not guess");
        List<string> names = new List<string>();
        string[] regRoots = new string[] {
            @"SOFTWARE\Microsoft\SecurityManager\CapabilityClasses",
            @"SOFTWARE\Microsoft\SecurityManager\CapAuthz\ApplicationsEx",
            @"SOFTWARE\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\CapabilityMappings",
        };
        for (int ri = 0; ri < regRoots.Length; ri++)
        {
            string rr = regRoots[ri];
            try
            {
                using (RegistryKey k = Registry.LocalMachine.OpenSubKey(rr))
                {
                    if (k == null) { W("  reg MISSING " + rr); continue; }
                    W("  reg " + rr);
                    string[] vns = k.GetValueNames();
                    for (int vi = 0; vi < vns.Length; vi++)
                    {
                        object v = k.GetValue(vns[vi]);
                        string[] arr = v as string[];
                        if (arr != null)
                        {
                            W("    value " + vns[vi] + " = " + arr.Length + " entries");
                            for (int ai = 0; ai < arr.Length; ai++)
                                if (!names.Contains(arr[ai])) names.Add(arr[ai]);
                        }
                        else W("    value " + vns[vi] + " = " + v);
                    }
                    string[] sks = k.GetSubKeyNames();
                    W("    subkeys=" + sks.Length);
                    for (int si = 0; si < sks.Length; si++)
                        if (!names.Contains(sks[si])) names.Add(sks[si]);
                }
            }
            catch (Exception e) { W("  reg ERR " + rr + ": " + e.Message); }
        }
        // Documented Win32-App-Isolation ("AppSilo") capability names plus the classic well-knowns, so
        // identification does not depend on the registry list being complete.
        string[] extra = new string[] {
            "isolatedWin32-promptForAccess", "isolatedWin32-userProfileMinimal",
            "isolatedWin32-shellExecuteFile", "isolatedWin32-accessToPublisherDirectory",
            "isolatedWin32-shellExtensionContextMenu", "isolatedWin32-print",
            "isolatedWin32-printDialog", "isolatedWin32-clipboard", "isolatedWin32-dragDrop",
            "isolatedWin32-systemAppUserModelId", "isolatedWin32-userDataFolder",
            "isolatedWin32-appCapability", "isolatedWin32-installedLocation",
            "broadFileSystemAccess", "internetClient", "internetClientServer",
            "privateNetworkClientServer", "documentsLibrary", "picturesLibrary",
            "videosLibrary", "musicLibrary", "removableStorage", "registryRead",
            "lpacAppExperience", "lpacInstrumentation", "lpacCom", "lpacCryptoServices",
            "lpacIdentityServices", "lpacMedia", "lpacPnPNotifications", "lpacServicesManagement",
            "lpacSessionManagement", "lpacPrinting", "lpacClientServices", "lpacDeviceAccess",
            "lpacWebPlatform", "lpacPayments", "lpacEnterprisePolicyChangeNotifications",
        };
        for (int i = 0; i < extra.Length; i++) if (!names.Contains(extra[i])) names.Add(extra[i]);
        W("  candidate capability names: " + names.Count);

        Dictionary<string, string> sidToName = new Dictionary<string, string>();
        int derived = 0;
        for (int i = 0; i < names.Count; i++)
        {
            IntPtr gs, cs; uint gc, cc;
            try
            {
                if (!DeriveCapabilitySidsFromName(names[i], out gs, out gc, out cs, out cc)) continue;
                derived++;
                for (uint j = 0; j < cc; j++)
                {
                    string ss = SidStr(Marshal.ReadIntPtr(cs, (int)j * IntPtr.Size));
                    if (!sidToName.ContainsKey(ss)) sidToName[ss] = names[i] + " (capability)";
                }
                for (uint j = 0; j < gc; j++)
                {
                    string ss = SidStr(Marshal.ReadIntPtr(gs, (int)j * IntPtr.Size));
                    if (!sidToName.ContainsKey(ss)) sidToName[ss] = names[i] + " (group)";
                }
            }
            catch (Exception) { }
        }
        Fact("capability-names-derived", derived + "/" + names.Count);
        Fact("distinct-derived-sids", sidToName.Count.ToString());
        int matchedCount = 0;
        for (int i = 0; i < harvested.Count; i++)
        {
            string nm;
            if (sidToName.TryGetValue(harvested[i], out nm))
            { W("  IDENTIFIED   " + harvested[i] + " = " + nm); matchedCount++; }
            else W("  UNIDENTIFIED " + harvested[i]);
        }
        Fact("harvested-sids-identified", matchedCount + "/" + harvested.Count);
        foreach (KeyValuePair<string, string> kv in sidToName)
            if (kv.Key.StartsWith("S-1-15-3-65536-"))
                W("  appsilo-class derived: " + kv.Key + " = " + kv.Value);

        // ═══ 3 — THE experiment: does HOLDING those sids pass the second gate? ═══
        W("");
        W("SECTION 3  token arms");
        IntPtr package = PackageSid("nub-both-gates-probe");
        Fact("package-sid", SidStr(package));

        string[] wellKnownCaps = new string[] {
            "S-1-15-3-1", "S-1-15-3-2", "S-1-15-3-3", "S-1-15-3-4", "S-1-15-3-5", "S-1-15-3-6",
            "S-1-15-3-7", "S-1-15-3-8", "S-1-15-3-9", "S-1-15-3-10", "S-1-15-3-11", "S-1-15-3-12",
        };
        List<string> harvestedCaps = new List<string>();
        for (int i = 0; i < harvested.Count; i++)
            if (harvested[i].StartsWith("S-1-15-3-")) harvestedCaps.Add(harvested[i]);
        List<string> everything = new List<string>(harvestedCaps);
        for (int i = 0; i < wellKnownCaps.Length; i++)
            if (!everything.Contains(wellKnownCaps[i])) everything.Add(wellKnownCaps[i]);
        foreach (KeyValuePair<string, string> kv in sidToName)
            if (kv.Key.StartsWith("S-1-15-3-") && !everything.Contains(kv.Key)) everything.Add(kv.Key);
        Fact("harvested-capability-count", harvestedCaps.Count.ToString());
        Fact("all-capability-count", everything.Count.ToString());

        IntPtr pkg = package;
        string[] hc = harvestedCaps.ToArray();
        string[] all = everything.ToArray();

        Add("A0-baseline-own-token", delegate { return ForCheck(OwnToken()); });
        Add("A1-restricted-medium-il", delegate { return RestrictedToken("S-1-16-8192", null); });
        Add("A2-restricted-low-il", delegate { return RestrictedToken("S-1-16-4096", null); });
        Add("A3-lowbox-zero-caps-GATECTL", delegate {
            IntPtr b = OwnToken(); try { return LowBoxToken(b, pkg, null); } finally { CloseHandle(b); } });
        Add("A4-lowbox-HARVESTED-caps", delegate {
            IntPtr b = OwnToken(); try { return LowBoxToken(b, pkg, hc); } finally { CloseHandle(b); } });
        Add("A5-lowbox-ALL-derived-caps", delegate {
            IntPtr b = OwnToken(); try { return LowBoxToken(b, pkg, all); } finally { CloseHandle(b); } });
        Add("A6-lowbox-wellknown-caps", delegate {
            IntPtr b = OwnToken(); try { return LowBoxToken(b, pkg, wellKnownCaps); } finally { CloseHandle(b); } });
        Add("A7-lowbox-AAP-as-cap", delegate {
            IntPtr b = OwnToken();
            try { return LowBoxToken(b, pkg, new string[] { "S-1-15-2-1" }); } finally { CloseHandle(b); } });
        Add("A8-lowbox-usersid-as-cap", delegate {
            IntPtr b = OwnToken();
            try { return LowBoxToken(b, pkg, new string[] { meSid }); } finally { CloseHandle(b); } });
        Add("A9-lowbox-builtinusers-as-cap", delegate {
            IntPtr b = OwnToken();
            try { return LowBoxToken(b, pkg, new string[] { "S-1-5-32-545" }); } finally { CloseHandle(b); } });
        Add("A10-lowbox-on-restricted-low-HARVESTED", delegate {
            IntPtr b = RestrictedToken("S-1-16-4096", null);
            try { return LowBoxToken(b, pkg, hc); } finally { CloseHandle(b); } });
        Add("A11-lowbox-on-restricted-low-ALL", delegate {
            IntPtr b = RestrictedToken("S-1-16-4096", null);
            try { return LowBoxToken(b, pkg, all); } finally { CloseHandle(b); } });

        // The other direction: restricting sids, which add a SECOND check of the same shape as the
        // LowBox gate but against sids that already appear in real DACLs. If a socket's descriptor
        // names a trustee the set omits while `C:\`'s names one it includes, egress dies and reads
        // live. Chromium's own tiers are included as reference points.
        Add("R1-restrict-builtin-users", delegate {
            return RestrictedToken("S-1-16-4096", new string[] { "S-1-5-32-545" }); });
        Add("R2-restrict-users-plus-self", delegate {
            return RestrictedToken("S-1-16-4096", new string[] { "S-1-5-32-545", meSid }); });
        Add("R3-restrict-world", delegate {
            return RestrictedToken("S-1-16-4096", new string[] { "S-1-1-0" }); });
        Add("R4-restrict-chromium-USER_LIMITED", delegate {
            return RestrictedToken("S-1-16-4096", new string[] { "S-1-5-32-545", "S-1-1-0", "S-1-5-12" }); });
        Add("R5-restrict-null-LOCKDOWN", delegate {
            return RestrictedToken("S-1-16-4096", new string[] { "S-1-0-0" }); });
        Add("R6-restrict-self-only", delegate {
            return RestrictedToken("S-1-16-4096", new string[] { meSid }); });
        Add("R7-restrict-authusers", delegate {
            return RestrictedToken("S-1-16-4096", new string[] { "S-1-5-11" }); });
        Add("R8-restrict-users-authusers-self", delegate {
            return RestrictedToken("S-1-16-4096", new string[] { "S-1-5-32-545", "S-1-5-11", meSid }); });

        // ═══ 4 — the objects, including what a socket actually opens ═══
        W("");
        W("SECTION 4  objects under test");
        List<string> objNames = new List<string>();
        List<byte[]> objSds = new List<byte[]>();
        string[] fsObjs = new string[] { @"C:\", @"C:\Users", profile,
            @"C:\Windows\System32", projRoot, projFile };
        for (int i = 0; i < fsObjs.Length; i++)
        {
            byte[] sd = null; string err = "";
            try { sd = FileSd(fsObjs[i]); } catch (Exception e) { err = e.Message; }
            objNames.Add("fs:" + fsObjs[i]); objSds.Add(sd);
            W("  obj fs:" + fsObjs[i] + (sd == null ? "  UNREADABLE " + err : "  " + Sddl(sd)));
        }
        string sockNote; byte[] sockSd = SocketSd(out sockNote);
        objNames.Add("socket:live-afd-endpoint"); objSds.Add(sockSd);
        W("  obj socket:live-afd-endpoint" + (sockSd == null ? "  UNREADABLE " + sockNote : "  " + Sddl(sockSd)));
        Prop("socket-descriptor-readable", sockSd != null,
            "the AFD hypothesis cannot be tested against a descriptor we cannot read");
        string[] devs = new string[] { @"\Device\Afd", @"\Device\Afd\Endpoint", @"\Device\Tcp",
            @"\Device\Tcp6", @"\Device\Udp", @"\Device\Nsi", @"\Device\Http", @"\Device\RawIp" };
        for (int i = 0; i < devs.Length; i++)
        {
            string n; byte[] d = DeviceSd(devs[i], out n);
            objNames.Add("dev:" + devs[i]); objSds.Add(d);
            W("  obj dev:" + devs[i] + (d == null ? "  UNREADABLE " + n : "  " + Sddl(d)));
        }
        string[] regs = new string[] {
            @"SYSTEM\CurrentControlSet\Services\WinSock2\Parameters",
            @"SYSTEM\CurrentControlSet\Services\WinSock2\Parameters\Protocol_Catalog9",
            @"SYSTEM\CurrentControlSet\Services\WinSock2\Parameters\NameSpace_Catalog5",
            @"SYSTEM\CurrentControlSet\Services\Tcpip\Parameters" };
        for (int i = 0; i < regs.Length; i++)
        {
            string n; byte[] r = RegSd(regs[i], out n);
            objNames.Add("reg:HKLM\\" + regs[i]); objSds.Add(r);
            W("  obj reg:HKLM\\" + regs[i] + (r == null ? "  UNREADABLE " + n : "  " + Sddl(r)));
        }

        // ═══ 5 — the matrix ═══
        W("");
        W("SECTION 5  the matrix");
        for (int a = 0; a < armNames.Count; a++)
        {
            W("  arm " + armNames[a] + "   " + TokenShape(armTokens[a]));
            for (int o = 0; o < objNames.Count; o++)
            {
                if (objSds[o] == null) continue;
                bool isFs = objNames[o].StartsWith("fs:");
                uint rdMask = isFs ? READ_SET : (FILE_READ_DATA | FILE_WRITE_DATA);
                string rd = Check(armTokens[a], objSds[o], rdMask);
                string wr = isFs ? Check(armTokens[a], objSds[o], WRITE_SET) : "-";
                W(string.Format("    {0,-52} use={1,-22} write={2}", objNames[o], rd, wr));
            }
        }

        // ═══ 6 — the controls that make the matrix interpretable ═══
        W("");
        W("SECTION 6  controls");
        int iBase = armNames.IndexOf("A0-baseline-own-token");
        int iGate = armNames.IndexOf("A3-lowbox-zero-caps-GATECTL");
        int iCRoot = objNames.IndexOf(@"fs:C:\");
        int iCUsers = objNames.IndexOf(@"fs:C:\Users");
        int iProj = objNames.IndexOf("fs:" + projRoot);
        int iSys32 = objNames.IndexOf(@"fs:C:\Windows\System32");
        if (iBase >= 0)
        {
            int[] must = new int[] { iCRoot, iCUsers, iProj };
            for (int i = 0; i < must.Length; i++)
                Prop("baseline-reads-" + objNames[must[i]],
                    must[i] >= 0 && Check(armTokens[iBase], objSds[must[i]], READ_SET) == "GRANTED",
                    "an unrestricted token must read this, or AccessCheck is being misused here");
        }
        else Prop("baseline-arm-exists", false, "no baseline; the whole table is unattributable");
        if (iGate >= 0 && iCRoot >= 0)
        {
            Prop("lowbox-gate-is-modelled",
                Check(armTokens[iGate], objSds[iCRoot], READ_SET).StartsWith("DENIED"),
                "a zero-capability LowBox token must be DENIED on C:\\, or AccessCheck is not " +
                "applying the AppContainer gate and every composed row means nothing");
            Prop("lowbox-second-gate-CAN-pass",
                iSys32 >= 0 && Check(armTokens[iGate], objSds[iSys32], READ_SET) == "GRANTED",
                "System32 carries an ALL APPLICATION PACKAGES ace, so a LowBox token must be " +
                "GRANTED there — without this, a table of DENIEDs cannot be told from a gate that " +
                "denies unconditionally");
        }
        else Prop("gate-control-exists", false, "no gate control");

        // ═══ 7 — what AccessCheck cannot model ═══
        W("");
        W("SECTION 7  privileges, and the limit of this method");
        for (int a = 0; a < armNames.Count; a++)
        {
            string pv = Privileges(armTokens[a]);
            Fact("privs " + armNames[a], pv.Length == 0 ? "(none)" : pv);
        }
        W("  NOTE bypass-traverse (SeChangeNotifyPrivilege) makes the object manager SKIP the");
        W("       traverse check on every INTERMEDIATE path component and check only the leaf. Where");
        W("       it is held+enabled, a DENIED row on C:\\ or C:\\Users does NOT imply a deep open");
        W("       fails — only that lstat/readdir/chdir ON THOSE TWO PATHS fails. AccessCheck");
        W("       evaluates one descriptor and cannot model this; only a real launch settles it.");

        for (int i = 0; i < armTokens.Count; i++) CloseHandle(armTokens[i]);
        FreeSid(package);
        try { Directory.Delete(projRoot, true); } catch (Exception) { }
        W("");
        W("PROBE end fails=" + fails);
        return fails > 0 ? 1 : 0;
    }
}
'@

try {
  Add-Type -TypeDefinition $src -Language CSharp -ReferencedAssemblies 'System.dll','System.Core.dll' -ErrorAction Stop
} catch {
  Write-Host "ADD-TYPE FAILED: $($_.Exception.Message)"
  if ($_.Exception.InnerException) { Write-Host $_.Exception.InnerException.Message }
  $_.Exception.Data.Keys | ForEach-Object { Write-Host "$_ = $($_.Exception.Data[$_])" }
  exit 2
}
exit [Probe]::Run()
