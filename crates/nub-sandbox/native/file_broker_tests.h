#pragma once

extern "C" NTSTATUS sandbox_file_broker_test_rename_handle(HANDLE file, const wchar_t* destination) {
    using namespace nub_sandbox::file_broker;
    auto set = reinterpret_cast<NtSetInformation>(
        GetProcAddress(GetModuleHandleW(L"ntdll.dll"), "NtSetInformationFile"));
    if (!set) return kDenied;
    NameInformation name = {};
    if (swprintf_s(name.name, L"\\??\\%s", destination) < 0) return kInvalid;
    name.length = DWORD(wcslen(name.name) * sizeof(wchar_t));
    IO_STATUS_BLOCK io = {};
    return set(file, &io, &name, DWORD(offsetof(NameInformation, name) + name.length),
               static_cast<FILE_INFORMATION_CLASS>(10));
}

extern "C" DWORD sandbox_file_broker_test_requested_name(const wchar_t* root) {
    using namespace nub_sandbox::file_broker;
    Api api;
    Request request = {};
    if (swprintf_s(request.path, L"%s\\name-source.json", root) < 0) return 1;
    request.length = DWORD(wcslen(request.path));
    ParentPath parent;
    IO_STATUS_BLOCK io = {};
    if (resolve_parent(api, request, parent, io)) return 2;
    Handle file;
    if (open_relative(api, parent.handle(), parent.path + parent.leaf,
        request.length - parent.leaf, FILE_GENERIC_READ,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, FILE_OPEN,
        FILE_NON_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT, 0, file, io)) return 3;
    wchar_t canonical[kPath] = {};
    DWORD length = 0;
    if (!requested_name(file.value, parent, canonical, length)) return 4;
    for (DWORD i = 0; i < parent.length; ++i) parent.canonical[i] = towupper(parent.canonical[i]);
    if (!requested_name(file.value, parent, canonical, length)) return 5;
    Request alias = {};
    if (swprintf_s(alias.path, L"%s\\name-alias.txt", root) < 0) return 6;
    alias.length = DWORD(wcslen(alias.path));
    ParentPath other;
    if (resolve_parent(api, alias, other, io)) return 7;
    // The other name is a hardlink to the same file, not an unrelated object.
    Handle linked;
    if (open_relative(api, other.handle(), other.path + other.leaf,
        alias.length - other.leaf, FILE_GENERIC_READ,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, FILE_OPEN,
        FILE_NON_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT, 0, linked, io) ||
        !same_file(file.value, linked.value)) return 8;
    if (requested_name(file.value, other, canonical, length)) return 9;
    if (!requested_name(linked.value, other, canonical, length)) return 10;
    Request moved = {};
    if (swprintf_s(moved.path, L"%s\\name-moved.json", root) < 0 ||
        !MoveFileExW(request.path, moved.path, 0)) return 11;
    moved.length = DWORD(wcslen(moved.path));
    // The original handle stays usable, but its changed name must not
    // authorize a fresh request for the old spelling.
    if (requested_name(file.value, parent, canonical, length)) return 12;
    ParentPath destination;
    if (resolve_parent(api, moved, destination, io) ||
        !requested_name(file.value, destination, canonical, length)) return 13;
    return 0;
}

