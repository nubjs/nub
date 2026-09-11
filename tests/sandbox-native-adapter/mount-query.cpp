// Standalone ABI and lifetime tests for native/mount_query.h.
// Compile on Windows with:
//   cl /nologo /std:c++17 /W4 /WX /MT /EHsc mount-query.cpp /Fe:mount-query.exe
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <winternl.h>
#include <atomic>
#include <cstddef>
#include <cstdio>
#include <cstring>
#include <cwchar>
#include <thread>
#include "../../crates/nub-sandbox/native/mount_query.h"

using nub_sandbox::mount_query::Api;
using nub_sandbox::mount_query::Bridge;
using nub_sandbox::mount_query::CloseTicket;
using nub_sandbox::mount_query::DuplicateTicket;
using nub_sandbox::mount_query::IoctlDisposition;
using nub_sandbox::mount_query::MountPoint;
using nub_sandbox::mount_query::MountPointsHeader;

using NativeClose = NTSTATUS(NTAPI*)(HANDLE);
static NativeClose native_close = nullptr;
static std::atomic<unsigned long> owned_identity_closes{0};

static NTSTATUS NTAPI close_adapter(HANDLE handle) {
    ++owned_identity_closes;
    return native_close(handle);
}

static int fail(const char* message, int line) {
    std::fprintf(stderr, "mount-query test failed at %d: %s\n", line, message);
    return 1;
}
#define CHECK(expression) do { if (!(expression)) return fail(#expression, __LINE__); } while (false)

struct Selector {
    MountPoint point = {};
    wchar_t device[MAX_PATH] = {};
};

static Selector selector_for(const wchar_t* device) {
    Selector selector = {};
    const size_t length = wcslen(device);
    memcpy(selector.device, device, (length + 1) * sizeof(wchar_t));
    selector.point.device_name_offset = offsetof(Selector, device);
    selector.point.device_name_length = static_cast<USHORT>(length * sizeof(wchar_t));
    return selector;
}

static IoctlDisposition call_query(Bridge& bridge, HANDLE handle, Selector* selector,
                                   PIO_STATUS_BLOCK io, void* output, ULONG output_size,
                                   NTSTATUS* status) {
    return bridge.device_io(handle, nullptr, nullptr, nullptr, io,
                            nub_sandbox::mount_query::kQueryPoints, selector, sizeof(*selector),
                            output, output_size, status);
}

static NTSTATUS open_capability(Bridge& bridge, HANDLE* handle) {
    IO_STATUS_BLOCK io = {};
    NTSTATUS status = bridge.substitute_open(handle, &io);
    if (status == nub_sandbox::mount_query::kStatusSuccess &&
        (io.Status != status || io.Information != FILE_OPENED)) return nub_sandbox::mount_query::kStatusInvalidParameter;
    return status;
}

