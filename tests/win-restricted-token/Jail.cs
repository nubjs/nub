// Throwaway probe for the unprivileged Windows build-jail mechanism:
// restricted token + LOW integrity level, with the write-allowlist expressed
// as a LOW mandatory label on the objects we want writable.
//
// PROVENANCE: token shape copied from .repos/srt/vendor/srt-win-src/src/token.rs
// (deny-only Admins + logon SIDs, LUA_TOKEN, delete all privileges but
// SeChangeNotify, no RestrictingSids). The one deliberate divergence is the
// integrity level: srt runs the child at MEDIUM on purpose; we run LOW, because
// Medium is not a jail (a standard user can already write their whole profile).
//
// Build:  csc.exe /nologo /unsafe- /optimize+ /out:Jail.exe Jail.cs
using System;
using System.Collections.Generic;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;

static class Jail
{
    // ---------------- interop ----------------
    const uint TOKEN_ALL_ACCESS = 0xF01FF;
    const uint LUA_TOKEN = 0x4;
    const uint SE_GROUP_LOGON_ID = 0xC0000000;
    const uint SE_GROUP_INTEGRITY = 0x20;

    const int TokenUser = 1, TokenGroups = 2, TokenPrivileges = 3,
              TokenIntegrityLevel = 25, TokenElevation = 20, TokenDefaultDacl = 6;

    const uint IL_UNTRUSTED = 0x0000, IL_LOW = 0x1000, IL_MEDIUM = 0x2000;

    const int SecurityImpersonation = 2, TokenPrimary = 1;

    const uint CREATE_SUSPENDED = 0x4, CREATE_UNICODE_ENVIRONMENT = 0x400,
               CREATE_NO_WINDOW = 0x08000000, CREATE_NEW_CONSOLE = 0x10;
    const uint STARTF_USESTDHANDLES = 0x100;

    const uint LABEL_SECURITY_INFORMATION = 0x00000010;
    const int SE_FILE_OBJECT = 1;

    const uint GENERIC_WRITE = 0x40000000, GENERIC_READ = 0x80000000;
    const uint FILE_SHARE_RW = 0x3;
    const uint CREATE_ALWAYS = 2, OPEN_EXISTING = 3;

    [StructLayout(LayoutKind.Sequential)] struct SID_AND_ATTRIBUTES { public IntPtr Sid; public uint Attributes; }
    [StructLayout(LayoutKind.Sequential)] struct LUID { public uint Low; public int High; }
    [StructLayout(LayoutKind.Sequential)] struct LUID_AND_ATTRIBUTES { public LUID Luid; public uint Attributes; }
    [StructLayout(LayoutKind.Sequential)] struct TOKEN_MANDATORY_LABEL { public SID_AND_ATTRIBUTES Label; }

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    struct STARTUPINFO
    {
        public int cb; public string lpReserved, lpDesktop, lpTitle;
        public int dwX, dwY, dwXSize, dwYSize, dwXCountChars, dwYCountChars, dwFillAttribute;
        public uint dwFlags; public short wShowWindow, cbReserved2;
        public IntPtr lpReserved2, hStdInput, hStdOutput, hStdError;
    }
    [StructLayout(LayoutKind.Sequential)] struct PROCESS_INFORMATION { public IntPtr hProcess, hThread; public int pid, tid; }
    [StructLayout(LayoutKind.Sequential)] struct SECURITY_ATTRIBUTES { public int nLength; public IntPtr lpSD; public int bInherit; }

