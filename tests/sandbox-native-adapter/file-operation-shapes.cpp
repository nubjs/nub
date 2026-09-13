// Records the NT request shapes produced by a small set of ordinary Win32
// filesystem calls. This is a diagnostic fixture, not an adapter or policy.
#if !defined(_WIN32_WINNT) || _WIN32_WINNT < 0x0602
#undef _WIN32_WINNT
#define _WIN32_WINNT 0x0602
#endif
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
static thread_local bool open_status_recorded = false;
static thread_local NTSTATUS open_status = 0;
static thread_local bool rename_status_recorded = false;
static thread_local NTSTATUS rename_status = 0;
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
    open_status_recorded = true;
    open_status = status;
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
    DWORD last_error = GetLastError();
    print_open("NtOpenFile", access, attributes, share, FILE_OPEN, options, status);
    SetLastError(last_error);
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
    DWORD last_error = GetLastError();
    print_open("NtCreateFile", access, attributes, share, disposition, options, status);
    SetLastError(last_error);
    return status;
}

static NTSTATUS NTAPI record_nt_set_information_file(HANDLE file, PIO_STATUS_BLOCK io,
                                                      PVOID information, ULONG length,
                                                      ULONG information_class) {
    NTSTATUS status = real_nt_set_information_file(file, io, information, length, information_class);
    DWORD last_error = GetLastError();
    if (!current_operation || emitting) return status;
    if (information_class == 10 || information_class == 65) {
        rename_status_recorded = true;
        rename_status = status;
    }
    HANDLE root = nullptr;
    DWORD info_flags = 0;
    bool info_flags_available = false;
    if (information && (information_class == 10 || information_class == 11) &&
        length >= sizeof(BOOLEAN)) {
        info_flags = *static_cast<BOOLEAN*>(information);
        info_flags_available = true;
    }
    if (information && (information_class == 64 || information_class == 65 ||
                        information_class == 72) && length >= sizeof(DWORD)) {
        info_flags = *static_cast<DWORD*>(information);
        info_flags_available = true;
    }
    // FileRenameInformation/FileLinkInformation and their Ex forms use this
    // prefix. Other information classes deliberately report a zero root.
    if ((information_class == 10 || information_class == 11 || information_class == 65 ||
         information_class == 72) &&
        information && length >= sizeof(RenameOrLinkTarget)) {
        root = static_cast<RenameOrLinkTarget*>(information)->root_directory;
    }
    ++operation_events;
    emitting = true;
    std::printf("NT_REQUEST operation=%s api=NtSetInformationFile file=0x%llx info_class=%lu information_length=%lu relative_root=0x%llx info_flags=",
                current_operation, numeric_handle(file), static_cast<unsigned long>(information_class),
                static_cast<unsigned long>(length), numeric_handle(root));
    if (info_flags_available)
        std::printf("0x%08lx", static_cast<unsigned long>(info_flags));
    else
        std::printf("not-applicable");
    std::printf(" status=0x%08lx\n", static_cast<unsigned long>(status));
    std::fflush(stdout);
    emitting = false;
    SetLastError(last_error);
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
    LONG error = DetourTransactionBegin();
    if (error != NO_ERROR) { SetLastError(error); return false; }
    error = DetourUpdateThread(GetCurrentThread());
    if (error == NO_ERROR)
        error = DetourAttach(reinterpret_cast<PVOID*>(&real_nt_open_file), record_nt_open_file);
    if (error == NO_ERROR)
        error = DetourAttach(reinterpret_cast<PVOID*>(&real_nt_create_file), record_nt_create_file);
    if (error == NO_ERROR)
        error = DetourAttach(reinterpret_cast<PVOID*>(&real_nt_set_information_file), record_nt_set_information_file);
    if (error != NO_ERROR) {
        DetourTransactionAbort();
        SetLastError(error);
        return false;
    }
    error = DetourTransactionCommit();
    SetLastError(error);
    return error == NO_ERROR;
}

