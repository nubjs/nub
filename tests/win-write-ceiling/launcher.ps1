# The AppContainer launcher + security helpers for the write-ceiling probe.
#
# Derived from `tests/win-bypass-traverse/launcher.ps1`, which is the version that has actually
# been measured. Three things are ADDED here, each because this probe asks a question that one
# could not:
#
#   1. `LaunchCaps` REALLY passes capability sids. The bypass-traverse copy declares the parameter
#      and then unconditionally writes `Capabilities = IntPtr.Zero / CapabilityCount = 0`, so every
#      arm it ever ran was a zero-capability arm regardless of what the caller asked for. This
#      probe's whole separability question is "does a broad WRITE grant cost the net axis", which
#      is unanswerable if the capability array is silently dropped.
#   2. `SetAce` / `ReadAceMask`, so ACE writes are attempted and READ BACK from this process rather
#      than through `icacls` string-scraping. A propagation slip and a kernel denial look identical
#      in a child's output; only a DACL read-back tells them apart.
#   3. `RunDeelevated`, an impersonation gate. The DACL-write question ("where can nub install a
#      grant?") is meaningless measured from an elevated runner, because an elevated token can
#      write a DACL on `C:\` and a real user cannot.
#
# CharSet.Unicode is LOAD-BEARING everywhere below, not decoration: the ANSI default marshals a
# name into a UTF-16 API and every call fails in a way that reads as "mechanism unavailable"
# (MECHANISM-FACTS §5e cost a whole run to exactly that).

$src = @'
using System;
using System.Runtime.InteropServices;
using System.Text;

