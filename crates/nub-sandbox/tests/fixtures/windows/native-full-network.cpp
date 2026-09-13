// Adversarial fixture for the Windows native network adapter.
//
// The test harness runs this same binary in plain, raw AppContainer, and
// native-adapter AppContainer modes.  It owns every peer endpoint and decides
// the mode-specific verdict from these markers; this program never assumes
// that socket()/bind()/sendto() failing is the denial signal.  A raw AppContainer
// can report successful Winsock setup while no peer ever observes traffic.
//
// Build contract (owned by the parent test workflow, not this fixture):
//   cl /nologo /std:c++17 /W4 /WX /EHsc native-full-network.cpp /link ws2_32.lib dnsapi.lib advapi32.lib
//
// Arguments: <case>, one of tcp4, tcp6, udp4, udp6, listen4, listen6,
// connectex4, acceptex4, concurrent4, descendant4, token-attest, or fs-canary.
// NUB_FULL_NETWORK_ENDPOINT is HOST:PORT (IPv6 uses [::1]:PORT).  Listener
// cases print FULL_NETWORK_READY before accepting.  Every peer exchange uses
// the fixed request/reply bytes below so the parent can independently prove
// both directions of traffic.

#define WIN32_LEAN_AND_MEAN
#include <winsock2.h>
#include <mswsock.h>
#include <ws2tcpip.h>
#include <windns.h>
#include <windows.h>

#include <array>
#include <atomic>
#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <thread>
#include <vector>

