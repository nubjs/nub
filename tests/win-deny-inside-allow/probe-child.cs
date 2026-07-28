// Minimal AppContainer child for the nub Windows AppContainer file-access probes.
// Tiny load surface (no PowerShell, no heavy CLR module probing) so an AppContainer
// child starts cleanly and the probe measures the SECURITY outcome, not a host-init crash.
//
// Usage + exit-code contract (read by the parent probe):
//   probe-child.exe whoami <dumpPath>  -> 0 ; dumps its own token (IsAppContainer, AC SID,
//                                        integrity level, package/capability groups AND every
//                                        token PRIVILEGE with its enabled state) to <dumpPath>
//                                        so the parent can PROVE the child is really in the
//                                        AppContainer and inspect SeChangeNotifyPrivilege.
//   probe-child.exe read   <path>  -> 0 read OK | 5 ACCESS_DENIED | 9 other error
//   probe-child.exe write  <path>  -> 0 write OK | 5 ACCESS_DENIED | 9 other error
//   probe-child.exe getenv <NAME>  -> 0 present (prints value) | 4 absent
//
// Anything that prints "CHILD ..." goes to stdout so the parent's captured log shows it.
using System;
using System.IO;
using System.Runtime.InteropServices;
using System.Security.Principal;
using System.Text;

static class ProbeChild {
    [DllImport("kernel32.dll")] static extern IntPtr GetCurrentProcess();
    [DllImport("advapi32.dll", SetLastError=true)] static extern bool OpenProcessToken(IntPtr h, uint access, out IntPtr tok);
    [DllImport("advapi32.dll", SetLastError=true)] static extern bool GetTokenInformation(IntPtr tok, int cls, IntPtr buf, int len, out int ret);
    [DllImport("advapi32.dll", SetLastError=true, CharSet=CharSet.Unicode)] static extern bool ConvertSidToStringSid(IntPtr sid, out IntPtr str);
    [DllImport("advapi32.dll", SetLastError=true, CharSet=CharSet.Unicode)] static extern bool LookupPrivilegeName(string sys, ref LUID luid, StringBuilder name, ref int cch);
    [DllImport("kernel32.dll")] static extern IntPtr LocalFree(IntPtr h);
    // -- ascendant-env read surface (openparent) --
    [DllImport("kernel32.dll", SetLastError=true)] static extern IntPtr OpenProcess(uint access, bool inherit, uint pid);
    [DllImport("kernel32.dll", SetLastError=true)] static extern bool CloseHandle(IntPtr h);
    [DllImport("kernel32.dll", SetLastError=true)] static extern bool ReadProcessMemory(IntPtr h, IntPtr addr, byte[] buf, IntPtr size, out IntPtr read);
    [DllImport("ntdll.dll")] static extern int NtQueryInformationProcess(IntPtr h, int cls, ref PROCESS_BASIC_INFORMATION info, int len, out int ret);
    const uint PROCESS_VM_READ = 0x0010, PROCESS_QUERY_LIMITED_INFORMATION = 0x1000;
    [StructLayout(LayoutKind.Sequential)] struct PROCESS_BASIC_INFORMATION {
        public IntPtr ExitStatus, PebBaseAddress, AffinityMask, BasePriority, UniqueProcessId, InheritedFromUniqueProcessId;
    }

    const uint TOKEN_QUERY = 0x0008;
    const int TokenPrivileges = 3, TokenIntegrityLevel = 25, TokenIsAppContainer = 29, TokenAppContainerSid = 31;
    const uint SE_PRIVILEGE_ENABLED = 0x2, SE_PRIVILEGE_ENABLED_BY_DEFAULT = 0x1;

    [StructLayout(LayoutKind.Sequential)] struct LUID { public uint Low; public int High; }
    [StructLayout(LayoutKind.Sequential)] struct LUID_AND_ATTRIBUTES { public LUID Luid; public uint Attributes; }

