# The AppContainer launcher shared by this directory's two probes, as a SUPERSET of the validated
# one in `tests/win-bypass-traverse/probe.ps1`.
#
# WHY A SEPARATE TYPE (`AcJail`) INSTEAD OF EDITING `Bt` IN PLACE. That launcher is validated
# against real runs (MECHANISM-FACTS §5h) and another lane still pushes to it; a shared refactor
# would put both probes on one untested edit. So the P/Invoke shape is duplicated deliberately and
# the additions live only here. Every signature that also exists on `Bt` is byte-identical to it,
# including the `CharSet.Unicode` on each string-taking import — the ANSI default marshals into a
# UTF-16 API, every open fails `err=2`, and that reads exactly like "mechanism unavailable" (a bug
# that already cost this effort a whole run, §5e).
#
# WHAT IS NEW HERE, and why each is needed:
#
#   Launch(..., capSids)          A capability array instead of a hardcoded count of zero. The UNC
#                                 arms need `privateNetworkClientServer`, because an AppContainer
#                                 with zero capabilities is denied the NETWORK, and a denial for
#                                 that reason says nothing about path traversal.
#   Launch(..., deletePrivs)      Launch via `CreateProcessAsUserW` from a `CreateRestrictedToken`
#                                 with named privileges DELETED. This is the one-variable
#                                 differential that separates the two candidate mechanisms for the
#                                 traverse skip: withhold `SeChangeNotifyPrivilege` and, if the
#                                 skip is the privilege, deep reads must start failing on the very
#                                 volume where they currently succeed.
#   token read-back               The created child's token is opened and its `TokenIsAppContainer`
#                                 flag and privilege list are reported. Without this, "the privilege
#                                 was deleted" is an assumption about an API call rather than an
#                                 observation about the token that ran — the same reason every arm
#                                 reads the target's DACL back.
#   VolumeChar(path)              `NtQueryVolumeInformationFile(FileFsDeviceInformation)` on a
#                                 volume root, which returns the device Characteristics bitmask the
#                                 volume-flag hypothesis is ABOUT. Measuring the premise directly is
#                                 worth more than inferring it from behaviour.

$acJailSrc = @'
using System;
using System.Runtime.InteropServices;
using System.Text;

public static class AcJail
{
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

    [StructLayout(LayoutKind.Sequential)]
    struct LUID { public uint LowPart; public int HighPart; }

    [StructLayout(LayoutKind.Sequential)]
    struct LUID_AND_ATTRIBUTES { public LUID Luid; public uint Attributes; }

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

    [StructLayout(LayoutKind.Sequential)]
    struct IO_STATUS_BLOCK { public IntPtr Status; public IntPtr Information; }