    [DllImport("advapi32", SetLastError = true)] static extern bool OpenProcessToken(IntPtr p, uint acc, out IntPtr t);
    [DllImport("kernel32")] static extern IntPtr GetCurrentProcess();
    [DllImport("kernel32", SetLastError = true)] static extern IntPtr GetStdHandle(int n);
    [DllImport("kernel32", SetLastError = true)] static extern bool CloseHandle(IntPtr h);
    // CharSet.Unicode is load-bearing: without it the ANSI default marshals the
    // name as bytes into a UTF-16 API, which silently CREATES a garbage-named
    // output file and then fails to open "NUL" with ERROR_FILE_NOT_FOUND.
    [DllImport("kernel32", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern IntPtr CreateFileW(string name, uint acc, uint share, ref SECURITY_ATTRIBUTES sa,
        uint disp, uint flags, IntPtr tmpl);
    [DllImport("kernel32", SetLastError = true)] static extern uint WaitForSingleObject(IntPtr h, uint ms);
    [DllImport("kernel32", SetLastError = true)] static extern bool GetExitCodeProcess(IntPtr h, out uint code);
    [DllImport("kernel32", SetLastError = true)] static extern uint ResumeThread(IntPtr h);

    [DllImport("advapi32", SetLastError = true)]
    static extern bool GetTokenInformation(IntPtr t, int cls, IntPtr buf, uint len, out uint ret);
    [DllImport("advapi32", SetLastError = true)]
    static extern bool SetTokenInformation(IntPtr t, int cls, IntPtr buf, uint len);
    [DllImport("advapi32", SetLastError = true)]
    static extern bool CreateRestrictedToken(IntPtr baseTok, uint flags,
        uint nDisable, [In] SID_AND_ATTRIBUTES[] disable,
        uint nDelPriv, [In] LUID_AND_ATTRIBUTES[] delPriv,
        uint nRestrict, [In] SID_AND_ATTRIBUTES[] restrict_, out IntPtr newTok);
    [DllImport("advapi32", SetLastError = true)]
    static extern bool DuplicateTokenEx(IntPtr t, uint acc, IntPtr sa, int lvl, int type, out IntPtr outTok);
    [DllImport("advapi32", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern bool ConvertStringSidToSidW(string s, out IntPtr sid);
    [DllImport("advapi32", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern bool ConvertSidToStringSidW(IntPtr sid, out IntPtr s);
    [DllImport("advapi32", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern bool LookupPrivilegeValueW(string sys, string name, out LUID luid);
    [DllImport("advapi32", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern bool LookupPrivilegeNameW(string sys, ref LUID luid, StringBuilder name, ref int cch);
    [DllImport("advapi32")] static extern uint GetLengthSid(IntPtr sid);
    [DllImport("advapi32", SetLastError = true)] static extern IntPtr FreeSid(IntPtr sid);
    [DllImport("advapi32", SetLastError = true)]
    static extern bool AllocateAndInitializeSid(byte[] auth, byte cnt, uint r1, uint r2, uint r3,
        uint r4, uint r5, uint r6, uint r7, uint r8, out IntPtr sid);

    [DllImport("advapi32", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern bool ConvertStringSecurityDescriptorToSecurityDescriptorW(
        string sddl, uint rev, out IntPtr psd, out uint size);
    [DllImport("advapi32", SetLastError = true)]
    static extern bool GetSecurityDescriptorSacl(IntPtr psd, out bool present, out IntPtr sacl, out bool defaulted);
    [DllImport("advapi32", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern uint SetNamedSecurityInfoW(string obj, int type, uint secInfo,
        IntPtr owner, IntPtr group, IntPtr dacl, IntPtr sacl);
    [DllImport("advapi32", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern uint GetNamedSecurityInfoW(string obj, int type, uint secInfo,
        out IntPtr owner, out IntPtr group, out IntPtr dacl, out IntPtr sacl, out IntPtr psd);
    [DllImport("kernel32")] static extern IntPtr LocalFree(IntPtr p);

    [DllImport("advapi32", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern bool CreateProcessAsUserW(IntPtr tok, string app, string cmd,
        IntPtr pa, IntPtr ta, bool inherit, uint flags, IntPtr env, string cwd,
        ref STARTUPINFO si, out PROCESS_INFORMATION pi);
    [DllImport("advapi32", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern bool CreateProcessWithTokenW(IntPtr tok, uint logonFlags, string app, string cmd,
        uint flags, IntPtr env, string cwd, ref STARTUPINFO si, out PROCESS_INFORMATION pi);

    [DllImport("user32", SetLastError = true)] static extern IntPtr GetProcessWindowStation();
    [DllImport("user32", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern bool GetUserObjectInformationW(IntPtr h, int idx, IntPtr buf, uint len, out uint need);
    [DllImport("user32", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern IntPtr CreateDesktopW(string desk, string dev, IntPtr devmode, uint flags, uint acc, IntPtr sa);
    [DllImport("user32", SetLastError = true)]
    static extern bool SetUserObjectSecurity(IntPtr h, ref uint si, IntPtr psd);

    // ---- unique-restricting-sid additions ----
    const uint OWNER_SI = 0x1, GROUP_SI = 0x2, DACL_SI = 0x4;

    [StructLayout(LayoutKind.Sequential)] struct GENERIC_MAPPING
    { public uint GenericRead, GenericWrite, GenericExecute, GenericAll; }

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    struct TRUSTEE_W
    {
        public IntPtr pMultipleTrustee; public int MultipleTrusteeOperation;
        public int TrusteeForm; public int TrusteeType; public IntPtr ptstrName;
    }
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    struct EXPLICIT_ACCESS_W
    {
        public uint grfAccessPermissions; public int grfAccessMode; public uint grfInheritance;
        public TRUSTEE_W Trustee;
    }

    [DllImport("advapi32", SetLastError = true)]
    static extern bool AccessCheck(IntPtr psd, IntPtr tok, uint desired, ref GENERIC_MAPPING map,
        IntPtr privSet, ref uint privSetLen, out uint granted, out int status);
    [DllImport("advapi32")] static extern void MapGenericMask(ref uint access, ref GENERIC_MAPPING map);
    [DllImport("advapi32", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern uint SetEntriesInAclW(uint cEntries, [In] EXPLICIT_ACCESS_W[] entries, IntPtr oldAcl, out IntPtr newAcl);
    [DllImport("advapi32", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern bool ConvertSecurityDescriptorToStringSecurityDescriptorW(
        IntPtr psd, uint rev, uint secInfo, out IntPtr sddl, out uint len);
    [DllImport("advapi32", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern bool LookupAccountSidW(string sys, IntPtr sid, StringBuilder name, ref uint cchName,
        StringBuilder dom, ref uint cchDom, out int use);
    // Chromium's two-token startup (broker_services.cc:296-305): the target is
    // created SUSPENDED with the lockdown token as primary, and its main thread
    // impersonates a permissive token so loader/user32 init can complete --
    // "otherwise it will crash too early for us to help". Chromium then relies
    // on the target calling TargetServices::LowerToken() itself. node.exe will
    // never call that, so the only non-cooperative equivalent is for the PARENT
    // to strip the impersonation with SetThreadToken(thread, NULL) once startup
    // is past. These two imports are what test whether that is even possible
    // unprivileged.
    [DllImport("advapi32", SetLastError = true)]
    static extern bool SetThreadToken(ref IntPtr thread, IntPtr token);
    [DllImport("kernel32", SetLastError = true)]
    static extern bool GetThreadTimes(IntPtr h, out long create, out long exit, out long kernel, out long user);

    static int Err() { return Marshal.GetLastWin32Error(); }
    static void P(string s) { Console.Out.WriteLine(s); Console.Out.Flush(); }

    // ---------------- token construction ----------------

    static IntPtr SelfToken()
    {
        IntPtr t;
        if (!OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, out t))
            throw new Exception("OpenProcessToken err=" + Err());
        return t;
    }

    // INVARIANT: token info structs (TOKEN_USER, TOKEN_GROUPS, TOKEN_MANDATORY_LABEL)
    // embed pointers INTO their own buffer, so the buffer must outlive every read of
    // those SIDs. Returning a managed byte[] copy leaves the embedded pointers
    // dangling at the freed original. Hand back the live unmanaged buffer and leak
    // it — this is a short-lived probe process, and a stale SID read here would
    // silently corrupt the very table the run exists to produce.
    static IntPtr GetInfoPtr(IntPtr tok, int cls)
    {
        uint need;
        GetTokenInformation(tok, cls, IntPtr.Zero, 0, out need);
        if (need == 0) throw new Exception("GetTokenInformation(" + cls + ") sizing err=" + Err());
        IntPtr b = Marshal.AllocHGlobal((int)need);
        if (!GetTokenInformation(tok, cls, b, need, out need))
            throw new Exception("GetTokenInformation(" + cls + ") err=" + Err());
        return b;
    }

    // Only safe for pointer-free classes (TokenPrivileges, TokenElevation).
    static byte[] GetInfo(IntPtr tok, int cls)
    {
        uint need;
        GetTokenInformation(tok, cls, IntPtr.Zero, 0, out need);
        if (need == 0) throw new Exception("GetTokenInformation(" + cls + ") sizing err=" + Err());
        IntPtr b = Marshal.AllocHGlobal((int)need);
        try
        {
            if (!GetTokenInformation(tok, cls, b, need, out need))
                throw new Exception("GetTokenInformation(" + cls + ") err=" + Err());
            byte[] o = new byte[need]; Marshal.Copy(b, o, 0, (int)need); return o;
        }
        finally { Marshal.FreeHGlobal(b); }
    }

    static string SidStr(IntPtr sid)
    {
        IntPtr s;
        if (!ConvertSidToStringSidW(sid, out s)) return "(unconvertible err=" + Err() + ")";
        string r = Marshal.PtrToStringUni(s); LocalFree(s); return r;
    }

    static List<KeyValuePair<string, uint>> Groups(IntPtr tok)
    {
        IntPtr b = GetInfoPtr(tok, TokenGroups);
        int n = Marshal.ReadInt32(b);
        var o = new List<KeyValuePair<string, uint>>();
        int stride = Marshal.SizeOf(typeof(SID_AND_ATTRIBUTES));
        // Groups[] follows a DWORD count at pointer alignment (offset 8 on x64).
        long baseOff = IntPtr.Size;
        for (int i = 0; i < n; i++)
        {
            IntPtr e = new IntPtr(b.ToInt64() + baseOff + (long)i * stride);
            var sa = (SID_AND_ATTRIBUTES)Marshal.PtrToStructure(e, typeof(SID_AND_ATTRIBUTES));
            o.Add(new KeyValuePair<string, uint>(SidStr(sa.Sid), sa.Attributes));
        }
        return o;
    }

    static SID_AND_ATTRIBUTES[] DisableSids(IntPtr baseTok, bool keepLogonSid)
    {
        var l = new List<SID_AND_ATTRIBUTES>();
        IntPtr admins; ConvertStringSidToSidW("S-1-5-32-544", out admins);
        l.Add(new SID_AND_ATTRIBUTES { Sid = admins, Attributes = 0 });
        if (!keepLogonSid)
            foreach (var g in Groups(baseTok))
                if ((g.Value & SE_GROUP_LOGON_ID) == SE_GROUP_LOGON_ID)
                {
                    IntPtr s; ConvertStringSidToSidW(g.Key, out s);
                    l.Add(new SID_AND_ATTRIBUTES { Sid = s, Attributes = 0 });
                }
        return l.ToArray();
    }

    static LUID_AND_ATTRIBUTES[] PrivsExcept(IntPtr tok, string keep)
    {
        LUID k; LookupPrivilegeValueW(null, keep, out k);
        byte[] raw = GetInfo(tok, TokenPrivileges);
        int n = BitConverter.ToInt32(raw, 0);
        var o = new List<LUID_AND_ATTRIBUTES>();
        for (int i = 0; i < n; i++)
        {
            int off = 4 + i * 12;
            var la = new LUID_AND_ATTRIBUTES
            {
                Luid = new LUID { Low = BitConverter.ToUInt32(raw, off), High = BitConverter.ToInt32(raw, off + 4) },
                Attributes = 0
            };
            if (la.Luid.Low == k.Low && la.Luid.High == k.High) continue;
            o.Add(la);
        }
        return o.ToArray();
    }

    static void SetIl(IntPtr tok, uint rid)
    {
        IntPtr sid;
        if (!AllocateAndInitializeSid(new byte[] { 0, 0, 0, 0, 0, 16 }, 1, rid, 0, 0, 0, 0, 0, 0, 0, out sid))
            throw new Exception("AllocateAndInitializeSid err=" + Err());
        var tml = new TOKEN_MANDATORY_LABEL { Label = new SID_AND_ATTRIBUTES { Sid = sid, Attributes = SE_GROUP_INTEGRITY } };
        int sz = Marshal.SizeOf(typeof(TOKEN_MANDATORY_LABEL));
        IntPtr b = Marshal.AllocHGlobal(sz);
        Marshal.StructureToPtr(tml, b, false);
        bool ok = SetTokenInformation(tok, TokenIntegrityLevel, b, (uint)(sz + GetLengthSid(sid)));
        int e = Err();
        Marshal.FreeHGlobal(b); FreeSid(sid);
        if (!ok) throw new Exception("SetTokenInformation(IntegrityLevel," + rid.ToString("x") + ") err=" + e);
    }

    // Parse a comma-separated restricting-SID spec. Tokens are SDDL sid strings
    // ("S-1-5-32-545") or the aliases below; "unique" resolves to the caller's
    // per-run unique sid (see UniqueSid), which is the whole point of this probe.
    static SID_AND_ATTRIBUTES[] ParseSidList(string spec, IntPtr baseTok)
    {
        var r = new List<SID_AND_ATTRIBUTES>();
        foreach (string raw in spec.Split(','))
        {
            string s = raw.Trim();
            if (s.Length == 0) continue;
            IntPtr p;
            if (s == "unique") p = UniqueSid();
            else if (s == "self") p = Marshal.ReadIntPtr(GetInfoPtr(baseTok, TokenUser));
            else if (s == "users") { ConvertStringSidToSidW("S-1-5-32-545", out p); }
            else if (s == "world") { ConvertStringSidToSidW("S-1-1-0", out p); }
            else if (s == "restricted") { ConvertStringSidToSidW("S-1-5-12", out p); }
            else if (s == "logon")
            {
                p = IntPtr.Zero;
                foreach (var g in Groups(baseTok))
                    if ((g.Value & SE_GROUP_LOGON_ID) == SE_GROUP_LOGON_ID) ConvertStringSidToSidW(g.Key, out p);
                if (p == IntPtr.Zero) throw new Exception("no logon sid on base token");
            }
            else if (!ConvertStringSidToSidW(s, out p)) throw new Exception("bad sid '" + s + "' err=" + Err());
            r.Add(new SID_AND_ATTRIBUTES { Sid = p, Attributes = 0 });
        }
        return r.ToArray();
    }

    // The unique restricting sid. NULL authority (S-1-0-*) with four random
    // subauthorities, matching Chromium's Sid::GenerateRandomSid — a binary
    // structure that resolves to no account and appears in no DACL on the disk.
    // Cached per process so `--restrict unique` and `grant` agree, and settable
    // from the environment so a launcher can pass the SAME sid it ACE'd.
    static IntPtr uniqueSidCache_ = IntPtr.Zero;
    static string uniqueSidStr_ = null;
    static IntPtr UniqueSid()
    {
        if (uniqueSidCache_ != IntPtr.Zero) return uniqueSidCache_;
        string fromEnv = Environment.GetEnvironmentVariable("NUB_JAIL_UNIQUE_SID");
        IntPtr sid;
        if (!string.IsNullOrEmpty(fromEnv))
        {
            if (!ConvertStringSidToSidW(fromEnv, out sid))
                throw new Exception("NUB_JAIL_UNIQUE_SID unparseable err=" + Err());
        }
        else
        {
            var rnd = new byte[16];
            new System.Security.Cryptography.RNGCryptoServiceProvider().GetBytes(rnd);
            uint a = BitConverter.ToUInt32(rnd, 0), b = BitConverter.ToUInt32(rnd, 4),
                 c = BitConverter.ToUInt32(rnd, 8), d = BitConverter.ToUInt32(rnd, 12);
            // SECURITY_NULL_SID_AUTHORITY = {0,0,0,0,0,0}
            if (!AllocateAndInitializeSid(new byte[] { 0, 0, 0, 0, 0, 0 }, 4, a, b, c, d, 0, 0, 0, 0, out sid))
                throw new Exception("AllocateAndInitializeSid(unique) err=" + Err());
        }
        uniqueSidCache_ = sid; uniqueSidStr_ = SidStr(sid);
        return sid;
    }

    // Chromium pairs every unique restricting sid with a default-dacl grant for
    // it (restricted_token_utils.cc:49-56) so the process can still reach the
    // objects it creates itself — without it the second check fails on the
    // child's own handles/heaps and nothing works.
    static void AddUniqueToDefaultDacl(IntPtr tok, IntPtr uniq)
    {
        IntPtr buf = GetInfoPtr(tok, TokenDefaultDacl);   // TOKEN_DEFAULT_DACL { PACL }
        IntPtr oldAcl = Marshal.ReadIntPtr(buf);
        var ea = new EXPLICIT_ACCESS_W[1];
        ea[0].grfAccessPermissions = 0x10000000 /*GENERIC_ALL*/;
        ea[0].grfAccessMode = 1 /*GRANT_ACCESS*/;
        ea[0].grfInheritance = 0;
        ea[0].Trustee.TrusteeForm = 0 /*TRUSTEE_IS_SID*/;
        ea[0].Trustee.TrusteeType = 0 /*TRUSTEE_IS_UNKNOWN*/;
        ea[0].Trustee.ptstrName = uniq;
        IntPtr newAcl;
        uint rc = SetEntriesInAclW(1, ea, oldAcl, out newAcl);
        if (rc != 0) { P("default_dacl SetEntriesInAcl rc=" + rc); return; }
        IntPtr nb = Marshal.AllocHGlobal(IntPtr.Size);
        Marshal.WriteIntPtr(nb, newAcl);
        bool ok = SetTokenInformation(tok, TokenDefaultDacl, nb, (uint)IntPtr.Size);
        P("default_dacl_grant_unique=" + (ok ? "OK" : "ERR=" + Err()));
        Marshal.FreeHGlobal(nb);
    }

    // mode: none | medium | low | untrusted
    static IntPtr BuildToken(string mode, bool keepLogonSid, bool restrictSids)
    { return BuildToken(mode, keepLogonSid, restrictSids, null); }

    static IntPtr BuildToken(string mode, bool keepLogonSid, bool restrictSids, string restrictSpec)
    {
        IntPtr baseTok = SelfToken();
        if (mode == "none")
        {
            IntPtr dup;
            if (!DuplicateTokenEx(baseTok, TOKEN_ALL_ACCESS, IntPtr.Zero, SecurityImpersonation, TokenPrimary, out dup))
                throw new Exception("DuplicateTokenEx(baseline) err=" + Err());
            return dup;
        }
        var disable = DisableSids(baseTok, keepLogonSid);
        var delPriv = PrivsExcept(baseTok, "SeChangeNotifyPrivilege");
        SID_AND_ATTRIBUTES[] restrict_ = null; uint nRestrict = 0;
        if (restrictSpec != null)
        {
            restrict_ = ParseSidList(restrictSpec, baseTok); nRestrict = (uint)restrict_.Length;
            var names = new List<string>();
            foreach (var sa in restrict_) names.Add(SidStr(sa.Sid));
            P("restricting_sids(" + nRestrict + ")=" + string.Join(" ", names.ToArray()));
        }
        else if (restrictSids)
        {
            // Chromium-style USER_LIMITED restricting set: SIDs already present
            // in most DACLs so reads survive the second access-check pass.
            var r = new List<SID_AND_ATTRIBUTES>();
            foreach (string s in new[] { "S-1-5-32-545" /*Users*/, "S-1-1-0" /*Everyone*/, "S-1-5-12" /*RESTRICTED*/ })
            { IntPtr p; ConvertStringSidToSidW(s, out p); r.Add(new SID_AND_ATTRIBUTES { Sid = p, Attributes = 0 }); }
            restrict_ = r.ToArray(); nRestrict = (uint)r.Count;
        }
        IntPtr tok;
        if (!CreateRestrictedToken(baseTok, LUA_TOKEN,
                (uint)disable.Length, disable,
                (uint)delPriv.Length, delPriv.Length == 0 ? null : delPriv,
                nRestrict, restrict_, out tok))
            throw new Exception("CreateRestrictedToken err=" + Err());

        if (restrictSpec != null && restrictSpec.Contains("unique"))
            AddUniqueToDefaultDacl(tok, UniqueSid());

        uint rid = mode == "low" ? IL_LOW : mode == "untrusted" ? IL_UNTRUSTED : IL_MEDIUM;
        SetIl(tok, rid);

        IntPtr prim;
        if (!DuplicateTokenEx(tok, TOKEN_ALL_ACCESS, IntPtr.Zero, SecurityImpersonation, TokenPrimary, out prim))
            throw new Exception("DuplicateTokenEx(primary) err=" + Err());
        CloseHandle(tok);
        return prim;
    }

    // ---------------- label (question 1) ----------------

    static int LabelOne(string path, string sddl)
    {
        IntPtr psd; uint sz;
        if (!ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl, 1, out psd, out sz))
            return -Err();
        bool present, def; IntPtr sacl;
        if (!GetSecurityDescriptorSacl(psd, out present, out sacl, out def)) { LocalFree(psd); return -Err(); }
        uint rc = SetNamedSecurityInfoW(path, SE_FILE_OBJECT, LABEL_SECURITY_INFORMATION,
            IntPtr.Zero, IntPtr.Zero, IntPtr.Zero, sacl);
        LocalFree(psd);
        return (int)rc;
    }

    static void CmdLabel(string[] a)
    {
        string path = a[1];
        string level = a.Length > 2 ? a[2] : "low";
        bool recurse = Array.IndexOf(a, "-r") >= 0;
        string dirSddl, fileSddl;
        if (level == "remove") { dirSddl = fileSddl = "S:"; }
        else if (level == "noreadup")
        {
            // A Medium label with NO_READ_UP denies a LOW-il process the READ,
            // which is the mandatory-policy DENYLIST alternative to the
            // discretionary restricting-sid allowlist. A Medium process (every
            // normal app) is unaffected, since NO_READ_UP only blocks lower il.
            dirSddl = "S:(ML;OICI;NRNWNX;;;ME)";
            fileSddl = "S:(ML;;NRNWNX;;;ME)";
        }
        else
        {
            string lvl = level == "low" ? "LW" : level == "untrusted" ? "S-1-16-0" : "ME";
            dirSddl = "S:(ML;OICI;NW;;;" + lvl + ")";
            fileSddl = "S:(ML;;NW;;;" + lvl + ")";
        }
        int rc = LabelOne(path, Directory.Exists(path) ? dirSddl : fileSddl);
        P("label " + (rc == 0 ? "OK" : "ERR=" + rc) + "  " + path + "  [" + (Directory.Exists(path) ? dirSddl : fileSddl) + "]");
        if (rc != 0) { Environment.Exit(1); }
        if (recurse && Directory.Exists(path))
        {
            int nOk = 0, nErr = 0; string firstErr = null;
            var stack = new Stack<string>(); stack.Push(path);
            while (stack.Count > 0)
            {
                string d = stack.Pop();
                string[] subs, files;
                try { subs = Directory.GetDirectories(d); files = Directory.GetFiles(d); }
                catch (Exception ex) { nErr++; if (firstErr == null) firstErr = d + ": " + ex.Message; continue; }
                foreach (string f in files)
                { int r = LabelOne(f, fileSddl); if (r == 0) nOk++; else { nErr++; if (firstErr == null) firstErr = f + " rc=" + r; } }
                foreach (string s in subs)
                { int r = LabelOne(s, dirSddl); if (r == 0) nOk++; else { nErr++; if (firstErr == null) firstErr = s + " rc=" + r; } stack.Push(s); }
            }
            P("recurse labeled=" + nOk + " errors=" + nErr + (firstErr != null ? "  first=" + firstErr : ""));
        }
    }

    static void CmdShowLabel(string[] a)
    {
        string path = a[1];
        IntPtr o, g, d, s, psd;
        uint rc = GetNamedSecurityInfoW(path, SE_FILE_OBJECT, LABEL_SECURITY_INFORMATION | 0x1 /*OWNER*/,
            out o, out g, out d, out s, out psd);
        if (rc != 0) { P("getsec ERR=" + rc); return; }
        string owner = "?";
        if (o != IntPtr.Zero) { IntPtr t; ConvertSidToStringSidW(o, out t); owner = Marshal.PtrToStringUni(t); LocalFree(t); }
        string lbl = "(none => implicit Medium)";
        if (s != IntPtr.Zero)
        {
            // SACL: rev(1) sbz(1) size(2) count(2) sbz2(2), then ACEs.
            int cnt = Marshal.ReadInt16(s, 4);
            var sb = new StringBuilder();
            long p = s.ToInt64() + 8;
            for (int i = 0; i < cnt; i++)
            {
                byte type = Marshal.ReadByte(new IntPtr(p));
                byte flags = Marshal.ReadByte(new IntPtr(p + 1));
                short size = Marshal.ReadInt16(new IntPtr(p + 2));
                uint mask = (uint)Marshal.ReadInt32(new IntPtr(p + 4));
                IntPtr sid = new IntPtr(p + 8);
                IntPtr t; ConvertSidToStringSidW(sid, out t);
                sb.Append("[type=" + type + " flags=0x" + flags.ToString("x") + " policy=0x" + mask.ToString("x") + " sid=" + Marshal.PtrToStringUni(t) + "] ");
                LocalFree(t);
                p += size;
            }
            lbl = sb.ToString();
        }
        P("path=" + path + "\n  owner=" + owner + "\n  label=" + lbl);
        LocalFree(psd);
    }

    // ---------------- report ----------------

    static void CmdReport()
    {
        IntPtr t = SelfToken();
        P("user_sid=" + SidStr(Marshal.ReadIntPtr(GetInfoPtr(t, TokenUser))));
        P("integrity=" + SidStr(Marshal.ReadIntPtr(GetInfoPtr(t, TokenIntegrityLevel))));
        byte[] el = GetInfo(t, TokenElevation);
        P("elevated=" + (BitConverter.ToInt32(el, 0) != 0));
        int nAdmin = 0, nLogon = 0;
        foreach (var g in Groups(t))
        {
            if (g.Key == "S-1-5-32-544") { nAdmin++; P("group ADMINS attrs=0x" + g.Value.ToString("x")); }
            if ((g.Value & SE_GROUP_LOGON_ID) == SE_GROUP_LOGON_ID) { nLogon++; P("group LOGONSID " + g.Key + " attrs=0x" + g.Value.ToString("x")); }
        }
        P("in_admins=" + (nAdmin > 0) + " logon_sids=" + nLogon);
        byte[] raw = GetInfo(t, TokenPrivileges);
        int n = BitConverter.ToInt32(raw, 0);
        var sb = new StringBuilder();
        for (int i = 0; i < n; i++)
        {
            int off = 4 + i * 12;
            var luid = new LUID { Low = BitConverter.ToUInt32(raw, off), High = BitConverter.ToInt32(raw, off + 4) };
            var name = new StringBuilder(128); int cch = 128;
            LookupPrivilegeNameW(null, ref luid, name, ref cch);
            sb.Append(name + "(0x" + BitConverter.ToUInt32(raw, off + 8).ToString("x") + ") ");
        }
        P("privileges(" + n + ")=" + sb);
    }

    // ---------------- launch (question 2 + 3) ----------------

    static IntPtr InheritableFile(string path, bool write)
    {
        var sa = new SECURITY_ATTRIBUTES { nLength = Marshal.SizeOf(typeof(SECURITY_ATTRIBUTES)), lpSD = IntPtr.Zero, bInherit = 1 };
        IntPtr h = CreateFileW(path, write ? GENERIC_WRITE : GENERIC_READ, FILE_SHARE_RW, ref sa,
            write ? CREATE_ALWAYS : OPEN_EXISTING, 0x80, IntPtr.Zero);
        if (h == new IntPtr(-1)) throw new Exception("CreateFileW(" + path + ") err=" + Err());
        return h;
    }

    static void CmdLaunch(string[] a)
    {
        string mode = "low", api = "asuser", desktop = null, outFile = null, cwd = null, restrictSpec = null,
               startupImp = null;
        int revertAfterMs = -1;
        bool keepLogon = false, restrictSids = false, newDesk = false;
        int i = 1;
        for (; i < a.Length; i++)
        {
            string s = a[i];
            if (s == "--") { i++; break; }
            else if (s == "--il") mode = a[++i];
            else if (s == "--restrict") restrictSpec = a[++i];
            else if (s == "--startup-impersonate") startupImp = a[++i];
            else if (s == "--revert-after") revertAfterMs = int.Parse(a[++i]);
            else if (s == "--api") api = a[++i];
            else if (s == "--desktop") desktop = a[++i];
            else if (s == "--out") outFile = a[++i];
            else if (s == "--cwd") cwd = a[++i];
            else if (s == "--keep-logon-sid") keepLogon = true;
            else if (s == "--restrict-sids") restrictSids = true;
            else if (s == "--new-desktop") newDesk = true;
            else throw new Exception("unknown flag " + s);
        }
        var rest = new List<string>();
        for (; i < a.Length; i++) rest.Add(a[i]);
        if (rest.Count == 0) throw new Exception("no command");
        string cmdline = string.Join(" ", rest.ToArray());

        if (newDesk)
        {
            string nm = "nubjail" + DateTime.Now.Ticks % 100000;
            IntPtr h = CreateDesktopW(nm, null, IntPtr.Zero, 0, 0x1FF | 0x80000 | 0x40000 | 0x20000, IntPtr.Zero);
            if (h == IntPtr.Zero) { P("CreateDesktopW ERR=" + Err()); }
            else
            {
                // Lower the desktop's own mandatory label so a low-IL child can attach.
                IntPtr psd; uint sz;
                ConvertStringSecurityDescriptorToSecurityDescriptorW("S:(ML;;NWNRNX;;;LW)", 1, out psd, out sz);
                uint si = LABEL_SECURITY_INFORMATION;
                bool ok = SetUserObjectSecurity(h, ref si, psd);
                P("new_desktop=" + nm + " label_set=" + ok + (ok ? "" : " err=" + Err()));
                desktop = StationName() + "\\" + nm;
            }
        }

        IntPtr tok = BuildToken(mode, keepLogon, restrictSids, restrictSpec);

        var si2 = new STARTUPINFO();
        si2.cb = Marshal.SizeOf(typeof(STARTUPINFO));
        si2.lpDesktop = desktop;
        IntPtr hOut = IntPtr.Zero, hIn = IntPtr.Zero;
        if (outFile != null)
        {
            hOut = InheritableFile(outFile, true);
            hIn = InheritableFile("NUL", false);
            si2.dwFlags = STARTF_USESTDHANDLES;
            si2.hStdInput = hIn; si2.hStdOutput = hOut; si2.hStdError = hOut;
        }

        PROCESS_INFORMATION pi;
        uint flags = CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW;
        if (startupImp != null) flags |= CREATE_SUSPENDED;
        bool ok2;
        if (api == "asuser")
            ok2 = CreateProcessAsUserW(tok, null, cmdline, IntPtr.Zero, IntPtr.Zero,
                outFile != null, flags, IntPtr.Zero, cwd, ref si2, out pi);
        else
            ok2 = CreateProcessWithTokenW(tok, 0, null, cmdline, flags, IntPtr.Zero, cwd, ref si2, out pi);
        int e2 = Err();
        P("api=" + api + " il=" + mode + " keep_logon_sid=" + keepLogon + " restrict_sids=" + restrictSids
          + " restrict=" + (restrictSpec ?? "(none)")
          + " desktop=" + (desktop ?? "(inherit)") + " => " + (ok2 ? "LAUNCHED pid=" + pi.pid : "FAILED err=" + e2));
        if (!ok2) { Environment.Exit(2); }
        if (startupImp != null)
        {
            IntPtr baseTok2 = SelfToken();
            IntPtr src;
            if (startupImp == "self-full")
            {   // the closest analogue of Chromium's USER_RESTRICTED_SAME_ACCESS initial token
                if (!DuplicateTokenEx(baseTok2, TOKEN_ALL_ACCESS, IntPtr.Zero, SecurityImpersonation, 2, out src))
                    throw new Exception("DuplicateTokenEx(initial) err=" + Err());
            }
            else
            {
                IntPtr prim2 = BuildToken(mode, keepLogon, false, startupImp);
                if (!DuplicateTokenEx(prim2, TOKEN_ALL_ACCESS, IntPtr.Zero, SecurityImpersonation, 2, out src))
                    throw new Exception("DuplicateTokenEx(initial2) err=" + Err());
            }
            IntPtr th = pi.hThread;
            bool okS = SetThreadToken(ref th, src);
            P("startup_impersonate=" + startupImp + " SetThreadToken=" + (okS ? "OK" : "ERR=" + Err()));
            ResumeThread(pi.hThread);
            if (revertAfterMs >= 0)
            {
                System.Threading.Thread.Sleep(revertAfterMs);
                IntPtr th2 = pi.hThread;
                bool okR = SetThreadToken(ref th2, IntPtr.Zero);
                P("revert_after_ms=" + revertAfterMs + " SetThreadToken(NULL)=" + (okR ? "OK" : "ERR=" + Err()));
            }
        }
        WaitForSingleObject(pi.hProcess, 0xFFFFFFFF);
        uint code; GetExitCodeProcess(pi.hProcess, out code);
        P("child_exit=0x" + code.ToString("x") + " (" + (int)code + ")");
        if (hOut != IntPtr.Zero) { CloseHandle(hOut); CloseHandle(hIn); }
        if (outFile != null)
        {
            P("---- child output ----");
            try { P(File.ReadAllText(outFile)); } catch (Exception ex) { P("(unreadable: " + ex.Message + ")"); }
        }
        Environment.Exit((int)code);
    }

    static string StationName()
    {
        IntPtr h = GetProcessWindowStation();
        IntPtr b = Marshal.AllocHGlobal(512); uint need;
        GetUserObjectInformationW(h, 2 /*UOI_NAME*/, b, 512, out need);
        string s = Marshal.PtrToStringUni(b); Marshal.FreeHGlobal(b); return s;
    }

    // ---------------- SETUP GATE: is an arbitrary sid usable with zero registration? ----------------

    // GATE Q1. Construct a sid with AllocateAndInitializeSid and show that the OS
    // maps it to NO account. If this needed a database entry, LookupAccountSid
    // would have to succeed for the sid to be usable anywhere — it must not.
    static void CmdSidGen(string[] a)
    {
        IntPtr sid = UniqueSid();
        P("unique_sid=" + SidStr(sid));
        P("sid_len=" + GetLengthSid(sid));
        var nm = new StringBuilder(256); uint cn = 256;
        var dm = new StringBuilder(256); uint cd = 256; int use;
        bool ok = LookupAccountSidW(null, sid, nm, ref cn, dm, ref cd, out use);
        P("lookup_account=" + (ok ? "RESOLVED " + dm + "\\" + nm + " use=" + use
                                  : "UNRESOLVED err=" + Err() + " (1332=ERROR_NONE_MAPPED)"));
        // Round-trip through the SDDL form so the string a caller passes to
        // `grant`/`--restrict` is provably the same binary sid.
        IntPtr back;
        P("sddl_roundtrip=" + (ConvertStringSidToSidW(SidStr(sid), out back) ? "OK" : "ERR=" + Err()));
        // GATE Q3, in isolation: does CreateRestrictedToken take it?
        IntPtr baseTok = SelfToken();
        var r = new SID_AND_ATTRIBUTES[1]; r[0].Sid = sid; r[0].Attributes = 0;
        IntPtr t;
        bool okT = CreateRestrictedToken(baseTok, 0, 0, null, 0, null, 1, r, out t);
        P("CreateRestrictedToken(SidsToRestrict=[unique])=" + (okT ? "ACCEPTED" : "REFUSED err=" + Err()));
        if (okT)
        {
            // Prove it landed in the token, not just that the call returned.
            int n = 0;
            foreach (var g in Groups(t)) if (g.Key == SidStr(sid)) n++;
            P("  unique_sid_in_token_groups=" + (n > 0) + " (restricting sids report via TokenRestrictedSids; groups check is indicative only)");
            CloseHandle(t);
        }
    }

    // GATE Q2 + capability Q2: write an ACCESS_ALLOWED ace for an arbitrary sid.
    // This is the step most likely to reject an unresolvable trustee.
    static int GrantOne(string path, IntPtr sid, uint mask, int mode, uint inherit)
    {
        IntPtr o, g, d, s, psd;
        uint rc = GetNamedSecurityInfoW(path, SE_FILE_OBJECT, DACL_SI, out o, out g, out d, out s, out psd);
        if (rc != 0) return (int)rc;
        var ea = new EXPLICIT_ACCESS_W[1];
        ea[0].grfAccessPermissions = mask;
        ea[0].grfAccessMode = mode;              // 1 GRANT_ACCESS, 4 REVOKE_ACCESS
        ea[0].grfInheritance = inherit;          // 3 = OBJECT_INHERIT|CONTAINER_INHERIT
        ea[0].Trustee.TrusteeForm = 0;           // TRUSTEE_IS_SID
        ea[0].Trustee.TrusteeType = 0;           // TRUSTEE_IS_UNKNOWN
        ea[0].Trustee.ptstrName = sid;
        IntPtr newAcl;
        uint rc2 = SetEntriesInAclW(1, ea, d, out newAcl);
        if (rc2 != 0) { LocalFree(psd); return (int)(0x10000 | rc2); }
        uint rc3 = SetNamedSecurityInfoW(path, SE_FILE_OBJECT, DACL_SI, IntPtr.Zero, IntPtr.Zero, newAcl, IntPtr.Zero);
        LocalFree(psd); LocalFree(newAcl);
        return (int)rc3;
    }

    static void CmdGrant(string[] a)
    {
        string path = a[1];
        string sidSpec = a.Length > 2 ? a[2] : "unique";
        bool revoke = Array.IndexOf(a, "-revoke") >= 0;
        bool recurse = Array.IndexOf(a, "-r") >= 0;
        uint mask = 0x1200a9;   // FILE_GENERIC_READ | FILE_GENERIC_EXECUTE (read+list+traverse)
        for (int i = 3; i < a.Length; i++)
            if (a[i] == "--mask") mask = Convert.ToUInt32(a[i + 1], 16);
        IntPtr sid = sidSpec == "unique" ? UniqueSid() : ParseSidList(sidSpec, SelfToken())[0].Sid;
        int mode = revoke ? 4 : 1;
        int rc = GrantOne(path, sid, mask, mode, 3);
        P((revoke ? "revoke " : "grant ") + (rc == 0 ? "OK" : "ERR=" + rc) + "  " + path
          + "  sid=" + SidStr(sid) + " mask=0x" + mask.ToString("x"));
        if (rc != 0) Environment.Exit(1);
        if (recurse && Directory.Exists(path))
        {
            int nOk = 0, nErr = 0; string firstErr = null;
            var stack = new Stack<string>(); stack.Push(path);
            var sw = System.Diagnostics.Stopwatch.StartNew();
            while (stack.Count > 0)
            {
                string dd = stack.Pop(); string[] subs, files;
                try { subs = Directory.GetDirectories(dd); files = Directory.GetFiles(dd); }
                catch (Exception ex) { nErr++; if (firstErr == null) firstErr = dd + ": " + ex.Message; continue; }
                foreach (string f in files)
                { int r2 = GrantOne(f, sid, mask, mode, 0); if (r2 == 0) nOk++; else { nErr++; if (firstErr == null) firstErr = f + " rc=" + r2; } }
                foreach (string sd2 in subs)
                { int r2 = GrantOne(sd2, sid, mask, mode, 3); if (r2 == 0) nOk++; else { nErr++; if (firstErr == null) firstErr = sd2 + " rc=" + r2; } stack.Push(sd2); }
            }
            P("recurse granted=" + nOk + " errors=" + nErr + " ms=" + sw.ElapsedMilliseconds
              + (firstErr != null ? "  first=" + firstErr : ""));
        }
    }

    // Registry variant. SE_REGISTRY_KEY object names use the
    // "CURRENT_USER\..." / "MACHINE\..." form, not "HKCU:\...". HKCU is the
    // prime suspect for the user-sid-specific startup dependency: its root key
    // dacl names the user's own sid, and the user owns it, so an unprivileged
    // WRITE_DAC should be available.
    static void CmdGrantReg(string[] a)
    {
        const int SE_REGISTRY_KEY = 4;
        string key = a[1];
        IntPtr sid = a.Length > 2 && a[2] != "unique" ? ParseSidList(a[2], SelfToken())[0].Sid : UniqueSid();
        uint mask = 0x20019;   // KEY_READ
        for (int i = 3; i < a.Length; i++) if (a[i] == "--mask") mask = Convert.ToUInt32(a[i + 1], 16);
        IntPtr o, g, d, s, psd;
        uint rc = GetNamedSecurityInfoW(key, SE_REGISTRY_KEY, DACL_SI, out o, out g, out d, out s, out psd);
        if (rc != 0) { P("grantreg GETSEC ERR=" + rc + "  " + key); Environment.Exit(1); }
        var ea = new EXPLICIT_ACCESS_W[1];
        ea[0].grfAccessPermissions = mask; ea[0].grfAccessMode = 1; ea[0].grfInheritance = 2 /*CONTAINER_INHERIT*/;
        ea[0].Trustee.TrusteeForm = 0; ea[0].Trustee.TrusteeType = 0; ea[0].Trustee.ptstrName = sid;
        IntPtr newAcl;
        uint rc2 = SetEntriesInAclW(1, ea, d, out newAcl);
        if (rc2 != 0) { P("grantreg SETENTRIES ERR=" + rc2); Environment.Exit(1); }
        uint rc3 = SetNamedSecurityInfoW(key, SE_REGISTRY_KEY, DACL_SI, IntPtr.Zero, IntPtr.Zero, newAcl, IntPtr.Zero);
        P("grantreg " + (rc3 == 0 ? "OK" : "ERR=" + rc3) + "  " + key + "  sid=" + SidStr(sid) + " mask=0x" + mask.ToString("x"));
        IntPtr str; uint len;
        IntPtr o2, g2, d2, s2, psd2;
        if (GetNamedSecurityInfoW(key, SE_REGISTRY_KEY, DACL_SI, out o2, out g2, out d2, out s2, out psd2) == 0
            && ConvertSecurityDescriptorToStringSecurityDescriptorW(psd2, 1, DACL_SI, out str, out len))
        { P("  now=" + Marshal.PtrToStringUni(str)); LocalFree(str); }
        if (rc3 != 0) Environment.Exit(1);
    }

    static void CmdDacl(string[] a)
    {
        IntPtr o, g, d, s, psd;
        uint rc = GetNamedSecurityInfoW(a[1], SE_FILE_OBJECT, OWNER_SI | GROUP_SI | DACL_SI | LABEL_SECURITY_INFORMATION,
            out o, out g, out d, out s, out psd);
        if (rc != 0) { P("dacl " + a[1] + " ERR=" + rc); return; }
        IntPtr str; uint len;
        if (ConvertSecurityDescriptorToStringSecurityDescriptorW(psd, 1,
                OWNER_SI | GROUP_SI | DACL_SI | LABEL_SECURITY_INFORMATION, out str, out len))
        { P("dacl " + a[1] + " = " + Marshal.PtrToStringUni(str)); LocalFree(str); }
        else P("dacl " + a[1] + " sddl ERR=" + Err());
        LocalFree(psd);
    }

    // ---------------- AccessCheck matrix ----------------

    static GENERIC_MAPPING FileMapping()
    {
        var m = new GENERIC_MAPPING();
        m.GenericRead = 0x120089; m.GenericWrite = 0x120116;
        m.GenericExecute = 0x1200a0; m.GenericAll = 0x1f01ff;
        return m;
    }

    static string One(IntPtr imp, string path, uint desired)
    {
        IntPtr o, g, d, s, psd;
        uint rc = GetNamedSecurityInfoW(path, SE_FILE_OBJECT,
            OWNER_SI | GROUP_SI | DACL_SI | LABEL_SECURITY_INFORMATION,
            out o, out g, out d, out s, out psd);
        if (rc != 0) return "GETSEC_ERR=" + rc;
        var map = FileMapping();
        uint des = desired; MapGenericMask(ref des, ref map);
        uint plen = 256; IntPtr pbuf = Marshal.AllocHGlobal((int)plen);
        Marshal.WriteInt32(pbuf, 0, 0); Marshal.WriteInt32(pbuf, 4, 0);
        uint granted; int status;
        bool ok = AccessCheck(psd, imp, des, ref map, pbuf, ref plen, out granted, out status);
        int e = Err();
        Marshal.FreeHGlobal(pbuf); LocalFree(psd);
        if (!ok) return "ACCESSCHECK_ERR=" + e;
        return status != 0 ? "GRANTED(0x" + granted.ToString("x") + ")" : "DENIED";
    }

    static void CmdCheck(string[] a)
    {
        string mode = "low", spec = null; bool keepLogon = false;
        int i = 1;
        for (; i < a.Length; i++)
        {
            string s = a[i];
            if (s == "--") { i++; break; }
            else if (s == "--il") mode = a[++i];
            else if (s == "--restrict") spec = a[++i];
            else if (s == "--keep-logon-sid") keepLogon = true;
            else throw new Exception("unknown flag " + s);
        }
        IntPtr prim = BuildToken(mode, keepLogon, false, spec);
        IntPtr imp;
        if (!DuplicateTokenEx(prim, TOKEN_ALL_ACCESS, IntPtr.Zero, SecurityImpersonation, 2 /*TokenImpersonation*/, out imp))
            throw new Exception("DuplicateTokenEx(impersonation) err=" + Err());
        P("== il=" + mode + " restrict=" + (spec ?? "(none)") + " ==");
        for (; i < a.Length; i++)
        {
            string p = Environment.ExpandEnvironmentVariables(a[i]);
            P("  " + p
              + "\n      READ=" + One(imp, p, 0x120089)
              + "  TRAVERSE=" + One(imp, p, 0x20)
              + "  WRITE=" + One(imp, p, 0x120116));
        }
    }

    static int Main(string[] args)
    {
        if (args.Length == 0) { P("usage: Jail <report|label|showlabel|station|launch|sidgen|grant|dacl|check> ..."); return 64; }
        try
        {
            switch (args[0])
            {
                case "report": CmdReport(); break;
                case "label": CmdLabel(args); break;
                case "showlabel": CmdShowLabel(args); break;
                case "station": P("station=" + StationName()); break;
                case "launch": CmdLaunch(args); break;
                case "sidgen": CmdSidGen(args); break;
                case "grant": CmdGrant(args); break;
                case "dacl": CmdDacl(args); break;
                case "grantreg": CmdGrantReg(args); break;
                case "check": CmdCheck(args); break;
                default: P("unknown " + args[0]); return 64;
            }
        }
        catch (Exception ex) { P("EXC " + ex.Message); return 1; }
        return 0;
    }
}
