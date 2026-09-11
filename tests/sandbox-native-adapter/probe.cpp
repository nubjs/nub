// Native compatibility feasibility probe. AppContainer remains the security boundary.
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <winternl.h>
#include <securityappcontainer.h>
#include <cstdio>
#include <cstdlib>
#include <cwchar>
#include <cstring>
#include <cstdint>
#include <cstdarg>
#include <intrin.h>
#include <initializer_list>
#include "detours.h"

static const GUID payload_id = {0x19c47458, 0xe2ad, 0x421d, {0x81, 0x37, 0x52, 0xa1, 0x85, 0xf7, 0xb8, 0x15}};
struct Payload {
    HANDLE null_device;
    char directory[MAX_PATH];
    wchar_t devices[26][MAX_PATH];
    DWORD user_sid[SECURITY_MAX_SID_SIZE / sizeof(DWORD)];
    DWORD package_sid[SECURITY_MAX_SID_SIZE / sizeof(DWORD)];
    BOOL identities_captured;
    BOOL directory_read_probe;
    BOOL mount_query_probe;
};
static Payload state = {};

static bool capture_identities(HANDLE process, Payload& payload) {
    HANDLE token = nullptr;
    if (!OpenProcessToken(process, TOKEN_QUERY, &token)) return false;
    alignas(void*) BYTE user[512], package[512];
    DWORD needed = 0;
    BOOL ok = GetTokenInformation(token, TokenUser, user, sizeof(user), &needed) &&
              GetTokenInformation(token, TokenAppContainerSid, package, sizeof(package), &needed);
    DWORD error = GetLastError();
    CloseHandle(token);
    if (!ok) { SetLastError(error); return false; }
    auto package_sid = reinterpret_cast<TOKEN_APPCONTAINER_INFORMATION*>(package)->TokenAppContainer;
    if (!package_sid) { SetLastError(ERROR_INVALID_SID); return false; }
    if (!CopySid(sizeof(payload.user_sid), payload.user_sid, reinterpret_cast<TOKEN_USER*>(user)->User.Sid) ||
        !CopySid(sizeof(payload.package_sid), payload.package_sid, package_sid)) return false;
    payload.identities_captured = TRUE;
    return true;
}

static USHORT image_machine(HANDLE process) {
    // Suspended x64 processes on ARM64 can report native-machine via IsWow64Process2.
    // Read the mapped executable header, not the image path (which can be replaced).
    uintptr_t address = 0;
    MEMORY_BASIC_INFORMATION region;
    while (VirtualQueryEx(process, reinterpret_cast<void*>(address), &region, sizeof(region))) {
        auto base = static_cast<const BYTE*>(region.BaseAddress);
        if (region.Type == MEM_IMAGE && region.State == MEM_COMMIT &&
            region.AllocationBase == region.BaseAddress && !(region.Protect & (PAGE_GUARD | PAGE_NOACCESS))) {
            IMAGE_DOS_HEADER dos;
            IMAGE_NT_HEADERS32 pe;
            if (ReadProcessMemory(process, base, &dos, sizeof(dos), nullptr) &&
                dos.e_magic == IMAGE_DOS_SIGNATURE && dos.e_lfanew >= static_cast<LONG>(sizeof(dos)) &&
                static_cast<SIZE_T>(dos.e_lfanew) < region.RegionSize &&
                ReadProcessMemory(process, base + dos.e_lfanew, &pe, sizeof(pe), nullptr) &&
                pe.Signature == IMAGE_NT_SIGNATURE && !(pe.FileHeader.Characteristics & IMAGE_FILE_DLL)) {
                return pe.FileHeader.Machine;
            }
        }
        uintptr_t next = reinterpret_cast<uintptr_t>(base) + region.RegionSize;
        if (next <= address) break;
        address = next;
    }
    return 0;
}

static BOOL inject(HANDLE process, const Payload& source) {
    Payload target = source;
    // CreateProcessW descendants keep this user/package identity even when a
    // runtime replaces its token DACL and can no longer query its own token.
    if (!target.identities_captured && !capture_identities(process, target)) return FALSE;
    USHORT reported = 0, native_machine = 0;
    IsWow64Process2(process, &reported, &native_machine);
    USHORT machine = image_machine(process);
    fprintf(stderr, "ADAPTER_MACHINE image=%04x wow=%04x native=%04x\n", machine, reported, native_machine);
    const char* arch = machine == IMAGE_FILE_MACHINE_AMD64 ? "x64" :
                       machine == IMAGE_FILE_MACHINE_ARM64 ? "arm64" : nullptr;
    if (!arch) { SetLastError(ERROR_EXE_MACHINE_TYPE_MISMATCH); return FALSE; }
    char path[MAX_PATH];
    if (sprintf_s(path, "%s\\probe-%s.dll", source.directory, arch) < 0) return FALSE;
    if (GetFileAttributesA(path) == INVALID_FILE_ATTRIBUTES) {
        DWORD error = GetLastError();
        fprintf(stderr, "ADAPTER_INJECT_FAILED attributes path=%s error=%lu\n", path, error);
        SetLastError(error); return FALSE;
    }
    if (!DuplicateHandle(GetCurrentProcess(), source.null_device, process,
                         &target.null_device, 0, FALSE, DUPLICATE_SAME_ACCESS)) {
        DWORD error = GetLastError();
        fprintf(stderr, "ADAPTER_INJECT_FAILED duplicate-null handle=%p error=%lu\n", source.null_device, error);
        SetLastError(error); return FALSE;
    }
    const char* dll = path;
    BOOL copied = DetourCopyPayloadToProcess(process, payload_id, &target, sizeof(target));
    BOOL updated = copied && DetourUpdateProcessWithDll(process, &dll, 1);
    DWORD error = GetLastError();
    fprintf(stderr, "ADAPTER_INJECT_RESULT child=%lu copied=%d updated=%d error=%lu\n", GetProcessId(process), copied, updated, error);
    SetLastError(error);
    return updated;
}

