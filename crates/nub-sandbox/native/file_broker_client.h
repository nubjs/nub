#pragma once

// Included only in the injected adapter, after its true CreateFileW trampoline.
static thread_local bool file_broker_active = false;

static bool capture_file_request(nub_sandbox::file_broker::Request& request,
                                 POBJECT_ATTRIBUTES attrs) {
    using namespace nub_sandbox::file_broker;
    // Neither pointers nor child handle values cross the protocol. Root-relative
    // calls require a separate authenticated handle-resolution protocol.
    __try {
        if (!attrs || attrs->Length != sizeof(*attrs) || attrs->RootDirectory ||
            attrs->SecurityDescriptor || attrs->SecurityQualityOfService ||
            attrs->Attributes != OBJ_CASE_INSENSITIVE || !attrs->ObjectName) return false;
        UNICODE_STRING name = *attrs->ObjectName;
        if (!name.Buffer || name.Length % sizeof(wchar_t) || name.Length < 8 * sizeof(wchar_t) ||
            name.Length > name.MaximumLength || name.Length / sizeof(wchar_t) >= kPath + 4) return false;
        // NT DOS paths only; the parent independently rejects unsupported names.
        if (wcsncmp(name.Buffer, L"\\??\\", 4)) return false;
        request.length = name.Length / sizeof(wchar_t) - 4;
        memcpy(request.path, name.Buffer + 4, request.length * sizeof(wchar_t));
        return validate(request) == 0;
    } __except (EXCEPTION_EXECUTE_HANDLER) {
        return false;
    }
}

static bool exchange_file_request(const nub_sandbox::file_broker::Request& request,
                                  nub_sandbox::file_broker::Response& response) {
    using namespace nub_sandbox;
    if (!state.file_broker[0] || file_broker_active) return false;
    file_broker_active = true;
    DWORD saved_error = GetLastError();
    HANDLE pipe = INVALID_HANDLE_VALUE;
    ULONGLONG deadline = GetTickCount64() + socket_broker::kTimeout;
    do {
        pipe = true_create_file(state.file_broker,
            FILE_READ_DATA | FILE_WRITE_DATA | FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES |
                READ_CONTROL | SYNCHRONIZE,
            0, nullptr, OPEN_EXISTING, FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT |
                SECURITY_IDENTIFICATION, nullptr);
        if (pipe != INVALID_HANDLE_VALUE || GetLastError() != ERROR_PIPE_BUSY) break;
        ULONGLONG now = GetTickCount64();
        if (now >= deadline || !WaitNamedPipeW(state.file_broker, DWORD(deadline - now))) break;
    } while (true);
    HANDLE event = nullptr;
    bool ok = false;
    if (pipe != INVALID_HANDLE_VALUE) {
        DWORD server = 0, mode = PIPE_READMODE_MESSAGE;
        if (GetNamedPipeServerProcessId(pipe, &server) && server == state.file_broker_pid &&
            SetNamedPipeHandleState(pipe, &mode, nullptr, nullptr)) {
            event = CreateEventW(nullptr, TRUE, FALSE, nullptr);
            if (event && socket_broker::transfer(pipe, event, nullptr,
                const_cast<file_broker::Request*>(&request), sizeof(request), true) &&
                socket_broker::transfer(pipe, event, nullptr, &response, sizeof(response), false)) {
                ok = response.version == file_broker::kVersion && response.size == sizeof(response) &&
                    !response.reserved && !response.padding && response.status <= 0;
            }
        }
    }
    if (event) CloseHandle(event);
    if (pipe != INVALID_HANDLE_VALUE) CloseHandle(pipe);
    SetLastError(saved_error);
    file_broker_active = false;
    return ok;
}

static NTSTATUS broker_file_open(DWORD operation, PHANDLE handle, ACCESS_MASK access,
    POBJECT_ATTRIBUTES attrs, PIO_STATUS_BLOCK io, ULONG share, ULONG disposition,
    ULONG options, ULONG attributes) {
    using namespace nub_sandbox::file_broker;
    Request request = {kVersion, sizeof(Request), operation, access, share, disposition, options, attributes};
    if (!state.file_broker[0] || !capture_file_request(request, attrs)) return kDenied;
    Response response = {};
    if (!exchange_file_request(request, response)) return kDenied;
    if (response.status) return response.status;
    if (!response.handle) return kDenied;
    HANDLE received = reinterpret_cast<HANDLE>(static_cast<uintptr_t>(response.handle));
    __try {
        *handle = received;
        io->Status = 0;
        io->Information = static_cast<ULONG_PTR>(response.information);
    } __except (EXCEPTION_EXECUTE_HANDLER) {
        CloseHandle(received);
        return kDenied;
    }
    return 0;
}

struct BrokerBasicInformation {
    LARGE_INTEGER creation, access, write, change;
    ULONG attributes;
};
struct BrokerFullInformation {
    LARGE_INTEGER creation, access, write, change, allocation, end;
    ULONG attributes;
};
static_assert(sizeof(BrokerBasicInformation) == 40);
static_assert(sizeof(BrokerFullInformation) == 56);
using NtQueryFileAttributes = NTSTATUS (NTAPI*)(POBJECT_ATTRIBUTES, PVOID);
static NtQueryFileAttributes true_query_attributes = nullptr;
static NtQueryFileAttributes true_query_full_attributes = nullptr;

static NTSTATUS broker_file_attributes(DWORD operation, POBJECT_ATTRIBUTES attrs, PVOID output) {
    using namespace nub_sandbox::file_broker;
    Request request = {kVersion, sizeof(Request), operation};
    if (!capture_file_request(request, attrs)) return kDenied;
    Response response = {};
    if (!exchange_file_request(request, response)) return kDenied;
    if (response.status) return response.status;
    if (response.handle || response.information) return kDenied;
    __try {
        if (operation == Basic) {
            BrokerBasicInformation info = {};
            info.creation.QuadPart = response.creation;
            info.access.QuadPart = response.access;
            info.write.QuadPart = response.write;
            info.change.QuadPart = response.change;
            info.attributes = response.attributes;
            memcpy(output, &info, sizeof(info));
        } else {
            BrokerFullInformation info = {};
            info.creation.QuadPart = response.creation;
            info.access.QuadPart = response.access;
            info.write.QuadPart = response.write;
            info.change.QuadPart = response.change;
            info.allocation.QuadPart = response.allocation;
            info.end.QuadPart = response.end;
            info.attributes = response.attributes;
            memcpy(output, &info, sizeof(info));
        }
    } __except (EXCEPTION_EXECUTE_HANDLER) { return kDenied; }
    return 0;
}

static NTSTATUS NTAPI query_file_attributes(POBJECT_ATTRIBUTES attrs, PVOID output) {
    NTSTATUS status = true_query_attributes(attrs, output);
    return status == nub_sandbox::file_broker::kDenied && state.file_broker[0]
        ? broker_file_attributes(nub_sandbox::file_broker::Basic, attrs, output) : status;
}
static NTSTATUS NTAPI query_full_file_attributes(POBJECT_ATTRIBUTES attrs, PVOID output) {
    NTSTATUS status = true_query_full_attributes(attrs, output);
    return status == nub_sandbox::file_broker::kDenied && state.file_broker[0]
        ? broker_file_attributes(nub_sandbox::file_broker::Full, attrs, output) : status;
}
