#pragma once

// Private, command-local socket creation protocol. No PID or destination is
// accepted from the client: NPFS identifies the connector and the Job owns it.
namespace nub_sandbox::socket_broker {
constexpr DWORD kVersion = 1;
constexpr DWORD kWorkers = 4;
constexpr DWORD kTimeout = 10000;
constexpr DWORD kClientAccess = FILE_READ_DATA | FILE_WRITE_DATA | FILE_READ_ATTRIBUTES |
    FILE_WRITE_ATTRIBUTES | READ_CONTROL | SYNCHRONIZE;
static_assert((kClientAccess & FILE_CREATE_PIPE_INSTANCE) == 0);
static_assert((kClientAccess & ~0x12019bu) == 0);
constexpr DWORD kSocketFlags = WSA_FLAG_OVERLAPPED | WSA_FLAG_NO_HANDLE_INHERIT |
    WSA_FLAG_MULTIPOINT_C_ROOT | WSA_FLAG_MULTIPOINT_C_LEAF |
    WSA_FLAG_MULTIPOINT_D_ROOT | WSA_FLAG_MULTIPOINT_D_LEAF | WSA_FLAG_REGISTERED_IO;
struct Request {
    DWORD version;
    int family;
    int type;
    int protocol;
    DWORD flags;
};
struct Response {
    DWORD version;
    int error;
    WSAPROTOCOL_INFOW info;
};
static_assert(sizeof(Request) == 20);
static_assert(sizeof(Response) == 636);

enum class Stage : LONG {
    Request = 1, PipeOpen, PipeWait, ServerPidQuery, ServerPidMismatch, ReadMode,
    ClientEvent, RequestWrite, ResponseRead, ResponseVersion, BrokerError,
    Reconstruct, AcknowledgementWrite, HostIdentity, HostCreate, HostFlags,
    HostDuplicate, HostRequestRead, HostResponseWrite, HostAcknowledgementRead,
};
static_assert(static_cast<LONG>(Stage::HostAcknowledgementRead) < 31);

inline bool diagnostics_enabled() {
    wchar_t value[2];
    return GetEnvironmentVariableW(L"NUB_JAIL_DUMP_POLICY", value, _countof(value)) != 0;
}

inline void diagnose(bool enabled, Stage stage, DWORD error) {
    if (!enabled) return;
    DWORD saved = GetLastError();
    // At most one small record per failure stage per process. Never log the
    // command endpoint, protocol record, policy, credentials or peer identity.
    static volatile LONG seen = 0;
    LONG bit = 1L << static_cast<LONG>(stage);
    if (!(InterlockedOr(&seen, bit) & bit)) {
        char message[80];
        int length = sprintf_s(message, "NUB_SOCKET_IPC stage=%ld error=%lu\r\n",
                               static_cast<LONG>(stage), error);
        DWORD written = 0;
        if (length > 0) WriteFile(GetStdHandle(STD_ERROR_HANDLE), message,
                                  DWORD(length), &written, nullptr);
    }
    SetLastError(saved);
}

inline DWORD configure_client(HANDLE pipe, DWORD expected_pid, Stage& stage) {
    DWORD server = 0;
    stage = Stage::ServerPidQuery;
    if (!GetNamedPipeServerProcessId(pipe, &server)) return GetLastError();
    stage = Stage::ServerPidMismatch;
    if (server != expected_pid) return ERROR_ACCESS_DENIED;
    stage = Stage::ReadMode;
    DWORD mode = PIPE_READMODE_MESSAGE;
    if (!SetNamedPipeHandleState(pipe, &mode, nullptr, nullptr)) return GetLastError();
    return ERROR_SUCCESS;
}

constexpr DWORD source_flags(DWORD requested) {
    // The host descriptor must never enter a concurrent launch's inheritance
    // set. The child reconstructs with the original requested flags instead.
    return requested | WSA_FLAG_NO_HANDLE_INHERIT;
}
static_assert(source_flags(0) == WSA_FLAG_NO_HANDLE_INHERIT);
static_assert(source_flags(WSA_FLAG_OVERLAPPED) ==
              (WSA_FLAG_OVERLAPPED | WSA_FLAG_NO_HANDLE_INHERIT));
static_assert(source_flags(kSocketFlags) == kSocketFlags);

inline int validate(const Request& request) {
    if (request.version != kVersion) return WSAEINVAL;
    if (request.family != AF_INET && request.family != AF_INET6) return WSAEAFNOSUPPORT;
    if (request.type != SOCK_STREAM && request.type != SOCK_DGRAM) return WSAESOCKTNOSUPPORT;
    if (request.protocol != 0 && request.protocol !=
        (request.type == SOCK_STREAM ? IPPROTO_TCP : IPPROTO_UDP)) return WSAEPROTONOSUPPORT;
    if (request.flags & ~kSocketFlags) return WSAEINVAL;
    return 0;
}

// Every pending operation is cancelled and drained before its stack buffer or
// OVERLAPPED is released, including timeout, owner cancellation and disconnect.
inline bool complete(HANDLE pipe, OVERLAPPED& operation, BOOL immediate,
                     HANDLE stop, DWORD timeout, DWORD& bytes) {
    if (!immediate && GetLastError() != ERROR_IO_PENDING) return false;
    if (!immediate) {
        HANDLE events[] = {stop, operation.hEvent};
        DWORD result = stop ? WaitForMultipleObjects(2, events, FALSE, timeout)
                            : WaitForSingleObject(operation.hEvent, timeout);
        if (result != (stop ? WAIT_OBJECT_0 + 1 : WAIT_OBJECT_0)) {
            DWORD error = result == WAIT_TIMEOUT ? ERROR_TIMEOUT :
                result == WAIT_FAILED ? GetLastError() : ERROR_OPERATION_ABORTED;
            CancelIoEx(pipe, &operation);
            GetOverlappedResult(pipe, &operation, &bytes, TRUE);
            SetLastError(error);
            return false;
        }
    }
    return GetOverlappedResult(pipe, &operation, &bytes, FALSE) != FALSE;
}

inline bool transfer(HANDLE pipe, HANDLE event, HANDLE stop, void* data,
                     DWORD size, bool writing) {
    OVERLAPPED operation = {};
    operation.hEvent = event;
    ResetEvent(event);
    DWORD bytes = 0;
    BOOL immediate = writing ? WriteFile(pipe, data, size, nullptr, &operation)
                             : ReadFile(pipe, data, size, nullptr, &operation);
    if (!complete(pipe, operation, immediate, stop, kTimeout, bytes)) return false;
    if (bytes != size) { SetLastError(ERROR_INVALID_DATA); return false; }
    return true;
}

#ifdef SANDBOX_COMPAT_HOST
struct Broker;
struct Worker {
    Broker* broker = nullptr;
    HANDLE pipe = INVALID_HANDLE_VALUE;
    HANDLE event = nullptr;
    HANDLE thread = nullptr;
};
struct Broker {
    // Borrowed from WindowsChild. The broker is stopped before that Job closes;
    // holding another Job handle would delay KILL_ON_JOB_CLOSE on owner loss.
    HANDLE job = nullptr;
    HANDLE stop = nullptr;
    bool winsock = false;
    bool diagnostics = false;
    wchar_t name[128] = {};
    Worker workers[kWorkers];