#ifdef PROBE_INJECTOR
int wmain(int argc, wchar_t** argv) {
    if (argc != 3) return 2;
    state.directory_read_probe = GetEnvironmentVariableW(L"NUB_NATIVE_DIRECTORY_MASK_PROBE", nullptr, 0) != 0;
    state.mount_query_probe = GetEnvironmentVariableW(L"NUB_NATIVE_MOUNT_QUERY_PROBE", nullptr, 0) != 0;
    DWORD pid = wcstoul(argv[1], nullptr, 10);
    HANDLE process = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_OPERATION |
                                 PROCESS_VM_READ | PROCESS_VM_WRITE | PROCESS_DUP_HANDLE,
                                 FALSE, pid);
    if (!process) { fprintf(stderr, "OpenProcess: %lu\n", GetLastError()); return 3; }
    state.null_device = CreateFileW(L"NUL", GENERIC_READ | GENERIC_WRITE,
                                   FILE_SHARE_READ | FILE_SHARE_WRITE, nullptr,
                                   OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, nullptr);
    if (state.null_device == INVALID_HANDLE_VALUE) return 4;
    BOOL substituted = FALSE;
    if (!WideCharToMultiByte(CP_ACP, WC_NO_BEST_FIT_CHARS, argv[2], -1,
                             state.directory, MAX_PATH, nullptr, &substituted) || substituted) return 5;
    for (int i = 0; i < 26; ++i) {
        wchar_t drive[] = {wchar_t(L'A' + i), L':', 0};
        QueryDosDeviceW(drive, state.devices[i], MAX_PATH);
    }
    BOOL ok = inject(process, state);
    DWORD error = GetLastError();
    CloseHandle(state.null_device);
    CloseHandle(process);
    fprintf(stderr, "INJECTION %s error=%lu\n", ok ? "ok" : "failed", error);
    return ok ? 0 : 6;
}
#else
// Startup can fail before the CRT's stderr stream or MSYS is initialized.
// One bounded raw write also avoids recursively entering the file hooks.
static volatile LONG diagnostic_active = 0;
static PVOID exception_handler = nullptr;
static void diagnostic(const char* format, ...) {
    DWORD error = GetLastError();
    if (InterlockedCompareExchange(&diagnostic_active, 1, 0) == 0) {
        char line[4096];
        va_list args;
        va_start(args, format);
        _vsnprintf_s(line, sizeof(line), _TRUNCATE, format, args);
        va_end(args);
        DWORD written = 0;
        WriteFile(GetStdHandle(STD_ERROR_HANDLE), line, DWORD(strlen(line)), &written, nullptr);
        InterlockedExchange(&diagnostic_active, 0);
    }
    SetLastError(error);
}

#include "msys-mapping.cpp"

static LONG CALLBACK startup_exception(EXCEPTION_POINTERS* fault) {
    DWORD error = GetLastError();
    static volatile LONG active = 0;
    static volatile LONG count = 0;
    if (InterlockedCompareExchange(&active, 1, 0) != 0)
        return EXCEPTION_CONTINUE_SEARCH;
    if (InterlockedIncrement(&count) <= 16) {
        msys_mapping::snapshot("exception");
        auto record = fault->ExceptionRecord;
        MEMORY_BASIC_INFORMATION region = {};
        VirtualQuery(record->ExceptionAddress, &region, sizeof(region));
        char module[MAX_PATH] = {};
        if (region.Type == MEM_IMAGE)
            GetModuleFileNameA(static_cast<HMODULE>(region.AllocationBase), module, MAX_PATH);
        diagnostic("ADAPTER_STARTUP_EXCEPTION pid=%lu code=%08lx address=%p module=%s base=%p offset=%llx info0=%llx info1=%llx\n",
            GetCurrentProcessId(), record->ExceptionCode, record->ExceptionAddress, module,
            region.AllocationBase,
            static_cast<unsigned long long>(reinterpret_cast<uintptr_t>(record->ExceptionAddress) -
                                            reinterpret_cast<uintptr_t>(region.AllocationBase)),
            static_cast<unsigned long long>(record->NumberParameters > 0 ? record->ExceptionInformation[0] : 0),
            static_cast<unsigned long long>(record->NumberParameters > 1 ? record->ExceptionInformation[1] : 0));
    }
    InterlockedExchange(&active, 0);
    SetLastError(error);
    return EXCEPTION_CONTINUE_SEARCH;
}

using NtTerminate = NTSTATUS (NTAPI*)(HANDLE, NTSTATUS);
static NtTerminate true_terminate_process = nullptr;
static auto true_exit_code = GetExitCodeProcess;
static auto true_virtual_alloc = VirtualAlloc;

static NTSTATUS NTAPI terminate_process(HANDLE process, NTSTATUS status) {
    DWORD error = GetLastError();
    msys_mapping::snapshot("terminate");
    DWORD pid = GetProcessId(process);
    diagnostic("ADAPTER_TERMINATE pid=%lu target=%lu handle=%p status=%08lx caller=%p\n",
        GetCurrentProcessId(), pid, process, static_cast<ULONG>(status), _ReturnAddress());
    SetLastError(error);
    return true_terminate_process(process, status);
}

static BOOL WINAPI exit_code(HANDLE process, LPDWORD code) {
    BOOL ok = true_exit_code(process, code);
    DWORD error = GetLastError();
    if (ok && *code != STILL_ACTIVE)
        diagnostic("ADAPTER_CHILD_EXIT pid=%lu child=%lu status=%08lx\n",
            GetCurrentProcessId(), GetProcessId(process), *code);
    SetLastError(error);
    return ok;
}

static LPVOID WINAPI virtual_alloc(LPVOID address, SIZE_T size, DWORD kind, DWORD protection) {
    LPVOID result = true_virtual_alloc(address, size, kind, protection);
    DWORD error = GetLastError();
    uintptr_t base = reinterpret_cast<uintptr_t>(address);
    // MSYS/Cygwin reserve this fixed arena before their ordinary startup traces.
    const uintptr_t low = 0x800000000ULL, high = 0xa00000000ULL;
    if (!result && address && base < high && (base >= low || size > low - base))
        diagnostic("ADAPTER_FIXED_ALLOCATION_FAILED pid=%lu address=%p size=%llx kind=%08lx protection=%08lx error=%lu caller=%p\n",
            GetCurrentProcessId(), address, static_cast<unsigned long long>(size), kind, protection,
            error, _ReturnAddress());
    SetLastError(error);
    return result;
}