namespace {

constexpr char kRequest[] = "nub-full-network-request";
constexpr char kReply[] = "nub-full-network-reply";
constexpr DWORD kTimeoutMs = 2000;
constexpr int kConcurrentClients = 12;

struct Socket final {
  SOCKET value = INVALID_SOCKET;
  Socket() = default;
  explicit Socket(SOCKET socket) : value(socket) {}
  Socket(const Socket&) = delete;
  Socket& operator=(const Socket&) = delete;
  Socket(Socket&& other) noexcept : value(other.value) {
    other.value = INVALID_SOCKET;
  }
  Socket& operator=(Socket&& other) noexcept {
    if (this != &other) {
      reset();
      value = other.value;
      other.value = INVALID_SOCKET;
    }
    return *this;
  }
  ~Socket() { reset(); }
  void reset() {
    if (value != INVALID_SOCKET) closesocket(value);
    value = INVALID_SOCKET;
  }
  explicit operator bool() const { return value != INVALID_SOCKET; }
};

struct Endpoint final {
  sockaddr_storage address{};
  int length = 0;
  int family = AF_UNSPEC;
};

void marker(const char* name, const std::string& value) {
  std::printf("%s=%s\n", name, value.c_str());
  std::fflush(stdout);
}

std::string error_code(int code = WSAGetLastError()) {
  return std::to_string(code);
}

bool set_timeout(SOCKET socket) {
  return setsockopt(socket, SOL_SOCKET, SO_RCVTIMEO,
                    reinterpret_cast<const char*>(&kTimeoutMs), sizeof(kTimeoutMs)) == 0 &&
         setsockopt(socket, SOL_SOCKET, SO_SNDTIMEO,
                    reinterpret_cast<const char*>(&kTimeoutMs), sizeof(kTimeoutMs)) == 0;
}

bool parse_endpoint(const char* text, int socktype, Endpoint* output) {
  if (text == nullptr || *text == '\0') return false;
  std::string host;
  std::string port;
  const std::string input(text);
  if (input.front() == '[') {
    const auto end = input.find("]:");
    if (end == std::string::npos) return false;
    host = input.substr(1, end - 1);
    port = input.substr(end + 2);
  } else {
    const auto separator = input.rfind(':');
    if (separator == std::string::npos) return false;
    host = input.substr(0, separator);
    port = input.substr(separator + 1);
  }
  addrinfo hints{};
  hints.ai_family = AF_UNSPEC;
  hints.ai_socktype = socktype;
  hints.ai_protocol = socktype == SOCK_DGRAM ? IPPROTO_UDP : IPPROTO_TCP;
  addrinfo* result = nullptr;
  if (getaddrinfo(host.c_str(), port.c_str(), &hints, &result) != 0 || result == nullptr) return false;
  if (result->ai_addrlen > sizeof(output->address)) {
    freeaddrinfo(result);
    return false;
  }
  std::memcpy(&output->address, result->ai_addr, result->ai_addrlen);
  output->length = static_cast<int>(result->ai_addrlen);
  output->family = result->ai_family;
  freeaddrinfo(result);
  return true;
}

Socket ordinary_socket(int family, int type, int protocol) {
  Socket socket(WSASocketW(family, type, protocol, nullptr, 0,
                           WSA_FLAG_OVERLAPPED | WSA_FLAG_NO_HANDLE_INHERIT));
  if (socket) set_timeout(socket.value);
  return socket;
}

bool wait_iocp(HANDLE port, OVERLAPPED* expected);

void cancel_and_drain(HANDLE port, SOCKET socket, OVERLAPPED* overlapped) {
  // An incomplete Winsock extension operation owns the caller's OVERLAPPED and
  // address buffers.  Cancel and observe its terminal completion before either
  // stack storage or the completion port can go away.
  CancelIoEx(reinterpret_cast<HANDLE>(socket), overlapped);
  (void)wait_iocp(port, overlapped);
}

// This must be the fixture's first named result.  The adapter routes this
// WSASocketW through its root broker.  A failure therefore distinguishes
// root-to-broker admission/RPC faults from later peer-oracle failures without
// exposing or depending on the broker's private named-pipe protocol.
bool root_broker_socket_diagnostic() {
  Socket probe = ordinary_socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
  if (!probe) {
    marker("FULL_NETWORK_ROOT_BROKER_SOCKET", "failed:" + error_code());
    return false;
  }
  marker("FULL_NETWORK_ROOT_BROKER_SOCKET", "ok");
  return true;
}

// This fixture only queries its own primary token.  In particular, it never
// opens the adapter, relay, or another helper process merely to make a token
// claim.  The capability count comes from TokenCapabilities and administrator
// membership uses the AppContainer-aware membership API.
bool self_token_marker(bool require_full_network) {
  HANDLE token = nullptr;
  if (OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &token) == 0) return false;
  DWORD app_container = 0;
  DWORD returned = 0;
  bool valid = GetTokenInformation(token, TokenIsAppContainer, &app_container,
                                   sizeof(app_container), &returned) != 0;
  DWORD capability_bytes = 0;
  GetTokenInformation(token, TokenCapabilities, nullptr, 0, &capability_bytes);
  std::vector<BYTE> capabilities(capability_bytes);
  if (capability_bytes < sizeof(TOKEN_GROUPS) ||
      GetTokenInformation(token, TokenCapabilities, capabilities.data(), capability_bytes,
                          &returned) == 0) {
    valid = false;
  }
  DWORD capability_count = 0;
  if (valid) {
    capability_count = reinterpret_cast<const TOKEN_GROUPS*>(capabilities.data())->GroupCount;
  }
  SID_IDENTIFIER_AUTHORITY authority = SECURITY_NT_AUTHORITY;
  PSID administrators = nullptr;
  BOOL administrator = FALSE;
  if (AllocateAndInitializeSid(&authority, 2, SECURITY_BUILTIN_DOMAIN_RID,
                               DOMAIN_ALIAS_RID_ADMINS, 0, 0, 0, 0, 0, 0,
                               &administrators) == 0 ||
      CheckTokenMembershipEx(token, administrators, CTMF_INCLUDE_APPCONTAINER,
                             &administrator) == 0) {
    valid = false;
  }
  if (administrators != nullptr) FreeSid(administrators);
  CloseHandle(token);
  marker("FULL_NETWORK_TOKEN", "appcontainer=" + std::to_string(app_container) +
          ":capabilities=" + std::to_string(capability_count) +
          ":admin=" + std::to_string(administrator != FALSE));
  return valid && (!require_full_network ||
                   (app_container == 1 && capability_count == 1 && administrator == FALSE));
}

