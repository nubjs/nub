// Bounded Mount Manager query virtualizer.  This is intentionally not a
// general device broker: it recognizes only the synchronous query used to map
// one captured local volume device name to one captured DOS drive alias.
#pragma once

#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <winternl.h>
#include <cstdint>
#include <cstring>
#include <cwchar>

namespace nub_sandbox::mount_query {

constexpr NTSTATUS kStatusSuccess = static_cast<NTSTATUS>(0x00000000L);
constexpr NTSTATUS kStatusAccessDenied = static_cast<NTSTATUS>(0xc0000022L);
constexpr NTSTATUS kStatusInvalidParameter = static_cast<NTSTATUS>(0xc000000dL);
constexpr NTSTATUS kStatusBufferOverflow = static_cast<NTSTATUS>(0x80000005L);
constexpr NTSTATUS kStatusInsufficientResources = static_cast<NTSTATUS>(0xc000009aL);
constexpr NTSTATUS kStatusAccessViolation = static_cast<NTSTATUS>(0xc0000005L);

constexpr ULONG kQueryPoints = 0x006d0008;
constexpr ULONG kKnownShare = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
constexpr ULONG kKnownOptions = FILE_NON_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT;
constexpr ULONG kFileOpen = 1;
constexpr size_t kMaxCapabilities = 32;

// These layouts deliberately avoid including the WDK-only mountmgr header in
// the native compatibility DLL.  They are ABI-identical to its two structs.
struct MountPoint {
    ULONG symbolic_link_offset;
    USHORT symbolic_link_length;
    USHORT reserved1;
    ULONG unique_id_offset;
    USHORT unique_id_length;
    USHORT reserved2;
    ULONG device_name_offset;
    USHORT device_name_length;
    USHORT reserved3;
};

struct MountPointsHeader {
    ULONG size;
    ULONG number_of_mount_points;
    MountPoint mount_points[1];
};

static_assert(sizeof(MountPoint) == 24, "MOUNTMGR_MOUNT_POINT ABI changed");
static_assert(sizeof(MountPointsHeader) == 32, "MOUNTMGR_MOUNT_POINTS ABI changed");

using NtClose = NTSTATUS(NTAPI*)(HANDLE);

// The parent DLL injects real functions, rather than calling its detoured
// NtClose.  This prevents capability cleanup from recursing through hooks.
struct Api {
    HANDLE(WINAPI* create_event)(LPSECURITY_ATTRIBUTES, LPCWSTR, DWORD, DWORD) = nullptr;
    BOOL(WINAPI* duplicate_handle)(HANDLE, HANDLE, HANDLE, LPHANDLE, DWORD, BOOL, DWORD) = nullptr;
    BOOL(WINAPI* compare_object_handles)(HANDLE, HANDLE) = nullptr;
    NtClose close = nullptr;

    static Api system(NtClose close_function) {
        Api api = {CreateEventExW, DuplicateHandle, nullptr, close_function};
        // Some SDKs declare this API without shipping Kernelbase.lib.
        auto module = GetModuleHandleW(L"kernelbase.dll");
        auto address = module ? GetProcAddress(module, "CompareObjectHandles") : nullptr;
        static_assert(sizeof(address) == sizeof(api.compare_object_handles));
        memcpy(&api.compare_object_handles, &address, sizeof(address));
        return api;
    }
};

enum class IoctlDisposition {
    kForward,
    kHandled,
};

struct CloseTicket {
    HANDLE public_handle = INVALID_HANDLE_VALUE;
    ULONGLONG generation = 0;
};

struct DuplicateTicket {
    HANDLE public_handle = INVALID_HANDLE_VALUE;
    ULONGLONG generation = 0;
};

class Bridge {
public:
    Bridge() = default;
    Bridge(const Bridge&) = delete;
    Bridge& operator=(const Bridge&) = delete;