static void device_failure_trace(ACCESS_MASK access, NTSTATUS status, POBJECT_ATTRIBUTES attrs) {
    static constexpr wchar_t device[] = L"\\??\\MountPointManager";
    if (access != SYNCHRONIZE || status != static_cast<NTSTATUS>(0xc0000022L) ||
        !attrs || !attrs->ObjectName || !attrs->ObjectName->Buffer ||
        attrs->ObjectName->Length != sizeof(device) - sizeof(wchar_t) ||
        _wcsnicmp(attrs->ObjectName->Buffer, device, _countof(device) - 1)) return;
    DWORD error = GetLastError();
    void* frames[16] = {};
    USHORT count = CaptureStackBackTrace(0, _countof(frames), frames, nullptr);
    for (USHORT i = 0; i < count; ++i) {
        MEMORY_BASIC_INFORMATION region = {};
        VirtualQuery(frames[i], &region, sizeof(region));
        char module[MAX_PATH] = {};
        if (region.Type == MEM_IMAGE)
            GetModuleFileNameA(static_cast<HMODULE>(region.AllocationBase), module, MAX_PATH);
        diagnostic("ADAPTER_DEVICE_CALLER pid=%lu frame=%hu module=%s offset=%llx\n",
            GetCurrentProcessId(), i, module,
            static_cast<unsigned long long>(reinterpret_cast<uintptr_t>(frames[i]) -
                                            reinterpret_cast<uintptr_t>(region.AllocationBase)));
    }
    SetLastError(error);
}

static auto true_create_file = CreateFileW;
static auto true_create_file_a = CreateFileA;
static auto true_final_path = GetFinalPathNameByHandleW;
static auto true_create_process = CreateProcessW;
static auto true_anonymous_pipe = CreatePipe;
static void diagnostic_state(const char* stage) {
    DWORD error = GetLastError();
    DWORD null_type = GetFileType(state.null_device);
    DWORD null_error = GetLastError();
    diagnostic("ADAPTER_PROCESS_STATE stage=%s pid=%lu state=%p null=%p null_type=%lu null_error=%lu create_process=%p create_file=%p\n",
        stage, GetCurrentProcessId(), &state, state.null_device, null_type, null_error,
        reinterpret_cast<void*>(true_create_process), reinterpret_cast<void*>(true_create_file));
    SetLastError(error);
}
static SECURITY_DESCRIPTOR private_descriptor;
alignas(ACL) static BYTE private_acl[512];

static bool initialize_private_security() {
    if (!state.identities_captured) return false;
    auto acl = reinterpret_cast<PACL>(private_acl);
    return InitializeSecurityDescriptor(&private_descriptor, SECURITY_DESCRIPTOR_REVISION) &&
           InitializeAcl(acl, sizeof(private_acl), ACL_REVISION) &&
           AddAccessAllowedAce(acl, ACL_REVISION, GENERIC_ALL, state.user_sid) &&
           AddAccessAllowedAce(acl, ACL_REVISION, GENERIC_ALL, state.package_sid) &&
           SetSecurityDescriptorDacl(&private_descriptor, TRUE, acl, FALSE);
}
// MSYS installs a user-only default DACL and process DACL. Preserve its ACEs,
// but keep this package able to use its own objects and reopen child processes.
class PackageAcl {
    PACL value_ = nullptr;
public:
    explicit PackageAcl(PACL original) {
        ACL_SIZE_INFORMATION info = {};
        if (!original || !GetAclInformation(original, &info, sizeof(info), AclSizeInformation)) return;
        DWORD bytes = info.AclBytesInUse + DWORD(sizeof(ACCESS_ALLOWED_ACE) - sizeof(DWORD)) +
                      GetLengthSid(state.package_sid);
        if (bytes > MAXWORD) return;
        auto acl = static_cast<PACL>(LocalAlloc(LPTR, bytes));
        if (!acl) return;
        memcpy(acl, original, info.AclBytesInUse);
        acl->AclSize = static_cast<WORD>(bytes);
        if (!AddAccessAllowedAce(acl, acl->AclRevision, GENERIC_ALL, state.package_sid)) {
            LocalFree(acl);
            return;
        }
        value_ = acl;
    }
    ~PackageAcl() { if (value_) LocalFree(value_); }
    PackageAcl(const PackageAcl&) = delete;
    PackageAcl& operator=(const PackageAcl&) = delete;
    PACL get() const { return value_; }
};
using NtToken = NTSTATUS (NTAPI*)(HANDLE, TOKEN_INFORMATION_CLASS, PVOID, ULONG);
using NtSecurity = NTSTATUS (NTAPI*)(HANDLE, SECURITY_INFORMATION, PSECURITY_DESCRIPTOR);
static NtToken true_set_token = nullptr;
static NtSecurity true_set_security = nullptr;

static NTSTATUS NTAPI set_token(HANDLE token, TOKEN_INFORMATION_CLASS kind, PVOID data, ULONG length) {
    if (kind != TokenDefaultDacl || !data || length != sizeof(TOKEN_DEFAULT_DACL))
        return true_set_token(token, kind, data, length);
    // Only adapt tokens belonging to the current package, never an unrelated token.
    alignas(void*) BYTE info[512];
    DWORD needed = 0;
    if (!GetTokenInformation(token, TokenAppContainerSid, info, sizeof(info), &needed))
        return true_set_token(token, kind, data, length);
    auto sid = reinterpret_cast<TOKEN_APPCONTAINER_INFORMATION*>(info)->TokenAppContainer;
    if (!sid || !EqualSid(sid, state.package_sid))
        return true_set_token(token, kind, data, length);
    auto original = static_cast<TOKEN_DEFAULT_DACL*>(data);
    if (!original->DefaultDacl) return true_set_token(token, kind, data, length);
    PackageAcl acl(original->DefaultDacl);
    if (!acl.get()) return static_cast<NTSTATUS>(0xc0000017L);
    TOKEN_DEFAULT_DACL adapted = {acl.get()};
    return true_set_token(token, kind, &adapted, sizeof(adapted));
}

