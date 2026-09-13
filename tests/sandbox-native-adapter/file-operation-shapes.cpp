// Records the NT request shapes produced by a small set of ordinary Win32
// filesystem calls. This is a diagnostic fixture, not an adapter or policy.
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <winternl.h>

#include <cstddef>
#include <cstdint>
#include <cstring>
#include <cstdio>
#include <cwchar>

#include "detours.h"

using NtOpenFileFn = NTSTATUS(NTAPI*)(PHANDLE, ACCESS_MASK, POBJECT_ATTRIBUTES,
                                     PIO_STATUS_BLOCK, ULONG, ULONG);
using NtCreateFileFn = NTSTATUS(NTAPI*)(PHANDLE, ACCESS_MASK, POBJECT_ATTRIBUTES,
                                        PIO_STATUS_BLOCK, PLARGE_INTEGER, ULONG, ULONG,
                                        ULONG, ULONG, PVOID, ULONG);
using NtSetInformationFileFn = NTSTATUS(NTAPI*)(HANDLE, PIO_STATUS_BLOCK, PVOID,
                                                ULONG, ULONG);

static NtOpenFileFn real_nt_open_file = nullptr;
static NtCreateFileFn real_nt_create_file = nullptr;
static NtSetInformationFileFn real_nt_set_information_file = nullptr;

static thread_local const char* current_operation = nullptr;
static thread_local unsigned operation_events = 0;
static thread_local bool emitting = false;

// The native rename/link structures start with ReplaceIfExists and then the
// optional target-directory handle. On x64 and ARM64 that handle is at offset
// eight; the fixture reports it without reading the path or any user content.
struct RenameOrLinkTarget {
    BOOLEAN replace_if_exists;
    BYTE reserved[sizeof(HANDLE) - sizeof(BOOLEAN)];
    HANDLE root_directory;
};
static_assert(offsetof(RenameOrLinkTarget, root_directory) == sizeof(HANDLE));

static unsigned long long numeric_handle(HANDLE handle) {
    return static_cast<unsigned long long>(reinterpret_cast<uintptr_t>(handle));
}

static unsigned long long object_root(POBJECT_ATTRIBUTES attributes) {
    return attributes ? numeric_handle(attributes->RootDirectory) : 0;
}

static void print_open(const char* api, ACCESS_MASK access, POBJECT_ATTRIBUTES attributes,
                       ULONG share, ULONG disposition, ULONG options, NTSTATUS status) {
    if (!current_operation || emitting) return;
    ++operation_events;
    emitting = true;
    std::printf("NT_REQUEST operation=%s api=%s access=0x%08lx share=0x%08lx disposition=%lu options=0x%08lx object_root=0x%llx status=0x%08lx\n",
                current_operation, api, static_cast<unsigned long>(access),
                static_cast<unsigned long>(share), static_cast<unsigned long>(disposition),
                static_cast<unsigned long>(options), object_root(attributes),
                static_cast<unsigned long>(status));
    std::fflush(stdout);
    emitting = false;
}

static NTSTATUS NTAPI record_nt_open_file(PHANDLE handle, ACCESS_MASK access,
                                          POBJECT_ATTRIBUTES attributes, PIO_STATUS_BLOCK io,
                                          ULONG share, ULONG options) {
    NTSTATUS status = real_nt_open_file(handle, access, attributes, io, share, options);
    print_open("NtOpenFile", access, attributes, share, FILE_OPEN, options, status);
    return status;
}

static NTSTATUS NTAPI record_nt_create_file(PHANDLE handle, ACCESS_MASK access,
                                            POBJECT_ATTRIBUTES attributes, PIO_STATUS_BLOCK io,
                                            PLARGE_INTEGER allocation, ULONG file_attributes,
                                            ULONG share, ULONG disposition, ULONG options,
                                            PVOID ea_buffer, ULONG ea_length) {
    NTSTATUS status = real_nt_create_file(handle, access, attributes, io, allocation,
                                          file_attributes, share, disposition, options,
                                          ea_buffer, ea_length);
    print_open("NtCreateFile", access, attributes, share, disposition, options, status);
    return status;
}

