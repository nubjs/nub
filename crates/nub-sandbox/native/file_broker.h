#pragma once

// Private native-adapter protocol. This is not a user configuration surface.
namespace nub_sandbox::file_broker {
constexpr DWORD kVersion = 1;
constexpr DWORD kPath = 1024;
constexpr NTSTATUS kDenied = static_cast<NTSTATUS>(0xc0000022);
constexpr NTSTATUS kInvalid = static_cast<NTSTATUS>(0xc000000d);
constexpr ULONG kOpenReparse = 0x00200000;
constexpr ULONG kCompleteIfOplocked = 0x00000100;
enum Operation : DWORD { Open = 1, Create = 2, Basic = 3, Full = 4 };
struct Request {
    DWORD version, size, operation, access, share, disposition, options, attributes, length;
    wchar_t path[kPath];
};
struct Response {
    DWORD version, size;
    NTSTATUS status;
    DWORD reserved;
    uint64_t handle, information;
    int64_t creation, access, write, change, allocation, end;
    DWORD attributes, padding;
};
static_assert(sizeof(Request) == 2084);
static_assert(sizeof(Response) == 88);
constexpr DWORD kRead = FILE_READ_DATA | FILE_READ_EA | FILE_READ_ATTRIBUTES |
    FILE_EXECUTE | READ_CONTROL | SYNCHRONIZE;
constexpr DWORD kWrite = FILE_WRITE_DATA | FILE_APPEND_DATA | FILE_WRITE_EA | FILE_WRITE_ATTRIBUTES;
constexpr DWORD kOptions = FILE_NON_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT |
    FILE_SEQUENTIAL_ONLY | FILE_RANDOM_ACCESS | FILE_WRITE_THROUGH;

inline DWORD access_mask(DWORD access) {
    if (access & GENERIC_READ) access = (access & ~GENERIC_READ) | FILE_GENERIC_READ;
    if (access & GENERIC_WRITE) access = (access & ~GENERIC_WRITE) | FILE_GENERIC_WRITE;
    if (access & GENERIC_EXECUTE) access = (access & ~GENERIC_EXECUTE) | FILE_GENERIC_EXECUTE;
    return access;
}

inline bool valid_path(const wchar_t* path, DWORD length) {
    // Only ordinary local-drive names. No remote provider, device namespace,
    // streams, relative roots, short-name spelling, dot segments or wildcards.
    if (length < 4 || length >= kPath || path[length] ||
        !((path[0] >= L'A' && path[0] <= L'Z') || (path[0] >= L'a' && path[0] <= L'z')) ||
        path[1] != L':' || path[2] != L'\\') return false;
    DWORD start = 3;
    for (DWORD i = 3; i <= length; ++i) {
        wchar_t c = path[i];
        if (i == length || c == L'\\') {
            if (i == start || path[i - 1] == L'.' || path[i - 1] == L' ') return false;
            start = i + 1;
        } else if (c < 32 || c == L'/' || c == L':' || c == L'*' || c == L'?' ||
                   c == L'"' || c == L'<' || c == L'>' || c == L'|' || c == L'~') return false;
    }
    for (DWORD i = length + 1; i < kPath; ++i) if (path[i]) return false;
    return true;
}

inline NTSTATUS validate(const Request& request) {
    if (request.version != kVersion || request.size != sizeof(Request) ||
        request.operation < Open || request.operation > Full ||
        !valid_path(request.path, request.length)) return kInvalid;
    if (request.operation >= Basic) {
        return request.access || request.share || request.disposition || request.options ||
            request.attributes ? kInvalid : 0;
    }
    DWORD access = access_mask(request.access);
    if (!access || (access & ~(kRead | kWrite)) ||
        (request.share & ~(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)) ||
        (request.options & ~kOptions) ||
        !(request.options & FILE_SYNCHRONOUS_IO_NONALERT) ||
        ((request.options & FILE_SYNCHRONOUS_IO_NONALERT) && !(access & SYNCHRONIZE)) ||
        (request.attributes && request.attributes != FILE_ATTRIBUTE_NORMAL)) return kInvalid;
    if (request.disposition < FILE_OPEN || request.disposition > FILE_OVERWRITE_IF) return kInvalid;
    if (request.operation == Open && request.disposition != FILE_OPEN) return kInvalid;
    if (request.disposition != FILE_OPEN && !(access & kWrite)) return kInvalid;
    return 0;
}

#ifdef SANDBOX_COMPAT_HOST
using Authorize = DWORD (*)(const void*, const wchar_t*, DWORD);
using NtCreate = NTSTATUS (NTAPI*)(PHANDLE, ACCESS_MASK, POBJECT_ATTRIBUTES,
    PIO_STATUS_BLOCK, PLARGE_INTEGER, ULONG, ULONG, ULONG, ULONG, PVOID, ULONG);
struct Handle {
    HANDLE value = nullptr;
    Handle() = default;
    Handle(const Handle&) = delete;
    Handle& operator=(const Handle&) = delete;
    ~Handle() { if (value && value != INVALID_HANDLE_VALUE) CloseHandle(value); }
};
struct Api {
    NtCreate create = nullptr;
    Api() {
        auto address = GetProcAddress(GetModuleHandleW(L"ntdll.dll"), "NtCreateFile");
        static_assert(sizeof(address) == sizeof(create));
        memcpy(&create, &address, sizeof(address));
    }
};

inline NTSTATUS open_relative(Api& api, HANDLE root, wchar_t* path, DWORD length,
    DWORD access, DWORD share, DWORD disposition, DWORD options, DWORD attributes,
    Handle& output, IO_STATUS_BLOCK& io) {
    UNICODE_STRING name = {USHORT(length * sizeof(wchar_t)), USHORT(length * sizeof(wchar_t)), path};
    OBJECT_ATTRIBUTES attrs = {};
    attrs.Length = sizeof(attrs);
    attrs.RootDirectory = root;
    attrs.ObjectName = &name;
    attrs.Attributes = OBJ_CASE_INSENSITIVE;
    if (!api.create) return kDenied;
    NTSTATUS status = api.create(&output.value, access, &attrs, &io, nullptr, attributes,
        share, disposition, options | kOpenReparse | kCompleteIfOplocked, nullptr, 0);
    // Do not interpret informational oplock/reparse statuses as completed opens.
    return status == 0 ? 0 : (status > 0 ? kDenied : status);
}

inline bool regular(HANDLE handle, bool directory) {
    BY_HANDLE_FILE_INFORMATION info = {};
    return GetFileType(handle) == FILE_TYPE_DISK && GetFileInformationByHandle(handle, &info) &&
        !(info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT) &&
        bool(info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY) == directory &&
        (directory || info.nNumberOfLinks == 1);
}

inline bool final_name(HANDLE handle, wchar_t (&path)[kPath], DWORD& length) {
    wchar_t extended[kPath + 4] = {};
    DWORD size = GetFinalPathNameByHandleW(handle, extended, DWORD(std::size(extended)),
                                          FILE_NAME_NORMALIZED | VOLUME_NAME_DOS);
    if (size < 8 || size >= std::size(extended) || wcsncmp(extended, L"\\\\?\\", 4)) return false;
    length = size - 4;
    memcpy(path, extended + 4, (length + 1) * sizeof(wchar_t));
    return valid_path(path, length);
}

// Every component remains pinned without FILE_SHARE_DELETE for the entire
// operation. The last open is relative to that held parent, never absolute.
inline NTSTATUS resolve(const Request& request, Authorize authorize, const void* context,
                        Handle& result, IO_STATUS_BLOCK& io, Response& response) {
    Api api;
    wchar_t root[] = {request.path[0], L':', L'\\', 0};
    if (GetDriveTypeW(root) != DRIVE_FIXED) return kDenied;
    wchar_t native_root[] = {L'\\', L'?', L'?', L'\\', request.path[0], L':', L'\\', 0};
    // Bound resource cost independently of path length.
    Handle directories[64];
    DWORD depth = 0;
    NTSTATUS status = open_relative(api, nullptr, native_root, 7,
        FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE, FILE_SHARE_READ | FILE_SHARE_WRITE,
        FILE_OPEN, FILE_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT, 0, directories[0], io);
    if (status || !regular(directories[0].value, true)) return status ? status : kDenied;
    wchar_t path[kPath];
    memcpy(path, request.path, sizeof(path));
    DWORD leaf = 3;
    for (DWORD i = 3; i < request.length; ++i) {
        if (path[i] != L'\\') continue;
        if (++depth == std::size(directories)) return kDenied;
        status = open_relative(api, directories[depth - 1].value, path + leaf, i - leaf,
            FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE, FILE_SHARE_READ | FILE_SHARE_WRITE,
            FILE_OPEN, FILE_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT, 0, directories[depth], io);
        if (status || !regular(directories[depth].value, true)) return status ? status : kDenied;
        leaf = i + 1;
    }
    Handle pin;
    DWORD disposition = request.disposition;
    DWORD access = request.operation >= Basic ? FILE_READ_ATTRIBUTES : access_mask(request.access);
    wchar_t canonical[kPath] = {};
    DWORD length = 0;
    if (disposition != FILE_CREATE || request.operation >= Basic) {
        status = open_relative(api, directories[depth].value, path + leaf, request.length - leaf,
            FILE_READ_ATTRIBUTES | SYNCHRONIZE, FILE_SHARE_READ | FILE_SHARE_WRITE,
            FILE_OPEN, FILE_SYNCHRONOUS_IO_NONALERT, 0, pin, io);
        if (status == static_cast<NTSTATUS>(0xc0000034) &&
            (disposition == FILE_OPEN_IF || disposition == FILE_OVERWRITE_IF)) disposition = FILE_CREATE;
        else {
            if (status) return status;
            BY_HANDLE_FILE_INFORMATION info = {};
            if (!GetFileInformationByHandle(pin.value, &info) ||
                !regular(pin.value, request.operation >= Basic && (info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY)) ||
                !final_name(pin.value, canonical, length)) return kDenied;
            if (disposition == FILE_OPEN_IF) disposition = FILE_OPEN;
            if (disposition == FILE_OVERWRITE_IF) disposition = FILE_OVERWRITE;
        }
    }
    if (disposition == FILE_CREATE) {
        // Final-name authority comes from the held directory. A new leaf never
        // follows an attacker-created object: FILE_CREATE rejects collisions.
        if (depth == 0) {
            // Verify the drive mapping itself rather than trusting its spelling.
            wchar_t volume[kPath] = {};
            DWORD size = GetFinalPathNameByHandleW(directories[0].value, volume, kPath, 0);
            if (size != 7 ||
                wcsncmp(volume, L"\\\\?\\", 4) || towupper(volume[4]) != towupper(root[0])) return kDenied;
            memcpy(canonical, root, 3 * sizeof(wchar_t));
            length = 3;
        } else {
            if (!final_name(directories[depth].value, canonical, length)) return kDenied;
            canonical[length++] = L'\\';
        }
        if (length + request.length - leaf >= kPath) return kDenied;
        memcpy(canonical + length, path + leaf, (request.length - leaf + 1) * sizeof(wchar_t));
        length += request.length - leaf;
    }
    DWORD rights = authorize(context, canonical, length);
    if (!(rights & 1) || ((access & kWrite) && !(rights & 2))) return kDenied;
    if (request.operation >= Basic) {
        FILE_BASIC_INFO basic = {};
        FILE_STANDARD_INFO standard = {};
        if (!GetFileInformationByHandleEx(pin.value, FileBasicInfo, &basic, sizeof(basic)) ||
            !GetFileInformationByHandleEx(pin.value, FileStandardInfo, &standard, sizeof(standard))) return kDenied;
        response.creation = basic.CreationTime.QuadPart;
        response.access = basic.LastAccessTime.QuadPart;
        response.write = basic.LastWriteTime.QuadPart;
        response.change = basic.ChangeTime.QuadPart;
        response.attributes = basic.FileAttributes;
        response.allocation = standard.AllocationSize.QuadPart;
        response.end = standard.EndOfFile.QuadPart;
        return 0;
    }
    status = open_relative(api, directories[depth].value, path + leaf, request.length - leaf,
        access | FILE_READ_ATTRIBUTES, request.share & ~FILE_SHARE_DELETE, disposition,
        request.options | FILE_NON_DIRECTORY_FILE, request.attributes, result, io);
    if (status) return status;
    wchar_t opened[kPath] = {};
    DWORD opened_length = 0;
    if (!regular(result.value, false) || !final_name(result.value, opened, opened_length) ||
        opened_length != length || _wcsicmp(opened, canonical)) return kDenied;
    response.information = io.Information;
    return 0;
}

struct Broker;
struct Worker {
    Broker* broker = nullptr;
    HANDLE pipe = INVALID_HANDLE_VALUE, event = nullptr, thread = nullptr;
};
struct Broker {
    HANDLE job = nullptr, stop = nullptr;
    Authorize authorize = nullptr;
    const void* context = nullptr;
    Worker workers[socket_broker::kWorkers];
    void cancel() {
        if (stop) SetEvent(stop);
        for (auto& worker : workers) {
            if (worker.thread) CancelSynchronousIo(worker.thread);
        }
    }
    ~Broker() {
        cancel();
        for (auto& worker : workers) {
            if (worker.thread) {
                WaitForSingleObject(worker.thread, INFINITE);
                CloseHandle(worker.thread);
            }
            if (worker.event) CloseHandle(worker.event);
            if (worker.pipe != INVALID_HANDLE_VALUE) CloseHandle(worker.pipe);
        }
        if (stop) CloseHandle(stop);
    }
};

inline void serve(Worker& worker) {
    Broker& broker = *worker.broker;
    DWORD pid = 0, recheck = 0;
    Handle identity;
    identity.value = socket_broker::requesting_process(worker.pipe, broker.job, pid);
    if (!identity.value) return;
    Handle process;
    process.value = OpenProcess(PROCESS_DUP_HANDLE | PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE,
                                FALSE, pid);
    BOOL member = FALSE;
    if (!process.value || !IsProcessInJob(process.value, broker.job, &member) || !member ||
        WaitForSingleObject(identity.value, 0) != WAIT_TIMEOUT ||
        WaitForSingleObject(process.value, 0) != WAIT_TIMEOUT ||
        !GetNamedPipeClientProcessId(worker.pipe, &recheck) || recheck != pid) return;
    Request request = {};
    Response response = {};
    response.version = kVersion;
    response.size = sizeof(response);
    if (!socket_broker::transfer(worker.pipe, worker.event, broker.stop, &request, sizeof(request), false)) return;
    response.status = validate(request);
    Handle file;
    IO_STATUS_BLOCK io = {};
    if (!response.status) {
        if (WaitForSingleObject(broker.stop, 0) != WAIT_TIMEOUT) response.status = kDenied;
        else response.status = resolve(request, broker.authorize, broker.context, file, io, response);
    }
    if (!response.status && file.value) {
        HANDLE target = nullptr;
        if (WaitForSingleObject(broker.stop, 0) != WAIT_TIMEOUT ||
            WaitForSingleObject(process.value, 0) != WAIT_TIMEOUT ||
            !DuplicateHandle(GetCurrentProcess(), file.value, process.value, &target,
                             access_mask(request.access), FALSE, 0)) response.status = kDenied;
        // The recipient owns this handle even if delivery fails. Never close a
        // remote numeric handle: its process may already have reused the value.
        // Each worker closes its source handle; command Job exit reclaims the
        // recipient's handles without limiting legitimate cumulative opens.
        else response.handle = reinterpret_cast<uintptr_t>(target);
    }
    socket_broker::transfer(worker.pipe, worker.event, broker.stop, &response, sizeof(response), true);
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
            ready = socket_broker::complete(worker.pipe, operation, connected,
                worker.broker->stop, INFINITE, bytes);
        }
        if (ready) serve(worker);
        DisconnectNamedPipe(worker.pipe);
    }
    return 0;
}

inline DWORD start(HANDLE job, const wchar_t* name, PSID user, PSID package,
                   Authorize authorize, const void* context, Broker** output) {
    *output = nullptr;
    if (!authorize || !context) return ERROR_INVALID_PARAMETER;
    auto broker = new (std::nothrow) Broker;
    if (!broker) return ERROR_NOT_ENOUGH_MEMORY;
    broker->job = job;
    broker->authorize = authorize;
    broker->context = context;
    broker->stop = CreateEventW(nullptr, TRUE, FALSE, nullptr);
    DWORD error = broker->stop ? ERROR_SUCCESS : GetLastError();
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
    for (DWORD i = 0; !error && i < socket_broker::kWorkers; ++i) {
        auto& worker = broker->workers[i];
        worker.broker = broker;
        worker.pipe = CreateNamedPipeW(name, PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED |
            (i == 0 ? FILE_FLAG_FIRST_PIPE_INSTANCE : 0),
            PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_REJECT_REMOTE_CLIENTS,
            socket_broker::kWorkers, sizeof(Response), sizeof(Request), socket_broker::kTimeout, &attributes);
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
} // namespace nub_sandbox::file_broker