static NTSTATUS NTAPI set_security(HANDLE handle, SECURITY_INFORMATION kind, PSECURITY_DESCRIPTOR descriptor) {
    if (kind != DACL_SECURITY_INFORMATION || GetProcessId(handle) != GetCurrentProcessId())
        return true_set_security(handle, kind, descriptor);
    PACL original = nullptr;
    BOOL present = FALSE, defaulted = FALSE;
    SECURITY_DESCRIPTOR_CONTROL control = 0;
    DWORD revision = 0;
    if (!GetSecurityDescriptorDacl(descriptor, &present, &original, &defaulted) ||
        !GetSecurityDescriptorControl(descriptor, &control, &revision) || !present || !original)
        return true_set_security(handle, kind, descriptor);
    PackageAcl acl(original);
    if (!acl.get()) return static_cast<NTSTATUS>(0xc0000017L);
    SECURITY_DESCRIPTOR adapted;
    const SECURITY_DESCRIPTOR_CONTROL flags =
        SE_DACL_PROTECTED | SE_DACL_AUTO_INHERIT_REQ | SE_DACL_AUTO_INHERITED;
    if (!InitializeSecurityDescriptor(&adapted, SECURITY_DESCRIPTOR_REVISION) ||
        !SetSecurityDescriptorDacl(&adapted, TRUE, acl.get(), defaulted) ||
        !SetSecurityDescriptorControl(&adapted, flags, static_cast<SECURITY_DESCRIPTOR_CONTROL>(control & flags)))
        return static_cast<NTSTATUS>(0xc0000079L);
    return true_set_security(handle, kind, &adapted);
}
using NtDirectory = NTSTATUS (NTAPI*)(PHANDLE, ACCESS_MASK, POBJECT_ATTRIBUTES);
static BOOL WINAPI anonymous_pipe(PHANDLE read, PHANDLE write, LPSECURITY_ATTRIBUTES security, DWORD size) {
    if (true_anonymous_pipe(read, write, security, size)) return TRUE;
    if (GetLastError() != ERROR_ACCESS_DENIED) return FALSE;
    // CreatePipe's internal name is not package-local. Keep the anonymous
    // stream contract, but create both ends in LOCAL with the package ACL.
    static volatile LONG serial = 0;
    wchar_t path[160];
    if (swprintf_s(path, L"\\\\.\\pipe\\LOCAL\\sandbox-anonymous-%lu-%lu",
                   GetCurrentProcessId(), static_cast<ULONG>(InterlockedIncrement(&serial))) < 0) return FALSE;
    SECURITY_ATTRIBUTES attrs = {sizeof(attrs), &private_descriptor, security && security->bInheritHandle};
    HANDLE input = CreateNamedPipeW(path, PIPE_ACCESS_INBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE,
        PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS, 1, size, size, 0, &attrs);
    if (input == INVALID_HANDLE_VALUE) return FALSE;
    HANDLE output = true_create_file(path, GENERIC_WRITE | FILE_READ_ATTRIBUTES, 0, &attrs, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, nullptr);
    if (output == INVALID_HANDLE_VALUE) {
        DWORD error = GetLastError();
        CloseHandle(input);
        SetLastError(error);
        return FALSE;
    }
    *read = input;
    *write = output;
    return TRUE;
}
static NtDirectory true_create_directory = nullptr;
static NtDirectory true_open_directory = nullptr;
using NtPipe = NTSTATUS (NTAPI*)(PHANDLE, ACCESS_MASK, POBJECT_ATTRIBUTES, PIO_STATUS_BLOCK,
    ULONG, ULONG, ULONG, ULONG, ULONG, ULONG, ULONG, ULONG, ULONG, PLARGE_INTEGER);
using NtOpen = NTSTATUS (NTAPI*)(PHANDLE, ACCESS_MASK, POBJECT_ATTRIBUTES, PIO_STATUS_BLOCK, ULONG, ULONG);
using NtCreate = NTSTATUS (NTAPI*)(PHANDLE, ACCESS_MASK, POBJECT_ATTRIBUTES, PIO_STATUS_BLOCK,
    PLARGE_INTEGER, ULONG, ULONG, ULONG, ULONG, PVOID, ULONG);
using NtObject = NTSTATUS (NTAPI*)(HANDLE, ULONG, PVOID, ULONG, PULONG);
static NtPipe true_create_pipe = nullptr;
static NtOpen true_open_file = nullptr;
static NtCreate true_nt_create_file = nullptr;
static NtObject query_object = nullptr;

// Diagnostic only: no real device handle is lent to the child. Zig's direct
// mount-point query can be answered from the drive map already in the payload.
static HANDLE mount_probe_handle() { return reinterpret_cast<HANDLE>(static_cast<INT_PTR>(-0x4e5542)); }
struct MountPoint {
    ULONG symbolic_offset;
    USHORT symbolic_length, reserved1;
    ULONG unique_offset;
    USHORT unique_length, reserved2;
    ULONG device_offset;
    USHORT device_length, reserved3;
};
static_assert(sizeof(MountPoint) == 24);
using NtIoControl = NTSTATUS (NTAPI*)(HANDLE, HANDLE, PVOID, PVOID, PIO_STATUS_BLOCK,
    ULONG, PVOID, ULONG, PVOID, ULONG);
using NtCloseFn = NTSTATUS (NTAPI*)(HANDLE);
static NtIoControl true_io_control = nullptr;
static NtCloseFn true_close = nullptr;

static bool mount_query_open(PHANDLE handle, ACCESS_MASK access, POBJECT_ATTRIBUTES attrs,
                             PIO_STATUS_BLOCK io, NTSTATUS status) {
    static constexpr wchar_t device[] = L"\\??\\MountPointManager";
    if (!state.mount_query_probe || status != static_cast<NTSTATUS>(0xc0000022L) ||
        access != SYNCHRONIZE || !handle || !io || !attrs || attrs->RootDirectory ||
        !attrs->ObjectName || !attrs->ObjectName->Buffer ||
        attrs->ObjectName->Length != sizeof(device) - sizeof(wchar_t) ||
        _wcsnicmp(attrs->ObjectName->Buffer, device, _countof(device) - 1)) return false;
    *handle = mount_probe_handle();
    io->Status = 0;
    io->Information = FILE_OPENED;
    return true;
}