    [StructLayout(LayoutKind.Sequential)]
    struct FILE_FS_DEVICE_INFORMATION { public uint DeviceType; public uint Characteristics; }

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
        IntPtr sa, uint disp, uint flags, IntPtr templ);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern IntPtr CreateFileW(string name, uint access, uint share,
        ref SECURITY_ATTRIBUTES sa, uint disp, uint flags, IntPtr templ);

    [DllImport("ntdll.dll")]
    static extern int NtQueryVolumeInformationFile(IntPtr h, out IO_STATUS_BLOCK iosb,
        out FILE_FS_DEVICE_INFORMATION info, uint len, uint cls);

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
    [DllImport("advapi32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool CreateProcessAsUserW(IntPtr token, string app, StringBuilder cmdline,
        IntPtr pa, IntPtr ta, bool inherit, uint flags, IntPtr env, string cwd,
        ref STARTUPINFOEXW si, out PROCESS_INFORMATION pi);
    [DllImport("kernel32.dll", SetLastError = true)] static extern uint WaitForSingleObject(IntPtr h, uint ms);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool GetExitCodeProcess(IntPtr h, out uint code);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool TerminateProcess(IntPtr h, uint code);
    [DllImport("kernel32.dll")] static extern IntPtr GetCurrentProcess();

    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool OpenProcessToken(IntPtr proc, uint access, out IntPtr token);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool GetTokenInformation(IntPtr token, int cls, IntPtr buf, uint len, out uint ret);
    [DllImport("advapi32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool LookupPrivilegeValueW(string sys, string name, out LUID luid);
    [DllImport("advapi32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool LookupPrivilegeNameW(string sys, ref LUID luid, StringBuilder name, ref int len);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool CreateRestrictedToken(IntPtr existing, uint flags,
        uint disableCount, IntPtr sidsToDisable,
        uint deleteCount, IntPtr privsToDelete,
        uint restrictCount, IntPtr sidsToRestrict, out IntPtr newToken);

    const uint GENERIC_READ = 0x80000000, GENERIC_WRITE = 0x40000000;
    const uint FILE_READ_ATTRIBUTES = 0x80;
    const uint FILE_SHARE_READ = 1, FILE_SHARE_WRITE = 2, FILE_SHARE_DELETE = 4;
    const uint CREATE_ALWAYS = 2, OPEN_EXISTING = 3, FILE_ATTRIBUTE_NORMAL = 0x80;
    const uint FILE_FLAG_BACKUP_SEMANTICS = 0x02000000;
    const uint EXTENDED_STARTUPINFO_PRESENT = 0x00080000;
    const uint STARTF_USESTDHANDLES = 0x00000100;
    static readonly IntPtr PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES = new IntPtr(0x00020009);
    const uint WAIT_TIMEOUT = 0x00000102;
    const uint SE_GROUP_ENABLED = 0x4;
    const uint TOKEN_QUERY = 0x0008, TOKEN_DUPLICATE = 0x0002, TOKEN_ASSIGN_PRIMARY = 0x0001;
    const int TokenPrivileges = 3, TokenIsAppContainer = 29;
    const uint FileFsDeviceInformation = 4;
    const uint DISABLE_MAX_PRIVILEGE = 0x1;

    public static int LastErr;

    /// An admin-stripped version of this process's own principal, suitable for `RunImpersonated` —
    /// `BUILTIN\Administrators` deny-only, plus privileges removed. It is what the "could nub repair
    /// the MSI's DACL without elevation?" arm impersonates.
    ///
    /// `hardDelete` IS THE WHOLE POINT OF THE PARAMETER, and run 1 is why it exists.
    /// `DISABLE_MAX_PRIVILEGE` only **disables** privileges; a disabled privilege is still HELD and
    /// can be re-enabled with `AdjustTokenPrivileges`, which .NET's `Set-Acl` path does for
    /// `SeSecurityPrivilege` / `SeRestorePrivilege` / `SeTakeOwnershipPrivilege` when it needs them.
    /// So the `false` variant measured an admin who had merely put his authority down, and reported
    /// that `C:\Program Files\nodejs` was rewritable unprivileged — the exact shape of a control that
    /// confirms a false claim. `true` enumerates the token's privileges and DELETES every one but
    /// `SeChangeNotifyPrivilege`, which is irreversible. Both variants are run and reported so the
    /// artifact stays visible rather than being quietly corrected away.
    ///
    /// Either way this is a SYNTHESIZED principal, not a separate standard-user account: the user SID
    /// survives, so `HKCU` and `%LOCALAPPDATA%` are still an admin's profile (the same residual
    /// MECHANISM-FACTS §5h records). That is fine for THIS question, which is only about whether the
    /// authority to rewrite a DACL on an admin-owned path survives.
    public static long DeElevatedToken(bool hardDelete)
    {
        LastErr = 0;
        IntPtr src = IntPtr.Zero, admins = IntPtr.Zero, buf = IntPtr.Zero, privBuf = IntPtr.Zero;
        try
        {
            if (!OpenProcessToken(GetCurrentProcess(),
                    TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_ASSIGN_PRIMARY, out src))
            { LastErr = Marshal.GetLastWin32Error(); return 0; }
            if (!ConvertStringSidToSidW("S-1-5-32-544", out admins))
            { LastErr = Marshal.GetLastWin32Error(); return 0; }
            SID_AND_ATTRIBUTES saa = new SID_AND_ATTRIBUTES();
            saa.Sid = admins;
            saa.Attributes = 0;
            buf = Marshal.AllocHGlobal(Marshal.SizeOf(typeof(SID_AND_ATTRIBUTES)));
            Marshal.StructureToPtr(saa, buf, false);

            uint flags = hardDelete ? 0 : DISABLE_MAX_PRIVILEGE;
            uint deleteCount = 0;
            if (hardDelete)
            {
                LUID keep;
                if (!LookupPrivilegeValueW(null, "SeChangeNotifyPrivilege", out keep))
                { LastErr = Marshal.GetLastWin32Error(); return 0; }
                uint need = 0;
                GetTokenInformation(src, TokenPrivileges, IntPtr.Zero, 0, out need);
                IntPtr q = Marshal.AllocHGlobal((int)need);
                try
                {
                    if (!GetTokenInformation(src, TokenPrivileges, q, need, out need))
                    { LastErr = Marshal.GetLastWin32Error(); return 0; }
                    int count = Marshal.ReadInt32(q);
                    int stride = Marshal.SizeOf(typeof(LUID_AND_ATTRIBUTES));
                    var del = new System.Collections.Generic.List<LUID_AND_ATTRIBUTES>();
                    for (int i = 0; i < count; i++)
                    {
                        LUID_AND_ATTRIBUTES la = (LUID_AND_ATTRIBUTES)Marshal.PtrToStructure(
                            new IntPtr(q.ToInt64() + 4 + (long)i * stride), typeof(LUID_AND_ATTRIBUTES));
                        if (la.Luid.LowPart == keep.LowPart && la.Luid.HighPart == keep.HighPart) continue;
                        la.Attributes = 0;
                        del.Add(la);
                    }
                    if (del.Count > 0)
                    {
                        privBuf = Marshal.AllocHGlobal(stride * del.Count);
                        for (int i = 0; i < del.Count; i++)
                            Marshal.StructureToPtr(del[i], new IntPtr(privBuf.ToInt64() + (long)i * stride), false);
                    }
                    deleteCount = (uint)del.Count;
                }
                finally { Marshal.FreeHGlobal(q); }
            }

            IntPtr tok;
            if (!CreateRestrictedToken(src, flags, 1, buf, deleteCount, privBuf,
                    0, IntPtr.Zero, out tok))
            { LastErr = Marshal.GetLastWin32Error(); return 0; }
            return tok.ToInt64();
        }
        finally
        {
            if (privBuf != IntPtr.Zero) Marshal.FreeHGlobal(privBuf);
            if (buf != IntPtr.Zero) Marshal.FreeHGlobal(buf);
            if (admins != IntPtr.Zero) LocalFree(admins);
            if (src != IntPtr.Zero) CloseHandle(src);
        }
    }

    /// The privilege list of an arbitrary token handle, so an impersonation arm can prove what it is
    /// actually running as instead of assuming the construction worked.
    public static string PrivsOf(long token) { return PrivsOfToken(new IntPtr(token)); }

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

    /// The device Characteristics bitmask of the volume a path lives on. This is the DIRECT
    /// measurement of the volume-flag hypothesis's premise: `windows.rs`'s traverse model credits
    /// `FILE_DEVICE_ALLOW_APPCONTAINER_TRAVERSAL` on the volume device object, and until now that
    /// bit has only ever been asserted, never read. Returned raw so the decode can be checked
    /// against the SDK header on the runner rather than against anyone's memory of the constant.
    public static string VolumeChar(string root)
    {
        IntPtr h = CreateFileW(root, FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, IntPtr.Zero,
            OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, IntPtr.Zero);
        if (h == new IntPtr(-1)) return "ERR open err=" + Marshal.GetLastWin32Error();
        try
        {
            IO_STATUS_BLOCK iosb;
            FILE_FS_DEVICE_INFORMATION info;
            int st = NtQueryVolumeInformationFile(h, out iosb, out info,
                (uint)Marshal.SizeOf(typeof(FILE_FS_DEVICE_INFORMATION)), FileFsDeviceInformation);
            if (st != 0) return "ERR nt=0x" + st.ToString("x8");
            return "type=0x" + info.DeviceType.ToString("x8") + " chars=0x" + info.Characteristics.ToString("x8");
        }
        finally { CloseHandle(h); }
    }

    static string PrivsOfToken(IntPtr token)
    {
        uint need = 0;
        GetTokenInformation(token, TokenPrivileges, IntPtr.Zero, 0, out need);
        if (need == 0) return "(query failed err=" + Marshal.GetLastWin32Error() + ")";
        IntPtr buf = Marshal.AllocHGlobal((int)need);
        try
        {
            if (!GetTokenInformation(token, TokenPrivileges, buf, need, out need))
                return "(query failed err=" + Marshal.GetLastWin32Error() + ")";
            int count = Marshal.ReadInt32(buf);
            var sb = new StringBuilder();
            int stride = Marshal.SizeOf(typeof(LUID_AND_ATTRIBUTES));
            for (int i = 0; i < count; i++)
            {
                IntPtr at = new IntPtr(buf.ToInt64() + 4 + (long)i * stride);
                LUID_AND_ATTRIBUTES la = (LUID_AND_ATTRIBUTES)Marshal.PtrToStructure(at, typeof(LUID_AND_ATTRIBUTES));
                var nm = new StringBuilder(128);
                int len = 128;
                LUID l = la.Luid;
                string name = LookupPrivilegeNameW(null, ref l, nm, ref len) ? nm.ToString() : "(luid)";
                if (sb.Length > 0) sb.Append(',');
                sb.Append(name).Append((la.Attributes & 2) != 0 ? ":on" : ":off");
            }
            return count == 0 ? "(none)" : sb.ToString();
        }
        finally { Marshal.FreeHGlobal(buf); }
    }

    static string IsAcOfToken(IntPtr token)
    {
        IntPtr buf = Marshal.AllocHGlobal(4);
        try
        {
            uint ret;
            if (!GetTokenInformation(token, TokenIsAppContainer, buf, 4, out ret))
                return "?err=" + Marshal.GetLastWin32Error();
            return Marshal.ReadInt32(buf) != 0 ? "1" : "0";
        }
        finally { Marshal.FreeHGlobal(buf); }
    }

    /// Launch `exe` with `cmdline`, stdout+stderr to `logPath`.
    ///
    /// `acSidStr` empty => an ordinary child (the unconfined baseline: the SAME code path minus the
    /// one attribute, which is what makes it a control rather than a different program).
    /// `capSidCsv` non-empty => those capability SIDs are granted (the UNC arms need
    /// `privateNetworkClientServer`; every other arm passes none, i.e. count zero as before).
    /// `deletePrivCsv` non-empty => the child runs on a `CreateRestrictedToken` of this process's
    /// token with those privileges DELETED, launched via `CreateProcessAsUserW`.
    ///
    /// The returned string carries the child token's `isAC` and privilege list, read back from the
    /// process that actually ran. A privilege differential asserted from the API call alone would
    /// be an assumption; this makes it an observation.
    public static string Launch(string acSidStr, string exe, string cmdline, string cwd,
        string logPath, uint timeoutMs, string capSidCsv, string deletePrivCsv)
    {
        IntPtr acSid = IntPtr.Zero;
        IntPtr attrList = IntPtr.Zero;
        IntPtr capsBuf = IntPtr.Zero;
        IntPtr capArray = IntPtr.Zero;
        IntPtr privBuf = IntPtr.Zero;
        IntPtr restricted = IntPtr.Zero;
        IntPtr srcToken = IntPtr.Zero;
        IntPtr hOut = new IntPtr(-1), hIn = new IntPtr(-1);
        var capSids = new System.Collections.Generic.List<IntPtr>();
        try
        {
            SECURITY_ATTRIBUTES sa = new SECURITY_ATTRIBUTES();
            sa.nLength = Marshal.SizeOf(typeof(SECURITY_ATTRIBUTES));
            sa.bInheritHandle = 1;
            // The log handle is opened by the UNCONFINED parent and inherited already-open. Access
            // is checked at open, so the child writes its table even when it can read nothing at
            // all — the difference between a negative result and no result.
            hOut = CreateFileW(logPath, GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE,
                ref sa, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, IntPtr.Zero);
            if (hOut == new IntPtr(-1)) return "launch-error CreateFileW(log) err=" + Marshal.GetLastWin32Error();
            hIn = CreateFileW("NUL", GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE,
                ref sa, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, IntPtr.Zero);
            if (hIn == new IntPtr(-1)) return "launch-error CreateFileW(NUL) err=" + Marshal.GetLastWin32Error();

            bool confined = !string.IsNullOrEmpty(acSidStr);
            bool asUser = !string.IsNullOrEmpty(deletePrivCsv);
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
                if (!string.IsNullOrEmpty(capSidCsv))
                {
                    foreach (string one in capSidCsv.Split(','))
                    {
                        string t = one.Trim();
                        if (t.Length == 0) continue;
                        IntPtr cs;
                        if (!ConvertStringSidToSidW(t, out cs))
                            return "launch-error ConvertStringSidToSid(cap " + t + ") err=" + Marshal.GetLastWin32Error();
                        capSids.Add(cs);
                    }
                    int stride = Marshal.SizeOf(typeof(SID_AND_ATTRIBUTES));
                    capArray = Marshal.AllocHGlobal(stride * capSids.Count);
                    for (int i = 0; i < capSids.Count; i++)
                    {
                        SID_AND_ATTRIBUTES saa = new SID_AND_ATTRIBUTES();
                        saa.Sid = capSids[i];
                        saa.Attributes = SE_GROUP_ENABLED;
                        Marshal.StructureToPtr(saa, new IntPtr(capArray.ToInt64() + (long)i * stride), false);
                    }
                    caps.Capabilities = capArray;
                    caps.CapabilityCount = (uint)capSids.Count;
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

            if (asUser)
            {
                if (!OpenProcessToken(GetCurrentProcess(),
                        TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_ASSIGN_PRIMARY, out srcToken))
                    return "launch-error OpenProcessToken err=" + Marshal.GetLastWin32Error();
                var luids = new System.Collections.Generic.List<LUID_AND_ATTRIBUTES>();
                foreach (string one in deletePrivCsv.Split(','))
                {
                    string t = one.Trim();
                    // "-" means: take the CreateProcessAsUser + CreateRestrictedToken path but
                    // delete NOTHING. That is the control arm of the privilege differential — it
                    // holds the launch mechanism constant so a failure in the treatment arm is
                    // attributable to the missing privilege rather than to CreateProcessAsUser.
                    if (t.Length == 0 || t == "-") continue;
                    LUID l;
                    if (!LookupPrivilegeValueW(null, t, out l))
                        return "launch-error LookupPrivilegeValue(" + t + ") err=" + Marshal.GetLastWin32Error();
                    LUID_AND_ATTRIBUTES la = new LUID_AND_ATTRIBUTES();
                    la.Luid = l;
                    la.Attributes = 0;
                    luids.Add(la);
                }
                int pstride = Marshal.SizeOf(typeof(LUID_AND_ATTRIBUTES));
                if (luids.Count > 0)
                {
                    privBuf = Marshal.AllocHGlobal(pstride * luids.Count);
                    for (int i = 0; i < luids.Count; i++)
                        Marshal.StructureToPtr(luids[i], new IntPtr(privBuf.ToInt64() + (long)i * pstride), false);
                }
                if (!CreateRestrictedToken(srcToken, 0, 0, IntPtr.Zero,
                        (uint)luids.Count, privBuf, 0, IntPtr.Zero, out restricted))
                    return "launch-error CreateRestrictedToken err=" + Marshal.GetLastWin32Error();
            }

            PROCESS_INFORMATION pi;
            StringBuilder cl = new StringBuilder(cmdline, cmdline.Length + 64);
            bool ok = asUser
                ? CreateProcessAsUserW(restricted, exe, cl, IntPtr.Zero, IntPtr.Zero, true, flags,
                    IntPtr.Zero, cwd, ref si, out pi)
                : CreateProcessW(exe, cl, IntPtr.Zero, IntPtr.Zero, true, flags,
                    IntPtr.Zero, cwd, ref si, out pi);
            if (!ok)
                return (asUser ? "launch-error CreateProcessAsUserW err=" : "launch-error CreateProcessW err=")
                    + Marshal.GetLastWin32Error();

            // Read the token back off the process that ACTUALLY ran, before it can exit.
            string tokInfo = "token=?";
            IntPtr childTok;
            if (OpenProcessToken(pi.hProcess, TOKEN_QUERY, out childTok))
            {
                tokInfo = "isAC=" + IsAcOfToken(childTok) + " privs=[" + PrivsOfToken(childTok) + "]";
                CloseHandle(childTok);
            }
            else tokInfo = "token=unreadable err=" + Marshal.GetLastWin32Error();

            uint wr = WaitForSingleObject(pi.hProcess, timeoutMs);
            uint code = 0xFFFFFFFF;
            string extra = "";
            if (wr == WAIT_TIMEOUT) { TerminateProcess(pi.hProcess, 0xDEAD); extra = " TIMED-OUT"; }
            else GetExitCodeProcess(pi.hProcess, out code);
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
            return "rc=" + code + " (0x" + code.ToString("x8") + ")" + extra + " " + tokInfo;
        }
        finally
        {
            if (hOut != new IntPtr(-1)) CloseHandle(hOut);
            if (hIn != new IntPtr(-1)) CloseHandle(hIn);
            if (attrList != IntPtr.Zero) { DeleteProcThreadAttributeList(attrList); Marshal.FreeHGlobal(attrList); }
            if (capsBuf != IntPtr.Zero) Marshal.FreeHGlobal(capsBuf);
            if (capArray != IntPtr.Zero) Marshal.FreeHGlobal(capArray);
            if (privBuf != IntPtr.Zero) Marshal.FreeHGlobal(privBuf);
            foreach (IntPtr cs in capSids) LocalFree(cs);
            if (restricted != IntPtr.Zero) CloseHandle(restricted);
            if (srcToken != IntPtr.Zero) CloseHandle(srcToken);
            if (acSid != IntPtr.Zero) LocalFree(acSid);
        }
    }
}
'@

Add-Type -TypeDefinition $acJailSrc -Language CSharp -ErrorAction Stop

# ── ACE plumbing, identical in shape to `tests/win-bypass-traverse/probe.ps1` ──
# Inheritable so an already-existing subtree picks the grant up; every write lands on a directory
# the invoking user owns or created, which needs no privilege.
function Grant-AcAce([string]$path, [string]$sid, [string]$rights) {
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

function Revoke-AcAce([string]$path, [string]$sid) {
  $acl = Get-Acl -LiteralPath $path
  $trustee = New-Object System.Security.Principal.SecurityIdentifier($sid)
  $null = $acl.PurgeAccessRules($trustee)
  Set-Acl -LiteralPath $path -AclObject $acl
}

# The mandatory per-arm read-back: did the inheritable ACE physically REACH the decisive target?
# Without this a propagation slip is indistinguishable from a kernel denial — the single control
# that makes a table of failures interpretable.
function Get-AceRightsFor([string]$path, [string]$sid) {
  try {
    $found = @((Get-Acl -LiteralPath $path).Access |
      Where-Object { $_.IdentityReference.Value -eq $sid } |
      ForEach-Object { "$($_.FileSystemRights)" })
    if ($found.Count) { return ($found -join ',') }
    return 'none'
  } catch { return "unreadable:$($_.Exception.GetType().Name)" }
}

# Every ACE's trustee AS A SID. `GetAccessRules(…, [SecurityIdentifier])` asks the framework to hand
# back SID-form identities rather than translating `NTAccount` strings afterwards — RUN 1 IS WHY:
# `IdentityReference.Translate([SecurityIdentifier])` silently produced no SIDs, so
# `has-all-application-packages` read False for `C:\Program Files` while the printed ACE list plainly
# showed both app-package ACEs on it. The derived boolean lied; only the raw dump caught it. Both are
# emitted for exactly that reason — a derived cell must always be checkable against the raw one.
function Get-DaclSids($acl) {
  if ($null -eq $acl) { return @() }
  try {
    return @($acl.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier]) |
      ForEach-Object { $_.IdentityReference.Value })
  } catch { return @("translate-failed:$($_.Exception.GetType().Name)") }
}

# Structured DACL dump: every ACE with its trustee, rights, inheritance and whether it was
# inherited, plus the owner and whether the DACL is PROTECTED (inheritance disabled). The MSI
# question turns on exactly those last two: a `D:PAI` descriptor is why nothing propagates in from
# `C:\Program Files`.
function Show-Dacl([string]$label, [string]$path) {
  if (-not (Test-Path -LiteralPath $path)) { Fact "dacl/$label" "ABSENT $path"; return $null }
  try { $acl = Get-Acl -LiteralPath $path } catch { Fact "dacl/$label" "UNREADABLE $path : $_"; return $null }
  Fact "dacl/$label/path" $path
  Fact "dacl/$label/owner" $acl.Owner
  Fact "dacl/$label/protected" $acl.AreAccessRulesProtected
  foreach ($a in $acl.Access) {
    W ("    ace[$label] " + $a.IdentityReference.Value + " | " + $a.AccessControlType + " | " +
       $a.FileSystemRights + " | inherit=" + $a.InheritanceFlags + " | inherited=" + $a.IsInherited)
  }
  $sids = Get-DaclSids $acl
  Fact "dacl/$label/sids" ($sids -join ',')
  Fact "dacl/$label/has-all-application-packages" ($sids -contains 'S-1-15-2-1')
  Fact "dacl/$label/has-all-restricted-application-packages" ($sids -contains 'S-1-15-2-2')
  Fact "dacl/$label/any-appcontainer-sid" (@($sids | Where-Object { $_ -like 'S-1-15-2-*' }).Count)
  return $acl
}
