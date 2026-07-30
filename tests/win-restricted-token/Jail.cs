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

    // mode: none | medium | low | untrusted
    static IntPtr BuildToken(string mode, bool keepLogonSid, bool restrictSids)
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
        if (restrictSids)
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
        string mode = "low", api = "asuser", desktop = null, outFile = null, cwd = null;
        bool keepLogon = false, restrictSids = false, newDesk = false;
        int i = 1;
        for (; i < a.Length; i++)
        {
            string s = a[i];
            if (s == "--") { i++; break; }
            else if (s == "--il") mode = a[++i];
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

        IntPtr tok = BuildToken(mode, keepLogon, restrictSids);

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
        bool ok2;
        if (api == "asuser")
            ok2 = CreateProcessAsUserW(tok, null, cmdline, IntPtr.Zero, IntPtr.Zero,
                outFile != null, flags, IntPtr.Zero, cwd, ref si2, out pi);
        else
            ok2 = CreateProcessWithTokenW(tok, 0, null, cmdline, flags, IntPtr.Zero, cwd, ref si2, out pi);
        int e2 = Err();
        P("api=" + api + " il=" + mode + " keep_logon_sid=" + keepLogon + " restrict_sids=" + restrictSids
          + " desktop=" + (desktop ?? "(inherit)") + " => " + (ok2 ? "LAUNCHED pid=" + pi.pid : "FAILED err=" + e2));
        if (!ok2) { Environment.Exit(2); }
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

    static int Main(string[] args)
    {
        if (args.Length == 0) { P("usage: Jail <report|label|showlabel|station|launch> ..."); return 64; }
        try
        {
            switch (args[0])
            {
                case "report": CmdReport(); break;
                case "label": CmdLabel(args); break;
                case "showlabel": CmdShowLabel(args); break;
                case "station": P("station=" + StationName()); break;
                case "launch": CmdLaunch(args); break;
                default: P("unknown " + args[0]); return 64;
            }
        }
        catch (Exception ex) { P("EXC " + ex.Message); return 1; }
        return 0;
    }
}