static NTSTATUS NTAPI mount_io_control(HANDLE handle, HANDLE event, PVOID apc, PVOID context,
    PIO_STATUS_BLOCK io, ULONG code, PVOID input, ULONG input_size, PVOID output, ULONG output_size) {
    if (!state.mount_query_probe || handle != mount_probe_handle())
        return true_io_control(handle, event, apc, context, io, code, input, input_size, output, output_size);
    auto finish = [&](NTSTATUS status, ULONG bytes = 0) {
        if (io) { io->Status = status; io->Information = bytes; }
        diagnostic("ADAPTER_MOUNT_QUERY pid=%lu code=%08lx status=%08lx bytes=%lu\n",
            GetCurrentProcessId(), code, static_cast<ULONG>(status), bytes);
        return status;
    };
    const NTSTATUS invalid = static_cast<NTSTATUS>(0xc000000dL);
    if (!io || event || apc || context || code != 0x006d0008 || !input || input_size < sizeof(MountPoint))
        return finish(invalid);
    MountPoint point;
    memcpy(&point, input, sizeof(point));
    if (point.symbolic_length || point.unique_length || !point.device_length ||
        point.device_length % sizeof(wchar_t) || point.device_offset < sizeof(point) ||
        point.device_offset > input_size || point.device_length > input_size - point.device_offset ||
        point.device_length >= MAX_PATH * sizeof(wchar_t)) return finish(invalid);
    wchar_t device[MAX_PATH] = {};
    memcpy(device, static_cast<BYTE*>(input) + point.device_offset, point.device_length);
    int drive = -1;
    for (int i = 0; i < 26; ++i)
        if (!_wcsicmp(device, state.devices[i])) { drive = i; break; }
    if (drive < 0) return finish(static_cast<NTSTATUS>(0xc0000034L));
    wchar_t link[] = L"\\DosDevices\\C:";
    link[_countof(link) - 3] = wchar_t(L'A' + drive);
    constexpr ULONG link_bytes = sizeof(link) - sizeof(wchar_t);
    const ULONG required = 8 + sizeof(MountPoint) + link_bytes + point.device_length;
    if (!output || output_size < required) return finish(static_cast<NTSTATUS>(0xc0000023L));
    MountPoint result = {};
    result.symbolic_offset = 8 + sizeof(MountPoint);
    result.symbolic_length = link_bytes;
    result.device_offset = result.symbolic_offset + link_bytes;
    result.device_length = point.device_length;
    const ULONG header[] = {required, 1};
    memcpy(output, header, sizeof(header));
    memcpy(static_cast<BYTE*>(output) + sizeof(header), &result, sizeof(result));
    memcpy(static_cast<BYTE*>(output) + result.symbolic_offset, link, link_bytes);
    memcpy(static_cast<BYTE*>(output) + result.device_offset, device, result.device_length);
    return finish(0, required);
}

static NTSTATUS NTAPI mount_close(HANDLE handle) {
    return state.mount_query_probe && handle == mount_probe_handle() ? 0 : true_close(handle);
}

static bool pipe_name(POBJECT_ATTRIBUTES original, OBJECT_ATTRIBUTES& redirected,
                      UNICODE_STRING& name, wchar_t (&path)[1024]) {
    if (!original || !original->ObjectName) return false;
    auto input = original->ObjectName;
    if (!input->Buffer || input->Length % sizeof(wchar_t) || input->Length >= 512 * sizeof(wchar_t)) return false;
    wchar_t source[512];
    memcpy(source, input->Buffer, input->Length);
    source[input->Length / sizeof(wchar_t)] = 0;
    const wchar_t* leaf = nullptr;
    if (original->RootDirectory) {
        alignas(void*) BYTE info[4096];
        ULONG size = 0;
        if (query_object(original->RootDirectory, 1, info, sizeof(info), &size) < 0) return false;
        auto root = reinterpret_cast<UNICODE_STRING*>(info);
        const wchar_t npfs[] = L"\\Device\\NamedPipe";
        if (root->Length != sizeof(npfs) - sizeof(wchar_t) ||
            _wcsnicmp(root->Buffer, npfs, root->Length / sizeof(wchar_t))) return false;
        leaf = source;
    } else {
        for (const wchar_t* prefix : {L"\\Device\\NamedPipe\\", L"\\??\\pipe\\"}) {
            if (!_wcsnicmp(source, prefix, wcslen(prefix))) { leaf = source + wcslen(prefix); break; }
        }
    }
    if (!leaf || (wcsncmp(leaf, L"msys-", 5) && wcsncmp(leaf, L"cygwin-", 7) &&
                  wcsncmp(leaf, L"uv\\", 3) && !(original->RootDirectory && wcsstr(leaf, L"-pipe-nt-")))) return false;
    if (swprintf_s(path, L"\\??\\pipe\\LOCAL\\%s", leaf) < 0) return false;
    name.Buffer = path;
    name.Length = USHORT(wcslen(path) * sizeof(wchar_t));
    name.MaximumLength = USHORT(name.Length + sizeof(wchar_t));
    redirected = *original;
    redirected.RootDirectory = nullptr;
    redirected.ObjectName = &name;
    // MSYS overwrites TokenDefaultDacl with a user-only ACL during startup.
    // Retain both the user and this package for NPFS's two access checks.
    redirected.SecurityDescriptor = &private_descriptor;
    return true;
}

static NTSTATUS NTAPI create_pipe(PHANDLE handle, ACCESS_MASK access, POBJECT_ATTRIBUTES attrs,
    PIO_STATUS_BLOCK io, ULONG share, ULONG disposition, ULONG options, ULONG type, ULONG read_mode,
    ULONG completion, ULONG instances, ULONG inbound, ULONG outbound, PLARGE_INTEGER timeout) {
    OBJECT_ATTRIBUTES redirected;
    UNICODE_STRING name;
    wchar_t path[1024];
    bool mapped = pipe_name(attrs, redirected, name, path);
    NTSTATUS status = true_create_pipe(handle, access, mapped ? &redirected : attrs, io, share,
        disposition, options, type, read_mode, completion, instances, inbound, outbound, timeout);
    if (mapped) fprintf(stderr, "ADAPTER_PIPE_CREATE path=%ls status=%08lx\n", path, static_cast<unsigned long>(status));
    return status;
}