static void begin_operation(const char* name) {
    current_operation = name;
    operation_events = 0;
    open_status_recorded = false;
    rename_status_recorded = false;
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

static bool format_path(wchar_t* destination, size_t destination_count, const wchar_t* root,
                        const wchar_t* leaf) {
    return _snwprintf_s(destination, destination_count, _TRUNCATE, L"%s\\%s", root, leaf) >= 0;
}

struct FileIdentity {
    enum class State { unknown, absent, present } state = State::unknown;
    bool available = false;
    DWORD error = ERROR_SUCCESS;
    FILE_ID_INFO value = {};
};

static FileIdentity identity_for_handle(HANDLE handle) {
    FileIdentity identity;
    identity.state = handle == INVALID_HANDLE_VALUE ? FileIdentity::State::unknown
                                                     : FileIdentity::State::present;
    identity.available = identity.state == FileIdentity::State::present && GetFileInformationByHandleEx(
        handle, FileIdInfo, &identity.value, sizeof(identity.value));
    if (!identity.available) identity.error = GetLastError();
    return identity;
}

static FileIdentity identity_for_path(const wchar_t* path) {
    FileIdentity identity;
    HANDLE handle = CreateFileW(path, FILE_READ_ATTRIBUTES | SYNCHRONIZE,
                                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                                nullptr, OPEN_EXISTING,
                                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_BACKUP_SEMANTICS, nullptr);
    if (handle == INVALID_HANDLE_VALUE) {
        identity.error = GetLastError();
        if (identity.error == ERROR_FILE_NOT_FOUND || identity.error == ERROR_PATH_NOT_FOUND)
            identity.state = FileIdentity::State::absent;
        return identity;
    }
    identity = identity_for_handle(handle);
    CloseHandle(handle);
    return identity;
}

static void format_identity(const FileIdentity& identity, char* output, size_t output_count) {
    if (identity.state == FileIdentity::State::absent) {
        _snprintf_s(output, output_count, _TRUNCATE, "absent");
        return;
    }
    if (identity.state != FileIdentity::State::present || !identity.available) {
        _snprintf_s(output, output_count, _TRUNCATE, "lookup-error-%lu",
                    static_cast<unsigned long>(identity.error));
        return;
    }
    _snprintf_s(output, output_count, _TRUNCATE,
                "%016llx-%02x%02x%02x%02x%02x%02x%02x%02x%02x%02x%02x%02x%02x%02x%02x%02x",
                static_cast<unsigned long long>(identity.value.VolumeSerialNumber),
                identity.value.FileId.Identifier[0], identity.value.FileId.Identifier[1],
                identity.value.FileId.Identifier[2], identity.value.FileId.Identifier[3],
                identity.value.FileId.Identifier[4], identity.value.FileId.Identifier[5],
                identity.value.FileId.Identifier[6], identity.value.FileId.Identifier[7],
                identity.value.FileId.Identifier[8], identity.value.FileId.Identifier[9],
                identity.value.FileId.Identifier[10], identity.value.FileId.Identifier[11],
                identity.value.FileId.Identifier[12], identity.value.FileId.Identifier[13],
                identity.value.FileId.Identifier[14], identity.value.FileId.Identifier[15]);
}

// Use the documented SetFileInformationByHandle buffer layout without making the
// fixture's compilation depend on a particular SDK's FileRenameInfoEx declaration.
struct RenameInformationBuffer {
    DWORD flags;
    HANDLE root_directory;
    DWORD file_name_length;
    WCHAR file_name[MAX_PATH];
};
static_assert(offsetof(RenameInformationBuffer, root_directory) == sizeof(HANDLE));

constexpr FILE_INFO_BY_HANDLE_CLASS kFileRenameInfo = FileRenameInfo;
constexpr FILE_INFO_BY_HANDLE_CLASS kFileRenameInfoEx =
    static_cast<FILE_INFO_BY_HANDLE_CLASS>(22);
constexpr DWORD kFileRenameFlagReplaceIfExists = 0x00000001;
constexpr DWORD kFileRenameFlagPosixSemantics = 0x00000002;

static bool rename_replace(HANDLE source, const wchar_t* destination, bool posix) {
    RenameInformationBuffer information = {};
    information.flags = posix ? kFileRenameFlagReplaceIfExists | kFileRenameFlagPosixSemantics
                              : kFileRenameFlagReplaceIfExists;
    size_t name_length = wcslen(destination);
    if (name_length >= MAX_PATH) {
        SetLastError(ERROR_FILENAME_EXCED_RANGE);
        return false;
    }
    std::memcpy(information.file_name, destination, name_length * sizeof(WCHAR));
    information.file_name_length = static_cast<DWORD>(name_length * sizeof(WCHAR));
    DWORD length = static_cast<DWORD>(offsetof(RenameInformationBuffer, file_name) +
                                      information.file_name_length);
    return SetFileInformationByHandle(source, posix ? kFileRenameInfoEx : kFileRenameInfo,
                                      &information, length) != FALSE;
}

// Owns every disposable path from the moment the root exists. Calling finish
// makes cleanup observable; the destructor is the all-exit fallback.
struct FixtureCleanup {
    static constexpr size_t kPinFileCases = 6;
    static constexpr size_t kPinDirectoryCases = 4;
    wchar_t root[MAX_PATH] = {};
    wchar_t enumeration_directory[MAX_PATH] = {};
    wchar_t enumeration_file[MAX_PATH] = {};
    wchar_t created_directory[MAX_PATH] = {};
    wchar_t rename_source[MAX_PATH] = {};
    wchar_t rename_destination[MAX_PATH] = {};
    wchar_t hard_link[MAX_PATH] = {};
    wchar_t pin_file_sources[kPinFileCases][MAX_PATH] = {};
    wchar_t pin_file_destinations[kPinFileCases][MAX_PATH] = {};
    wchar_t pin_directory_sources[kPinDirectoryCases][MAX_PATH] = {};
    wchar_t pin_directory_destinations[kPinDirectoryCases][MAX_PATH] = {};
    bool owned = false;
    bool finished = false;

    bool finish() {
        if (!owned) return true;
        if (finished) return is_absent(root);
        finished = true;
        DeleteFileW(hard_link);
        DeleteFileW(rename_source);
        DeleteFileW(rename_destination);
        DeleteFileW(enumeration_file);
        for (size_t index = 0; index < kPinFileCases; ++index) {
            DeleteFileW(pin_file_sources[index]);
            DeleteFileW(pin_file_destinations[index]);
        }
        for (size_t index = 0; index < kPinDirectoryCases; ++index) {
            RemoveDirectoryW(pin_directory_sources[index]);
            RemoveDirectoryW(pin_directory_destinations[index]);
        }
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

static bool format_pin_case_paths(const wchar_t* root, const wchar_t* label, wchar_t* source,
                                  wchar_t* destination) {
    wchar_t source_leaf[96] = {};
    wchar_t destination_leaf[96] = {};
    if (_snwprintf_s(source_leaf, _TRUNCATE, L"%s-source", label) < 0 ||
        _snwprintf_s(destination_leaf, _TRUNCATE, L"%s-destination", label) < 0)
        return false;
    return format_path(source, MAX_PATH, root, source_leaf) &&
           format_path(destination, MAX_PATH, root, destination_leaf);
}

static bool remove_pin_pair(const wchar_t* source, const wchar_t* destination, bool directory) {
    if (directory) {
        RemoveDirectoryW(source);
        RemoveDirectoryW(destination);
    } else {
        DeleteFileW(source);
        DeleteFileW(destination);
    }
    return is_absent(source) && is_absent(destination);
}

static void run_file_pin_case(const char* label, const wchar_t* source_path,
                              const wchar_t* destination_path, DWORD holder_access, bool posix) {
    bool setup = make_file(source_path) && make_file(destination_path);
    DWORD setup_error = setup ? ERROR_SUCCESS : GetLastError();
    HANDLE source = INVALID_HANDLE_VALUE;
    HANDLE holder = INVALID_HANDLE_VALUE;
    if (setup) {
        source = CreateFileW(source_path, DELETE | SYNCHRONIZE,
                             FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, nullptr,
                             OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, nullptr);
        if (source == INVALID_HANDLE_VALUE) setup_error = GetLastError();
        if (source != INVALID_HANDLE_VALUE && holder_access) {
            holder = CreateFileW(destination_path, holder_access, FILE_SHARE_READ | FILE_SHARE_WRITE,
                                 nullptr, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, nullptr);
            if (holder == INVALID_HANDLE_VALUE) setup_error = GetLastError();
        }
    }
    setup = setup && source != INVALID_HANDLE_VALUE &&
            (!holder_access || holder != INVALID_HANDLE_VALUE);

    FileIdentity source_before = source == INVALID_HANDLE_VALUE ? FileIdentity{} : identity_for_handle(source);
    FileIdentity destination_before = holder == INVALID_HANDLE_VALUE
        ? identity_for_path(destination_path) : identity_for_handle(holder);
    bool renamed = false;
    DWORD rename_error = setup ? ERROR_SUCCESS : setup_error;
    unsigned rename_events = 0;
    bool nt_status_available = false;
    NTSTATUS recorded_status = 0;
    if (setup) {
        begin_operation(label);
        renamed = rename_replace(source, destination_path, posix);
        rename_error = renamed ? ERROR_SUCCESS : GetLastError();
        rename_events = operation_events;
        nt_status_available = rename_status_recorded;
        recorded_status = rename_status;
        current_operation = nullptr;
    }
    if (holder != INVALID_HANDLE_VALUE) CloseHandle(holder);
    if (source != INVALID_HANDLE_VALUE) CloseHandle(source);

    FileIdentity source_after = identity_for_path(source_path);
    FileIdentity destination_after = identity_for_path(destination_path);
    bool cleaned = remove_pin_pair(source_path, destination_path, false);
    char source_before_text[64] = {};
    char destination_before_text[64] = {};
    char source_after_text[64] = {};
    char destination_after_text[64] = {};
    format_identity(source_before, source_before_text, sizeof(source_before_text));
    format_identity(destination_before, destination_before_text, sizeof(destination_before_text));
    format_identity(source_after, source_after_text, sizeof(source_after_text));
    format_identity(destination_after, destination_after_text, sizeof(destination_after_text));
    std::printf("PIN_CASE case=%s subject=target-file holder=%s holder_access=0x%08lx holder_share=0x%08lx setup=%s setup_error=%lu rename=%s execution=%s error=%lu nt_status=",
                label,
                holder_access ? "held" : "unheld", static_cast<unsigned long>(holder_access),
                static_cast<unsigned long>(FILE_SHARE_READ | FILE_SHARE_WRITE),
                setup ? "pass" : "notpass", static_cast<unsigned long>(setup_error),
                posix ? "replace-posix" : "replace-if-exists", renamed ? "pass" : "notpass",
                static_cast<unsigned long>(rename_error));
    if (nt_status_available)
        std::printf("0x%08lx", static_cast<unsigned long>(recorded_status));
    else
        std::printf("not-captured");
    std::printf(" nt_events=%u source_before=%s destination_before=%s source_after=%s destination_after=%s cleanup=%s\n",
                rename_events, source_before_text, destination_before_text, source_after_text,
                destination_after_text, cleaned ? "pass" : "notpass");
    std::fflush(stdout);
}

static void run_directory_pin_case(const char* label, const wchar_t* source_path,
                                   const wchar_t* destination_path, DWORD holder_access, bool posix) {
    bool setup = CreateDirectoryW(source_path, nullptr) != FALSE;
    DWORD setup_error = setup ? ERROR_SUCCESS : GetLastError();
    FileIdentity destination_before = identity_for_path(destination_path);
    if (destination_before.state != FileIdentity::State::absent) {
        setup = false;
        setup_error = destination_before.state == FileIdentity::State::present
            ? ERROR_ALREADY_EXISTS : destination_before.error;
    }
    HANDLE holder = INVALID_HANDLE_VALUE;
    if (setup && holder_access) {
        holder = CreateFileW(source_path, holder_access, FILE_SHARE_READ | FILE_SHARE_WRITE,
                             nullptr, OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, nullptr);
        if (holder == INVALID_HANDLE_VALUE) {
            setup = false;
            setup_error = GetLastError();
        }
    }

    HANDLE rename_source = INVALID_HANDLE_VALUE;
    bool delete_open_attempted = false;
    bool delete_opened = false;
    DWORD delete_open_error = setup ? ERROR_SUCCESS : setup_error;
    bool rename_attempted = false;
    bool renamed = false;
    DWORD rename_error = ERROR_SUCCESS;
    unsigned operation_count = 0;
    bool open_nt_status_available = false;
    NTSTATUS recorded_open_status = 0;
    bool rename_nt_status_available = false;
    NTSTATUS recorded_rename_status = 0;
    FileIdentity source_before;
    if (setup) {
        begin_operation(label);
        delete_open_attempted = true;
        rename_source = CreateFileW(source_path, DELETE | SYNCHRONIZE,
                                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                                    nullptr, OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, nullptr);
        delete_opened = rename_source != INVALID_HANDLE_VALUE;
        delete_open_error = delete_opened ? ERROR_SUCCESS : GetLastError();
        open_nt_status_available = open_status_recorded;
        recorded_open_status = open_status;
        if (delete_opened) {
            source_before = identity_for_handle(rename_source);
            rename_attempted = true;
            renamed = rename_replace(rename_source, destination_path, posix);
            rename_error = renamed ? ERROR_SUCCESS : GetLastError();
        }
        operation_count = operation_events;
        rename_nt_status_available = rename_status_recorded;
        recorded_rename_status = rename_status;
        current_operation = nullptr;
    }
    if (rename_source != INVALID_HANDLE_VALUE) CloseHandle(rename_source);
    if (holder != INVALID_HANDLE_VALUE) {
        if (source_before.state != FileIdentity::State::present)
            source_before = identity_for_handle(holder);
        CloseHandle(holder);
    }

    FileIdentity source_after = identity_for_path(source_path);
    FileIdentity destination_after = identity_for_path(destination_path);
    bool cleaned = remove_pin_pair(source_path, destination_path, true);
    char source_before_text[64] = {};
    char destination_before_text[64] = {};
    char source_after_text[64] = {};
    char destination_after_text[64] = {};
    format_identity(source_before, source_before_text, sizeof(source_before_text));
    format_identity(destination_before, destination_before_text, sizeof(destination_before_text));
    format_identity(source_after, source_after_text, sizeof(source_after_text));
    format_identity(destination_after, destination_after_text, sizeof(destination_after_text));
    std::printf("PIN_CASE case=%s subject=directory-parent holder=%s holder_access=0x%08lx holder_share=0x%08lx setup=%s setup_error=%lu delete_open=%s delete_error=%lu open_nt_status=",
                label, holder_access ? "held" : "unheld", static_cast<unsigned long>(holder_access),
                static_cast<unsigned long>(FILE_SHARE_READ | FILE_SHARE_WRITE),
                setup ? "pass" : "notpass", static_cast<unsigned long>(setup_error),
                !delete_open_attempted ? "not-attempted" : (delete_opened ? "pass" : "notpass"),
                static_cast<unsigned long>(delete_open_error));
    if (open_nt_status_available)
        std::printf("0x%08lx", static_cast<unsigned long>(recorded_open_status));
    else
        std::printf("not-captured");
    std::printf(" rename=%s execution=%s error=%lu nt_status=",
                posix ? "replace-posix" : "replace-if-exists",
                rename_attempted ? (renamed ? "pass" : "notpass") : "not-attempted",
                static_cast<unsigned long>(rename_attempted ? rename_error : ERROR_SUCCESS));
    if (rename_nt_status_available)
        std::printf("0x%08lx", static_cast<unsigned long>(recorded_rename_status));
    else
        std::printf("not-captured");
    std::printf(" nt_events=%u source_before=%s destination_before=%s source_after=%s destination_after=%s cleanup=%s\n",
                operation_count, source_before_text, destination_before_text, source_after_text,
                destination_after_text, cleaned ? "pass" : "notpass");
    std::fflush(stdout);
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
    FixtureCleanup cleanup;
    if (_snwprintf_s(cleanup.root, _TRUNCATE, L"%snub-file-operation-shapes-%lu", temporary,
                   static_cast<unsigned long>(GetCurrentProcessId())) < 0 ||
        !CreateDirectoryW(cleanup.root, nullptr)) return 4;
    cleanup.owned = true;

    if (_snwprintf_s(cleanup.enumeration_directory, _TRUNCATE, L"%s\\enumeration", cleanup.root) < 0 ||
        _snwprintf_s(cleanup.enumeration_file, _TRUNCATE, L"%s\\entry.txt", cleanup.enumeration_directory) < 0 ||
        _snwprintf_s(cleanup.created_directory, _TRUNCATE, L"%s\\created", cleanup.root) < 0 ||
        _snwprintf_s(cleanup.rename_source, _TRUNCATE, L"%s\\rename-source.txt", cleanup.root) < 0 ||
        _snwprintf_s(cleanup.rename_destination, _TRUNCATE, L"%s\\rename-destination.txt", cleanup.root) < 0 ||
        _snwprintf_s(cleanup.hard_link, _TRUNCATE, L"%s\\hard-link.txt", cleanup.root) < 0)
        return cleanup.finish() ? 5 : 6;
    if (!format_pin_case_paths(cleanup.root, L"pin-file-unheld-plain", cleanup.pin_file_sources[0],
                               cleanup.pin_file_destinations[0]) ||
        !format_pin_case_paths(cleanup.root, L"pin-file-unheld-posix", cleanup.pin_file_sources[1],
                               cleanup.pin_file_destinations[1]) ||
        !format_pin_case_paths(cleanup.root, L"pin-file-attributes-plain", cleanup.pin_file_sources[2],
                               cleanup.pin_file_destinations[2]) ||
        !format_pin_case_paths(cleanup.root, L"pin-file-attributes-posix", cleanup.pin_file_sources[3],
                               cleanup.pin_file_destinations[3]) ||
        !format_pin_case_paths(cleanup.root, L"pin-file-read-data-plain", cleanup.pin_file_sources[4],
                               cleanup.pin_file_destinations[4]) ||
        !format_pin_case_paths(cleanup.root, L"pin-file-read-data-posix", cleanup.pin_file_sources[5],
                               cleanup.pin_file_destinations[5]) ||
        !format_pin_case_paths(cleanup.root, L"pin-parent-unheld-plain", cleanup.pin_directory_sources[0],
                               cleanup.pin_directory_destinations[0]) ||
        !format_pin_case_paths(cleanup.root, L"pin-parent-unheld-posix", cleanup.pin_directory_sources[1],
                               cleanup.pin_directory_destinations[1]) ||
        !format_pin_case_paths(cleanup.root, L"pin-parent-traverse-plain", cleanup.pin_directory_sources[2],
                               cleanup.pin_directory_destinations[2]) ||
        !format_pin_case_paths(cleanup.root, L"pin-parent-traverse-posix", cleanup.pin_directory_sources[3],
                               cleanup.pin_directory_destinations[3]))
        return cleanup.finish() ? 5 : 6;

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
    if (_snwprintf_s(pattern, _TRUNCATE, L"%s\\*", cleanup.enumeration_directory) < 0)
        return cleanup.finish() ? 5 : 6;
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
    if (!system_length || system_length >= MAX_PATH ||
        _snwprintf_s(application, _TRUNCATE, L"%s\\cmd.exe", system_directory) < 0)
        return cleanup.finish() ? 5 : 6;
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

    // These cases report raw observations. A platform's replacement outcome is
    // deliberately not an assertion: the fixture exists to measure it.
    run_file_pin_case("pin-file-unheld-plain", cleanup.pin_file_sources[0],
                      cleanup.pin_file_destinations[0], 0, false);
    run_file_pin_case("pin-file-unheld-posix", cleanup.pin_file_sources[1],
                      cleanup.pin_file_destinations[1], 0, true);
    run_file_pin_case("pin-file-attributes-plain", cleanup.pin_file_sources[2],
                      cleanup.pin_file_destinations[2], FILE_READ_ATTRIBUTES | SYNCHRONIZE, false);
    run_file_pin_case("pin-file-attributes-posix", cleanup.pin_file_sources[3],
                      cleanup.pin_file_destinations[3], FILE_READ_ATTRIBUTES | SYNCHRONIZE, true);
    run_file_pin_case("pin-file-read-data-plain", cleanup.pin_file_sources[4],
                      cleanup.pin_file_destinations[4],
                      FILE_READ_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE, false);
    run_file_pin_case("pin-file-read-data-posix", cleanup.pin_file_sources[5],
                      cleanup.pin_file_destinations[5],
                      FILE_READ_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE, true);
    run_directory_pin_case("pin-parent-unheld-plain", cleanup.pin_directory_sources[0],
                           cleanup.pin_directory_destinations[0], 0, false);
    run_directory_pin_case("pin-parent-unheld-posix", cleanup.pin_directory_sources[1],
                           cleanup.pin_directory_destinations[1], 0, true);
    run_directory_pin_case("pin-parent-traverse-plain", cleanup.pin_directory_sources[2],
                           cleanup.pin_directory_destinations[2],
                           FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE, false);
    run_directory_pin_case("pin-parent-traverse-posix", cleanup.pin_directory_sources[3],
                           cleanup.pin_directory_destinations[3],
                           FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE, true);

    if (!cleanup.finish()) ++failures;
    std::printf("FIXTURE_RESULT result=%s failures=%d\n", failures ? "notpass" : "pass", failures);
    return failures ? 1 : 0;
}
