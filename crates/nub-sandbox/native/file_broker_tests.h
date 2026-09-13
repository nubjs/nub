#pragma once

extern "C" DWORD sandbox_file_broker_test_frames(const wchar_t* name) {
    using namespace nub_sandbox;
    file_broker::Handle server, client, incoming, outgoing, stop;
    server.value = CreateNamedPipeW(name, PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED |
        FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE |
        PIPE_REJECT_REMOTE_CLIENTS, 1, 4096, 4096, socket_broker::kTimeout, nullptr);
    if (server.value == INVALID_HANDLE_VALUE) return GetLastError();
    client.value = CreateFileW(name, GENERIC_READ | GENERIC_WRITE, 0, nullptr, OPEN_EXISTING,
                              FILE_FLAG_OVERLAPPED, nullptr);
    if (client.value == INVALID_HANDLE_VALUE) return GetLastError();
    incoming.value = CreateEventW(nullptr, TRUE, FALSE, nullptr);
    outgoing.value = CreateEventW(nullptr, TRUE, FALSE, nullptr);
    stop.value = CreateEventW(nullptr, TRUE, FALSE, nullptr);
    if (!incoming.value || !outgoing.value || !stop.value) return GetLastError();
    OVERLAPPED connection = {};
    connection.hEvent = incoming.value;
    DWORD bytes = 0;
    BOOL connected = ConnectNamedPipe(server.value, &connection);
    if ((connected || GetLastError() != ERROR_PIPE_CONNECTED) &&
        !socket_broker::complete(server.value, connection, connected, stop.value,
                                socket_broker::kTimeout, bytes)) return ERROR_PIPE_NOT_CONNECTED;
    for (DWORD size : {DWORD(sizeof(file_broker::Request) - 1), DWORD(sizeof(file_broker::Request)),
                       DWORD(sizeof(file_broker::Request) + 1)}) {
        BYTE frame[sizeof(file_broker::Request) + 1] = {};
        if (!socket_broker::transfer(client.value, outgoing.value, nullptr, frame, size, true)) return ERROR_WRITE_FAULT;
        file_broker::Request request = {};
        bool received = socket_broker::transfer(server.value, incoming.value, nullptr, &request, sizeof(request), false);
        if (received != (size == sizeof(request))) return ERROR_INVALID_DATA;
        if (size > sizeof(request)) {
            BYTE remainder;
            if (!socket_broker::transfer(server.value, incoming.value, nullptr, &remainder, 1, false)) return ERROR_READ_FAULT;
        }
    }
    SetEvent(stop.value);
    file_broker::Request request = {};
    if (socket_broker::transfer(server.value, incoming.value, stop.value, &request, sizeof(request), false)) return ERROR_INVALID_DATA;
    if (!socket_broker::transfer(client.value, outgoing.value, nullptr, &request, sizeof(request), true) ||
        !socket_broker::transfer(server.value, incoming.value, nullptr, &request, sizeof(request), false)) return ERROR_INVALID_DATA;
    return 0;
}

extern "C" DWORD sandbox_file_broker_test_foreign_client(const wchar_t* name) {
    using namespace nub_sandbox;
    file_broker::Handle token, job;
    if (!OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &token.value)) return GetLastError();
    alignas(void*) BYTE user[512];
    DWORD needed = 0;
    if (!GetTokenInformation(token.value, TokenUser, user, sizeof(user), &needed)) return GetLastError();
    PSID package = nullptr;
    if (!ConvertStringSidToSidW(L"S-1-15-2-1", &package)) return GetLastError();
    job.value = CreateJobObjectW(nullptr, nullptr);
    if (!job.value) { LocalFree(package); return GetLastError(); }
    bool called = false;
    auto authorize = [](const void* context, const wchar_t*, DWORD) -> DWORD {
        *const_cast<bool*>(static_cast<const bool*>(context)) = true;
        return 3;
    };
    file_broker::Broker* broker = nullptr;
    DWORD error = file_broker::start(job.value, name, reinterpret_cast<TOKEN_USER*>(user)->User.Sid,
                                    package, authorize, &called, &broker);
    LocalFree(package);
    if (error) return error;
    file_broker::Handle pipe, event;
    pipe.value = CreateFileW(name, GENERIC_READ | GENERIC_WRITE, 0, nullptr, OPEN_EXISTING,
                             FILE_FLAG_OVERLAPPED, nullptr);
    if (pipe.value == INVALID_HANDLE_VALUE) error = GetLastError();
    else {
        event.value = CreateEventW(nullptr, TRUE, FALSE, nullptr);
        if (!event.value) error = GetLastError();
        else {
            file_broker::Response response = {};
            bool received = socket_broker::transfer(pipe.value, event.value, nullptr, &response, sizeof(response), false);
            DWORD failure = GetLastError();
            if (received || (failure != ERROR_BROKEN_PIPE && failure != ERROR_PIPE_NOT_CONNECTED)) error = ERROR_INVALID_DATA;
        }
    }
    delete broker;
    if (called) error = ERROR_INVALID_DATA;
    return error;
}

