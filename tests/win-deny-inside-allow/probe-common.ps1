# Shared helpers for the nub Windows "deny-inside-allow" ground-truth probes.
#
# Derived from tests/win-appcontainer-probe/probe-common.ps1 (branch probe/win-appcontainer),
# extended with an LPAC (Less Privileged AppContainer) launch mode so the deny-ACE question can be
# asked of the LPAC principal too, not just the ordinary AppContainer principal.
#
# AC: AppContainer process launcher (CreateProcess + PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
#     optionally + PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY for LPAC opt-out).
# Build-ProbeChild <outDir>: compile probe-child.exe into <outDir> and return its path.
#
# ASCII ONLY. PowerShell 5.1 mis-decodes non-ASCII bytes in .ps1/.cs files.

$ErrorActionPreference = 'Stop'

function Build-ProbeChild([string]$outDir) {
    if (-not (Test-Path $outDir)) { New-Item -ItemType Directory -Path $outDir -Force | Out-Null }
    $exe = Join-Path $outDir 'probe-child.exe'
    if (Test-Path $exe) { return $exe }
    $src = Join-Path $PSScriptRoot 'probe-child.cs'
    if (-not (Test-Path $src)) { throw "probe-child.cs not found at $src" }
    $csc = Join-Path ([System.Runtime.InteropServices.RuntimeEnvironment]::GetRuntimeDirectory()) 'csc.exe'
    if (-not (Test-Path $csc)) { throw "csc.exe not found at $csc" }
    & $csc /nologo /target:exe "/out:$exe" $src 2>&1 | Write-Host
    if (-not (Test-Path $exe)) { throw "probe-child.exe failed to compile" }
    return $exe
}

# Compile the CLR-free native reader (see probe-reader.rs for why it is needed).
function Build-ProbeReader([string]$outDir) {
    if (-not (Test-Path $outDir)) { New-Item -ItemType Directory -Path $outDir -Force | Out-Null }
    $exe = Join-Path $outDir 'probe-reader.exe'
    if (Test-Path $exe) { return $exe }
    $src = Join-Path $PSScriptRoot 'probe-reader.rs'
    if (-not (Test-Path $src)) { throw "probe-reader.rs not found at $src" }
    & rustc -O -o $exe $src 2>&1 | Write-Host
    if (-not (Test-Path $exe)) { throw "probe-reader.exe failed to compile" }
    return $exe
}

Add-Type -Language CSharp -TypeDefinition @"
using System; using System.Runtime.InteropServices; using System.ComponentModel;
public static class AC {
    [DllImport("userenv.dll", CharSet=CharSet.Unicode)] public static extern int CreateAppContainerProfile(string n,string d,string desc,IntPtr c,int cc,out IntPtr sid);
    [DllImport("userenv.dll", CharSet=CharSet.Unicode)] public static extern int DeleteAppContainerProfile(string n);
    // CharSet=Unicode is LOAD-BEARING: without it these bind to the ANSI *A variants and the SID
    // string is read back with the wrong encoding, yielding a garbled SID.
    [DllImport("advapi32.dll", SetLastError=true, CharSet=CharSet.Unicode)] public static extern bool ConvertStringSidToSid(string s, out IntPtr sid);
    [DllImport("advapi32.dll", SetLastError=true, CharSet=CharSet.Unicode)] public static extern bool ConvertSidToStringSid(IntPtr Sid, out IntPtr s);
    [DllImport("kernel32.dll")] public static extern IntPtr LocalFree(IntPtr h);

    [StructLayout(LayoutKind.Sequential)] public struct SID_AND_ATTRIBUTES { public IntPtr Sid; public uint Attributes; }
    [StructLayout(LayoutKind.Sequential)] public struct SECURITY_CAPABILITIES { public IntPtr AppContainerSid; public IntPtr Capabilities; public int CapabilityCount; public int Reserved; }
    [StructLayout(LayoutKind.Sequential)] public struct STARTUPINFO { public int cb; public string r1; public string desk; public string title; public int dwX,dwY,dwXS,dwYS,dwXC,dwYC,dwFill,dwFlags; public short wShow,cbR2; public IntPtr r2,hIn,hOut,hErr; }
    [StructLayout(LayoutKind.Sequential)] public struct STARTUPINFOEX { public STARTUPINFO si; public IntPtr lpAttributeList; }
    [StructLayout(LayoutKind.Sequential)] public struct PROCESS_INFORMATION { public IntPtr hProcess,hThread; public int pid,tid; }
    [DllImport("kernel32.dll", SetLastError=true)] public static extern bool InitializeProcThreadAttributeList(IntPtr l,int c,int f,ref IntPtr s);
    [DllImport("kernel32.dll", SetLastError=true)] public static extern bool UpdateProcThreadAttribute(IntPtr l,uint f,IntPtr a,IntPtr v,IntPtr cb,IntPtr p,IntPtr r);
    [DllImport("kernel32.dll", SetLastError=true)] public static extern void DeleteProcThreadAttributeList(IntPtr l);
    [DllImport("kernel32.dll")] public static extern IntPtr GetProcessHeap();
    [DllImport("kernel32.dll")] public static extern IntPtr HeapAlloc(IntPtr h,uint f,IntPtr b);
    [DllImport("kernel32.dll")] public static extern bool HeapFree(IntPtr h,uint f,IntPtr m);
    [DllImport("kernel32.dll", SetLastError=true, CharSet=CharSet.Unicode)] public static extern bool CreateProcess(string app,string cmd,IntPtr pa,IntPtr ta,bool inh,uint flags,IntPtr env,string cwd,ref STARTUPINFOEX si,out PROCESS_INFORMATION pi);
    [DllImport("kernel32.dll", SetLastError=true)] public static extern uint WaitForSingleObject(IntPtr h,uint ms);
    [DllImport("kernel32.dll", SetLastError=true)] public static extern bool GetExitCodeProcess(IntPtr h,out uint c);
    [DllImport("kernel32.dll", SetLastError=true)] public static extern bool CloseHandle(IntPtr h);
    const uint EXTENDED=0x80000; const uint UNICODE_ENV=0x400;
    static readonly IntPtr ATTR_SECCAP=(IntPtr)0x00020009;
    // PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY. Setting it to OPT_OUT makes the
    // process an LPAC: ALL APPLICATION PACKAGES grants stop applying, so only the per-run AC SID
    // and ALL RESTRICTED APPLICATION PACKAGES (S-1-15-2-2) grants are honored.
    static readonly IntPtr ATTR_AAP_POLICY=(IntPtr)0x0002000F;
    const uint ALL_APPLICATION_PACKAGES_OPT_OUT=0x1;
    const uint SE_GROUP_ENABLED=0x4;