    // `devices` is the injection-time QueryDosDeviceW snapshot.  Only its
    // first NUL-terminated target is retained; multi-string aliases, failures,
    // MUP, and non-\\Device paths are excluded.
    bool initialize(const Api& api, const wchar_t devices[26][MAX_PATH]) {
        if (!api.create_event || !api.duplicate_handle || !api.compare_object_handles || !api.close || !devices) return false;
        api_ = api;
        for (size_t i = 0; i < 26; ++i) {
            maps_[i][0] = L'\0';
            size_t length = 0;
            while (length < MAX_PATH && devices[i][length]) ++length;
            if (length == MAX_PATH || !is_local_device(devices[i], length)) continue;
            memcpy(maps_[i], devices[i], (length + 1) * sizeof(wchar_t));
        }
        initialized_ = true;
        return true;
    }

    // Match only the observed absolute, synchronous open form.  This helper
    // does not touch a failed open's output pointers and is SEH-guarded because
    // ObjectAttributes is application memory at this interception boundary.
    bool matches_open(ACCESS_MASK access, POBJECT_ATTRIBUTES attributes, ULONG share,
                      ULONG disposition, ULONG options, bool is_create) const {
        if (!initialized_ || access != SYNCHRONIZE || share != kKnownShare ||
            options != kKnownOptions || disposition != (is_create ? kFileOpen : 0)) return false;
        return safe_mount_manager_name(attributes);
    }

    bool matches_open_file(ACCESS_MASK access, POBJECT_ATTRIBUTES attributes, ULONG share, ULONG options) const {
        return matches_open(access, attributes, share, 0, options, false);
    }

    bool matches_create_file(ACCESS_MASK access, POBJECT_ATTRIBUTES attributes, PLARGE_INTEGER allocation,
                             ULONG file_attributes, ULONG share, ULONG disposition, ULONG options,
                             PVOID ea_buffer, ULONG ea_length) const {
        return !allocation && (file_attributes == 0 || file_attributes == FILE_ATTRIBUTE_NORMAL) &&
               !ea_buffer && !ea_length &&
               matches_open(access, attributes, share, disposition, options, true);
    }

    // Replace a precisely matched ACCESS_DENIED open with a unique, valid
    // SYNCHRONIZE-only event handle.  The table's private duplicate disambiguates a
    // stale numeric handle after an unhooked raw close and kernel-slot reuse.
    NTSTATUS substitute_open(PHANDLE output_handle, PIO_STATUS_BLOCK io_status) {
        if (!initialized_) return finish(io_status, kStatusInsufficientResources, 0);
        HANDLE public_handle = api_.create_event(nullptr, nullptr, 0, SYNCHRONIZE);
        if (!public_handle || public_handle == INVALID_HANDLE_VALUE) {
            return finish(io_status, kStatusInsufficientResources, 0);
        }
        HANDLE identity = INVALID_HANDLE_VALUE;
        if (!api_.duplicate_handle(GetCurrentProcess(), public_handle, GetCurrentProcess(),
                                   &identity, 0, FALSE, DUPLICATE_SAME_ACCESS)) {
            api_.close(public_handle);
            return finish(io_status, kStatusInsufficientResources, 0);
        }
        ULONGLONG generation = 0;
        if (!insert(public_handle, identity, &generation)) {
            api_.close(identity);
            api_.close(public_handle);
            return finish(io_status, kStatusInsufficientResources, 0);
        }
        if (!safe_store_open(output_handle, io_status, public_handle)) {
            remove_and_close(public_handle, generation);
            api_.close(public_handle);
            return kStatusAccessViolation;
        }
        return kStatusSuccess;
    }

    // Call before and after the real NtClose.  Tickets borrow only the stable
    // generation; the removal winner takes ownership of the private identity.
    CloseTicket prepare_close(HANDLE handle) { return find_ticket<CloseTicket>(handle); }
    void complete_close(const CloseTicket& ticket, NTSTATUS status) {
        if (status >= 0 && ticket.generation) remove_and_close(ticket.public_handle, ticket.generation);
    }

    // Ordinary duplication is deliberately unsupported: on successful
    // duplication the original capability is invalidated, and the duplicate
    // never enters the table.  Hook NtDuplicateObject around this pair.
    DuplicateTicket prepare_duplicate(HANDLE source_process, HANDLE source_handle) {
        if (GetProcessId(source_process) != GetCurrentProcessId()) return {};
        return find_ticket<DuplicateTicket>(source_handle);
    }
    void complete_duplicate(const DuplicateTicket& ticket, NTSTATUS status) {
        if (status >= 0 && ticket.generation) remove_and_close(ticket.public_handle, ticket.generation);
    }