static NTSTATUS NTAPI open_file(PHANDLE handle, ACCESS_MASK access, POBJECT_ATTRIBUTES attrs,
                                PIO_STATUS_BLOCK io, ULONG share, ULONG options) {
    OBJECT_ATTRIBUTES redirected;
    UNICODE_STRING name;
    wchar_t path[1024];
    bool mapped = pipe_name(attrs, redirected, name, path);
    NTSTATUS status = true_open_file(handle, access, mapped ? &redirected : attrs, io, share, options);
    device_failure_trace(access, status, attrs);
    if (mount_query_open(handle, access, attrs, io, status)) return 0;
    if (state.directory_read_probe && status == static_cast<NTSTATUS>(0xc0000022L) &&
        (options & FILE_DIRECTORY_FILE) && access == 0x001200a9) {
        // Diagnostic only: Bun 1.3 requests READ_CONTROL and FILE_READ_EA in
        // addition to the listing/traverse rights available on ancestors.
        status = true_open_file(handle, 0x001000a1, mapped ? &redirected : attrs, io, share, options);
        fprintf(stderr, "ADAPTER_DIRECTORY_MASK original=%08lx reduced=001000a1 status=%08lx\n", access, static_cast<unsigned long>(status));
    }
    if (mapped) fprintf(stderr, "ADAPTER_PIPE_OPEN path=%ls status=%08lx\n", path, static_cast<unsigned long>(status));
    if (status == static_cast<NTSTATUS>(0xc0000022L) && attrs && attrs->ObjectName)
        fprintf(stderr, "ADAPTER_FILE_DENIED pid=%lu access=%08lx root=%p path=%.*ls\n", GetCurrentProcessId(), access, attrs->RootDirectory, int(attrs->ObjectName->Length / sizeof(wchar_t)), attrs->ObjectName->Buffer);
    return status;
}

static NTSTATUS NTAPI nt_create_file(PHANDLE handle, ACCESS_MASK access, POBJECT_ATTRIBUTES attrs,
    PIO_STATUS_BLOCK io, PLARGE_INTEGER allocation, ULONG attributes, ULONG share, ULONG disposition,
    ULONG options, PVOID ea, ULONG ea_length) {
    OBJECT_ATTRIBUTES redirected;
    UNICODE_STRING name;
    wchar_t path[1024];
    bool mapped = pipe_name(attrs, redirected, name, path);
    NTSTATUS status = true_nt_create_file(handle, access, mapped ? &redirected : attrs, io, allocation,
        attributes, share, disposition, options, ea, ea_length);
    device_failure_trace(access, status, attrs);
    if (disposition == FILE_OPEN && mount_query_open(handle, access, attrs, io, status)) return 0;
    if (state.directory_read_probe && status == static_cast<NTSTATUS>(0xc0000022L) &&
        (options & FILE_DIRECTORY_FILE) && disposition == FILE_OPEN && access == 0x001200a9) {
        status = true_nt_create_file(handle, 0x001000a1, mapped ? &redirected : attrs, io, allocation,
            attributes, share, disposition, options, ea, ea_length);
        fprintf(stderr, "ADAPTER_DIRECTORY_MASK original=%08lx reduced=001000a1 status=%08lx\n", access, static_cast<unsigned long>(status));
    }
    if (mapped) fprintf(stderr, "ADAPTER_PIPE_CLIENT path=%ls status=%08lx\n", path, static_cast<unsigned long>(status));
    if (status == static_cast<NTSTATUS>(0xc0000022L) && attrs && attrs->ObjectName)
        fprintf(stderr, "ADAPTER_FILE_DENIED pid=%lu access=%08lx root=%p path=%.*ls\n", GetCurrentProcessId(), access, attrs->RootDirectory, int(attrs->ObjectName->Length / sizeof(wchar_t)), attrs->ObjectName->Buffer);
    return status;
}

static bool msys_directory(POBJECT_ATTRIBUTES original, OBJECT_ATTRIBUTES& redirected,
                           UNICODE_STRING& name, wchar_t (&path)[1024]) {
    if (!original || original->RootDirectory || !original->ObjectName) return false;
    auto input = original->ObjectName;
    if (!input->Buffer || input->Length % sizeof(wchar_t) || input->Length >= 512 * sizeof(wchar_t)) return false;
    wchar_t source[512];
    memcpy(source, input->Buffer, input->Length);
    source[input->Length / sizeof(wchar_t)] = 0;
    const wchar_t* leaf = nullptr;
    const wchar_t global[] = L"\\BaseNamedObjects\\";
    const wchar_t session[] = L"\\Sessions\\BNOLINKS\\";
    if (!_wcsnicmp(source, global, wcslen(global))) leaf = source + wcslen(global);
    else if (!_wcsnicmp(source, session, wcslen(session))) {
        auto cursor = source + wcslen(session);
        auto start = cursor;
        while (*cursor >= L'0' && *cursor <= L'9') ++cursor;
        if (cursor != start && *cursor == L'\\') leaf = cursor + 1;
    }
    if (!leaf || (wcsncmp(leaf, L"msys-", 5) && wcsncmp(leaf, L"cygwin-", 7)) ||
        wcschr(leaf, L'\\') || wcschr(leaf, L'/')) return false;
    ULONG length = 0;
    if (!GetAppContainerNamedObjectPath(nullptr, nullptr, 1024, path, &length)) return false;
    if (path[0] != L'\\') {
        // The Win32 API returns a path relative to this session's BaseNamedObjects.
        wchar_t relative[1024];
        wcscpy_s(relative, path);
        DWORD session_id = 0;
        if (!ProcessIdToSessionId(GetCurrentProcessId(), &session_id)) return false;
        if (swprintf_s(path, L"\\Sessions\\%lu\\BaseNamedObjects\\%s", session_id, relative) < 0) return false;
    }
    size_t end = wcslen(path);
    if ((end && path[end - 1] != L'\\' && wcscat_s(path, L"\\")) || wcscat_s(path, leaf)) return false;
    name.Buffer = path;
    name.Length = USHORT(wcslen(path) * sizeof(wchar_t));
    name.MaximumLength = USHORT(name.Length + sizeof(wchar_t));
    redirected = *original;
    redirected.ObjectName = &name;
    // Keep shared runtime objects inside this package, using its token's default DACL.
    redirected.SecurityDescriptor = nullptr;
    return true;
}