// Real NT calls, also run unconfined before the raw/adapter pair. These do not
// call the resolver directly and cannot pass by testing only the matcher.
extern "C" DWORD sandbox_file_broker_test_namespace(const wchar_t* root, BOOL allowed) {
    using namespace nub_sandbox::file_broker;
    Api api;
    auto set = reinterpret_cast<NtSetInformation>(GetProcAddress(GetModuleHandleW(L"ntdll.dll"), "NtSetInformationFile"));
    using QueryDirectory = NTSTATUS (NTAPI*)(HANDLE, HANDLE, PVOID, PVOID, PIO_STATUS_BLOCK,
        PVOID, ULONG, FILE_INFORMATION_CLASS, BOOLEAN, PUNICODE_STRING, BOOLEAN);
    auto query = reinterpret_cast<QueryDirectory>(GetProcAddress(GetModuleHandleW(L"ntdll.dll"), "NtQueryDirectoryFile"));
    if (!api.create || !set || !query) return 1;
    auto open = [&](const wchar_t* leaf, DWORD access, DWORD disposition, bool directory, Handle& handle) {
        wchar_t path[kPath + 4];
        if (swprintf_s(path, L"\\??\\%s\\%s", root, leaf) < 0) return kInvalid;
        UNICODE_STRING name = {USHORT(wcslen(path) * sizeof(wchar_t)), USHORT(wcslen(path) * sizeof(wchar_t)), path};
        OBJECT_ATTRIBUTES attrs = {};
        attrs.Length = sizeof(attrs);
        attrs.Attributes = OBJ_CASE_INSENSITIVE;
        attrs.ObjectName = &name;
        IO_STATUS_BLOCK io = {};
        return api.create(&handle.value, access | SYNCHRONIZE, &attrs, &io, nullptr, 0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, disposition,
            FILE_SYNCHRONOUS_IO_NONALERT | (directory ? FILE_DIRECTORY_FILE : FILE_NON_DIRECTORY_FILE), nullptr, 0);
    };
    auto named = [&](HANDLE source, const wchar_t* leaf, DWORD kind) {
        NameInformation name = {};
        if (swprintf_s(name.name, L"\\??\\%s\\%s", root, leaf) < 0) return kInvalid;
        name.length = DWORD(wcslen(name.name) * sizeof(wchar_t));
        IO_STATUS_BLOCK io = {};
        return set(source, &io, &name, DWORD(offsetof(NameInformation, name) + name.length),
                   static_cast<FILE_INFORMATION_CLASS>(kind));
    };
    Handle directory, listing, source, remove, force_image, readonly;
    NTSTATUS status = open(L"created.dir", FILE_LIST_DIRECTORY | FILE_ADD_FILE | FILE_ADD_SUBDIRECTORY | DELETE,
                           FILE_CREATE, true, directory);
    if ((status == 0) != bool(allowed)) return 2;
    status = open(L"listing.dir", FILE_LIST_DIRECTORY, FILE_OPEN, true, listing);
    if ((status == 0) != bool(allowed)) return 3;
    status = open(L"source.json", GENERIC_READ | DELETE, FILE_OPEN, false, source);
    if ((status == 0) != bool(allowed)) return 4;
    status = open(L"remove.json", DELETE, FILE_OPEN, false, remove);
    if ((status == 0) != bool(allowed)) return 5;
    status = open(L"force-image.json", DELETE, FILE_OPEN, false, force_image);
    if ((status == 0) != bool(allowed)) return 24;
    if (!allowed) return 0;
    // The ordinary kernel path on a broker directory handle must not create a
    // child. Root-relative opens are not forwarded by the adapter.
    wchar_t child_name[] = L"unauthorized.txt";
    UNICODE_STRING child = {sizeof(child_name) - sizeof(wchar_t), sizeof(child_name), child_name};
    OBJECT_ATTRIBUTES attrs = {};
    attrs.Length = sizeof(attrs);
    attrs.RootDirectory = directory.value;
    attrs.ObjectName = &child;
    attrs.Attributes = OBJ_CASE_INSENSITIVE;
    Handle escaped;
    IO_STATUS_BLOCK io = {};
    // Only the confined arm lacks raw directory write authority.
    wchar_t mode[16] = {};
    bool confined = GetEnvironmentVariableW(L"NUB_FILE_BROKER_TEST_MODE", mode, _countof(mode)) && !wcscmp(mode, L"broker");
    using QueryInformation = NTSTATUS (NTAPI*)(HANDLE, PIO_STATUS_BLOCK, PVOID, ULONG, FILE_INFORMATION_CLASS);
    auto query_info = reinterpret_cast<QueryInformation>(GetProcAddress(GetModuleHandleW(L"ntdll.dll"), "NtQueryInformationFile"));
    DWORD granted = 0;
    if (confined && (!query_info ||
        query_info(directory.value, &io, &granted, sizeof(granted), static_cast<FILE_INFORMATION_CLASS>(8)) != 0 ||
        (granted & (DELETE | kWrite)))) return 17;
    if (confined && (query_info(source.value, &io, &granted, sizeof(granted), static_cast<FILE_INFORMATION_CLASS>(8)) != 0 ||
        (granted & DELETE))) return 18;
    if (confined && api.create(&escaped.value, FILE_WRITE_DATA | SYNCHRONIZE, &attrs, &io,
        nullptr, 0, FILE_SHARE_READ | FILE_SHARE_WRITE, FILE_CREATE,
        FILE_NON_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT, nullptr, 0) >= 0) return 6;
    alignas(8) BYTE entries[4096] = {};
    status = query(listing.value, nullptr, nullptr, nullptr, &io, entries, sizeof(entries),
                   static_cast<FILE_INFORMATION_CLASS>(12), FALSE, nullptr, TRUE);
    if (status != 0 || !io.Information) return 7;
    struct Entry { ULONG next, index, length; wchar_t name[1]; };
    bool found = false;
    for (ULONG offset = 0; offset < io.Information;) {
        if (io.Information - offset < offsetof(Entry, name)) return 19;
        auto entry = reinterpret_cast<const Entry*>(entries + offset);
        if (entry->length > io.Information - offset - offsetof(Entry, name)) return 19;
        if (entry->length == 9 * sizeof(wchar_t) && !wmemcmp(entry->name, L"entry.txt", 9)) found = true;
        if (!entry->next) break;
        if (entry->next < offsetof(Entry, name) || entry->next > io.Information - offset) return 19;
        offset += entry->next;
    }
    if (!found) return 20;
    if (confined && named(source.value, L"forbidden.txt", 10) >= 0) return 8;
    if (confined && named(source.value, L"forbidden.txt", 11) >= 0) return 9;
    if (named(source.value, L"renamed.json", 10) != 0) return 10;
    // A native handle remains bound to the source object after its name moves.
    // The broker must resolve this new name, not retain stale client text.
    if (named(source.value, L"new-link.json", 11) != 0) return 11;
    Handle linked;
    if (open(L"new-link.json", GENERIC_READ, FILE_OPEN, false, linked) != 0) return 12;
    if (open(L"readonly.txt", GENERIC_READ, FILE_OPEN, false, readonly) != 0) return 13;
    if (confined && named(readonly.value, L"amplified.json", 11) >= 0) return 14;
    if (confined) {
        wchar_t temp[kPath];
        DWORD length = GetTempPathW(kPath, temp);
        if (!length || length >= kPath) return 21;
        NameInformation alias = {};
        if (swprintf_s(alias.name, L"\\??\\%sbroker-amplification-%lu.json", temp, GetCurrentProcessId()) < 0) return 21;
        alias.length = DWORD(wcslen(alias.name) * sizeof(wchar_t));
        {
            Handle writable;
            writable.value = CreateFileW(alias.name + 4, GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE,
                                         nullptr, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, nullptr);
            if (writable.value == INVALID_HANDLE_VALUE) return 23;
        }
        if (!DeleteFileW(alias.name + 4)) return 23;
        // TEMP is directly writable by the child. A raw link there must not
        // amplify a read-only source, even before the broker sees the request.
        if (set(readonly.value, &io, &alias, DWORD(offsetof(NameInformation, name) + alias.length),
                static_cast<FILE_INFORMATION_CLASS>(11)) >= 0) return 22;
    }
    BOOLEAN deleted = TRUE;
    if (set(remove.value, &io, &deleted, sizeof(deleted), static_cast<FILE_INFORMATION_CLASS>(13)) != 0) return 15;
    // DELETE | POSIX | FORCE_IMAGE_SECTION_CHECK is the ordinary Win32
    // disposition shape. The final bit preserves the legacy image-section
    // safety check; it does not grant additional authority.
    DWORD force_image_delete = 0x7;
    if (set(force_image.value, &io, &force_image_delete, sizeof(force_image_delete),
            static_cast<FILE_INFORMATION_CLASS>(64)) != 0) return 25;
    if (set(directory.value, &io, &deleted, sizeof(deleted), static_cast<FILE_INFORMATION_CLASS>(13)) != 0) return 16;
    return 0;
}

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