    ~Broker() {
        if (stop) SetEvent(stop);
        for (auto& worker : workers) {
            if (worker.thread) {
                WaitForSingleObject(worker.thread, INFINITE);
                CloseHandle(worker.thread);
            }
            if (worker.event) CloseHandle(worker.event);
            if (worker.pipe != INVALID_HANDLE_VALUE) CloseHandle(worker.pipe);
        }
        if (stop) CloseHandle(stop);
        if (winsock) WSACleanup();
    }
};

inline HANDLE requesting_process(HANDLE pipe, HANDLE job, DWORD& pid) {
    if (!GetNamedPipeClientProcessId(pipe, &pid)) return nullptr;
    HANDLE process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, FALSE, pid);
    if (!process) return nullptr;
    BOOL member = FALSE;
    DWORD current = 0;
    // Keep the live process object referenced across socket transfer. Recheck
    // NPFS after opening it, and fail closed on terminated clients/PID reuse.
    if (!IsProcessInJob(process, job, &member) || !member ||
        WaitForSingleObject(process, 0) != WAIT_TIMEOUT ||
        !GetNamedPipeClientProcessId(pipe, &current) || current != pid) {
        CloseHandle(process);
        SetLastError(ERROR_ACCESS_DENIED);
        return nullptr;
    }
    return process;
}