static NTSTATUS NTAPI record_nt_set_information_file(HANDLE file, PIO_STATUS_BLOCK io,
                                                      PVOID information, ULONG length,
                                                      ULONG information_class) {
    NTSTATUS status = real_nt_set_information_file(file, io, information, length, information_class);
    if (!current_operation || emitting) return status;
    HANDLE root = nullptr;
    // FileRenameInformation/FileLinkInformation and their Ex forms use this
    // prefix. Other information classes deliberately report a zero root.
    if ((information_class == 10 || information_class == 11 || information_class == 65 ||
         information_class == 72) &&
        information && length >= sizeof(RenameOrLinkTarget)) {
        root = static_cast<RenameOrLinkTarget*>(information)->root_directory;
    }
    ++operation_events;
    emitting = true;
    std::printf("NT_REQUEST operation=%s api=NtSetInformationFile file=0x%llx info_class=%lu information_length=%lu relative_root=0x%llx status=0x%08lx\n",
                current_operation, numeric_handle(file), static_cast<unsigned long>(information_class),
                static_cast<unsigned long>(length), numeric_handle(root),
                static_cast<unsigned long>(status));
    std::fflush(stdout);
    emitting = false;
    return status;
}

template <typename T>
static bool resolve_nt(T& target, const char* name) {
    FARPROC address = GetProcAddress(GetModuleHandleW(L"ntdll.dll"), name);
    static_assert(sizeof(target) == sizeof(address));
    std::memcpy(&target, &address, sizeof(target));
    return address != nullptr;
}

static bool attach_detours() {
    if (!resolve_nt(real_nt_open_file, "NtOpenFile") ||
        !resolve_nt(real_nt_create_file, "NtCreateFile") ||
        !resolve_nt(real_nt_set_information_file, "NtSetInformationFile")) return false;
    if (DetourTransactionBegin() != NO_ERROR || DetourUpdateThread(GetCurrentThread()) != NO_ERROR ||
        DetourAttach(reinterpret_cast<PVOID*>(&real_nt_open_file), record_nt_open_file) != NO_ERROR ||
        DetourAttach(reinterpret_cast<PVOID*>(&real_nt_create_file), record_nt_create_file) != NO_ERROR ||
        DetourAttach(reinterpret_cast<PVOID*>(&real_nt_set_information_file),
                     record_nt_set_information_file) != NO_ERROR)
        return false;
    return DetourTransactionCommit() == NO_ERROR;
}

static void begin_operation(const char* name) {
    current_operation = name;
    operation_events = 0;
}

static bool end_operation(bool passed, DWORD error) {
    bool captured = operation_events != 0;
    emitting = true;
    std::printf("OPERATION_RESULT operation=%s execution=%s capture=%s error=%lu nt_events=%u\n",
                current_operation, passed ? "pass" : "notpass", captured ? "pass" : "notpass",
                static_cast<unsigned long>(error), operation_events);
    std::fflush(stdout);
    emitting = false;
    current_operation = nullptr;
    return passed && captured;
}

static bool make_file(const wchar_t* path) {
    HANDLE file = CreateFileW(path, GENERIC_WRITE, 0, nullptr, CREATE_ALWAYS,
                              FILE_ATTRIBUTE_NORMAL, nullptr);
    if (file == INVALID_HANDLE_VALUE) return false;
    CloseHandle(file);
    return true;
}

static bool is_absent(const wchar_t* path) {
    if (GetFileAttributesW(path) != INVALID_FILE_ATTRIBUTES) return false;
    DWORD error = GetLastError();
    return error == ERROR_FILE_NOT_FOUND || error == ERROR_PATH_NOT_FOUND;
}