extern "C" DWORD sandbox_file_broker_test_quality() {
    using namespace nub_sandbox::file_broker;
    if (valid_object_flags(0) || valid_object_flags(kIgnoreImpersonatedDeviceMap)) return ERROR_INVALID_DATA;
    for (DWORD flags : {DWORD(OBJ_CASE_INSENSITIVE), DWORD(OBJ_CASE_INSENSITIVE | kIgnoreImpersonatedDeviceMap)}) {
        if (!valid_object_flags(flags)) return ERROR_INVALID_DATA;
        for (unsigned bit = 0; bit < 32; ++bit) {
            DWORD added = 1u << bit;
            bool expected = (added & ~(OBJ_CASE_INSENSITIVE | kIgnoreImpersonatedDeviceMap)) == 0;
            if (valid_object_flags(flags | added) != expected) return ERROR_INVALID_DATA;
        }
    }
    SECURITY_QUALITY_OF_SERVICE quality = {};
    quality.Length = sizeof(quality);
    DWORD fields = 0;
    for (DWORD impersonation = SecurityAnonymous; impersonation <= SecurityDelegation; ++impersonation) {
        quality.ImpersonationLevel = static_cast<SECURITY_IMPERSONATION_LEVEL>(impersonation);
        for (SECURITY_CONTEXT_TRACKING_MODE tracking : {SECURITY_STATIC_TRACKING, SECURITY_DYNAMIC_TRACKING}) {
            quality.ContextTrackingMode = tracking;
            for (unsigned effective = 0; effective <= 0xff; ++effective) {
                quality.EffectiveOnly = static_cast<BOOLEAN>(effective);
                if (!valid_quality(quality, fields) || fields) return ERROR_INVALID_DATA;
            }
        }
    }
    quality.Length = sizeof(quality) - 1;
    if (valid_quality(quality, fields) || fields != QualityLength) return ERROR_INVALID_DATA;
    quality.Length = sizeof(quality);
    quality.ImpersonationLevel = static_cast<SECURITY_IMPERSONATION_LEVEL>(SecurityDelegation + 1);
    if (valid_quality(quality, fields) || fields != QualityImpersonation) return ERROR_INVALID_DATA;
    quality.ImpersonationLevel = SecurityImpersonation;
    quality.ContextTrackingMode = static_cast<SECURITY_CONTEXT_TRACKING_MODE>(2);
    if (valid_quality(quality, fields) || fields != QualityTracking) return ERROR_INVALID_DATA;
    quality.ContextTrackingMode = SECURITY_STATIC_TRACKING;
    return valid_quality(quality, fields) && !fields ? 0 : ERROR_INVALID_DATA;
}

