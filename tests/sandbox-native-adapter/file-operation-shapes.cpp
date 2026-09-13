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

using NtOpenFile = NTSTATUS(NTAPI*)(PHANDLE, ACCESS_MASK, POBJECT_ATTRIBUTES,
                                   PIO_STATUS_BLOCK, ULONG, ULONG);
using NtCreateFile = NTSTATUS(NTAPI*)(PHANDLE, ACCESS_MASK, POBJECT_ATTRIBUTES,
                                      PIO_STATUS_BLOCK, PLARGE_INTEGER, ULONG, ULONG,
                                      ULONG, ULONG, PVOID, ULONG);
using NtSetInformationFile = NTSTATUS(NTAPI*)(HANDLE, PIO_STATUS_BLOCK, PVOID,
                                              ULONG, ULONG);

static NtOpenFile real_nt_open_file = nullptr;
static NtCreateFile real_nt_create_file = nullptr;
static NtSetInformationFile real_nt_set_information_file = nullptr;

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

static void end_operation(bool passed, DWORD error) {
    emitting = true;
    std::printf("OPERATION_RESULT operation=%s result=%s error=%lu nt_events=%u\n",
                current_operation, passed ? "pass" : "notpass", static_cast<unsigned long>(error),
                operation_events);
    std::fflush(stdout);
    emitting = false;
    current_operation = nullptr;
}

static bool make_file(const wchar_t* path) {
    HANDLE file = CreateFileW(path, GENERIC_WRITE, 0, nullptr, CREATE_ALWAYS,
                              FILE_ATTRIBUTE_NORMAL, nullptr);
    if (file == INVALID_HANDLE_VALUE) return false;
    CloseHandle(file);
    return true;
}

int wmain() {
    if (!attach_detours()) {
        std::printf("FIXTURE_RESULT result=notpass stage=attach error=%lu\n",
                    static_cast<unsigned long>(GetLastError()));
        return 2;
    }

    wchar_t temporary[MAX_PATH] = {};
    DWORD temporary_length = GetTempPathW(MAX_PATH, temporary);
    if (!temporary_length || temporary_length >= MAX_PATH) return 3;
    wchar_t root[MAX_PATH] = {};
    if (swprintf_s(root, L"%snub-file-operation-shapes-%lu", temporary,
                   static_cast<unsigned long>(GetCurrentProcessId())) < 0 ||
        !CreateDirectoryW(root, nullptr)) return 4;

    wchar_t enumeration_directory[MAX_PATH] = {};
    wchar_t enumeration_file[MAX_PATH] = {};
    wchar_t created_directory[MAX_PATH] = {};
    wchar_t rename_source[MAX_PATH] = {};
    wchar_t rename_destination[MAX_PATH] = {};
    wchar_t hard_link[MAX_PATH] = {};
    swprintf_s(enumeration_directory, L"%s\\enumeration", root);
    swprintf_s(enumeration_file, L"%s\\entry.txt", enumeration_directory);
    swprintf_s(created_directory, L"%s\\created", root);
    swprintf_s(rename_source, L"%s\\rename-source.txt", root);
    swprintf_s(rename_destination, L"%s\\rename-destination.txt", root);
    swprintf_s(hard_link, L"%s\\hard-link.txt", root);

    int failures = 0;
    if (!CreateDirectoryW(enumeration_directory, nullptr) || !make_file(enumeration_file) ||
        !make_file(rename_source) || !make_file(rename_destination)) {
        RemoveDirectoryW(enumeration_directory);
        RemoveDirectoryW(root);
        return 5;
    }

    begin_operation("create-directory");
    BOOL created = CreateDirectoryW(created_directory, nullptr);
    DWORD create_error = created ? ERROR_SUCCESS : GetLastError();
    end_operation(created != FALSE, create_error);
    failures += created == FALSE;

    begin_operation("remove-directory");
    BOOL removed = RemoveDirectoryW(created_directory);
    DWORD remove_error = removed ? ERROR_SUCCESS : GetLastError();
    end_operation(removed != FALSE, remove_error);
    failures += removed == FALSE;

    wchar_t pattern[MAX_PATH] = {};
    swprintf_s(pattern, L"%s\\*", enumeration_directory);
    begin_operation("directory-enumeration");
    WIN32_FIND_DATAW entry = {};
    HANDLE enumeration = FindFirstFileW(pattern, &entry);
    BOOL enumerated = enumeration != INVALID_HANDLE_VALUE;
    DWORD enumeration_error = enumerated ? ERROR_SUCCESS : GetLastError();
    if (enumerated) FindClose(enumeration);
    end_operation(enumerated != FALSE, enumeration_error);
    failures += enumerated == FALSE;

    begin_operation("rename-replace");
    BOOL renamed = MoveFileExW(rename_source, rename_destination, MOVEFILE_REPLACE_EXISTING);
    DWORD rename_error = renamed ? ERROR_SUCCESS : GetLastError();
    end_operation(renamed != FALSE, rename_error);
    failures += renamed == FALSE;

    begin_operation("hardlink");
    BOOL linked = CreateHardLinkW(hard_link, rename_destination, nullptr);
    DWORD link_error = linked ? ERROR_SUCCESS : GetLastError();
    end_operation(linked != FALSE, link_error);
    // A normal same-volume hard link needs no privilege. Record a platform or
    // filesystem limitation explicitly instead of treating it as a pass.
    if (!linked && link_error == ERROR_PRIVILEGE_NOT_HELD)
        std::printf("OPERATION_NOTE operation=hardlink result=notpass reason=privilege-not-held\n");
    failures += linked == FALSE;

    begin_operation("delete");
    BOOL deleted = DeleteFileW(hard_link);
    DWORD delete_error = deleted ? ERROR_SUCCESS : GetLastError();
    end_operation(deleted != FALSE, delete_error);
    failures += deleted == FALSE;

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
        WaitForSingleObject(child.hProcess, INFINITE);
        DWORD exit_code = 1;
        GetExitCodeProcess(child.hProcess, &exit_code);
        launched = exit_code == 0;
        if (!launched) launch_error = exit_code;
        CloseHandle(child.hThread);
        CloseHandle(child.hProcess);
    }
    end_operation(launched != FALSE, launch_error);
    failures += launched == FALSE;

    DeleteFileW(hard_link);
    DeleteFileW(rename_source);
    DeleteFileW(rename_destination);
    DeleteFileW(enumeration_file);
    RemoveDirectoryW(enumeration_directory);
    RemoveDirectoryW(root);
    std::printf("FIXTURE_RESULT result=%s failures=%d\n", failures ? "notpass" : "pass", failures);
    return failures ? 1 : 0;
}
