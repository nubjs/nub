#pragma once

extern "C" DWORD sandbox_socket_broker_test_frames(const wchar_t* name) {
    using namespace nub_sandbox::socket_broker;
    PSID package = nullptr;
    if (!ConvertStringSidToSidW(L"S-1-15-2-1-2-3-4-5-6-7", &package)) return GetLastError();
    wchar_t qualified[256];
    bool valid_names =
        !server_name(L"\\\\.\\pipe\\LOCAL\\fixture", 0, package, qualified) &&
        !wcscmp(qualified, L"\\\\.\\pipe\\Sessions\\0\\AppContainerNamedObjects\\S-1-15-2-1-2-3-4-5-6-7\\fixture") &&
        !server_name(L"\\\\.\\pipe\\local\\fixture", 42, package, qualified) &&
        !wcscmp(qualified, L"\\\\.\\pipe\\Sessions\\42\\AppContainerNamedObjects\\S-1-15-2-1-2-3-4-5-6-7\\fixture");
    for (const wchar_t* invalid : {L"\\\\.\\pipe\\fixture", L"\\\\.\\pipe\\LOCAL\\",
                                   L"\\\\.\\pipe\\LOCAL\\nested\\fixture", L"\\\\host\\pipe\\LOCAL\\fixture"})
        valid_names = valid_names && server_name(invalid, 0, package, qualified) == ERROR_INVALID_NAME;
    wchar_t too_long[300];
    wcscpy_s(too_long, L"\\\\.\\pipe\\LOCAL\\");
    size_t prefix = wcslen(too_long);
    for (size_t i = prefix; i + 1 < _countof(too_long); ++i) too_long[i] = L'x';
    too_long[_countof(too_long) - 1] = 0;
    valid_names = valid_names &&
        server_name(too_long, 42, package, qualified) == ERROR_FILENAME_EXCED_RANGE;
    LocalFree(package);
    if (!ConvertStringSidToSidW(L"S-1-15-2-7-6-5-4-3-2-1", &package)) return GetLastError();
    valid_names = valid_names &&
        !server_name(L"\\\\.\\pipe\\LOCAL\\fixture", MAXDWORD, package, qualified) &&
        !wcscmp(qualified, L"\\\\.\\pipe\\Sessions\\4294967295\\AppContainerNamedObjects\\S-1-15-2-7-6-5-4-3-2-1\\fixture");
    LocalFree(package);
    if (!valid_names) return ERROR_INVALID_DATA;
    HANDLE server = CreateNamedPipeW(name, PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED |
        FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE |
        PIPE_REJECT_REMOTE_CLIENTS, 1, 1024, 1024, kTimeout, nullptr);
    if (server == INVALID_HANDLE_VALUE) return GetLastError();
    HANDLE client = CreateFileW(name, kClientAccess, 0, nullptr, OPEN_EXISTING,
                                FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT |
                                SECURITY_IDENTIFICATION, nullptr);
    DWORD error = client == INVALID_HANDLE_VALUE ? GetLastError() : ERROR_SUCCESS;
    Stage stage = Stage::PipeOpen;
    if (!error) {
        error = configure_client(client, GetCurrentProcessId(), stage);
        if (error) diagnose(true, stage, error);
        else if (configure_client(client, 0, stage) != ERROR_ACCESS_DENIED ||
                 stage != Stage::ServerPidMismatch) error = ERROR_INVALID_DATA;
    }
    HANDLE incoming = CreateEventW(nullptr, TRUE, FALSE, nullptr);
    HANDLE outgoing = CreateEventW(nullptr, TRUE, FALSE, nullptr);
    HANDLE stop = CreateEventW(nullptr, TRUE, FALSE, nullptr);
    if (!error && (!incoming || !outgoing || !stop)) error = ERROR_NOT_ENOUGH_MEMORY;
    if (!error) {
        OVERLAPPED connection = {};
        connection.hEvent = incoming;
        DWORD bytes = 0;
        BOOL connected = ConnectNamedPipe(server, &connection);
        if ((connected || GetLastError() != ERROR_PIPE_CONNECTED) &&
            !complete(server, connection, connected, stop, kTimeout, bytes)) error = ERROR_PIPE_NOT_CONNECTED;
    }
    for (DWORD size : {DWORD(sizeof(Request) - 1), DWORD(sizeof(Request)), DWORD(sizeof(Request) + 1)}) {
        if (error) break;
        BYTE frame[sizeof(Request) + 1] = {};
        if (!transfer(client, outgoing, nullptr, frame, size, true)) { error = ERROR_WRITE_FAULT; break; }
        Request request = {};
        bool received = transfer(server, incoming, nullptr, &request, sizeof(request), false);
        if (received != (size == sizeof(Request))) { error = ERROR_INVALID_DATA; break; }
        if (size > sizeof(Request)) {
            BYTE remainder;
            if (!transfer(server, incoming, nullptr, &remainder, 1, false)) error = ERROR_READ_FAULT;
        }
    }
    if (!error) {
        // No message is queued: this starts a real pending read, cancels it and
        // must drain its OVERLAPPED before returning to release the stack.
        SetEvent(stop);
        Request request = {};
        ULONGLONG before = GetTickCount64();
        if (transfer(server, incoming, stop, &request, sizeof(request), false) ||
            GetTickCount64() - before > 2000) error = ERROR_INVALID_DATA;
        // A subsequent exact exchange proves cancellation left no outstanding
        // read to consume a later command's buffer.
        if (!error && (!transfer(client, outgoing, nullptr, &request, sizeof(request), true) ||
                       !transfer(server, incoming, nullptr, &request, sizeof(request), false)))
            error = ERROR_INVALID_DATA;
    }
    if (!error) {
        // An empty request is a disconnect without a frame, not a zero-byte
        // WriteFile (whose null-write semantics vary by transport).
        CloseHandle(client);
        client = INVALID_HANDLE_VALUE;
        Request request = {};
        if (transfer(server, incoming, nullptr, &request, sizeof(request), false)) error = ERROR_INVALID_DATA;
    }
    if (client != INVALID_HANDLE_VALUE) CloseHandle(client);
    CloseHandle(server);
    if (incoming) CloseHandle(incoming);
    if (outgoing) CloseHandle(outgoing);
    if (stop) CloseHandle(stop);
    return error;
}