// Called in the real test child, so GetProcAddress observes installed Detours.
// `statuses` is an optional six-slot diagnostic sink used by the parent-side
// control. It records generic-only open/create, then the four actual calls,
// without changing the child verdict.
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
    constexpr ACCESS_MASK actual_read = GENERIC_READ | SYNCHRONIZE;
    IO_STATUS_BLOCK io = {};
    Handle first, second, generic_first, generic_second;
    alignas(8) BYTE metadata[56] = {};
    NTSTATUS results[] = {
        open(&first.value, actual_read, &attrs, &io, FILE_SHARE_READ | FILE_SHARE_WRITE,
             FILE_NON_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT),
        create(&second.value, actual_read, &attrs, &io, nullptr, 0,
            FILE_SHARE_READ | FILE_SHARE_WRITE, FILE_OPEN,
            FILE_NON_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT, nullptr, 0),
        basic(&attrs, metadata), full(&attrs, metadata),
    };
    if (statuses) {
        // Keep the unconfined discriminator out of the sandbox child: the
        // tested contract is the documented synchronous desired-access mask.
        statuses[0] = open(&generic_first.value, GENERIC_READ, &attrs, &io,
            FILE_SHARE_READ | FILE_SHARE_WRITE, FILE_NON_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT);
        statuses[1] = create(&generic_second.value, GENERIC_READ, &attrs, &io, nullptr, 0,
            FILE_SHARE_READ | FILE_SHARE_WRITE, FILE_OPEN,
            FILE_NON_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT, nullptr, 0);
        memcpy(statuses + 2, results, sizeof(results));
    }
    DWORD failures = 0;
    for (DWORD i = 0; i < std::size(results); ++i) {
        if (allowed ? results[i] != 0 : results[i] != kDenied) failures |= 1u << (i + 4);
    }
    attrs.Attributes |= kIgnoreImpersonatedDeviceMap;
    for (NTSTATUS status : {basic(&attrs, metadata), full(&attrs, metadata)}) {
        if (allowed ? status != 0 : status != kDenied) failures |= 1u << 8;
    }
    return failures;
}