static int test_open_matcher(Bridge& bridge) {
    static constexpr wchar_t expected[] = L"\\??\\MountPointManager";
    UNICODE_STRING name = {};
    name.Buffer = const_cast<wchar_t*>(expected);
    name.Length = sizeof(expected) - sizeof(wchar_t);
    name.MaximumLength = sizeof(expected);
    OBJECT_ATTRIBUTES attributes = {};
    attributes.Length = sizeof(attributes);
    attributes.ObjectName = &name;
    CHECK(bridge.matches_open(SYNCHRONIZE, &attributes, nub_sandbox::mount_query::kKnownShare,
                              nub_sandbox::mount_query::kFileOpen, nub_sandbox::mount_query::kKnownOptions, true));
    CHECK(bridge.matches_create_file(SYNCHRONIZE, &attributes, nullptr, FILE_ATTRIBUTE_NORMAL,
                                     nub_sandbox::mount_query::kKnownShare, nub_sandbox::mount_query::kFileOpen,
                                     nub_sandbox::mount_query::kKnownOptions, nullptr, 0));
    CHECK(bridge.matches_create_file(SYNCHRONIZE, &attributes, nullptr, 0,
                                     nub_sandbox::mount_query::kKnownShare, nub_sandbox::mount_query::kFileOpen,
                                     nub_sandbox::mount_query::kKnownOptions, nullptr, 0));
    CHECK(!bridge.matches_create_file(SYNCHRONIZE, &attributes, nullptr, FILE_ATTRIBUTE_HIDDEN,
                                      nub_sandbox::mount_query::kKnownShare, nub_sandbox::mount_query::kFileOpen,
                                      nub_sandbox::mount_query::kKnownOptions, nullptr, 0));
    attributes.Attributes = OBJ_CASE_INSENSITIVE;
    CHECK(!bridge.matches_create_file(SYNCHRONIZE, &attributes, nullptr, FILE_ATTRIBUTE_NORMAL,
                                      nub_sandbox::mount_query::kKnownShare, nub_sandbox::mount_query::kFileOpen,
                                      nub_sandbox::mount_query::kKnownOptions, nullptr, 0));
    attributes.Attributes = 0;
    attributes.Length = 0;
    CHECK(!bridge.matches_create_file(SYNCHRONIZE, &attributes, nullptr, FILE_ATTRIBUTE_NORMAL,
                                      nub_sandbox::mount_query::kKnownShare, nub_sandbox::mount_query::kFileOpen,
                                      nub_sandbox::mount_query::kKnownOptions, nullptr, 0));
    attributes.Length = sizeof(attributes);
    name.MaximumLength = name.Length - sizeof(wchar_t);
    CHECK(!bridge.matches_create_file(SYNCHRONIZE, &attributes, nullptr, FILE_ATTRIBUTE_NORMAL,
                                      nub_sandbox::mount_query::kKnownShare, nub_sandbox::mount_query::kFileOpen,
                                      nub_sandbox::mount_query::kKnownOptions, nullptr, 0));
    name.MaximumLength = sizeof(expected);
    attributes.SecurityDescriptor = reinterpret_cast<PVOID>(static_cast<INT_PTR>(1));
    CHECK(!bridge.matches_create_file(SYNCHRONIZE, &attributes, nullptr, FILE_ATTRIBUTE_NORMAL,
                                      nub_sandbox::mount_query::kKnownShare, nub_sandbox::mount_query::kFileOpen,
                                      nub_sandbox::mount_query::kKnownOptions, nullptr, 0));
    attributes.SecurityDescriptor = nullptr;
    attributes.SecurityQualityOfService = reinterpret_cast<PVOID>(static_cast<INT_PTR>(1));
    CHECK(!bridge.matches_create_file(SYNCHRONIZE, &attributes, nullptr, FILE_ATTRIBUTE_NORMAL,
                                      nub_sandbox::mount_query::kKnownShare, nub_sandbox::mount_query::kFileOpen,
                                      nub_sandbox::mount_query::kKnownOptions, nullptr, 0));
    attributes.SecurityQualityOfService = nullptr;
    CHECK(!bridge.matches_open(SYNCHRONIZE | FILE_READ_DATA, &attributes, nub_sandbox::mount_query::kKnownShare,
                               nub_sandbox::mount_query::kFileOpen, nub_sandbox::mount_query::kKnownOptions, true));
    CHECK(!bridge.matches_open(SYNCHRONIZE, &attributes, FILE_SHARE_READ,
                               nub_sandbox::mount_query::kFileOpen, nub_sandbox::mount_query::kKnownOptions, true));
    CHECK(!bridge.matches_open(SYNCHRONIZE, &attributes, nub_sandbox::mount_query::kKnownShare,
                               nub_sandbox::mount_query::kFileOpen, FILE_SYNCHRONOUS_IO_NONALERT, true));
    name.Buffer = const_cast<wchar_t*>(L"\\??\\MountPointManagerX");
    name.Length = static_cast<USHORT>(wcslen(name.Buffer) * sizeof(wchar_t));
    CHECK(!bridge.matches_open(SYNCHRONIZE, &attributes, nub_sandbox::mount_query::kKnownShare,
                               nub_sandbox::mount_query::kFileOpen, nub_sandbox::mount_query::kKnownOptions, true));
    return 0;
}