static NTSTATUS NTAPI create_directory(PHANDLE handle, ACCESS_MASK access, POBJECT_ATTRIBUTES attrs) {
    OBJECT_ATTRIBUTES redirected;
    UNICODE_STRING name;
    wchar_t path[1024];
    if (!msys_directory(attrs, redirected, name, path)) return true_create_directory(handle, access, attrs);
    NTSTATUS status = true_create_directory(handle, access, &redirected);
    fprintf(stderr, "ADAPTER_MSYS_DIRECTORY path=%ls status=%08lx\n", path, static_cast<unsigned long>(status));
    return status;
}

static NTSTATUS NTAPI open_directory(PHANDLE handle, ACCESS_MASK access, POBJECT_ATTRIBUTES attrs) {
    OBJECT_ATTRIBUTES redirected;
    UNICODE_STRING name;
    wchar_t path[1024];
    return true_open_directory(handle, access,
                               msys_directory(attrs, redirected, name, path) ? &redirected : attrs);
}

static bool is_null(LPCWSTR path) {
    if (!path) return false;
    if (!_wcsicmp(path, L"\\\\.\\NUL") || !_wcsicmp(path, L"\\\\?\\NUL")) return true;
    // PHP expands its NUL descriptor to an absolute DOS path before opening it.
    // Let Windows classify reserved DOS device names rather than matching file suffixes.
    using DosDeviceName = ULONG (NTAPI*)(PCWSTR);
    auto address = GetProcAddress(GetModuleHandleW(L"ntdll.dll"), "RtlIsDosDeviceName_U");
    DosDeviceName classify;
    memcpy(&classify, &address, sizeof(address));
    ULONG part = classify ? classify(path) : 0;
    return part && LOWORD(part) == 3 * sizeof(wchar_t) &&
           !_wcsnicmp(path + HIWORD(part) / sizeof(wchar_t), L"NUL", 3);
}

static HANDLE WINAPI create_file(LPCWSTR path, DWORD access, DWORD share,
                                LPSECURITY_ATTRIBUTES security, DWORD disposition,
                                DWORD flags, HANDLE template_file) {
    HANDLE result = true_create_file(path, access, share, security, disposition, flags, template_file);
    if (result == INVALID_HANDLE_VALUE && path && (wcsstr(path, L"NUL") || wcsstr(path, L"nul"))) {
        DWORD error = GetLastError();
        fprintf(stderr, "ADAPTER_NUL path=%ls access=%08lx flags=%08lx error=%lu recognized=%d\n", path, access, flags, error, is_null(path));
        SetLastError(error);
    }
    if (result != INVALID_HANDLE_VALUE || GetLastError() != ERROR_ACCESS_DENIED ||
        !is_null(path) || (flags & FILE_FLAG_OVERLAPPED)) return result;
    // Duplicate a parent-opened real null device, never a regular-file approximation.
    HANDLE duplicate = INVALID_HANDLE_VALUE;
    if (!DuplicateHandle(GetCurrentProcess(), state.null_device, GetCurrentProcess(),
                         &duplicate, access, security && security->bInheritHandle, 0)) {
        return INVALID_HANDLE_VALUE;
    }
    return duplicate;
}

static HANDLE WINAPI create_file_a(LPCSTR path, DWORD access, DWORD share,
                                   LPSECURITY_ATTRIBUTES security, DWORD disposition,
                                   DWORD flags, HANDLE template_file) {
    HANDLE result = true_create_file_a(path, access, share, security, disposition, flags, template_file);
    if (result != INVALID_HANDLE_VALUE || GetLastError() != ERROR_ACCESS_DENIED || !path) return result;
    if (_stricmp(path, "NUL") && _stricmp(path, "NUL:") &&
        _stricmp(path, "\\\\.\\NUL") && _stricmp(path, "\\\\?\\NUL")) return result;
    return create_file(L"NUL", access, share, security, disposition, flags, template_file);
}

static DWORD WINAPI final_path(HANDLE file, LPWSTR buffer, DWORD size, DWORD flags) {
    DWORD result = true_final_path(file, buffer, size, flags);
    DWORD error = GetLastError();
    if (!result) fprintf(stderr, "ADAPTER_FINAL_PATH handle=%p flags=%08lx size=%lu error=%lu\n", file, flags, size, error);
    SetLastError(error);
    if (result || error != ERROR_ACCESS_DENIED || (flags & 7) != VOLUME_NAME_DOS) return result;
    wchar_t native[32768];
    DWORD length = true_final_path(file, native, 32768, flags | VOLUME_NAME_NT);
    error = GetLastError();
    fprintf(stderr, "ADAPTER_FINAL_PATH_NT handle=%p length=%lu error=%lu path=%.*ls\n", file, length, error, int(length < 32768 ? length : 0), native);
    SetLastError(error);
    if (!length || length >= 32768) return 0;
    for (int i = 0; i < 26; ++i) {
        size_t prefix = wcslen(state.devices[i]);
        if (!prefix || prefix > length || _wcsnicmp(native, state.devices[i], prefix) ||
            (native[prefix] && native[prefix] != L'\\')) continue;
        // Preserve the normalized/opened suffix and use a host-resolved drive alias.
        DWORD needed = DWORD(6 + length - prefix);
        if (size <= needed) return needed + 1;
        const wchar_t start[] = {L'\\', L'\\', L'?', L'\\', wchar_t(L'A' + i), L':', 0};
        wcscpy_s(buffer, size, start);
        wcscat_s(buffer, size, native + prefix);
        return needed;
    }
    SetLastError(ERROR_ACCESS_DENIED);
    return 0;
}

static BOOL WINAPI create_process(LPCWSTR application, LPWSTR command,
                                  LPSECURITY_ATTRIBUTES process_attrs,
                                  LPSECURITY_ATTRIBUTES thread_attrs, BOOL inherit,
                                  DWORD flags, LPVOID environment, LPCWSTR cwd,
                                  LPSTARTUPINFOW startup, LPPROCESS_INFORMATION child) {
    diagnostic_state("create-process");
    msys_mapping::snapshot("create-process");
    diagnostic("ADAPTER_CREATE_PROCESS pid=%lu application=%ls command=%ls flags=%08lx process_attrs=%p thread_attrs=%p inherit=%d reserved_size=%u\n",
        GetCurrentProcessId(), application ? application : L"(null)", command ? command : L"(null)",
        flags, process_attrs, thread_attrs, inherit, startup ? startup->cbReserved2 : 0);
    if (!true_create_process(application, command, process_attrs, thread_attrs, inherit,
                             flags | CREATE_SUSPENDED, environment, cwd, startup, child)) return FALSE;
    if (!inject(child->hProcess, state)) {
        DWORD error = GetLastError();
        TerminateProcess(child->hProcess, 127);
        WaitForSingleObject(child->hProcess, INFINITE);
        CloseHandle(child->hThread);
        CloseHandle(child->hProcess);
        SetLastError(error);
        return FALSE;
    }
    if (!(flags & CREATE_SUSPENDED) && ResumeThread(child->hThread) == DWORD(-1)) {
        DWORD error = GetLastError();
        TerminateProcess(child->hProcess, 127);
        WaitForSingleObject(child->hProcess, INFINITE);
        CloseHandle(child->hThread);
        CloseHandle(child->hProcess);
        SetLastError(error);
        return FALSE;
    }
    return TRUE;
}