// Owns every disposable path from the moment the root exists. Calling finish
// makes cleanup observable; the destructor is the all-exit fallback.
struct FixtureCleanup {
    wchar_t root[MAX_PATH] = {};
    wchar_t enumeration_directory[MAX_PATH] = {};
    wchar_t enumeration_file[MAX_PATH] = {};
    wchar_t created_directory[MAX_PATH] = {};
    wchar_t rename_source[MAX_PATH] = {};
    wchar_t rename_destination[MAX_PATH] = {};
    wchar_t hard_link[MAX_PATH] = {};
    bool finished = false;

    bool finish() {
        if (finished) return is_absent(root);
        finished = true;
        DeleteFileW(hard_link);
        DeleteFileW(rename_source);
        DeleteFileW(rename_destination);
        DeleteFileW(enumeration_file);
        RemoveDirectoryW(created_directory);
        RemoveDirectoryW(enumeration_directory);
        RemoveDirectoryW(root);
        bool absent = is_absent(root);
        DWORD error = absent ? ERROR_SUCCESS : GetLastError();
        std::printf("CLEANUP_RESULT result=%s root_absent=%s error=%lu\n",
                    absent ? "pass" : "notpass", absent ? "pass" : "notpass",
                    static_cast<unsigned long>(error));
        return absent;
    }

    ~FixtureCleanup() {
        if (!finished) finish();
    }
};