bool self_token_attestation() {
  // A full-network AppContainer has only the internetClient capability.  The
  // ordinary-user fixture must therefore show a LowBox token, exactly that one
  // capability, and no Administrators membership from its own token.
  return self_token_marker(true);
}

bool send_all(SOCKET socket, const char* bytes, int count) {
  for (int sent = 0; sent < count;) {
    const int written = send(socket, bytes + sent, count - sent, 0);
    if (written <= 0) return false;
    sent += written;
  }
  return true;
}

bool receive_exact(SOCKET socket, const char* expected, int count) {
  std::string received(static_cast<size_t>(count), '\0');
  for (int offset = 0; offset < count;) {
    const int read = recv(socket, received.data() + offset, count - offset, 0);
    if (read <= 0) return false;
    offset += read;
  }
  return received == std::string(expected, static_cast<size_t>(count));
}

bool stream_round_trip(SOCKET socket, const Endpoint& endpoint) {
  if (connect(socket, reinterpret_cast<const sockaddr*>(&endpoint.address), endpoint.length) != 0) return false;
  return send_all(socket, kRequest, static_cast<int>(sizeof(kRequest) - 1)) &&
         receive_exact(socket, kReply, static_cast<int>(sizeof(kReply) - 1));
}

bool datagram_round_trip(SOCKET socket, const Endpoint& endpoint) {
  const int sent = sendto(socket, kRequest, static_cast<int>(sizeof(kRequest) - 1), 0,
                          reinterpret_cast<const sockaddr*>(&endpoint.address), endpoint.length);
  if (sent != static_cast<int>(sizeof(kRequest) - 1)) return false;
  return receive_exact(socket, kReply, static_cast<int>(sizeof(kReply) - 1));
}

bool bind_ephemeral(SOCKET socket, int family, sockaddr_storage* bound, int* bound_length) {
  if (family == AF_INET) {
    sockaddr_in address{};
    address.sin_family = AF_INET;
    address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    if (bind(socket, reinterpret_cast<sockaddr*>(&address), sizeof(address)) != 0) return false;
  } else if (family == AF_INET6) {
    sockaddr_in6 address{};
    address.sin6_family = AF_INET6;
    address.sin6_addr = in6addr_loopback;
    if (bind(socket, reinterpret_cast<sockaddr*>(&address), sizeof(address)) != 0) return false;
  } else {
    return false;
  }
  *bound_length = sizeof(*bound);
  return getsockname(socket, reinterpret_cast<sockaddr*>(bound), bound_length) == 0;
}

std::string printable_endpoint(const sockaddr_storage& address) {
  char host[NI_MAXHOST]{};
  char service[NI_MAXSERV]{};
  const int length = address.ss_family == AF_INET ? sizeof(sockaddr_in) : sizeof(sockaddr_in6);
  if (getnameinfo(reinterpret_cast<const sockaddr*>(&address), length, host, sizeof(host), service,
                  sizeof(service), NI_NUMERICHOST | NI_NUMERICSERV) != 0) return "unknown";
  return std::string(address.ss_family == AF_INET6 ? "[" : "") + host +
         (address.ss_family == AF_INET6 ? "]:" : ":") + service;
}