    // Returns kForward without inspecting caller buffers unless this is a
    // current capability and the exact synchronous IOCTL shape is present.
    IoctlDisposition device_io(HANDLE handle, HANDLE event, PVOID apc,
                               PVOID apc_context, PIO_STATUS_BLOCK io_status,
                               ULONG control_code, PVOID input, ULONG input_length,
                               PVOID output, ULONG output_length, NTSTATUS* status) {
        if (!status || control_code != kQueryPoints || event || apc || apc_context ||
            !current_capability(handle)) return IoctlDisposition::kForward;
        *status = emulate(io_status, input, input_length, output, output_length);
        return IoctlDisposition::kHandled;
    }

private:
    struct Capability {
        HANDLE public_handle = INVALID_HANDLE_VALUE;
        HANDLE identity = INVALID_HANDLE_VALUE;
        ULONGLONG generation = 0;
    };

    Api api_ = {};
    wchar_t maps_[26][MAX_PATH] = {};
    Capability capabilities_[kMaxCapabilities] = {};
    SRWLOCK lock_ = SRWLOCK_INIT;
    ULONGLONG next_generation_ = 1;
    bool initialized_ = false;

    static bool equal_folded(const wchar_t* left, const wchar_t* right, size_t length) {
        for (size_t i = 0; i < length; ++i) {
            wchar_t a = left[i], b = right[i];
            if (a >= L'A' && a <= L'Z') a = wchar_t(a + (L'a' - L'A'));
            if (b >= L'A' && b <= L'Z') b = wchar_t(b + (L'a' - L'A'));
            if (a != b) return false;
        }
        return true;
    }

    static bool is_local_device(const wchar_t* value, size_t length) {
        static constexpr wchar_t prefix[] = L"\\Device\\";
        static constexpr wchar_t mup[] = L"\\Device\\Mup";
        const size_t mup_length = _countof(mup) - 1;
        // Redirected drives name a subpath, not a local volume device.
        for (size_t i = _countof(prefix) - 1; i < length; ++i)
            if (value[i] == L'\\') return false;
        return length > _countof(prefix) - 1 &&
               equal_folded(value, prefix, _countof(prefix) - 1) &&
               !(length >= mup_length && equal_folded(value, mup, mup_length) &&
                 (length == mup_length || value[mup_length] == L'\\'));
    }

    static bool safe_mount_manager_name(POBJECT_ATTRIBUTES attributes) {
        static constexpr wchar_t expected[] = L"\\??\\MountPointManager";
        __try {
            if (!attributes || attributes->Length != sizeof(OBJECT_ATTRIBUTES) ||
                attributes->RootDirectory || attributes->Attributes ||
                attributes->SecurityDescriptor || attributes->SecurityQualityOfService ||
                !attributes->ObjectName || !attributes->ObjectName->Buffer ||
                attributes->ObjectName->Length != sizeof(expected) - sizeof(wchar_t)) return false;
            if (attributes->ObjectName->MaximumLength < attributes->ObjectName->Length) return false;
            const wchar_t* value = attributes->ObjectName->Buffer;
            return equal_folded(value, expected, _countof(expected) - 1);
        } __except (EXCEPTION_EXECUTE_HANDLER) {
            return false;
        }
    }

    static bool safe_store_open(PHANDLE output_handle, PIO_STATUS_BLOCK io_status, HANDLE handle) {
        __try {
            if (!output_handle || !io_status) return false;
            *output_handle = handle;
            io_status->Status = kStatusSuccess;
            io_status->Information = FILE_OPENED;
            return true;
        } __except (EXCEPTION_EXECUTE_HANDLER) {
            return false;
        }
    }