extern "C" __declspec(dllexport) void ProbeMarker() {}
template<typename T> static bool resolve_nt(T& function, const char* name) {
    auto address = GetProcAddress(GetModuleHandleW(L"ntdll.dll"), name);
    static_assert(sizeof(function) == sizeof(address));
    memcpy(&function, &address, sizeof(address));
    return address != nullptr;
}
BOOL WINAPI DllMain(HINSTANCE, DWORD reason, LPVOID) {
    if (DetourIsHelperProcess()) return TRUE;
    if (reason == DLL_PROCESS_DETACH) msys_mapping::stop();
    if (reason == DLL_PROCESS_DETACH && exception_handler) {
        RemoveVectoredExceptionHandler(exception_handler);
        exception_handler = nullptr;
    }
    if (reason != DLL_PROCESS_ATTACH) return TRUE;
    diagnostic("ADAPTER_ATTACH_BEGIN pid=%lu\n", GetCurrentProcessId());
    msys_mapping::start();
    exception_handler = AddVectoredExceptionHandler(1, startup_exception);
    if (!exception_handler)
        diagnostic("ADAPTER_DIAGNOSTIC_FAILED operation=exception-handler error=%lu\n", GetLastError());
    auto failed_attach = []() -> BOOL {
        msys_mapping::stop();
        if (exception_handler) RemoveVectoredExceptionHandler(exception_handler);
        exception_handler = nullptr;
        return FALSE;
    };
    BOOL restored = DetourRestoreAfterWith();
    diagnostic("ADAPTER_RESTORE pid=%lu restored=%d error=%lu\n", GetCurrentProcessId(), restored, GetLastError());
    DWORD size = 0;
    auto payload = static_cast<Payload*>(DetourFindPayloadEx(payload_id, &size));
    if (!payload || size != sizeof(Payload)) {
        diagnostic("ADAPTER_ATTACH_FAILED payload pid=%lu pointer=%p size=%lu expected=%zu error=%lu\n",
            GetCurrentProcessId(), payload, size, sizeof(Payload), GetLastError());
        return failed_attach();
    }
    state = *payload;
    if (!initialize_private_security()) {
        DWORD error = GetLastError();
        fprintf(stderr, "ADAPTER_ATTACH_FAILED private-security pid=%lu error=%lu\n", GetCurrentProcessId(), error);
        return failed_attach();
    }
    if (!resolve_nt(true_terminate_process, "NtTerminateProcess") ||
        !resolve_nt(true_set_token, "NtSetInformationToken") ||
        !resolve_nt(true_set_security, "NtSetSecurityObject") ||
        !resolve_nt(true_create_directory, "NtCreateDirectoryObject") ||
        !resolve_nt(true_open_directory, "NtOpenDirectoryObject") ||
        !resolve_nt(true_create_pipe, "NtCreateNamedPipeFile") ||
        !resolve_nt(true_open_file, "NtOpenFile") ||
        !resolve_nt(true_nt_create_file, "NtCreateFile") ||
        !resolve_nt(true_io_control, "NtDeviceIoControlFile") ||
        !resolve_nt(true_close, "NtClose") ||
        !resolve_nt(query_object, "NtQueryObject")) {
        diagnostic("ADAPTER_ATTACH_FAILED resolve pid=%lu error=%lu\n", GetCurrentProcessId(), GetLastError());
        return failed_attach();
    }
    DetourTransactionBegin();
    DetourUpdateThread(GetCurrentThread());
    DetourAttach(reinterpret_cast<PVOID*>(&true_terminate_process), terminate_process);
    DetourAttach(reinterpret_cast<PVOID*>(&true_exit_code), exit_code);
    DetourAttach(reinterpret_cast<PVOID*>(&true_virtual_alloc), virtual_alloc);
    DetourAttach(reinterpret_cast<PVOID*>(&true_set_token), set_token);
    DetourAttach(reinterpret_cast<PVOID*>(&true_set_security), set_security);
    DetourAttach(reinterpret_cast<PVOID*>(&true_create_file), create_file);
    DetourAttach(reinterpret_cast<PVOID*>(&true_create_file_a), create_file_a);
    DetourAttach(reinterpret_cast<PVOID*>(&true_final_path), final_path);
    DetourAttach(reinterpret_cast<PVOID*>(&true_create_process), create_process);
    DetourAttach(reinterpret_cast<PVOID*>(&true_anonymous_pipe), anonymous_pipe);
    DetourAttach(reinterpret_cast<PVOID*>(&true_create_directory), create_directory);
    DetourAttach(reinterpret_cast<PVOID*>(&true_open_directory), open_directory);
    DetourAttach(reinterpret_cast<PVOID*>(&true_create_pipe), create_pipe);
    DetourAttach(reinterpret_cast<PVOID*>(&true_open_file), open_file);
    DetourAttach(reinterpret_cast<PVOID*>(&true_nt_create_file), nt_create_file);
    DetourAttach(reinterpret_cast<PVOID*>(&true_io_control), mount_io_control);
    DetourAttach(reinterpret_cast<PVOID*>(&true_close), mount_close);
    LONG result = DetourTransactionCommit();
    diagnostic("ADAPTER_ATTACH_RESULT pid=%lu result=%ld\n", GetCurrentProcessId(), result);
    if (result == NO_ERROR) diagnostic_state("attached");
    return result == NO_ERROR ? TRUE : failed_attach();
}
#endif