static int test_abi_and_malformed(Bridge& bridge) {
    HANDLE handle = INVALID_HANDLE_VALUE;
    CHECK(open_capability(bridge, &handle) == nub_sandbox::mount_query::kStatusSuccess);
    DWORD flags = 0;
    CHECK(GetHandleInformation(handle, &flags));

    Selector selector = selector_for(L"\\device\\volume-z");
    BYTE output[512] = {};
    IO_STATUS_BLOCK io = {};
    NTSTATUS status = nub_sandbox::mount_query::kStatusInvalidParameter;
    CHECK(call_query(bridge, handle, &selector, &io, output, sizeof(output), &status) == IoctlDisposition::kHandled);
    CHECK(status == nub_sandbox::mount_query::kStatusSuccess);
    CHECK(io.Status == status);
    auto* response = reinterpret_cast<MountPointsHeader*>(output);
    CHECK(io.Information == response->size);
    CHECK(response->number_of_mount_points == 1);
    CHECK(response->mount_points[0].symbolic_link_offset == sizeof(MountPointsHeader));
    CHECK(response->mount_points[0].unique_id_length == 0 && response->mount_points[0].unique_id_offset == 0);
    CHECK(response->mount_points[0].device_name_offset == sizeof(MountPointsHeader) + response->mount_points[0].symbolic_link_length);
    CHECK(!memcmp(output + response->mount_points[0].symbolic_link_offset, L"\\DosDevices\\Z:", sizeof(L"\\DosDevices\\Z:") - sizeof(wchar_t)));
    CHECK(!memcmp(output + response->mount_points[0].device_name_offset, L"\\Device\\Volume-Z", sizeof(L"\\Device\\Volume-Z") - sizeof(wchar_t)));

    status = static_cast<NTSTATUS>(0x12345678L);
    CHECK(bridge.device_io(handle, nullptr, nullptr, nullptr, &io, 0x006d0009,
                           &selector, sizeof(selector), output, sizeof(output), &status) == IoctlDisposition::kForward);
    CHECK(status == static_cast<NTSTATUS>(0x12345678L));
    CHECK(bridge.device_io(handle, reinterpret_cast<HANDLE>(static_cast<INT_PTR>(1)), nullptr, nullptr, &io,
                           nub_sandbox::mount_query::kQueryPoints, &selector, sizeof(selector),
                           output, sizeof(output), &status) == IoctlDisposition::kForward);
    CHECK(bridge.device_io(handle, nullptr, reinterpret_cast<void*>(static_cast<INT_PTR>(1)), nullptr, &io,
                           nub_sandbox::mount_query::kQueryPoints, &selector, sizeof(selector),
                           output, sizeof(output), &status) == IoctlDisposition::kForward);

    selector.point.symbolic_link_offset = sizeof(MountPoint);
    CHECK(call_query(bridge, handle, &selector, &io, output, sizeof(output), &status) == IoctlDisposition::kHandled);
    CHECK(status == nub_sandbox::mount_query::kStatusInvalidParameter && io.Status == status && io.Information == 0);
    selector = selector_for(L"\\Device\\Volume-Z");
    selector.point.device_name_offset++;
    CHECK(call_query(bridge, handle, &selector, &io, output, sizeof(output), &status) == IoctlDisposition::kHandled);
    CHECK(status == nub_sandbox::mount_query::kStatusInvalidParameter);
    selector = selector_for(L"\\Device\\Volume-Z");
    selector.point.device_name_length--;
    CHECK(call_query(bridge, handle, &selector, &io, output, sizeof(output), &status) == IoctlDisposition::kHandled);
    CHECK(status == nub_sandbox::mount_query::kStatusInvalidParameter);
    selector = selector_for(L"\\Device\\Missing");
    CHECK(call_query(bridge, handle, &selector, &io, output, sizeof(output), &status) == IoctlDisposition::kHandled);
    CHECK(status == nub_sandbox::mount_query::kStatusInvalidParameter);
    selector = selector_for(L"\\Device\\Mup");
    CHECK(call_query(bridge, handle, &selector, &io, output, sizeof(output), &status) == IoctlDisposition::kHandled);
    CHECK(status == nub_sandbox::mount_query::kStatusInvalidParameter);
    selector = selector_for(L"\\Device\\LanmanRedirector\\;Y:123\\server\\share");
    CHECK(call_query(bridge, handle, &selector, &io, output, sizeof(output), &status) == IoctlDisposition::kHandled);
    CHECK(status == nub_sandbox::mount_query::kStatusInvalidParameter);
    selector = selector_for(L"\\Device\\Volume-Z");
    CHECK(bridge.device_io(handle, nullptr, nullptr, nullptr, &io, nub_sandbox::mount_query::kQueryPoints,
                           &selector, sizeof(MountPoint) - 1, output, sizeof(output), &status) == IoctlDisposition::kHandled);
    CHECK(status == nub_sandbox::mount_query::kStatusInvalidParameter);
    CHECK(call_query(bridge, handle, &selector, &io, output, sizeof(MountPointsHeader) - 1, &status) == IoctlDisposition::kHandled);
    CHECK(status == nub_sandbox::mount_query::kStatusInvalidParameter);
    memset(output, 0xa5, sizeof(output));
    CHECK(call_query(bridge, handle, &selector, &io, output, sizeof(MountPointsHeader), &status) == IoctlDisposition::kHandled);
    CHECK(status == nub_sandbox::mount_query::kStatusBufferOverflow && io.Status == status && io.Information == 0);
    CHECK(reinterpret_cast<MountPointsHeader*>(output)->size > sizeof(MountPointsHeader));

    CloseTicket ticket = bridge.prepare_close(handle);
    CHECK(ticket.generation != 0);
    CHECK(native_close(handle) == nub_sandbox::mount_query::kStatusSuccess);
    bridge.complete_close(ticket, nub_sandbox::mount_query::kStatusSuccess);
    CHECK(call_query(bridge, handle, &selector, &io, output, sizeof(output), &status) == IoctlDisposition::kForward);
    return 0;
}