// Called only by the Rust unit harness. All handles belong to this test process;
// the empty command Job deliberately does not authorize the connecting process.
extern "C" DWORD sandbox_socket_broker_test_foreign_client(const wchar_t* name) {
    using namespace nub_sandbox::socket_broker;
    HANDLE token = nullptr;
    if (!OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &token)) return GetLastError();
    alignas(void*) BYTE user[512];
    DWORD needed = 0;
    BOOL queried = GetTokenInformation(token, TokenUser, user, sizeof(user), &needed);
    DWORD error = queried ? ERROR_SUCCESS : GetLastError();
    CloseHandle(token);
    if (error) return error;
    PSID package = nullptr;
    if (!ConvertStringSidToSidW(L"S-1-15-2-1", &package)) return GetLastError();
    HANDLE job = CreateJobObjectW(nullptr, nullptr);
    if (!job) { error = GetLastError(); LocalFree(package); return error; }
    Broker* broker = nullptr;
    error = start(job, name, reinterpret_cast<TOKEN_USER*>(user)->User.Sid, package, &broker);
    LocalFree(package);
    if (!error) {
        HANDLE pipe = CreateFileW(name, GENERIC_READ | GENERIC_WRITE, 0, nullptr, OPEN_EXISTING,
                                   FILE_FLAG_OVERLAPPED, nullptr);
        if (pipe == INVALID_HANDLE_VALUE) error = GetLastError();
        else {
            HANDLE event = CreateEventW(nullptr, TRUE, FALSE, nullptr);
            if (!event) error = GetLastError();
            else {
                Response response = {};
                bool received = transfer(pipe, event, nullptr, &response, sizeof(response), false);
                DWORD failure = GetLastError();
                // A timeout is not a passing rejection: NPFS must report the
                // server's disconnect before any socket information arrives.
                if (received || (failure != ERROR_BROKEN_PIPE && failure != ERROR_PIPE_NOT_CONNECTED))
                    error = ERROR_INVALID_DATA;
                CloseHandle(event);
            }
            CloseHandle(pipe);
        }
    }
    // The other three workers are awaiting connects; drop must cancel and join
    // them instead of waiting for a client or keeping the borrowed Job alive.
    ULONGLONG stopped = GetTickCount64();
    delete broker;
    if (!error && GetTickCount64() - stopped > 2000) error = WAIT_TIMEOUT;
    CloseHandle(job);
    return error;
}