// Called in the real test child, so GetProcAddress observes installed Detours.
// `statuses` is an optional fixed-size diagnostic sink used by the parent-side
// control. It records raw NTSTATUS values without changing the child verdict.
extern "C" DWORD sandbox_file_broker_test_four_calls(const wchar_t* path, BOOL allowed,
                                                      NTSTATUS* statuses) {
    using namespace nub_sandbox::file_broker;
    wchar_t native[kPath + 4];
    if (swprintf_s(native, L"\\??\\%s", path) < 0) return 1;
    UNICODE_STRING name = {};
    name.Buffer = native;
    name.Length = name.MaximumLength = USHORT(wcslen(native) * sizeof(wchar_t));
    OBJECT_ATTRIBUTES attrs = {};
    attrs.Length = sizeof(attrs);
    attrs.Attributes = OBJ_CASE_INSENSITIVE;
    attrs.ObjectName = &name;
    using OpenFn = NTSTATUS (NTAPI*)(PHANDLE, ACCESS_MASK, POBJECT_ATTRIBUTES, PIO_STATUS_BLOCK, ULONG, ULONG);
    using QueryFn = NTSTATUS (NTAPI*)(POBJECT_ATTRIBUTES, PVOID);
    auto module = GetModuleHandleW(L"ntdll.dll");
    auto open = reinterpret_cast<OpenFn>(GetProcAddress(module, "NtOpenFile"));
    auto create = reinterpret_cast<NtCreate>(GetProcAddress(module, "NtCreateFile"));
    auto basic = reinterpret_cast<QueryFn>(GetProcAddress(module, "NtQueryAttributesFile"));
    auto full = reinterpret_cast<QueryFn>(GetProcAddress(module, "NtQueryFullAttributesFile"));
    if (!open || !create || !basic || !full) return 2;
    IO_STATUS_BLOCK io = {};
    Handle first, second;
    alignas(8) BYTE metadata[56] = {};
    NTSTATUS results[] = {
        open(&first.value, GENERIC_READ, &attrs, &io, FILE_SHARE_READ | FILE_SHARE_WRITE,
             FILE_NON_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT),
        create(&second.value, GENERIC_READ, &attrs, &io, nullptr, 0,
            FILE_SHARE_READ | FILE_SHARE_WRITE, FILE_OPEN,
            FILE_NON_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT, nullptr, 0),
        basic(&attrs, metadata), full(&attrs, metadata),
    };
    if (statuses) memcpy(statuses, results, sizeof(results));
    DWORD failures = 0;
    for (DWORD i = 0; i < std::size(results); ++i) {
        if (allowed ? results[i] != 0 : results[i] != kDenied) failures |= 1u << (i + 4);
    }
    return failures;
}

extern "C" DWORD sandbox_file_broker_test_loader(const wchar_t* path, BOOL allowed) {
    HMODULE module = LoadLibraryW(path);
    if (!allowed) {
        if (module) { FreeLibrary(module); return ERROR_INVALID_DATA; }
        return GetLastError() == ERROR_ACCESS_DENIED ? 0 : ERROR_INVALID_DATA;
    }
    if (!module) return GetLastError();
    using Value = DWORD (*)();
    auto value = reinterpret_cast<Value>(GetProcAddress(module, "SandboxFileBrokerValue"));
    DWORD result = value && value() == 1138 ? 0 : ERROR_INVALID_DATA;
    FreeLibrary(module);
    return result;
}

extern "C" DWORD sandbox_file_broker_test_reparse_after_open(const wchar_t* path) {
    // Non-Microsoft GUID tags require no symlink privilege. This deliberately
    // mutates an already-issued ordinary rw capability, then tests a fresh open.
    struct Reparse {
        DWORD tag;
        WORD length, reserved;
        GUID guid;
    } data = {0x42, 0, 0, {0x781c2b02, 0x81a1, 0x4d82, {0xa7, 0x33, 0x82, 0x01, 0x41, 0xd1, 0x34, 0x1f}}};
    nub_sandbox::file_broker::Handle file;
    file.value = CreateFileW(path, GENERIC_READ | GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE,
                             nullptr, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, nullptr);
    if (file.value == INVALID_HANDLE_VALUE) return GetLastError();
    DWORD bytes = 0;
    if (!DeviceIoControl(file.value, 0x000900a4 /* FSCTL_SET_REPARSE_POINT */, &data, sizeof(data),
                         nullptr, 0, &bytes, nullptr)) return GetLastError();
    BY_HANDLE_FILE_INFORMATION info = {};
    if (!GetFileInformationByHandle(file.value, &info) ||
        !(info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT)) return ERROR_INVALID_DATA;
    return 0;
}