    static NTSTATUS finish(PIO_STATUS_BLOCK io_status, NTSTATUS status, ULONG_PTR information) {
        __try {
            if (!io_status) return kStatusAccessViolation;
            io_status->Status = status;
            io_status->Information = information;
            return status;
        } __except (EXCEPTION_EXECUTE_HANDLER) {
            return kStatusAccessViolation;
        }
    }

    bool insert(HANDLE public_handle, HANDLE identity, ULONGLONG* generation) {
        HANDLE stale = INVALID_HANDLE_VALUE;
        AcquireSRWLockExclusive(&lock_);
        Capability* empty = nullptr;
        for (auto& capability : capabilities_) {
            if (capability.public_handle == public_handle) {
                // A raw close can leave a stale table entry.  A new real handle
                // with that number is never accepted until identity proves it.
                if (api_.compare_object_handles(public_handle, capability.identity)) {
                    ReleaseSRWLockExclusive(&lock_);
                    return false;
                }
                stale = capability.identity;
                capability = {};
            }
            if (capability.public_handle == INVALID_HANDLE_VALUE && !empty) empty = &capability;
        }
        if (empty) {
            empty->public_handle = public_handle;
            empty->identity = identity;
            empty->generation = next_generation_++;
            if (!next_generation_) ++next_generation_;
            *generation = empty->generation;
        }
        ReleaseSRWLockExclusive(&lock_);
        if (stale != INVALID_HANDLE_VALUE) api_.close(stale);
        return empty != nullptr;
    }

    HANDLE remove(HANDLE public_handle, ULONGLONG generation) {
        HANDLE identity = INVALID_HANDLE_VALUE;
        AcquireSRWLockExclusive(&lock_);
        for (auto& capability : capabilities_) {
            if (capability.public_handle == public_handle && capability.generation == generation) {
                identity = capability.identity;
                capability = {};
                break;
            }
        }
        ReleaseSRWLockExclusive(&lock_);
        return identity;
    }

    void remove_and_close(HANDLE public_handle, ULONGLONG generation) {
        HANDLE identity = remove(public_handle, generation);
        if (identity != INVALID_HANDLE_VALUE) api_.close(identity);
    }

    bool current_capability(HANDLE handle) {
        HANDLE identity = INVALID_HANDLE_VALUE;
        ULONGLONG generation = 0;
        AcquireSRWLockShared(&lock_);
        for (const auto& capability : capabilities_) {
            if (capability.public_handle == handle) {
                identity = capability.identity;
                generation = capability.generation;
                break;
            }
        }
        bool current = identity != INVALID_HANDLE_VALUE && api_.compare_object_handles(handle, identity);
        ReleaseSRWLockShared(&lock_);
        if (current || !generation) return current;
        // An unhooked raw close followed by numeric reuse reached a former
        // table slot.  Retire it before allowing the unknown call to forward.
        remove_and_close(handle, generation);
        return false;
    }

    template <typename Ticket>
    Ticket find_ticket(HANDLE handle) {
        Ticket ticket = {};
        AcquireSRWLockShared(&lock_);
        for (const auto& capability : capabilities_) {
            if (capability.public_handle == handle &&
                api_.compare_object_handles(handle, capability.identity)) {
                ticket.public_handle = handle;
                ticket.generation = capability.generation;
                break;
            }
        }
        ReleaseSRWLockShared(&lock_);
        return ticket;
    }

    bool map_device(const wchar_t* value, size_t length, size_t* drive) const {
        for (size_t i = 0; i < 26; ++i) {
            size_t map_length = 0;
            while (map_length < MAX_PATH && maps_[i][map_length]) ++map_length;
            if (map_length == length && map_length && equal_folded(value, maps_[i], length)) {
                *drive = i;
                return true;
            }
        }
        return false;
    }