inline void serve(Worker& worker) {
    Broker& broker = *worker.broker;
    DWORD pid = 0;
    HANDLE process = requesting_process(worker.pipe, broker.job, pid);
    if (!process) { diagnose(broker.diagnostics, Stage::HostIdentity, GetLastError()); return; }
    Request request = {};
    Response response = {};
    response.version = kVersion;
    SOCKET socket = INVALID_SOCKET;
    if (transfer(worker.pipe, worker.event, broker.stop, &request, sizeof(request), false)) {
        response.error = validate(request);
        if (!response.error && WaitForSingleObject(broker.stop, 0) == WAIT_TIMEOUT &&
            WaitForSingleObject(process, 0) == WAIT_TIMEOUT) {
            // No binding or connecting here: the child's normal Winsock calls
            // retain ConnectEx/AcceptEx/IOCP and datagram/listener semantics.
            socket = WSASocketW(request.family, request.type, request.protocol,
                                nullptr, 0, source_flags(request.flags));
            if (socket == INVALID_SOCKET) {
                response.error = WSAGetLastError();
                diagnose(broker.diagnostics, Stage::HostCreate, response.error);
            }
            if (socket != INVALID_SOCKET) {
                if (!SetHandleInformation(reinterpret_cast<HANDLE>(socket), HANDLE_FLAG_INHERIT, 0)) {
                    diagnose(broker.diagnostics, Stage::HostFlags, GetLastError());
                    response.error = WSAEACCES;
                } else if (WSADuplicateSocketW(socket, pid, &response.info) == SOCKET_ERROR) {
                    response.error = WSAGetLastError();
                    diagnose(broker.diagnostics, Stage::HostDuplicate, response.error);
                }
            }
        } else if (!response.error) response.error = WSA_OPERATION_ABORTED;
        if (!transfer(worker.pipe, worker.event, broker.stop, &response, sizeof(response), true)) {
            diagnose(broker.diagnostics, Stage::HostResponseWrite, GetLastError());
        } else if (!response.error) {
            // Keep the source descriptor alive until reconstruction completes.
            DWORD acknowledgement = 0;
            if (!transfer(worker.pipe, worker.event, broker.stop, &acknowledgement,
                          sizeof(acknowledgement), false))
                diagnose(broker.diagnostics, Stage::HostAcknowledgementRead, GetLastError());
        }
    } else diagnose(broker.diagnostics, Stage::HostRequestRead, GetLastError());
    if (socket != INVALID_SOCKET) closesocket(socket);
    CloseHandle(process);
}

inline DWORD WINAPI run(void* context) {
    auto& worker = *static_cast<Worker*>(context);
    while (WaitForSingleObject(worker.broker->stop, 0) == WAIT_TIMEOUT) {
        OVERLAPPED operation = {};
        operation.hEvent = worker.event;
        ResetEvent(worker.event);
        DWORD bytes = 0;
        BOOL connected = ConnectNamedPipe(worker.pipe, &operation);
        DWORD error = connected ? ERROR_SUCCESS : GetLastError();
        bool ready = error == ERROR_PIPE_CONNECTED;
        if (!ready) {
            SetLastError(error);
            ready = complete(worker.pipe, operation, connected, worker.broker->stop,
                             INFINITE, bytes);
        }
        if (ready) serve(worker);
        DisconnectNamedPipe(worker.pipe);
    }
    return 0;
}

inline DWORD start(HANDLE job, const wchar_t* name, PSID user, PSID package, Broker** output) {
    *output = nullptr;
    auto broker = new (std::nothrow) Broker;
    if (!broker) return ERROR_NOT_ENOUGH_MEMORY;
    broker->job = job;
    broker->diagnostics = diagnostics_enabled();
    DWORD error = ERROR_SUCCESS;
    if (wcscpy_s(broker->name, name)) error = ERROR_INVALID_NAME;
    WSADATA data;
    if (!error) {
        error = WSAStartup(MAKEWORD(2, 2), &data);
        broker->winsock = !error;
    }
    if (!error) {
        broker->stop = CreateEventW(nullptr, TRUE, FALSE, nullptr);
        if (!broker->stop) error = GetLastError();
    }
    // The package may read/write a pipe, never create another server instance.
    // A low mandatory label admits the confined client's write to this host object.
    LPWSTR user_text = nullptr, package_text = nullptr;
    PSECURITY_DESCRIPTOR descriptor = nullptr;
    wchar_t sddl[512];
    if (!error && (!ConvertSidToStringSidW(user, &user_text) ||
                   !ConvertSidToStringSidW(package, &package_text))) error = GetLastError();
    if (!error && swprintf_s(sddl, L"D:P(A;;GA;;;%s)(A;;0x12019b;;;%s)S:(ML;;NW;;;LW)",
                            user_text, package_text) < 0) error = ERROR_INVALID_SECURITY_DESCR;
    if (!error && !ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl, SDDL_REVISION_1, &descriptor, nullptr)) error = GetLastError();
    SECURITY_ATTRIBUTES attributes = {sizeof(attributes), descriptor, FALSE};
    for (DWORD i = 0; !error && i < kWorkers; ++i) {
        auto& worker = broker->workers[i];
        worker.broker = broker;
        worker.pipe = CreateNamedPipeW(name, PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED |
            (i == 0 ? FILE_FLAG_FIRST_PIPE_INSTANCE : 0),
            PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_REJECT_REMOTE_CLIENTS,
            kWorkers, sizeof(Response), sizeof(Request), kTimeout, &attributes);
        if (worker.pipe == INVALID_HANDLE_VALUE) { error = GetLastError(); break; }
        worker.event = CreateEventW(nullptr, TRUE, FALSE, nullptr);
        if (!worker.event) { error = GetLastError(); break; }
        worker.thread = CreateThread(nullptr, 0, run, &worker, 0, nullptr);
        if (!worker.thread) error = GetLastError();
    }
    if (descriptor) LocalFree(descriptor);
    if (user_text) LocalFree(user_text);
    if (package_text) LocalFree(package_text);
    if (error) delete broker;
    else *output = broker;
    return error;
}
#endif
} // namespace nub_sandbox::socket_broker
