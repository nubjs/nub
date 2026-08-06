using System;
using System.Runtime.InteropServices;

// Reads the RUNTIME kernel code-integrity state, which is the only thing that
// decides whether a self-signed driver can load. The BCD `testsigning` value is
// a boot-time REQUEST and can disagree with what the running kernel enforces.
public static class CI
{
    [StructLayout(LayoutKind.Sequential)]
    public struct SCII
    {
        public uint Length;
        public uint CodeIntegrityOptions;
    }

    [DllImport("ntdll.dll")]
    private static extern int NtQuerySystemInformation(int cls, ref SCII info, int len, out int ret);

    // SystemCodeIntegrityInformation == 103
    public static uint Options()
    {
        SCII i = new SCII();
        i.Length = (uint)Marshal.SizeOf(typeof(SCII));
        int r;
        int st = NtQuerySystemInformation(103, ref i, Marshal.SizeOf(typeof(SCII)), out r);
        if (st != 0) { throw new Exception("NTSTATUS 0x" + st.ToString("X8")); }
        return i.CodeIntegrityOptions;
    }
}