    NTSTATUS emulate(PIO_STATUS_BLOCK io_status, PVOID input, ULONG input_length,
                     PVOID output, ULONG output_length) {
        MountPoint selector = {};
        wchar_t device[MAX_PATH] = {};
        size_t device_length = 0;
        NTSTATUS selector_status = safe_selector(input, input_length, &selector, device, &device_length);
        if (selector_status != kStatusSuccess) return finish(io_status, selector_status, 0);
        size_t drive = 0;
        if (!map_device(device, device_length, &drive))
            return finish(io_status, kStatusInvalidParameter, 0);
        wchar_t alias[] = {L'\\', L'D', L'o', L's', L'D', L'e', L'v', L'i', L'c', L'e', L's', L'\\', wchar_t(L'A' + drive), L':'};
        const size_t device_bytes = wcslen(maps_[drive]) * sizeof(wchar_t);
        const ULONG response_size = ULONG(sizeof(MountPointsHeader) + sizeof(alias) + device_bytes);
        if (output_length < sizeof(MountPointsHeader))
            return finish(io_status, kStatusInvalidParameter, 0);
        if (output_length < response_size) {
            if (!safe_overflow_header(output, response_size)) return finish(io_status, kStatusAccessViolation, 0);
            return finish(io_status, kStatusBufferOverflow, 0);
        }
        if (!safe_response(output, response_size, alias, sizeof(alias), maps_[drive], device_bytes))
            return finish(io_status, kStatusAccessViolation, 0);
        return finish(io_status, kStatusSuccess, response_size);
    }

    static NTSTATUS safe_selector(PVOID input, ULONG input_length, MountPoint* selector,
                                  wchar_t device[MAX_PATH], size_t* device_length) {
        __try {
            if (!input || input_length < sizeof(MountPoint)) return kStatusInvalidParameter;
            memcpy(selector, input, sizeof(*selector));
            if (selector->reserved1 || selector->reserved2 || selector->reserved3 ||
                selector->symbolic_link_length || selector->symbolic_link_offset ||
                selector->unique_id_length || selector->unique_id_offset ||
                !selector->device_name_length || !selector->device_name_offset ||
                (selector->device_name_length & 1) || (selector->device_name_offset & 1) ||
                selector->device_name_offset < sizeof(MountPoint) ||
                selector->device_name_offset > input_length ||
                selector->device_name_length > input_length - selector->device_name_offset ||
                selector->device_name_length >= MAX_PATH * sizeof(wchar_t)) return kStatusInvalidParameter;
            uintptr_t base = reinterpret_cast<uintptr_t>(input);
            if (base > ~uintptr_t(0) - selector->device_name_offset) return kStatusInvalidParameter;
            uintptr_t address = base + selector->device_name_offset;
            if (address & 1) return kStatusInvalidParameter;
            memcpy(device, reinterpret_cast<const void*>(address), selector->device_name_length);
            *device_length = selector->device_name_length / sizeof(wchar_t);
            device[*device_length] = L'\0';
            return kStatusSuccess;
        } __except (EXCEPTION_EXECUTE_HANDLER) {
            return kStatusAccessViolation;
        }
    }

    static bool safe_overflow_header(PVOID output, ULONG size) {
        __try {
            auto* header = static_cast<MountPointsHeader*>(output);
            memset(header, 0, sizeof(*header));
            header->size = size;
            return true;
        } __except (EXCEPTION_EXECUTE_HANDLER) {
            return false;
        }
    }

    static bool safe_response(PVOID output, ULONG size, const wchar_t* alias, size_t alias_size,
                              const wchar_t* device, size_t device_size) {
        __try {
            auto* header = static_cast<MountPointsHeader*>(output);
            memset(header, 0, size);
            header->size = size;
            header->number_of_mount_points = 1;
            header->mount_points[0].symbolic_link_offset = sizeof(MountPointsHeader);
            header->mount_points[0].symbolic_link_length = static_cast<USHORT>(alias_size);
            header->mount_points[0].device_name_offset = sizeof(MountPointsHeader) + static_cast<ULONG>(alias_size);
            header->mount_points[0].device_name_length = static_cast<USHORT>(device_size);
            memcpy(reinterpret_cast<BYTE*>(output) + sizeof(MountPointsHeader), alias, alias_size);
            memcpy(reinterpret_cast<BYTE*>(output) + header->mount_points[0].device_name_offset, device, device_size);
            return true;
        } __except (EXCEPTION_EXECUTE_HANDLER) {
            return false;
        }
    }
};

}  // namespace nub_sandbox::mount_query
