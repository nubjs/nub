#pragma once

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
