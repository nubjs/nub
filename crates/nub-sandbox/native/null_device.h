#pragma once

// Bounded Nt{Open,Create}File fallback for runtimes that name the real null
// device directly. The parent injects a duplicate of its already-opened NUL
// handle; this never consults host device ACLs or substitutes a regular file.
namespace nub_sandbox::null_device {

inline bool duplicate_after_access_denied(HANDLE source, PHANDLE handle,
                                          ACCESS_MASK access, POBJECT_ATTRIBUTES attrs,
                                          PIO_STATUS_BLOCK io, ULONG share, ULONG options) {
    constexpr wchar_t kNull[] = L"\\Device\\Null";
    constexpr ACCESS_MASK kAllowedAccess =
        GENERIC_READ | GENERIC_WRITE | READ_CONTROL | SYNCHRONIZE | FILE_READ_ATTRIBUTES;
    constexpr ULONG kAllowedOptions =
        FILE_SYNCHRONOUS_IO_NONALERT | FILE_NON_DIRECTORY_FILE | FILE_OPEN_FOR_BACKUP_INTENT;
    constexpr ULONG kAllowedShare = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;

    // Return the original Nt status for every shape outside the narrowly
    // observed MSYS call. Invalid user pointers must take that path too.
    bool allowed = false;
    __try {
        allowed = source && source != INVALID_HANDLE_VALUE && handle && io && attrs &&
            attrs->Length == sizeof(OBJECT_ATTRIBUTES) && !attrs->RootDirectory &&
            !attrs->SecurityDescriptor && !attrs->SecurityQualityOfService &&
            !(attrs->Attributes & ~(OBJ_CASE_INSENSITIVE | OBJ_INHERIT)) &&
            attrs->ObjectName && attrs->ObjectName->Buffer &&
            attrs->ObjectName->Length == sizeof(kNull) - sizeof(wchar_t) &&
            attrs->ObjectName->MaximumLength >= attrs->ObjectName->Length &&
            !_wcsnicmp(attrs->ObjectName->Buffer, kNull, _countof(kNull) - 1) &&
            (options & FILE_SYNCHRONOUS_IO_NONALERT) && !(options & ~kAllowedOptions) &&
            !(access & ~kAllowedAccess) && !(share & ~kAllowedShare);
    } __except (EXCEPTION_EXECUTE_HANDLER) {
        return false;
    }
    if (!allowed) return false;

    HANDLE duplicate = INVALID_HANDLE_VALUE;
    __try {
        if (!DuplicateHandle(GetCurrentProcess(), source, GetCurrentProcess(), &duplicate,
                             access, (attrs->Attributes & OBJ_INHERIT) != 0, 0)) return false;
        *handle = duplicate;
        io->Status = 0;
        io->Information = FILE_OPENED;
    } __except (EXCEPTION_EXECUTE_HANDLER) {
        if (duplicate != INVALID_HANDLE_VALUE) CloseHandle(duplicate);
        return false;
    }
    return true;
}

} // namespace nub_sandbox::null_device
