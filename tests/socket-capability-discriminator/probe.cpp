#define WIN32_LEAN_AND_MEAN
#include <winsock2.h>
#include <windows.h>
#include <userenv.h>
#include <securityappcontainer.h>
#include <aclapi.h>
#include <cstdio>
#include <string>
#include <vector>

struct Packet { int mode, op; unsigned short port; WSAPROTOCOL_INFOW info; };
struct Result { int ok, error; unsigned short port; DWORD app, caps, admin; };
bool transfer(HANDLE h, void* p, DWORD n, bool write) {
  DWORD got = 0; return (write ? WriteFile(h,p,n,&got,nullptr) : ReadFile(h,p,n,&got,nullptr)) && got == n;
}
sockaddr_in addr(unsigned short port) { sockaddr_in a={}; a.sin_family=AF_INET; a.sin_addr.s_addr=htonl(INADDR_LOOPBACK); a.sin_port=htons(port); return a; }
bool ready(SOCKET s) { fd_set f; FD_ZERO(&f); FD_SET(s,&f); timeval t={2,0}; return select(0,&f,nullptr,nullptr,&t)==1; }
void timeout(SOCKET s) { DWORD t=2000; setsockopt(s,SOL_SOCKET,SO_RCVTIMEO,(char*)&t,sizeof(t)); setsockopt(s,SOL_SOCKET,SO_SNDTIMEO,(char*)&t,sizeof(t)); }
unsigned short portof(SOCKET s) { sockaddr_in a={}; int n=sizeof(a); return getsockname(s,(sockaddr*)&a,&n)==0 ? ntohs(a.sin_port) : 0; }
void identity(Result& r) {
  HANDLE t=nullptr; DWORD n=0; OpenProcessToken(GetCurrentProcess(),TOKEN_QUERY,&t);
  GetTokenInformation(t,TokenIsAppContainer,&r.app,sizeof(r.app),&n);
  GetTokenInformation(t,TokenCapabilities,nullptr,0,&n); std::vector<BYTE> b(n);
  if(n && GetTokenInformation(t,TokenCapabilities,b.data(),n,&n)) r.caps=((TOKEN_GROUPS*)b.data())->GroupCount;
  BYTE sid[SECURITY_MAX_SID_SIZE]; DWORD sn=sizeof(sid); BOOL admin=FALSE;
  CreateWellKnownSid(WinBuiltinAdministratorsSid,nullptr,sid,&sn); CheckTokenMembership(nullptr,sid,&admin); r.admin=admin;
  CloseHandle(t);
}
int child() {
  Packet p={}; Result r={}; identity(r);
  if(!transfer(GetStdHandle(STD_INPUT_HANDLE),&p,sizeof(p),false)) return 20;
  SOCKET s=p.mode==2 ? WSASocketW(FROM_PROTOCOL_INFO,FROM_PROTOCOL_INFO,FROM_PROTOCOL_INFO,&p.info,0,WSA_FLAG_OVERLAPPED) : socket(AF_INET,p.op==2?SOCK_DGRAM:SOCK_STREAM,0);
  r.ok=s!=INVALID_SOCKET; r.error=r.ok?0:WSAGetLastError();
  if(r.ok) {
    timeout(s); sockaddr_in a=addr(p.port);
    int rc=0;
    if(p.op>=3) { a=addr(0); rc=bind(s,(sockaddr*)&a,sizeof(a)); if(!rc) rc=listen(s,1); r.port=portof(s); }
    else if(p.op==2) rc=sendto(s,"Q",1,0,(sockaddr*)&a,sizeof(a))==1?0:SOCKET_ERROR;
    else if(!(p.op==0 && p.mode==2)) rc=connect(s,(sockaddr*)&a,sizeof(a));
    r.ok=rc==0; r.error=r.ok?0:WSAGetLastError();
  }
  if(!transfer(GetStdHandle(STD_OUTPUT_HANDLE),&r,sizeof(r),true)) return 21;
  if(p.op==4 && r.ok) Sleep(INFINITE);
  if(r.ok) {
    SOCKET io=s; char c=0;
    if(p.op==3) { io=ready(s)?accept(s,nullptr,nullptr):INVALID_SOCKET; if(io!=INVALID_SOCKET) timeout(io); }
    if(p.op!=2) { r.ok=io!=INVALID_SOCKET && recv(io,&c,1,0)==1 && c=='R' && send(io,"Q",1,0)==1; }
    else { r.ok=recv(s,&c,1,0)==1 && c=='R'; }
    r.error=r.ok?0:WSAGetLastError();
    if(io!=s && io!=INVALID_SOCKET) closesocket(io);
  }
  transfer(GetStdHandle(STD_OUTPUT_HANDLE),&r,sizeof(r),true);
  if(s!=INVALID_SOCKET) closesocket(s); return 0;
}
bool grant(const std::wstring& path, PSID sid) {
  PACL old=nullptr, next=nullptr; PSECURITY_DESCRIPTOR sd=nullptr;
  if(GetNamedSecurityInfoW(path.c_str(),SE_FILE_OBJECT,DACL_SECURITY_INFORMATION,nullptr,nullptr,&old,nullptr,&sd)) return false;
  EXPLICIT_ACCESSW e={}; e.grfAccessPermissions=GENERIC_READ|GENERIC_EXECUTE; e.grfAccessMode=GRANT_ACCESS;
  e.grfInheritance=SUB_CONTAINERS_AND_OBJECTS_INHERIT; e.Trustee.TrusteeForm=TRUSTEE_IS_SID; e.Trustee.ptstrName=(LPWSTR)sid;
  bool ok=!SetEntriesInAclW(1,&e,old,&next) && !SetNamedSecurityInfoW((LPWSTR)path.c_str(),SE_FILE_OBJECT,DACL_SECURITY_INFORMATION,nullptr,nullptr,next,nullptr);
  if(next) LocalFree(next); if(sd) LocalFree(sd); return ok;
}
bool run(int mode,int op,PSID sid,const std::wstring& exe) {
  SOCKET host=socket(AF_INET,op==2?SOCK_DGRAM:SOCK_STREAM,0), source=INVALID_SOCKET, peer=INVALID_SOCKET;
  sockaddr_in a=addr(0); bind(host,(sockaddr*)&a,sizeof(a)); if(op!=2) listen(host,1); timeout(host);
  Packet p={}; p.mode=mode;p.op=op;p.port=portof(host);
  if(mode==2) { source=socket(AF_INET,op==2?SOCK_DGRAM:SOCK_STREAM,0); if(op==0) { a=addr(p.port); connect(source,(sockaddr*)&a,sizeof(a)); peer=accept(host,nullptr,nullptr); } }
  SECURITY_ATTRIBUTES sa={sizeof(sa),nullptr,TRUE}; HANDLE inR,inW,outR,outW;
  CreatePipe(&inR,&inW,&sa,0); CreatePipe(&outR,&outW,&sa,0); SetHandleInformation(inW,HANDLE_FLAG_INHERIT,0); SetHandleInformation(outR,HANDLE_FLAG_INHERIT,0);
  SIZE_T bytes=0; InitializeProcThreadAttributeList(nullptr,mode?2:1,0,&bytes); std::vector<BYTE> buf(bytes);
  auto attrs=(LPPROC_THREAD_ATTRIBUTE_LIST)buf.data(); InitializeProcThreadAttributeList(attrs,mode?2:1,0,&bytes);
  HANDLE inherited[]={inR,outW}; UpdateProcThreadAttribute(attrs,0,PROC_THREAD_ATTRIBUTE_HANDLE_LIST,inherited,sizeof(inherited),nullptr,nullptr);
  SECURITY_CAPABILITIES caps={sid,nullptr,0,0}; if(mode) UpdateProcThreadAttribute(attrs,0,PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,&caps,sizeof(caps),nullptr,nullptr);
  STARTUPINFOEXW si={}; si.StartupInfo.cb=sizeof(si); si.StartupInfo.dwFlags=STARTF_USESTDHANDLES; si.StartupInfo.hStdInput=inR;si.StartupInfo.hStdOutput=outW;si.StartupInfo.hStdError=outW;si.lpAttributeList=attrs;
  PROCESS_INFORMATION pi={}; std::wstring cmd=L"\""+exe+L"\" child";
  HANDLE job=CreateJobObjectW(nullptr,nullptr); JOBOBJECT_EXTENDED_LIMIT_INFORMATION limits={}; limits.BasicLimitInformation.LimitFlags=JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
  SetInformationJobObject(job,JobObjectExtendedLimitInformation,&limits,sizeof(limits));
  bool launched=CreateProcessW(exe.c_str(),cmd.data(),nullptr,nullptr,TRUE,CREATE_SUSPENDED|EXTENDED_STARTUPINFO_PRESENT|CREATE_NO_WINDOW,nullptr,nullptr,&si.StartupInfo,&pi)!=0;
  bool assigned=launched && AssignProcessToJobObject(job,pi.hProcess); bool duplicated=mode!=2 || (launched && WSADuplicateSocketW(source,pi.dwProcessId,&p.info)==0);
  int duplicate_error=duplicated?0:WSAGetLastError(); if(source!=INVALID_SOCKET) closesocket(source);
  CloseHandle(inR);CloseHandle(outW);DeleteProcThreadAttributeList(attrs);
  Result first={},last={}; bool wire=false, peer_connected=false, peer_payload=false;
  if(assigned && duplicated) {
    transfer(inW,&p,sizeof(p),true); ResumeThread(pi.hThread);
    wire=transfer(outR,&first,sizeof(first),false);
    if(wire && first.ok && op==4) {
      CloseHandle(job);job=nullptr;
      bool dead=WaitForSingleObject(pi.hProcess,5000)==WAIT_OBJECT_0;
      SOCKET check=socket(AF_INET,SOCK_STREAM,0);a=addr(first.port);
      bool closed=connect(check,(sockaddr*)&a,sizeof(a))==SOCKET_ERROR;closesocket(check);
      last=first;last.ok=dead&&closed;
      std::printf("OWNER_HANDLE_LOSS child_exited=%d listener_closed=%d\n",dead,closed);
    } else if(wire && first.ok) {
      if(op==3) { peer=socket(AF_INET,SOCK_STREAM,0);a=addr(first.port);peer_connected=connect(peer,(sockaddr*)&a,sizeof(a))==0; }
      else if(op==1 || (op==0 && mode!=2)) { peer=ready(host)?accept(host,nullptr,nullptr):INVALID_SOCKET;peer_connected=peer!=INVALID_SOCKET; }
      else if(op==0) peer_connected=peer!=INVALID_SOCKET;
      char c=0;
      if(op==2) { int n=sizeof(a); peer_payload=ready(host) && recvfrom(host,&c,1,0,(sockaddr*)&a,&n)==1 && c=='Q';if(peer_payload) sendto(host,"R",1,0,(sockaddr*)&a,n); }
      else if(peer_connected) { timeout(peer);send(peer,"R",1,0);peer_payload=recv(peer,&c,1,0)==1 && c=='Q'; }
    }
    if(op!=4) wire=wire && transfer(outR,&last,sizeof(last),false);
  }
  bool token=wire && first.app==(mode?1u:0u) && !first.admin && !first.caps;
  bool expected=mode==1 ? (!last.ok && !peer_connected && !peer_payload) : (last.ok && (op==4 || peer_payload));
  bool ok=assigned && duplicated && token && expected;
  if(launched) { if(WaitForSingleObject(pi.hProcess,5000)!=WAIT_OBJECT_0) {TerminateJobObject(job,99);ok=false;} CloseHandle(pi.hThread);CloseHandle(pi.hProcess); }
  if(job)CloseHandle(job);CloseHandle(inW);CloseHandle(outR);closesocket(host);if(peer!=INVALID_SOCKET)closesocket(peer);
  std::printf("CASE mode=%d op=%d launched=%d assigned=%d duplicate=%d duplicate_error=%d app=%lu caps=%lu admin=%lu initial_ok=%d initial_error=%d io_ok=%d io_error=%d peer_connected=%d peer_payload=%d RESULT=%s\n",mode,op,launched,assigned,duplicated,duplicate_error,first.app,first.caps,first.admin,first.ok,first.error,last.ok,last.error,peer_connected,peer_payload,ok?"PASS":"FAIL"); fflush(stdout); return ok;
}
int wmain(int argc,wchar_t**) {
  WSADATA w={};if(WSAStartup(MAKEWORD(2,2),&w))return 2;if(argc>1)return child();
  Result id={};identity(id);std::printf("PARENT admin=%lu app=%lu caps=%lu\n",id.admin,id.app,id.caps);if(id.admin || id.app)return 3;
  wchar_t path[MAX_PATH];GetModuleFileNameW(nullptr,path,MAX_PATH);std::wstring exe=path,dir=exe.substr(0,exe.find_last_of(L"\\"));
  std::wstring name=L"nub.socket.discriminator."+std::to_wstring(GetCurrentProcessId());PSID sid=nullptr;
  HRESULT hr=CreateAppContainerProfile(name.c_str(),name.c_str(),name.c_str(),nullptr,0,&sid);if(FAILED(hr))return 4;
  bool ok=grant(dir,sid);if(ok)for(int mode=0;mode<3;mode++)for(int op=0;op<4;op++)ok=run(mode,op,sid,exe)&&ok;
  ok=run(2,4,sid,exe)&&ok;
  FreeSid(sid);hr=DeleteAppContainerProfile(name.c_str());std::printf("PROFILE_CLEANUP hr=%08lx\n",(unsigned long)hr);ok=ok&&SUCCEEDED(hr);
  WSACleanup();std::printf("RESULT=%s\n",ok?"PASS":"FAIL");return ok?0:1;
}