public static class Wc
{
    // ───────────────────────────── process / token ─────────────────────────────

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
    [DllImport("advapi32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool ConvertSidToStringSidW(IntPtr sid, out IntPtr str);
    [DllImport("advapi32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool ConvertStringSidToSidW(string s, out IntPtr sid);
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
    static readonly IntPtr PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES = new IntPtr(0x00020009);
    const uint WAIT_TIMEOUT = 0x00000102;
    const uint SE_GROUP_ENABLED = 0x00000004;

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

    /// Launch `exe` with `cmdline` in `cwd`, stdout+stderr to `logPath`.
    ///
    /// `acSidStr` empty  ⇒ an ORDINARY child: the same code path minus one attribute, which is
    /// what makes the `plain` arm a control rather than a different program. `plain` models what
    /// the shipping full-disk tier does today on Windows (decline the token entirely).
    ///
    /// `capabilitySids` non-empty ⇒ those capabilities are REQUESTED and the array is really
    /// passed. `internetClient` is `S-1-15-3-1`; passing none is the coarse egress deny.
    public static string Launch(string acSidStr, string exe, string cmdline, string cwd,
        string logPath, uint timeoutMs, string[] capabilitySids)
    {
        IntPtr acSid = IntPtr.Zero;
        IntPtr attrList = IntPtr.Zero;
        IntPtr capsBuf = IntPtr.Zero;
        IntPtr capArray = IntPtr.Zero;
        IntPtr hOut = new IntPtr(-1), hIn = new IntPtr(-1);
        System.Collections.Generic.List<IntPtr> capSids = new System.Collections.Generic.List<IntPtr>();
        try
        {
            SECURITY_ATTRIBUTES sa = new SECURITY_ATTRIBUTES();
            sa.nLength = Marshal.SizeOf(typeof(SECURITY_ATTRIBUTES));
            sa.bInheritHandle = 1;
            // The log handle is opened by the UNCONFINED parent and inherited already-open.
            // Access is checked at open and cached in the handle, so the child writes its table
            // even when it can read nothing at all — the difference between a negative result and
            // no result.
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
                        // SE_GROUP_ENABLED: a capability sid in SECURITY_CAPABILITIES must be
                        // enabled or it is present-and-inert, which reads as "the capability did
                        // not help" when in fact it was never held.
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
            bool ok = CreateProcessW(exe, cl, IntPtr.Zero, IntPtr.Zero, true, flags,
                IntPtr.Zero, cwd, ref si, out pi);
            if (!ok) return "launch-error CreateProcessW err=" + Marshal.GetLastWin32Error();

            uint wr = WaitForSingleObject(pi.hProcess, timeoutMs);
            uint code = 0xFFFFFFFF;
            string extra = "";
            if (wr == WAIT_TIMEOUT) { TerminateProcess(pi.hProcess, 0xDEAD); extra = " TIMED-OUT"; }
            else { GetExitCodeProcess(pi.hProcess, out code); }
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
            foreach (IntPtr p in capSids) LocalFree(p);
            if (capArray != IntPtr.Zero) Marshal.FreeHGlobal(capArray);
            if (acSid != IntPtr.Zero) LocalFree(acSid);
        }
    }

    // ───────────────────────────── ACE write / read-back ─────────────────────────────

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
    const int GRANT_ACCESS = 1, REVOKE_ACCESS = 4;
    const uint SUB_CONTAINERS_AND_OBJECTS_INHERIT = 3; // CONTAINER_INHERIT | OBJECT_INHERIT
    const uint NO_INHERITANCE = 0;

    /// Add (`mode` = 1) or remove (`mode` = 4) an ACE for `sidStr` on `path`.
    ///
    /// `inherit` selects the two shapes this probe has to tell apart: an INHERITABLE grant, which
    /// `SetNamedSecurityInfoW` propagates into every existing child (that walk is the cost), and a
    /// this-object-only grant, which is O(1) and reaches no child at all.
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
            // 5 = ERROR_ACCESS_DENIED — the answer to "may an unprivileged owner ACE this root".
            return rc == 0 ? "OK" : "ERR set=" + rc;
        }
        finally
        {
            if (newDacl != IntPtr.Zero) LocalFree(newDacl);
            if (sd != IntPtr.Zero) LocalFree(sd);
            if (sid != IntPtr.Zero) LocalFree(sid);
        }
    }

    /// The effective rights `sidStr` holds on `path`'s OWN dacl — the read-back that separates a
    /// propagation slip from a kernel denial. Without it the two are indistinguishable, which is
    /// the mistake `win-bypass-traverse` documents as mandatory to avoid.
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

    // ───────────────────────────── de-elevation gate ─────────────────────────────
    //
    // The runner is elevated. "Can nub install this ACE?" answered from an elevated token is a
    // fact about CI and not about a user, so every DACL-write row has to be taken again under a
    // token that holds no admin authority. Impersonation is the cheapest honest form: the access
    // check for `SetNamedSecurityInfoW` uses the THREAD's effective token, so impersonating a
    // restricted token gives the standard-user answer in-process.

    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool CreateRestrictedToken(IntPtr existing, uint flags,
        uint disableCount, IntPtr disableSids, uint deleteCount, IntPtr deletePrivs,
        uint restrictCount, IntPtr restrictSids, out IntPtr newToken);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool DuplicateTokenEx(IntPtr existing, uint access, IntPtr attrs,
        int impLevel, int tokenType, out IntPtr newToken);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool ImpersonateLoggedOnUser(IntPtr token);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool RevertToSelf();
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool OpenProcessToken(IntPtr proc, uint access, out IntPtr token);
    [DllImport("kernel32.dll")] static extern IntPtr GetCurrentProcess();
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool AllocateAndInitializeSid(byte[] auth, byte count,
        uint a0, uint a1, uint a2, uint a3, uint a4, uint a5, uint a6, uint a7, out IntPtr sid);
    [DllImport("advapi32.dll")] static extern IntPtr FreeSid(IntPtr sid);

    const uint DISABLE_MAX_PRIVILEGE = 1;
    const uint TOKEN_ALL_ACCESS = 0xF01FF;
    const uint TOKEN_DUPLICATE = 2, TOKEN_QUERY = 8, TOKEN_ASSIGN_PRIMARY = 1, TOKEN_IMPERSONATE = 4;

    static IntPtr impersonating = IntPtr.Zero;

    /// Impersonate a de-elevated restricted token derived from our OWN token: `Administrators`
    /// deny-only, every privilege bar `SeChangeNotify` dropped. Same shape MECHANISM-FACTS §5h
    /// used for its de-elevated arm. Returns "OK" or an error string.
    public static string BeginDeelevated()
    {
        IntPtr self, restricted, dup;
        if (!OpenProcessToken(GetCurrentProcess(),
                TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY | TOKEN_IMPERSONATE, out self))
            return "ERR open=" + Marshal.GetLastWin32Error();

        // SECURITY_NT_AUTHORITY, BUILTIN\Administrators (S-1-5-32-544) as a DENY-ONLY sid.
        IntPtr adminSid;
        byte[] ntAuth = new byte[] { 0, 0, 0, 0, 0, 5 };
        if (!AllocateAndInitializeSid(ntAuth, 2, 32, 544, 0, 0, 0, 0, 0, 0, out adminSid))
            return "ERR sid=" + Marshal.GetLastWin32Error();
        int stride = Marshal.SizeOf(typeof(SID_AND_ATTRIBUTES));
        IntPtr disable = Marshal.AllocHGlobal(stride);
        SID_AND_ATTRIBUTES sa = new SID_AND_ATTRIBUTES();
        sa.Sid = adminSid; sa.Attributes = 0;
        Marshal.StructureToPtr(sa, disable, false);

        bool ok = CreateRestrictedToken(self, DISABLE_MAX_PRIVILEGE, 1, disable, 0, IntPtr.Zero,
            0, IntPtr.Zero, out restricted);
        int err = Marshal.GetLastWin32Error();
        Marshal.FreeHGlobal(disable);
        FreeSid(adminSid);
        CloseHandle(self);
        if (!ok) return "ERR restrict=" + err;

        // SecurityImpersonation(2), TokenImpersonation(2).
        if (!DuplicateTokenEx(restricted, TOKEN_ALL_ACCESS, IntPtr.Zero, 2, 2, out dup))
        { CloseHandle(restricted); return "ERR dup=" + Marshal.GetLastWin32Error(); }
        CloseHandle(restricted);

        if (!ImpersonateLoggedOnUser(dup))
        { CloseHandle(dup); return "ERR impersonate=" + Marshal.GetLastWin32Error(); }
        impersonating = dup;
        return "OK";
    }

    [DllImport("advapi32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool CreateProcessAsUserW(IntPtr token, string app, StringBuilder cmdline,
        IntPtr pa, IntPtr ta, bool inherit, uint flags, IntPtr env, string cwd,
        ref STARTUPINFOEXW si, out PROCESS_INFORMATION pi);

    /// Launch WITHOUT an AppContainer token but WITH a de-elevated restricted one — the honest
    /// baseline for "what does declining the LowBox token actually give a package".
    ///
    /// The `plain` arm on CI runs as an ELEVATED runner, so it writes `C:\Program Files` and
    /// `C:\Windows` and overstates the no-token tier by exactly the amount CI differs from a
    /// developer. This arm is the same launch under a token holding no admin authority.
    /// `CreateProcessAsUserW` needs no privilege for a self-derived restricted token — the
    /// documented `CreateRestrictedToken` exemption from `SE_ASSIGNPRIMARYTOKEN`, measured in
    /// MECHANISM-FACTS §5e.
    public static string LaunchDeelevated(string exe, string cmdline, string cwd,
        string logPath, uint timeoutMs)
    {
        IntPtr self = IntPtr.Zero, restricted = IntPtr.Zero, adminSid = IntPtr.Zero, disable = IntPtr.Zero;
        IntPtr hOut = new IntPtr(-1), hIn = new IntPtr(-1);
        try
        {
            if (!OpenProcessToken(GetCurrentProcess(),
                    TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY | TOKEN_IMPERSONATE, out self))
                return "launch-error OpenProcessToken err=" + Marshal.GetLastWin32Error();
            byte[] ntAuth = new byte[] { 0, 0, 0, 0, 0, 5 };
            if (!AllocateAndInitializeSid(ntAuth, 2, 32, 544, 0, 0, 0, 0, 0, 0, out adminSid))
                return "launch-error AllocateAndInitializeSid err=" + Marshal.GetLastWin32Error();
            disable = Marshal.AllocHGlobal(Marshal.SizeOf(typeof(SID_AND_ATTRIBUTES)));
            SID_AND_ATTRIBUTES sd = new SID_AND_ATTRIBUTES();
            sd.Sid = adminSid; sd.Attributes = 0;
            Marshal.StructureToPtr(sd, disable, false);
            if (!CreateRestrictedToken(self, DISABLE_MAX_PRIVILEGE, 1, disable, 0, IntPtr.Zero,
                    0, IntPtr.Zero, out restricted))
                return "launch-error CreateRestrictedToken err=" + Marshal.GetLastWin32Error();

            SECURITY_ATTRIBUTES sa = new SECURITY_ATTRIBUTES();
            sa.nLength = Marshal.SizeOf(typeof(SECURITY_ATTRIBUTES));
            sa.bInheritHandle = 1;
            hOut = CreateFileW(logPath, GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE,
                ref sa, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, IntPtr.Zero);
            if (hOut == new IntPtr(-1)) return "launch-error CreateFileW(log) err=" + Marshal.GetLastWin32Error();
            hIn = CreateFileW("NUL", GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE,
                ref sa, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, IntPtr.Zero);
            if (hIn == new IntPtr(-1)) return "launch-error CreateFileW(NUL) err=" + Marshal.GetLastWin32Error();

            STARTUPINFOEXW si = new STARTUPINFOEXW();
            si.StartupInfo.cb = (uint)Marshal.SizeOf(typeof(STARTUPINFOW));
            si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
            si.StartupInfo.hStdInput = hIn;
            si.StartupInfo.hStdOutput = hOut;
            si.StartupInfo.hStdError = hOut;

            PROCESS_INFORMATION pi;
            StringBuilder cl = new StringBuilder(cmdline, cmdline.Length + 64);
            if (!CreateProcessAsUserW(restricted, exe, cl, IntPtr.Zero, IntPtr.Zero, true, 0,
                    IntPtr.Zero, cwd, ref si, out pi))
                return "launch-error CreateProcessAsUserW err=" + Marshal.GetLastWin32Error();

            uint wr = WaitForSingleObject(pi.hProcess, timeoutMs);
            uint code = 0xFFFFFFFF;
            string extra = "";
            if (wr == WAIT_TIMEOUT) { TerminateProcess(pi.hProcess, 0xDEAD); extra = " TIMED-OUT"; }
            else { GetExitCodeProcess(pi.hProcess, out code); }
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
            return "rc=" + code + " (0x" + code.ToString("x8") + ")" + extra;
        }
        finally
        {
            if (hOut != new IntPtr(-1)) CloseHandle(hOut);
            if (hIn != new IntPtr(-1)) CloseHandle(hIn);
            if (disable != IntPtr.Zero) Marshal.FreeHGlobal(disable);
            if (adminSid != IntPtr.Zero) FreeSid(adminSid);
            if (restricted != IntPtr.Zero) CloseHandle(restricted);
            if (self != IntPtr.Zero) CloseHandle(self);
        }
    }

    public static string EndDeelevated()
    {
        if (!RevertToSelf()) return "ERR revert=" + Marshal.GetLastWin32Error();
        if (impersonating != IntPtr.Zero) { CloseHandle(impersonating); impersonating = IntPtr.Zero; }
        return "OK";
    }
}
'@

Add-Type -TypeDefinition $src -Language CSharp -ErrorAction Stop