    public static string SidToString(IntPtr sid){ IntPtr s; ConvertSidToStringSid(sid,out s); string r=Marshal.PtrToStringUni(s); LocalFree(s); return r; }

    // Launch cmd under AppContainer acSid. caps = capability SID strings, or null for none.
    // lpac=true adds the ALL_APPLICATION_PACKAGES opt-out attribute (LPAC). Returns child exit code.
    // Throws Win32Exception if the process could not be created at all -- the caller MUST treat
    // that as INCONCLUSIVE, never as a deny.
    public static uint LaunchEx(IntPtr acSid, string[] caps, bool lpac, string cmd, string cwd){
        var sc=new SECURITY_CAPABILITIES(); sc.AppContainerSid=acSid;
        IntPtr capsPtr=IntPtr.Zero; int capCount = (caps==null)?0:caps.Length;
        if(capCount>0){
            int sz=Marshal.SizeOf(typeof(SID_AND_ATTRIBUTES));
            capsPtr=Marshal.AllocHGlobal(sz*capCount);
            for(int i=0;i<capCount;i++){
                IntPtr capSid; if(!ConvertStringSidToSid(caps[i], out capSid)) throw new Win32Exception(Marshal.GetLastWin32Error(),"capSid "+caps[i]);
                var saa=new SID_AND_ATTRIBUTES(); saa.Sid=capSid; saa.Attributes=SE_GROUP_ENABLED;
                Marshal.StructureToPtr(saa,(IntPtr)(capsPtr.ToInt64()+i*sz),false);
            }
            sc.Capabilities=capsPtr;
        }
        sc.CapabilityCount=capCount;
        int attrCount = lpac ? 2 : 1;
        IntPtr size=IntPtr.Zero; InitializeProcThreadAttributeList(IntPtr.Zero,attrCount,0,ref size);
        IntPtr al=HeapAlloc(GetProcessHeap(),0,size);
        if(!InitializeProcThreadAttributeList(al,attrCount,0,ref size)) throw new Win32Exception(Marshal.GetLastWin32Error(),"Init");
        IntPtr scP=Marshal.AllocHGlobal(Marshal.SizeOf(typeof(SECURITY_CAPABILITIES))); Marshal.StructureToPtr(sc,scP,false);
        if(!UpdateProcThreadAttribute(al,0,ATTR_SECCAP,scP,(IntPtr)Marshal.SizeOf(typeof(SECURITY_CAPABILITIES)),IntPtr.Zero,IntPtr.Zero)) throw new Win32Exception(Marshal.GetLastWin32Error(),"Update SECCAP");
        IntPtr lpacP=IntPtr.Zero;
        if(lpac){
            lpacP=Marshal.AllocHGlobal(4); Marshal.WriteInt32(lpacP,(int)ALL_APPLICATION_PACKAGES_OPT_OUT);
            if(!UpdateProcThreadAttribute(al,0,ATTR_AAP_POLICY,lpacP,(IntPtr)4,IntPtr.Zero,IntPtr.Zero)) throw new Win32Exception(Marshal.GetLastWin32Error(),"Update AAP_POLICY");
        }
        var si=new STARTUPINFOEX(); si.si.cb=Marshal.SizeOf(typeof(STARTUPINFOEX)); si.lpAttributeList=al;
        PROCESS_INFORMATION pi;
        bool ok=CreateProcess(null,cmd,IntPtr.Zero,IntPtr.Zero,false,EXTENDED|UNICODE_ENV,IntPtr.Zero,cwd,ref si,out pi);
        int err=Marshal.GetLastWin32Error();
        DeleteProcThreadAttributeList(al); HeapFree(GetProcessHeap(),0,al); Marshal.FreeHGlobal(scP);
        if(lpacP!=IntPtr.Zero) Marshal.FreeHGlobal(lpacP);
        if(capsPtr!=IntPtr.Zero) Marshal.FreeHGlobal(capsPtr);
        if(!ok) throw new Win32Exception(err,"CreateProcess");
        WaitForSingleObject(pi.hProcess,60000); uint code; GetExitCodeProcess(pi.hProcess,out code);
        CloseHandle(pi.hProcess); CloseHandle(pi.hThread);
        return code;
    }
    public static uint Launch(IntPtr acSid, string cmd, string cwd){ return LaunchEx(acSid,null,false,cmd,cwd); }
}
"@