bool listener_round_trip(int family) {
  Socket listener = ordinary_socket(family, SOCK_STREAM, IPPROTO_TCP);
  sockaddr_storage bound{};
  int bound_length = 0;
  if (!listener || !bind_ephemeral(listener.value, family, &bound, &bound_length) || listen(listener.value, 1) != 0) {
    marker("FULL_NETWORK_PEER", "0");
    return false;
  }
  marker("FULL_NETWORK_READY", printable_endpoint(bound));
  u_long nonblocking = 1;
  ioctlsocket(listener.value, FIONBIO, &nonblocking);
  Socket accepted;
  const auto deadline = std::chrono::steady_clock::now() + std::chrono::milliseconds(kTimeoutMs);
  while (!accepted && std::chrono::steady_clock::now() < deadline) {
    accepted = Socket(accept(listener.value, nullptr, nullptr));
    if (!accepted && WSAGetLastError() != WSAEWOULDBLOCK) break;
    std::this_thread::sleep_for(std::chrono::milliseconds(10));
  }
  if (accepted) set_timeout(accepted.value);
  const bool peer = accepted && receive_exact(accepted.value, kRequest, static_cast<int>(sizeof(kRequest) - 1)) &&
                    send_all(accepted.value, kReply, static_cast<int>(sizeof(kReply) - 1));
  marker("FULL_NETWORK_PEER", peer ? "1" : "0");
  return peer;
}

bool listener_hold(int family) {
  Socket listener = ordinary_socket(family, SOCK_STREAM, IPPROTO_TCP);
  sockaddr_storage bound{}; int bound_length = 0;
  if (!listener || !bind_ephemeral(listener.value, family, &bound, &bound_length) || listen(listener.value, 1) != 0) return false;
  marker("FULL_NETWORK_READY", printable_endpoint(bound));
  std::this_thread::sleep_for(std::chrono::seconds(30));
  return true;
}

GUID connect_ex_guid() {
  return {0x25a207b9, 0xddf3, 0x4660, {0x8e, 0xe9, 0x76, 0xe5, 0x8c, 0x74, 0x06, 0x3e}};
}

GUID accept_ex_guid() {
  return {0xb5367df1, 0xcbac, 0x11cf, {0x95, 0xca, 0x00, 0x80, 0x5f, 0x48, 0xa1, 0x92}};
}

template <typename Procedure>
bool extension(SOCKET socket, const GUID& guid, Procedure* procedure) {
  DWORD bytes = 0;
  return WSAIoctl(socket, SIO_GET_EXTENSION_FUNCTION_POINTER, const_cast<GUID*>(&guid), sizeof(guid), procedure,
                  sizeof(*procedure), &bytes, nullptr, nullptr) == 0;
}

bool wait_iocp(HANDLE port, OVERLAPPED* expected) {
  DWORD bytes = 0;
  ULONG_PTR key = 0;
  OVERLAPPED* completed = nullptr;
  const BOOL ok = GetQueuedCompletionStatus(port, &bytes, &key, &completed, kTimeoutMs);
  return ok != 0 && completed == expected;
}

bool connect_ex_round_trip(const Endpoint& endpoint) {
  if (endpoint.family != AF_INET) return false;
  Socket socket = ordinary_socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
  if (!socket) return false;
  sockaddr_in local{};
  local.sin_family = AF_INET;
  local.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  if (bind(socket.value, reinterpret_cast<sockaddr*>(&local), sizeof(local)) != 0) return false;
  LPFN_CONNECTEX connect_ex = nullptr;
  const GUID guid = connect_ex_guid();
  if (!extension(socket.value, guid, &connect_ex) || connect_ex == nullptr) return false;
  HANDLE port = CreateIoCompletionPort(reinterpret_cast<HANDLE>(socket.value), nullptr, 1, 0);
  if (port == nullptr) return false;
  OVERLAPPED overlapped{};
  const BOOL immediate = connect_ex(socket.value, reinterpret_cast<const sockaddr*>(&endpoint.address), endpoint.length,
                                    nullptr, 0, nullptr, &overlapped);
  const int error = immediate ? 0 : WSAGetLastError();
  // A successful overlapped call still has to deliver its completion packet:
  // the point of this arm is to exercise the adapter with an IOCP, not merely
  // to exercise the extension entry point's synchronous fast path.
  const bool completed = (immediate || error == ERROR_IO_PENDING) && wait_iocp(port, &overlapped);
  if (!completed) {
    cancel_and_drain(port, socket.value, &overlapped);
    CloseHandle(port);
    return false;
  }
  CloseHandle(port);
  if (setsockopt(socket.value, SOL_SOCKET, SO_UPDATE_CONNECT_CONTEXT, nullptr, 0) != 0) return false;
  return send_all(socket.value, kRequest, static_cast<int>(sizeof(kRequest) - 1)) &&
         receive_exact(socket.value, kReply, static_cast<int>(sizeof(kReply) - 1));
}