int wmain() {
    if (!attach_detours()) {
        std::printf("FIXTURE_RESULT result=notpass stage=attach error=%lu\n",
                    static_cast<unsigned long>(GetLastError()));
        return 2;
    }

    wchar_t temporary[MAX_PATH] = {};
    DWORD temporary_length = GetTempPathW(MAX_PATH, temporary);
    if (!temporary_length || temporary_length >= MAX_PATH) return 3;
    FixtureCleanup cleanup;
    if (swprintf_s(cleanup.root, L"%snub-file-operation-shapes-%lu", temporary,
                   static_cast<unsigned long>(GetCurrentProcessId())) < 0 ||
        !CreateDirectoryW(cleanup.root, nullptr)) return 4;

    swprintf_s(cleanup.enumeration_directory, L"%s\\enumeration", cleanup.root);
    swprintf_s(cleanup.enumeration_file, L"%s\\entry.txt", cleanup.enumeration_directory);
    swprintf_s(cleanup.created_directory, L"%s\\created", cleanup.root);
    swprintf_s(cleanup.rename_source, L"%s\\rename-source.txt", cleanup.root);
    swprintf_s(cleanup.rename_destination, L"%s\\rename-destination.txt", cleanup.root);
    swprintf_s(cleanup.hard_link, L"%s\\hard-link.txt", cleanup.root);

    int failures = 0;
    if (!CreateDirectoryW(cleanup.enumeration_directory, nullptr) || !make_file(cleanup.enumeration_file) ||
        !make_file(cleanup.rename_source) || !make_file(cleanup.rename_destination)) {
        bool cleaned = cleanup.finish();
        return cleaned ? 5 : 6;
    }

    begin_operation("create-directory");
    BOOL created = CreateDirectoryW(cleanup.created_directory, nullptr);
    DWORD create_error = created ? ERROR_SUCCESS : GetLastError();
    failures += !end_operation(created != FALSE, create_error);

    begin_operation("remove-directory");
    BOOL removed = RemoveDirectoryW(cleanup.created_directory);
    DWORD remove_error = removed ? ERROR_SUCCESS : GetLastError();
    failures += !end_operation(removed != FALSE, remove_error);

    wchar_t pattern[MAX_PATH] = {};
    swprintf_s(pattern, L"%s\\*", cleanup.enumeration_directory);
    begin_operation("directory-enumeration");
    WIN32_FIND_DATAW entry = {};
    HANDLE enumeration = FindFirstFileW(pattern, &entry);
    BOOL enumerated = enumeration != INVALID_HANDLE_VALUE;
    DWORD enumeration_error = enumerated ? ERROR_SUCCESS : GetLastError();
    bool entry_found = false;
    while (enumerated) {
        if (wcscmp(entry.cFileName, L"entry.txt") == 0) entry_found = true;
        if (!FindNextFileW(enumeration, &entry)) {
            DWORD next_error = GetLastError();
            if (next_error != ERROR_NO_MORE_FILES) enumeration_error = next_error;
            break;
        }
    }
    if (enumeration != INVALID_HANDLE_VALUE) FindClose(enumeration);
    enumerated = enumerated && entry_found && enumeration_error == ERROR_SUCCESS;
    if (!entry_found && enumeration_error == ERROR_SUCCESS) enumeration_error = ERROR_FILE_NOT_FOUND;
    failures += !end_operation(enumerated != FALSE, enumeration_error);

    begin_operation("rename-replace");
    BOOL renamed = MoveFileExW(cleanup.rename_source, cleanup.rename_destination, MOVEFILE_REPLACE_EXISTING);
    DWORD rename_error = renamed ? ERROR_SUCCESS : GetLastError();
    failures += !end_operation(renamed != FALSE, rename_error);

    begin_operation("hardlink");
    BOOL linked = CreateHardLinkW(cleanup.hard_link, cleanup.rename_destination, nullptr);
    DWORD link_error = linked ? ERROR_SUCCESS : GetLastError();
    bool hardlink_passed = end_operation(linked != FALSE, link_error);
    // A normal same-volume hard link needs no privilege. Record a platform or
    // filesystem limitation explicitly instead of treating it as a pass.
    if (!linked && link_error == ERROR_PRIVILEGE_NOT_HELD)
        std::printf("OPERATION_NOTE operation=hardlink result=notpass reason=privilege-not-held\n");
    failures += !hardlink_passed;

    begin_operation("delete");
    BOOL deleted = DeleteFileW(cleanup.hard_link);
    DWORD delete_error = deleted ? ERROR_SUCCESS : GetLastError();
    failures += !end_operation(deleted != FALSE, delete_error);

    wchar_t system_directory[MAX_PATH] = {};
    DWORD system_length = GetSystemDirectoryW(system_directory, MAX_PATH);
    wchar_t application[MAX_PATH] = {};
    swprintf_s(application, L"%s\\cmd.exe", system_directory);
    wchar_t command[] = L"cmd.exe /d /c exit 0";
    begin_operation("launch-system-command");
    STARTUPINFOW startup = {sizeof(startup)};
    PROCESS_INFORMATION child = {};
    BOOL launched = system_length && system_length < MAX_PATH &&
                    CreateProcessW(application, command, nullptr, nullptr, FALSE, 0, nullptr,
                                   nullptr, &startup, &child);
    DWORD launch_error = launched ? ERROR_SUCCESS : GetLastError();
    if (launched) {
        DWORD wait = WaitForSingleObject(child.hProcess, 30000);
        DWORD exit_code = 1;
        if (wait == WAIT_OBJECT_0 && GetExitCodeProcess(child.hProcess, &exit_code) && exit_code == 0) {
            launched = TRUE;
        } else {
            launched = FALSE;
            launch_error = wait == WAIT_TIMEOUT ? ERROR_TIMEOUT :
                           (wait == WAIT_FAILED ? GetLastError() : exit_code);
            if (wait == WAIT_TIMEOUT) {
                TerminateProcess(child.hProcess, ERROR_TIMEOUT);
                WaitForSingleObject(child.hProcess, 30000);
            }
        }
        CloseHandle(child.hThread);
        CloseHandle(child.hProcess);
    }
    failures += !end_operation(launched != FALSE, launch_error);

    if (!cleanup.finish()) ++failures;
    std::printf("FIXTURE_RESULT result=%s failures=%d\n", failures ? "notpass" : "pass", failures);
    return failures ? 1 : 0;
}