// The matching caller requests no read share. Run first without confinement,
// then in raw and brokered AppContainer children; this cannot pass by calling
// the resolver directly.
extern "C" DWORD sandbox_file_broker_test_exclusive_overwrite(const wchar_t* root, BOOL allowed) {
    using namespace nub_sandbox::file_broker;
    Api api;
    if (!api.create) return 1;
    const NTSTATUS denied = kDenied;
    const NTSTATUS missing = static_cast<NTSTATUS>(0xc0000034);
    auto overwrite = [&](const wchar_t* leaf, ULONG disposition, ULONG share,
                         NTSTATUS expected, ULONG_PTR information, bool zero_length,
                         bool check_read_conflict) {
        wchar_t path[kPath + 4] = {};
        if (swprintf_s(path, L"\\??\\%s\\%s", root, leaf) < 0) return false;
        UNICODE_STRING name = {USHORT(wcslen(path) * sizeof(wchar_t)),
                               USHORT(wcslen(path) * sizeof(wchar_t)), path};
        OBJECT_ATTRIBUTES attrs = {};
        attrs.Length = sizeof(attrs);
        attrs.Attributes = OBJ_CASE_INSENSITIVE;
        attrs.ObjectName = &name;
        IO_STATUS_BLOCK io = {};
        Handle file;
        NTSTATUS status = api.create(&file.value, FILE_WRITE_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE, &attrs, &io,
            nullptr, FILE_ATTRIBUTE_NORMAL, share, disposition,
            FILE_NON_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT, nullptr, 0);
        std::fprintf(stderr,
            "FILE_BROKER_OVERWRITE leaf=%ls allowed=%d disposition=%lu share=%lu status=%08lx expected=%08lx handle=%d\n",
            leaf, int(allowed), disposition, share, static_cast<unsigned long>(status),
            static_cast<unsigned long>(expected), file.value != nullptr);
        // A noncreating open of an absent file may report absence before the
        // raw access check. Neither outcome may return a handle or create the
        // missing name; the parent also checks the host filesystem afterward.
        bool raw_absent_denial = !allowed && expected == missing && status == denied;
        if (status != expected && !raw_absent_denial) return false;
        if (status) return file.value == nullptr;
        if (!zero_length) return true;
        if (check_read_conflict) {
            IO_STATUS_BLOCK conflict_io = {};
            Handle conflict;
            NTSTATUS conflict_status = api.create(&conflict.value, FILE_READ_DATA | SYNCHRONIZE, &attrs,
                &conflict_io, nullptr, 0, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                FILE_OPEN, FILE_NON_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT, nullptr, 0);
            if (conflict_status != static_cast<NTSTATUS>(0xc0000043)) return false;
        }
        LARGE_INTEGER size = {};
        return io.Information == information && GetFileSizeEx(file.value, &size) && !size.QuadPart;
    };
    const NTSTATUS existing = allowed ? 0 : denied;
    const NTSTATUS absent = missing;
    const NTSTATUS create = allowed ? 0 : denied;
    DWORD failures = 0;
    if (!overwrite(L"exclusive-overwrite.json", FILE_OVERWRITE, 0, existing,
                   FILE_OVERWRITTEN, allowed, true)) failures |= 1;
    if (!overwrite(L"overwrite-if-no-read.json", FILE_OVERWRITE_IF, FILE_SHARE_WRITE, existing,
                   FILE_OVERWRITTEN, allowed, true)) failures |= 2;
    if (!overwrite(L"missing-overwrite.json", FILE_OVERWRITE, 0, absent, 0, false, false)) failures |= 4;
    if (!overwrite(L"missing-overwrite-if.json", FILE_OVERWRITE_IF, 0, create,
                   FILE_CREATED, allowed, false)) failures |= 8;
    if (!overwrite(L"overwrite-if-delete-share.json", FILE_OVERWRITE_IF,
                   FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, existing,
                   FILE_OVERWRITTEN, allowed, false)) failures |= 16;
    if (!overwrite(L"missing-overwrite-if-delete-share.json", FILE_OVERWRITE_IF,
                   FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, create,
                   FILE_CREATED, allowed, false)) failures |= 32;
    return failures;
}