bool accept_ex_round_trip() {
  Socket listener = ordinary_socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
  sockaddr_storage bound{};
  int bound_length = 0;
  if (!listener || !bind_ephemeral(listener.value, AF_INET, &bound, &bound_length) || listen(listener.value, 1) != 0) return false;
  LPFN_ACCEPTEX accept_ex = nullptr;
  const GUID guid = accept_ex_guid();
  if (!extension(listener.value, guid, &accept_ex) || accept_ex == nullptr) return false;
  Socket accepted = ordinary_socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
  if (!accepted) return false;
  HANDLE port = CreateIoCompletionPort(reinterpret_cast<HANDLE>(listener.value), nullptr, 1, 0);
  if (port == nullptr) return false;
  std::array<char, 2 * (sizeof(sockaddr_in) + 16)> addresses{};
  OVERLAPPED overlapped{};
  DWORD bytes = 0;
  const BOOL immediate = accept_ex(listener.value, accepted.value, addresses.data(), 0,
                                  sizeof(sockaddr_in) + 16, sizeof(sockaddr_in) + 16, &bytes, &overlapped);
  const int error = immediate ? 0 : WSAGetLastError();
  marker("FULL_NETWORK_READY", printable_endpoint(bound));
  const bool completed = (immediate || error == ERROR_IO_PENDING) && wait_iocp(port, &overlapped);
  if (!completed) {
    cancel_and_drain(port, listener.value, &overlapped);
    CloseHandle(port);
    return false;
  }
  CloseHandle(port);
  if (setsockopt(accepted.value, SOL_SOCKET, SO_UPDATE_ACCEPT_CONTEXT,
                 reinterpret_cast<const char*>(&listener.value), sizeof(listener.value)) != 0) return false;
  set_timeout(accepted.value);
  return receive_exact(accepted.value, kRequest, static_cast<int>(sizeof(kRequest) - 1)) &&
         send_all(accepted.value, kReply, static_cast<int>(sizeof(kReply) - 1));
}

bool concurrent_round_trips(const Endpoint& endpoint) {
  std::atomic<int> successes{0};
  std::vector<std::thread> workers;
  for (int index = 0; index != kConcurrentClients; ++index) {
    workers.emplace_back([&] {
      Socket socket = ordinary_socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
      if (socket && stream_round_trip(socket.value, endpoint)) ++successes;
    });
  }
  for (auto& worker : workers) worker.join();
  marker("FULL_NETWORK_CONCURRENT", std::to_string(successes.load()));
  return successes == kConcurrentClients;
}

bool descendant_round_trip(const char* executable, const char* endpoint) {
  std::wstring command = L"\"";
  int length = MultiByteToWideChar(CP_UTF8, 0, executable, -1, nullptr, 0);
  if (length <= 0) return false;
  std::vector<wchar_t> executable_wide(static_cast<size_t>(length));
  MultiByteToWideChar(CP_UTF8, 0, executable, -1, executable_wide.data(), length);
  command += executable_wide.data();
  command += L"\" --descendant-client";
  if (SetEnvironmentVariableA("NUB_FULL_NETWORK_ENDPOINT", endpoint) == 0) return false;
  STARTUPINFOW startup{};
  startup.cb = sizeof(startup);
  PROCESS_INFORMATION process{};
  if (CreateProcessW(nullptr, command.data(), nullptr, nullptr, TRUE, 0, nullptr, nullptr, &startup, &process) == 0) return false;
  const DWORD waited = WaitForSingleObject(process.hProcess, kTimeoutMs + 1000);
  DWORD code = 1;
  if (waited != WAIT_OBJECT_0) {
    TerminateProcess(process.hProcess, 1);
    WaitForSingleObject(process.hProcess, kTimeoutMs);
  }
  GetExitCodeProcess(process.hProcess, &code);
  CloseHandle(process.hThread);
  CloseHandle(process.hProcess);
  marker("FULL_NETWORK_DESCENDANT", waited == WAIT_OBJECT_0 && code == 0 ? "1" : "0");
  return waited == WAIT_OBJECT_0 && code == 0;
}