    static int GetDword(IntPtr tok, int cls) {
        IntPtr buf = Marshal.AllocHGlobal(4);
        try { int ret; return GetTokenInformation(tok, cls, buf, 4, out ret) ? Marshal.ReadInt32(buf) : -1; }
        finally { Marshal.FreeHGlobal(buf); }
    }
    static string SidStr(IntPtr sid) {
        if (sid == IntPtr.Zero) return "<null>";
        IntPtr s; if (!ConvertSidToStringSid(sid, out s)) return "<convfail " + Marshal.GetLastWin32Error() + ">";
        string r = Marshal.PtrToStringUni(s); LocalFree(s); return r;
    }
    // The first pointer-sized field of TOKEN_APPCONTAINER_INFORMATION and TOKEN_MANDATORY_LABEL.Label
    // is a PSID, so one helper reads both the AppContainer SID and the integrity-level SID.
    static string GetLeadingSid(IntPtr tok, int cls) {
        int len; GetTokenInformation(tok, cls, IntPtr.Zero, 0, out len);
        if (len <= 0) return "<none/err " + Marshal.GetLastWin32Error() + ">";
        IntPtr buf = Marshal.AllocHGlobal(len);
        try {
            if (!GetTokenInformation(tok, cls, buf, len, out len)) return "<err " + Marshal.GetLastWin32Error() + ">";
            return SidStr(Marshal.ReadIntPtr(buf));
        } finally { Marshal.FreeHGlobal(buf); }
    }
    // Enumerate TOKEN_PRIVILEGES and report each privilege name + its enabled state. This is the
    // direct evidence for the traverse-bypass question: SeChangeNotifyPrivilege (Bypass Traverse
    // Checking) present + enabled on the LowBox token means intermediate-dir ACLs are not checked.
    static void DumpPrivileges(IntPtr tok, Action<string> emit) {
        int len; GetTokenInformation(tok, TokenPrivileges, IntPtr.Zero, 0, out len);
        if (len <= 0) { emit("CHILD priv <none/err " + Marshal.GetLastWin32Error() + ">"); return; }
        IntPtr buf = Marshal.AllocHGlobal(len);
        try {
            if (!GetTokenInformation(tok, TokenPrivileges, buf, len, out len)) { emit("CHILD priv <err " + Marshal.GetLastWin32Error() + ">"); return; }
            int count = Marshal.ReadInt32(buf);
            emit("CHILD priv count=" + count);
            int recSz = Marshal.SizeOf(typeof(LUID_AND_ATTRIBUTES));
            long baseAddr = buf.ToInt64() + 4; // skip the leading PrivilegeCount DWORD
            for (int i = 0; i < count; i++) {
                var la = (LUID_AND_ATTRIBUTES)Marshal.PtrToStructure((IntPtr)(baseAddr + i * recSz), typeof(LUID_AND_ATTRIBUTES));
                LUID luid = la.Luid;
                int cch = 0; LookupPrivilegeName(null, ref luid, null, ref cch);
                var sb2 = new StringBuilder(cch + 1); int cch2 = cch + 1;
                string name = LookupPrivilegeName(null, ref luid, sb2, ref cch2) ? sb2.ToString() : ("<luid " + la.Luid.Low + ">");
                bool enabled = (la.Attributes & SE_PRIVILEGE_ENABLED) != 0;
                bool enabledByDefault = (la.Attributes & SE_PRIVILEGE_ENABLED_BY_DEFAULT) != 0;
                emit("CHILD priv " + name + " enabled=" + enabled + " enabledByDefault=" + enabledByDefault + " attr=0x" + la.Attributes.ToString("X"));
            }
        } finally { Marshal.FreeHGlobal(buf); }
    }
    // An AppContainer child's stdout is NOT reliably captured by the parent's redirected log, so the
    // token dump is TEE'd to outPath (a file in a dir the AC SID was granted write) -- that file is
    // the reliable diagnostic channel the parent reads back.
    static void DumpToken(string outPath) {
        var sb = new StringBuilder();
        Action<string> emit = line => { Console.WriteLine(line); sb.AppendLine(line); };
        try {
            emit("CHILD whoami user=" + WindowsIdentity.GetCurrent().Name);
            IntPtr tok;
            if (!OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, out tok)) {
                emit("CHILD whoami OpenProcessToken failed " + Marshal.GetLastWin32Error());
            } else {
                emit("CHILD whoami TokenIsAppContainer=" + GetDword(tok, TokenIsAppContainer));
                emit("CHILD whoami TokenAppContainerSid=" + GetLeadingSid(tok, TokenAppContainerSid));
                emit("CHILD whoami IntegrityLevelSid=" + GetLeadingSid(tok, TokenIntegrityLevel));
                foreach (IdentityReference g in WindowsIdentity.GetCurrent().Groups) {
                    string s = g.Value;
                    if (s.StartsWith("S-1-15-2") || s.StartsWith("S-1-15-3") || s.StartsWith("S-1-16"))
                        emit("CHILD whoami group=" + s);
                }
                DumpPrivileges(tok, emit);
            }
        } catch (Exception e) { emit("CHILD whoami ERR: " + e.Message); }
        if (outPath != null) {
            try { File.WriteAllText(outPath, sb.ToString()); }
            catch (Exception e) { Console.WriteLine("CHILD whoami could not write dump to " + outPath + ": " + e.Message); }
        }
    }

    // Read a pointer-sized value from another process.
    static IntPtr ReadPtr(IntPtr h, long addr) {
        byte[] b = new byte[IntPtr.Size]; IntPtr rd;
        if (!ReadProcessMemory(h, (IntPtr)addr, b, (IntPtr)b.Length, out rd) || rd.ToInt64() != b.Length) return IntPtr.Zero;
        return (IntPtr)BitConverter.ToInt64(b, 0);
    }
    // Ascendant-env read: OpenProcess(PROCESS_VM_READ|QUERY_LIMITED) on `pid`, walk the PEB
    // to the environment block, and search it for `secret`. Exit contract:
    //   5 = OpenProcess DENIED (the confinement result -- no handle, nothing readable)
    //   0 = secret READ from the target's env (LEAK)
    //   6 = handle GRANTED but the PEB/env read failed (OpenProcess NOT blocked -- concern)
    //   7 = handle granted + env read but secret not present (control would fail if this hits)
    static int OpenParent(uint pid, string secret) {
        IntPtr h = OpenProcess(PROCESS_VM_READ | PROCESS_QUERY_LIMITED_INFORMATION, false, pid);
        if (h == IntPtr.Zero) {
            Console.WriteLine("CHILD openparent OpenProcess DENIED err=" + Marshal.GetLastWin32Error());
            return 5;
        }
        try {
            var pbi = new PROCESS_BASIC_INFORMATION(); int ret;
            int st = NtQueryInformationProcess(h, 0, ref pbi, Marshal.SizeOf(typeof(PROCESS_BASIC_INFORMATION)), out ret);
            if (st != 0 || pbi.PebBaseAddress == IntPtr.Zero) { Console.WriteLine("CHILD openparent NtQIP fail st=0x" + st.ToString("X")); return 6; }
            // x64 offsets: PEB+0x20 = ProcessParameters; RTL_USER_PROCESS_PARAMETERS+0x80 = Environment.
            IntPtr pp = ReadPtr(h, pbi.PebBaseAddress.ToInt64() + 0x20);
            if (pp == IntPtr.Zero) { Console.WriteLine("CHILD openparent read ProcessParameters fail"); return 6; }
            IntPtr env = ReadPtr(h, pp.ToInt64() + 0x80);
            if (env == IntPtr.Zero) { Console.WriteLine("CHILD openparent read Environment ptr fail"); return 6; }
            var sb = new StringBuilder();
            byte[] chunk = new byte[4096]; IntPtr rd;
            for (int i = 0; i < 32; i++) { // up to 128 KB, stop at the first unreadable page
                if (!ReadProcessMemory(h, (IntPtr)(env.ToInt64() + i * 4096), chunk, (IntPtr)chunk.Length, out rd) || rd.ToInt64() == 0) break;
                sb.Append(Encoding.Unicode.GetString(chunk, 0, (int)rd.ToInt64()));
            }
            if (sb.ToString().Contains(secret)) { Console.WriteLine("CHILD openparent SECRET READ (LEAK)"); return 0; }
            Console.WriteLine("CHILD openparent opened+read but secret NOT found (chars=" + sb.Length + ")"); return 7;
        } catch (Exception e) { Console.WriteLine("CHILD openparent ERR: " + e.Message); return 9; }
        finally { CloseHandle(h); }
    }

    static int Main(string[] a) {
        try {
            if (a.Length < 1) { Console.WriteLine("CHILD bad-args"); return 2; }
            if (a[0] == "whoami") { DumpToken(a.Length >= 2 ? a[1] : null); return 0; }
            if (a.Length < 2) { Console.WriteLine("CHILD bad-args"); return 2; }
            switch (a[0]) {
                case "read": {
                    try {
                        string s = File.ReadAllText(a[1]);
                        Console.WriteLine("CHILD read OK len=" + s.Length);
                        return 0;
                    } catch (UnauthorizedAccessException e) {
                        Console.WriteLine("CHILD read DENIED: " + e.Message); return 5;
                    } catch (Exception e) {
                        Console.WriteLine("CHILD read ERR: " + e.GetType().Name + " " + e.Message); return 9;
                    }
                }
                case "write": {
                    try {
                        File.WriteAllText(a[1], "from-appcontainer-child");
                        Console.WriteLine("CHILD write OK");
                        return 0;
                    } catch (UnauthorizedAccessException e) {
                        Console.WriteLine("CHILD write DENIED: " + e.Message); return 5;
                    } catch (Exception e) {
                        Console.WriteLine("CHILD write ERR: " + e.GetType().Name + " " + e.Message); return 9;
                    }
                }
                case "getenv": {
                    string v = Environment.GetEnvironmentVariable(a[1]);
                    if (v == null) { Console.WriteLine("CHILD env ABSENT"); return 4; }
                    Console.WriteLine("CHILD env PRESENT: " + v); return 0;
                }
                case "openparent": {
                    // openparent <pid> <secret> -- read the target process's env for <secret>.
                    if (a.Length < 3) { Console.WriteLine("CHILD openparent bad-args"); return 2; }
                    uint pid; if (!uint.TryParse(a[1], out pid)) { Console.WriteLine("CHILD openparent bad-pid"); return 2; }
                    return OpenParent(pid, a[2]);
                }
                default:
                    Console.WriteLine("CHILD unknown-cmd"); return 2;
            }
        } catch (Exception e) {
            Console.WriteLine("CHILD FATAL: " + e); return 3;
        }
    }
}