static int test_raw_close_reuse_and_duplication(Bridge& bridge) {
    HANDLE handle = INVALID_HANDLE_VALUE;
    CHECK(open_capability(bridge, &handle) == nub_sandbox::mount_query::kStatusSuccess);
    // Bypass complete_close to model a raw syscall close.  Reusing the numeric
    // slot with an unrelated event must never regain virtual I/O.
    CHECK(native_close(handle) == nub_sandbox::mount_query::kStatusSuccess);
    HANDLE reused = INVALID_HANDLE_VALUE;
    for (int attempt = 0; attempt < 64 && reused != handle; ++attempt) {
        HANDLE candidate = CreateEventExW(nullptr, nullptr, 0, SYNCHRONIZE);
        CHECK(candidate != nullptr && candidate != INVALID_HANDLE_VALUE);
        if (candidate == handle) reused = candidate;
        else CHECK(native_close(candidate) == nub_sandbox::mount_query::kStatusSuccess);
    }
    CHECK(reused == handle);
    Selector selector = selector_for(L"\\Device\\Volume-Z");
    BYTE output[512] = {};
    IO_STATUS_BLOCK io = {};
    NTSTATUS status = nub_sandbox::mount_query::kStatusSuccess;
    CHECK(call_query(bridge, reused, &selector, &io, output, sizeof(output), &status) == IoctlDisposition::kForward);
    CHECK(native_close(reused) == nub_sandbox::mount_query::kStatusSuccess);

    HANDLE source = INVALID_HANDLE_VALUE;
    CHECK(open_capability(bridge, &source) == nub_sandbox::mount_query::kStatusSuccess);
    DuplicateTicket ticket = bridge.prepare_duplicate(GetCurrentProcess(), source);
    CHECK(ticket.generation != 0);
    HANDLE duplicate = INVALID_HANDLE_VALUE;
    CHECK(DuplicateHandle(GetCurrentProcess(), source, GetCurrentProcess(), &duplicate, 0, FALSE, DUPLICATE_SAME_ACCESS));
    bridge.complete_duplicate(ticket, nub_sandbox::mount_query::kStatusSuccess);
    CHECK(call_query(bridge, source, &selector, &io, output, sizeof(output), &status) == IoctlDisposition::kForward);
    CHECK(call_query(bridge, duplicate, &selector, &io, output, sizeof(output), &status) == IoctlDisposition::kForward);
    CHECK(native_close(source) == nub_sandbox::mount_query::kStatusSuccess);
    CHECK(native_close(duplicate) == nub_sandbox::mount_query::kStatusSuccess);
    return 0;
}

static int test_concurrent_lifetimes(Bridge& bridge) {
    std::atomic<int> failures{0};
    std::thread workers[8];
    for (auto& worker : workers) {
        worker = std::thread([&bridge, &failures] {
            for (int iteration = 0; iteration < 32; ++iteration) {
                HANDLE handle = INVALID_HANDLE_VALUE;
                if (open_capability(bridge, &handle) != nub_sandbox::mount_query::kStatusSuccess) { ++failures; return; }
                Selector selector = selector_for(L"\\Device\\Volume-Z");
                BYTE output[256] = {};
                IO_STATUS_BLOCK io = {};
                NTSTATUS status = nub_sandbox::mount_query::kStatusInvalidParameter;
                if (call_query(bridge, handle, &selector, &io, output, sizeof(output), &status) != IoctlDisposition::kHandled ||
                    status != nub_sandbox::mount_query::kStatusSuccess) { ++failures; return; }
                CloseTicket ticket = bridge.prepare_close(handle);
                NTSTATUS closed = native_close(handle);
                bridge.complete_close(ticket, closed);
                if (closed != nub_sandbox::mount_query::kStatusSuccess) { ++failures; return; }
            }
        });
    }
    for (auto& worker : workers) worker.join();
    CHECK(failures.load() == 0);
    return 0;
}