bool filesystem_canary() {
  const char* path = std::getenv("NUB_FULL_NETWORK_FS_CANARY");
  if (path == nullptr) return false;
  HANDLE readable = CreateFileA(path, GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE, nullptr, OPEN_EXISTING,
                                FILE_ATTRIBUTE_NORMAL, nullptr);
  const DWORD read_error = readable == INVALID_HANDLE_VALUE ? GetLastError() : ERROR_SUCCESS;
  if (readable != INVALID_HANDLE_VALUE) CloseHandle(readable);
  HANDLE writable = CreateFileA(path, GENERIC_WRITE, FILE_SHARE_READ, nullptr, OPEN_EXISTING,
                                FILE_ATTRIBUTE_NORMAL, nullptr);
  const DWORD write_error = writable == INVALID_HANDLE_VALUE ? GetLastError() : ERROR_SUCCESS;
  if (writable != INVALID_HANDLE_VALUE) CloseHandle(writable);
  const bool denied = readable == INVALID_HANDLE_VALUE && writable == INVALID_HANDLE_VALUE;
  marker("FULL_NETWORK_FS_CANARY", "read=" + std::to_string(read_error) + ":write=" + std::to_string(write_error));
  return denied;
}

bool dns_lookup(bool query_ex) {
  const char* name = std::getenv("NUB_FULL_NETWORK_DNS_NAME");
  if (name == nullptr || *name == '\0') return false;
  int length = MultiByteToWideChar(CP_UTF8, 0, name, -1, nullptr, 0);
  if (length <= 0) return false;
  std::vector<wchar_t> wide(static_cast<size_t>(length));
  MultiByteToWideChar(CP_UTF8, 0, name, -1, wide.data(), length);
  const char* expected = std::getenv("NUB_FULL_NETWORK_DNS_EXPECTED");
  if (expected == nullptr) expected = "1.1.1.1";
  if (!query_ex) {
    ADDRINFOW hints{}; hints.ai_family = AF_INET; hints.ai_socktype = SOCK_STREAM;
    ADDRINFOW* records = nullptr; const int status = GetAddrInfoW(wide.data(), nullptr, &hints, &records);
    bool matched = false;
    for (auto* item = records; item != nullptr; item = item->ai_next) {
      char address[INET_ADDRSTRLEN]{};
      if (item->ai_family == AF_INET && InetNtopA(AF_INET, &reinterpret_cast<sockaddr_in*>(item->ai_addr)->sin_addr, address, sizeof(address)) != nullptr && std::strcmp(address, expected) == 0) matched = true;
    }
    if (records != nullptr) FreeAddrInfoW(records);
    marker("FULL_NETWORK_DNS", "getaddrinfo:" + std::to_string(status) + ":" + (matched ? expected : "unexpected")); return status == 0 && matched;
  }
  DNS_QUERY_REQUEST request{}; request.Version = DNS_QUERY_REQUEST_VERSION1; request.QueryName = wide.data();
  request.QueryType = DNS_TYPE_A; request.QueryOptions = DNS_QUERY_WIRE_ONLY | DNS_QUERY_NO_HOSTS_FILE;
  DNS_QUERY_RESULT result{}; result.Version = DNS_QUERY_RESULTS_VERSION1;
  const DNS_STATUS status = DnsQueryEx(&request, &result, nullptr);
  bool matched = false;
  for (auto* item = result.pQueryRecords; item != nullptr; item = item->pNext) {
    char address[INET_ADDRSTRLEN]{};
    if (item->wType == DNS_TYPE_A && InetNtopA(AF_INET, &item->Data.A.IpAddress, address, sizeof(address)) != nullptr && std::strcmp(address, expected) == 0) matched = true;
  }
  if (result.pQueryRecords != nullptr) DnsRecordListFree(result.pQueryRecords, DnsFreeRecordList);
  marker("FULL_NETWORK_DNS", "dnsqueryex:" + std::to_string(status) + ":" + (matched ? expected : "unexpected")); return status == ERROR_SUCCESS && matched;
}

