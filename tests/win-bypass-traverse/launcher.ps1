# The AppContainer launcher, shared by every probe in this directory.
#
# Extracted from `probe.ps1` unchanged so a SECOND probe (`tools.ps1`, the invoked-tool startup
# matrix) launches through the byte-identical code path. Two copies of a P/Invoke launcher is how
# you get two probes that disagree for a reason no one can find — and this file specifically has
# already cost this effort a run to a marshalling bug (see the CharSet.Unicode note below).
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

    [StructLayout(LayoutKind.Sequential)]
    struct SID_AND_ATTRIBUTES { public IntPtr Sid; public uint Attributes; }

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
    // ProcThreadAttributeAllApplicationPackagesPolicy(15) | PROC_THREAD_ATTRIBUTE_INPUT(0x20000).
    // Setting it to OPT_OUT is what makes a LowBox an LPAC: the token stops honouring the
    // `ALL APPLICATION PACKAGES` aces that blanket System32 and most of Program Files, so access
    // must come from a capability instead. Microsoft's `mxc` exposes this as a per-run flag
    // defaulting to FALSE (`sdk/node/src/types.ts:157`), which is why an ordinary AppContainer and
    // an LPAC are both live hypotheses for the host-prep disagreement and must be measured, not
    // argued.
    static readonly IntPtr PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY = new IntPtr(0x0002000F);
    const uint PROCESS_CREATION_ALL_APPLICATION_PACKAGES_OPT_OUT = 1;
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
        return LaunchEx(acSidStr, exe, cmdline, cwd, logPath, timeoutMs, false);
    }

    /// As `Launch`, plus the LPAC opt-out attribute when `lpac` is set. Separate entry point so
    /// the two existing probes keep the byte-identical six-argument call they were measured with.
    public static string LaunchEx(string acSidStr, string exe, string cmdline, string cwd,
        string logPath, uint timeoutMs, bool lpac)
    {
        return LaunchCaps(acSidStr, exe, cmdline, cwd, logPath, timeoutMs, lpac, null);
    }

    /// As `LaunchEx`, plus REQUESTED CAPABILITY SIDS — the half of nub's production launch that no
    /// arm in this directory reproduced, and the reason a zero-capability arm measures a strictly
    /// WEAKER jail than the product's. `crates/nub-sandbox/src/backend/windows.rs` writes a
    /// non-inherited traverse ace where the user holds `WRITE_DAC`, and where it does not (`C:\`,
    /// `C:\Users`) it REQUESTS the capability Windows already granted on that exact path — `C:\`
    /// carries `(A;;0x1000a1;;;S-1-15-3-65536-…)`, the same traverse+read-attributes mask. Holding
    /// the capability buys that access with no ace write and NO ELEVATION.
    public static string LaunchCaps(string acSidStr, string exe, string cmdline, string cwd,
        string logPath, uint timeoutMs, bool lpac, string[] capabilitySids)
    {
        IntPtr acSid = IntPtr.Zero;
        IntPtr attrList = IntPtr.Zero;
        IntPtr capsBuf = IntPtr.Zero;
        IntPtr lpacBuf = IntPtr.Zero;
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

                int attrCount = lpac ? 2 : 1;
                IntPtr size = IntPtr.Zero;
                InitializeProcThreadAttributeList(IntPtr.Zero, attrCount, 0, ref size);
                attrList = Marshal.AllocHGlobal(size);
                if (!InitializeProcThreadAttributeList(attrList, attrCount, 0, ref size))
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
                if (lpac)
                {
                    lpacBuf = Marshal.AllocHGlobal(sizeof(uint));
                    Marshal.WriteInt32(lpacBuf, (int)PROCESS_CREATION_ALL_APPLICATION_PACKAGES_OPT_OUT);
                    if (!UpdateProcThreadAttribute(attrList, 0,
                            PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY, lpacBuf,
                            new IntPtr(sizeof(uint)), IntPtr.Zero, IntPtr.Zero))
                        return "launch-error UpdateProcThreadAttribute(LPAC) err=" + Marshal.GetLastWin32Error();
                }
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
            if (lpacBuf != IntPtr.Zero) Marshal.FreeHGlobal(lpacBuf);
            if (acSid != IntPtr.Zero) LocalFree(acSid);
        }
    }
}
'@

Add-Type -TypeDefinition $src -Language CSharp -ErrorAction Stop
