// The child for every arm of the same-package loopback probe (W1).
//
// ONE PROGRAM FOR EVERY ARM. The listener and the connector are modes of this binary, and the
// plain and AppContainer arms run the identical image -- so an arm difference is a token
// difference and nothing else.
//
// Why a compiled .NET Framework console exe rather than `node`:
//
//   1. IT REPORTS ITS OWN TOKEN. MECHANISM-FACTS 5i: `tests/win-bypass-traverse/launcher.ps1`
//      asked for capabilities and passed `CapabilityCount = 0`, so every arm it ran was a
//      zero-capability arm and nothing in its output could have said so. The launcher reads the
//      child's token through its process handle; this reads the SAME four values from INSIDE the
//      child. Two independent readings, and an arm that cannot show both is not that arm.
//   2. IT REPORTS THE WINSOCK NUMBER. `SocketException.ErrorCode` is the native WSA code
//      (10013 WSAEACCES, 10060 WSAETIMEDOUT, 10061 WSAECONNREFUSED). libuv collapses distinct
//      statuses onto one errno, and the whole reason W1 is worth measuring is that the prior
//      loopback failure was `ETIMEDOUT` (a receive-side drop) where a real outbound denial was
//      `EACCES` -- a distinction the errno name barely carries and the number carries exactly.
//   3. It has no module resolution, so it cannot die in a realpath walk the way an unflagged
//      confined `node` does (MECHANICS 5h: `EPERM lstat 'C:\'` in `resolveMainPath`).
//
// `child.js` is the fallback for the same three modes, selected only if this binary cannot run
// confined. It is the PROVEN runtime (win-fsnet-ceiling ran node children in AppContainers) and
// it can answer the loopback question; it just cannot answer 1 or 2.
//
// C# 5 only -- this is compiled by the .NET Framework `csc.exe`, not Roslyn.

using System;
using System.Diagnostics;
using System.Net;
using System.Net.Sockets;
using System.Runtime.InteropServices;
using System.Text;

public static class Child
{
    [StructLayout(LayoutKind.Sequential)]
    struct SID_AND_ATTRIBUTES { public IntPtr Sid; public uint Attributes; }