bool endpoint_case(const std::string& name, int type, bool (*operation)(SOCKET, const Endpoint&)) {
  Endpoint endpoint{};
  if (!parse_endpoint(std::getenv("NUB_FULL_NETWORK_ENDPOINT"), type, &endpoint)) return false;
  Socket socket = ordinary_socket(endpoint.family, type, type == SOCK_DGRAM ? IPPROTO_UDP : IPPROTO_TCP);
  const bool peer = socket && operation(socket.value, endpoint);
  marker("FULL_NETWORK_PEER", peer ? "1" : "0");
  marker("FULL_NETWORK_CASE", name);
  return peer;
}

int run_case(const std::string& name, const char* executable) {
  if (name == "fs-canary") return filesystem_canary() ? 0 : 1;
  if (name == "getaddrinfo") return dns_lookup(false) ? 0 : 1;
  if (name == "dnsqueryex") return dns_lookup(true) ? 0 : 1;
  if (name == "token-attest") return self_token_attestation() ? 0 : 1;
  if (!root_broker_socket_diagnostic()) return 1;
  if (name == "tcp4" || name == "tcp6") return endpoint_case(name, SOCK_STREAM, stream_round_trip) ? 0 : 1;
  if (name == "udp4" || name == "udp6") return endpoint_case(name, SOCK_DGRAM, datagram_round_trip) ? 0 : 1;
  if (name == "listen4") return listener_round_trip(AF_INET) ? 0 : 1;
  if (name == "listen6") return listener_round_trip(AF_INET6) ? 0 : 1;
  if (name == "owner-hold4") return listener_hold(AF_INET) ? 0 : 1;
  if (name == "connectex4") {
    Endpoint endpoint{};
    const bool peer = parse_endpoint(std::getenv("NUB_FULL_NETWORK_ENDPOINT"), SOCK_STREAM, &endpoint) && connect_ex_round_trip(endpoint);
    marker("FULL_NETWORK_PEER", peer ? "1" : "0");
    return peer ? 0 : 1;
  }
  if (name == "acceptex4") {
    const bool peer = accept_ex_round_trip();
    marker("FULL_NETWORK_PEER", peer ? "1" : "0");
    return peer ? 0 : 1;
  }
  if (name == "concurrent4") {
    Endpoint endpoint{};
    return parse_endpoint(std::getenv("NUB_FULL_NETWORK_ENDPOINT"), SOCK_STREAM, &endpoint) && concurrent_round_trips(endpoint) ? 0 : 1;
  }
  if (name == "descendant4") return descendant_round_trip(executable, std::getenv("NUB_FULL_NETWORK_ENDPOINT")) ? 0 : 1;
  if (name == "--descendant-client") {
    const bool token = self_token_marker(false);
    return token && endpoint_case("descendant-client", SOCK_STREAM, stream_round_trip) ? 0 : 1;
  }
  marker("FULL_NETWORK_CASE", "unknown");
  return 2;
}

}  // namespace

int main(int argc, char** argv) {
  WSADATA data{};
  if (WSAStartup(MAKEWORD(2, 2), &data) != 0) {
    marker("FULL_NETWORK_WINSOCK", "startup-failed");
    return 3;
  }
  const int result = argc == 2 ? run_case(argv[1], argv[0]) : 2;
  WSACleanup();
  return result;
}