static int test_guard_pages_and_capacity(Bridge& bridge) {
    HANDLE handle = INVALID_HANDLE_VALUE;
    CHECK(open_capability(bridge, &handle) == nub_sandbox::mount_query::kStatusSuccess);
    Selector selector = selector_for(L"\\Device\\Volume-Z");
    BYTE output[512] = {};
    IO_STATUS_BLOCK io = {};
    NTSTATUS status = nub_sandbox::mount_query::kStatusInvalidParameter;
    SYSTEM_INFO system = {};
    GetSystemInfo(&system);
    void* inaccessible = VirtualAlloc(nullptr, system.dwPageSize, MEM_RESERVE | MEM_COMMIT, PAGE_NOACCESS);
    CHECK(inaccessible != nullptr);
    CHECK(bridge.device_io(handle, nullptr, nullptr, nullptr, &io, nub_sandbox::mount_query::kQueryPoints,
                           inaccessible, sizeof(MountPoint), output, sizeof(output), &status) == IoctlDisposition::kHandled);
    CHECK(status == nub_sandbox::mount_query::kStatusAccessViolation && io.Status == status);
    CHECK(call_query(bridge, handle, &selector, &io, inaccessible, sizeof(output), &status) == IoctlDisposition::kHandled);
    CHECK(status == nub_sandbox::mount_query::kStatusAccessViolation && io.Status == status);
    CHECK(bridge.device_io(handle, nullptr, nullptr, nullptr, static_cast<PIO_STATUS_BLOCK>(inaccessible),
                           nub_sandbox::mount_query::kQueryPoints, &selector, sizeof(selector),
                           output, sizeof(output), &status) == IoctlDisposition::kHandled);
    CHECK(status == nub_sandbox::mount_query::kStatusAccessViolation);
    CHECK(VirtualFree(inaccessible, 0, MEM_RELEASE));
    CloseTicket handle_ticket = bridge.prepare_close(handle);
    CHECK(native_close(handle) == nub_sandbox::mount_query::kStatusSuccess);
    bridge.complete_close(handle_ticket, nub_sandbox::mount_query::kStatusSuccess);

    DWORD baseline = 0;
    CHECK(GetProcessHandleCount(GetCurrentProcess(), &baseline));
    for (int cycle = 0; cycle < 2; ++cycle) {
        HANDLE handles[nub_sandbox::mount_query::kMaxCapabilities] = {};
        for (size_t i = 0; i < nub_sandbox::mount_query::kMaxCapabilities; ++i)
            CHECK(open_capability(bridge, &handles[i]) == nub_sandbox::mount_query::kStatusSuccess);
        HANDLE extra = INVALID_HANDLE_VALUE;
        IO_STATUS_BLOCK extra_io = {};
        CHECK(bridge.substitute_open(&extra, &extra_io) == nub_sandbox::mount_query::kStatusInsufficientResources);
        CHECK(extra_io.Status == nub_sandbox::mount_query::kStatusInsufficientResources && extra_io.Information == 0);
        for (HANDLE current : handles) {
            CloseTicket ticket = bridge.prepare_close(current);
            CHECK(ticket.generation != 0);
            CHECK(native_close(current) == nub_sandbox::mount_query::kStatusSuccess);
            bridge.complete_close(ticket, nub_sandbox::mount_query::kStatusSuccess);
        }
        DWORD after_cycle = 0;
        CHECK(GetProcessHandleCount(GetCurrentProcess(), &after_cycle));
        CHECK(after_cycle == baseline);
    }
    return 0;
}