extern "C" DWORD sandbox_file_broker_test_exclusive_overwrite_denied(const wchar_t* path, BOOL exact_denial) {
    using namespace nub_sandbox::file_broker;
    Api api;
    if (!api.create) return 1;
    wchar_t native[kPath + 4] = {};
    if (swprintf_s(native, L"\\??\\%s", path) < 0) return 2;
    UNICODE_STRING name = {USHORT(wcslen(native) * sizeof(wchar_t)),
                           USHORT(wcslen(native) * sizeof(wchar_t)), native};
    OBJECT_ATTRIBUTES attrs = {};
    attrs.Length = sizeof(attrs);
    attrs.Attributes = OBJ_CASE_INSENSITIVE;
    attrs.ObjectName = &name;
    IO_STATUS_BLOCK io = {};
    Handle file;
    NTSTATUS status = api.create(&file.value, FILE_WRITE_DATA | SYNCHRONIZE, &attrs, &io,
        nullptr, FILE_ATTRIBUTE_NORMAL, 0, FILE_OVERWRITE,
        FILE_NON_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT, nullptr, 0);
    return status < 0 && (!exact_denial || status == kDenied) ? 0 : ERROR_INVALID_DATA;
}

extern "C" DWORD sandbox_file_broker_test_exclusive_overwrite_metadata_fixture(const wchar_t* root) {
    struct Entry { const wchar_t* leaf; DWORD attributes; } entries[] = {
        {L"hidden-overwrite.json", FILE_ATTRIBUTE_HIDDEN},
        {L"system-overwrite.json", FILE_ATTRIBUTE_SYSTEM},
    };
    for (const auto& entry : entries) {
        wchar_t path[nub_sandbox::file_broker::kPath] = {};
        if (swprintf_s(path, L"%s\\%s", root, entry.leaf) < 0) return ERROR_INVALID_NAME;
        if (!SetFileAttributesW(path, entry.attributes)) return GetLastError();
    }
    return 0;
}

extern "C" NTSTATUS sandbox_file_broker_test_exclusive_truncate(const wchar_t* path) {
    using namespace nub_sandbox::file_broker;
    Api api;
    if (!api.create) return kDenied;
    wchar_t native[kPath + 4] = {};
    if (swprintf_s(native, L"\\??\\%s", path) < 0) return kInvalid;
    UNICODE_STRING name = {USHORT(wcslen(native) * sizeof(wchar_t)),
                           USHORT(wcslen(native) * sizeof(wchar_t)), native};
    OBJECT_ATTRIBUTES attrs = {};
    attrs.Length = sizeof(attrs);
    attrs.Attributes = OBJ_CASE_INSENSITIVE;
    attrs.ObjectName = &name;
    IO_STATUS_BLOCK io = {};
    Handle file;
    return api.create(&file.value, FILE_WRITE_DATA | SYNCHRONIZE, &attrs, &io,
        nullptr, FILE_ATTRIBUTE_NORMAL, 0, FILE_OVERWRITE,
        FILE_NON_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT, nullptr, 0);
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