    [DllImport("advapi32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool ConvertSidToStringSidW(IntPtr sid, out IntPtr str);
    [DllImport("kernel32.dll")] static extern IntPtr LocalFree(IntPtr p);
    [DllImport("kernel32.dll")] static extern IntPtr GetCurrentProcess();
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool CloseHandle(IntPtr h);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool OpenProcessToken(IntPtr proc, uint access, out IntPtr token);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool GetTokenInformation(IntPtr token, int cls, IntPtr info, uint len, out uint ret);

    const uint TOKEN_QUERY = 8;
    const int TokenIntegrityLevel = 25, TokenIsAppContainer = 29, TokenCapabilities = 30,
              TokenAppContainerSid = 31;

    static void P(string s)
    {
        // Explicit flush after every line: the parent polls this file for `listening=1` to
        // sequence the connector behind the listener, so a buffered ready line is a deadlock.
        Console.Out.WriteLine(s);
        Console.Out.Flush();
    }

    static string SidStr(IntPtr sid)
    {
        IntPtr s;
        if (sid == IntPtr.Zero || !ConvertSidToStringSidW(sid, out s)) return "?";
        string r = Marshal.PtrToStringUni(s);
        LocalFree(s);
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
            // TOKEN_GROUPS is { DWORD GroupCount; SID_AND_ATTRIBUTES Groups[]; }; on 64-bit the
            // array starts at the pointer alignment, not at offset 4.
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

    /// THE ARM'S OWN ACCOUNT OF WHAT IT IS, read from inside. Printed by every mode, first,
    /// before any socket call -- so a child that dies in the socket work still leaves proof of
    /// which token it was running under.
    static void ReportToken(string role)
    {
        P("self:role=" + role);
        P("self:pid=" + Process.GetCurrentProcess().Id);
        IntPtr tok = IntPtr.Zero;
        if (!OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, out tok))
        {
            P("self:token=ERR open=" + Marshal.GetLastWin32Error());
            return;
        }
        try
        {
            P("self:isAppContainer=" + QueryIsAc(tok));
            P("self:packageSid=" + QueryAcSid(tok));
            P("self:capabilities=[" + QueryCaps(tok) + "]");
            P("self:integrity=" + QueryIl(tok));
        }
        finally { CloseHandle(tok); }
    }

    static string SockErr(SocketException ex)
    {
        // ErrorCode IS the native Winsock number on Windows; SocketErrorCode is the friendly name.
        return "SocketError=" + ex.SocketErrorCode + " wsa=" + ex.ErrorCode
            + " native=" + ex.NativeErrorCode;
    }

    static int Listen(int port)
    {
        Socket l = new Socket(AddressFamily.InterNetwork, SocketType.Stream, ProtocolType.Tcp);
        try
        {
            try { l.Bind(new IPEndPoint(IPAddress.Loopback, port)); P("listen:bind=OK"); }
            catch (SocketException ex) { P("listen:bind=FAILED " + SockErr(ex)); return 21; }
            try { l.Listen(4); P("listen:listen=OK"); }
            catch (SocketException ex) { P("listen:listen=FAILED " + SockErr(ex)); return 22; }
            // THE READY SIGNAL the parent polls for. Nothing downstream is sequenced on a sleep.
            P("listen:listening=1 port=" + port);

            // Poll takes microseconds. 25 s comfortably outlives the connector's own 30 s ceiling
            // only for the arms that connect fast; a blocked arm's listener simply times out here
            // and says so, which is a result rather than a hang.
            if (!l.Poll(25000000, SelectMode.SelectRead)) { P("listen:accept=TIMEOUT-25s"); return 23; }
            Socket c = l.Accept();
            P("listen:accept=OK peer=" + c.RemoteEndPoint);
            c.ReceiveTimeout = 5000;
            byte[] buf = new byte[32];
            int n = c.Receive(buf);
            P("listen:recv=" + Encoding.ASCII.GetString(buf, 0, n) + " bytes=" + n);
            c.Send(Encoding.ASCII.GetBytes("PONG"));
            P("listen:sent=PONG");
            c.Close();
            return 0;
        }
        catch (SocketException ex) { P("listen:unexpected " + SockErr(ex)); return 24; }
        catch (Exception ex) { P("listen:exception " + ex.GetType().Name + " " + ex.Message); return 25; }
        finally { try { l.Close(); } catch (Exception) { } }
    }

    /// `roundTrip` false is the EGRESS GATE control: connect to a public address and report,
    /// without a peer to talk to. It is what proves, inside this very run, that withholding
    /// `internetClient` is actually confining the child at the network layer -- so a CONNECTED
    /// loopback arm cannot be explained by "the AppContainer attribute was never applied".
    /// MECHANISM-FACTS 5l 4 measured that denial as EACCES; here it should be WSAEACCES 10013.
    static int Connect(string host, int port, bool roundTrip)
    {
        Socket s = new Socket(AddressFamily.InterNetwork, SocketType.Stream, ProtocolType.Tcp);
        Stopwatch sw = Stopwatch.StartNew();
        try
        {
            IAsyncResult ar = s.BeginConnect(IPAddress.Parse(host), port, null, null);
            // TWO BOUNDS, because the two failure shapes are the finding. An outbound capability
            // denial returns in single-digit ms; a receive-side DROP leaves the SYN unanswered
            // and only surfaces when Windows' own TCP retry budget runs out (~21 s). A single 5 s
            // ceiling would report both as "no completion" and erase the distinction that
            // MECHANISM-FACTS 5l 4 rests on.
            bool done = ar.AsyncWaitHandle.WaitOne(5000, false);
            if (!done)
            {
                P("connect:at5s=PENDING");
                done = ar.AsyncWaitHandle.WaitOne(25000, false);
            }
            if (!done)
            {
                P("connect:result=NO-COMPLETION-30s elapsedMs=" + sw.ElapsedMilliseconds);
                return 31;
            }
            try { s.EndConnect(ar); }
            catch (SocketException ex)
            {
                P("connect:result=FAILED " + SockErr(ex) + " elapsedMs=" + sw.ElapsedMilliseconds);
                return 32;
            }
            P("connect:result=CONNECTED elapsedMs=" + sw.ElapsedMilliseconds);
            if (!roundTrip) return 0;

            // A completed handshake is not yet a data path: on a filtered loopback a connect can
            // in principle succeed and the stream carry nothing. The round trip is the claim.
            s.Send(Encoding.ASCII.GetBytes("PING"));
            P("connect:sent=PING");
            s.ReceiveTimeout = 5000;
            byte[] buf = new byte[32];
            int n = s.Receive(buf);
            string got = Encoding.ASCII.GetString(buf, 0, n);
            P("connect:recv=" + got + " bytes=" + n);
            P("connect:roundtrip=" + (got == "PONG" ? "OK" : "MISMATCH"));
            return got == "PONG" ? 0 : 33;
        }
        catch (SocketException ex)
        {
            P("connect:unexpected " + SockErr(ex) + " elapsedMs=" + sw.ElapsedMilliseconds);
            return 34;
        }
        catch (Exception ex) { P("connect:exception " + ex.GetType().Name + " " + ex.Message); return 35; }
        finally { try { s.Close(); } catch (Exception) { } }
    }

    public static int Main(string[] args)
    {
        try
        {
            string mode = args.Length > 0 ? args[0] : "selftest";
            int port = args.Length > 1 ? int.Parse(args[1]) : 0;
            ReportToken(mode);
            if (mode == "selftest") { P("selftest:ok=1"); return 0; }
            if (mode == "listen") return Listen(port);
            if (mode == "connect") return Connect("127.0.0.1", port, true);
            if (mode == "egress") return Connect("1.1.1.1", 443, false);
            P("mode:unknown=" + mode);
            return 90;
        }
        catch (Exception ex)
        {
            P("fatal:" + ex.GetType().Name + " " + ex.Message);
            return 91;
        }
    }
}