static int test_ticket_races_and_generation_reuse(Bridge& bridge) {
    Selector selector = selector_for(L"\\Device\\Volume-Z");
    BYTE output[512] = {};
    IO_STATUS_BLOCK io = {};
    NTSTATUS status = nub_sandbox::mount_query::kStatusInvalidParameter;

    HANDLE source = INVALID_HANDLE_VALUE;
    CHECK(open_capability(bridge, &source) == nub_sandbox::mount_query::kStatusSuccess);
    DuplicateTicket duplicate_ticket = bridge.prepare_duplicate(GetCurrentProcess(), source);
    CHECK(duplicate_ticket.generation != 0);
    CHECK(bridge.prepare_duplicate(nullptr, source).generation == 0);
    HANDLE duplicate = INVALID_HANDLE_VALUE;
    CHECK(DuplicateHandle(GetCurrentProcess(), source, GetCurrentProcess(), &duplicate, 0, FALSE, DUPLICATE_SAME_ACCESS));
    CloseTicket close_ticket = bridge.prepare_close(source);
    CHECK(close_ticket.generation == duplicate_ticket.generation);
    const unsigned long before = owned_identity_closes.load();
    CHECK(native_close(source) == nub_sandbox::mount_query::kStatusSuccess);
    std::thread close_completion([&bridge, close_ticket] {
        bridge.complete_close(close_ticket, nub_sandbox::mount_query::kStatusSuccess);
    });
    std::thread duplicate_completion([&bridge, duplicate_ticket] {
        bridge.complete_duplicate(duplicate_ticket, nub_sandbox::mount_query::kStatusSuccess);
    });
    close_completion.join();
    duplicate_completion.join();
    CHECK(owned_identity_closes.load() == before + 1);
    CHECK(call_query(bridge, source, &selector, &io, output, sizeof(output), &status) == IoctlDisposition::kForward);
    CHECK(call_query(bridge, duplicate, &selector, &io, output, sizeof(output), &status) == IoctlDisposition::kForward);
    CHECK(native_close(duplicate) == nub_sandbox::mount_query::kStatusSuccess);

    HANDLE old_handle = INVALID_HANDLE_VALUE;
    CHECK(open_capability(bridge, &old_handle) == nub_sandbox::mount_query::kStatusSuccess);
    CloseTicket old_ticket = bridge.prepare_close(old_handle);
    CHECK(old_ticket.generation != 0);
    CHECK(native_close(old_handle) == nub_sandbox::mount_query::kStatusSuccess);
    HANDLE replacement = INVALID_HANDLE_VALUE;
    for (int attempt = 0; attempt < 64 && replacement != old_handle; ++attempt) {
        HANDLE candidate = INVALID_HANDLE_VALUE;
        CHECK(open_capability(bridge, &candidate) == nub_sandbox::mount_query::kStatusSuccess);
        if (candidate == old_handle) replacement = candidate;
        else {
            CloseTicket candidate_ticket = bridge.prepare_close(candidate);
            CHECK(native_close(candidate) == nub_sandbox::mount_query::kStatusSuccess);
            bridge.complete_close(candidate_ticket, nub_sandbox::mount_query::kStatusSuccess);
        }
    }
    CHECK(replacement == old_handle);
    CloseTicket replacement_ticket = bridge.prepare_close(replacement);
    CHECK(replacement_ticket.generation != 0 && replacement_ticket.generation != old_ticket.generation);
    // A late completion from the old numeric slot must not remove the new one.
    bridge.complete_close(old_ticket, nub_sandbox::mount_query::kStatusSuccess);
    CHECK(call_query(bridge, replacement, &selector, &io, output, sizeof(output), &status) == IoctlDisposition::kHandled);
    CHECK(status == nub_sandbox::mount_query::kStatusSuccess);
    CHECK(native_close(replacement) == nub_sandbox::mount_query::kStatusSuccess);
    bridge.complete_close(replacement_ticket, nub_sandbox::mount_query::kStatusSuccess);
    return 0;
}

int wmain() {
    FARPROC raw_close = GetProcAddress(GetModuleHandleW(L"ntdll.dll"), "NtClose");
    static_assert(sizeof(raw_close) == sizeof(native_close));
    memcpy(&native_close, &raw_close, sizeof(native_close));
    CHECK(native_close != nullptr);
    wchar_t devices[26][MAX_PATH] = {};
    wcscpy_s(devices[0], L"\\Device\\Mup");
    wcscpy_s(devices[24], L"\\Device\\LanmanRedirector\\;Y:123\\server\\share");
    wcscpy_s(devices[25], L"\\Device\\Volume-Z");
    Bridge bridge;
    Api api = {CreateEventExW, DuplicateHandle, CompareObjectHandles, close_adapter};
    CHECK(bridge.initialize(api, devices));
    if (test_open_matcher(bridge) || test_abi_and_malformed(bridge) ||
        test_raw_close_reuse_and_duplication(bridge) || test_concurrent_lifetimes(bridge) ||
        test_guard_pages_and_capacity(bridge) ||
        test_ticket_races_and_generation_reuse(bridge)) {
        return 1;
    }
    std::puts("mount-query tests passed");
    return 0;
}
