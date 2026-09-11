// Read-only observer for the Git 2.55.0(5) x64 MSYS runtime. Include in the
// diagnostic DLL after diagnostic() is defined; this does not alter the runtime,
// loader, or process policy. Do not compile as a separate translation unit.
#include <windows.h>
#include <winternl.h>
#include <cstring>
#include <cstdint>

namespace msys_mapping {
using ReadMemory = NTSTATUS (NTAPI*)(HANDLE, const void*, void*, SIZE_T, SIZE_T*);
struct Notification {
    ULONG flags;
    const UNICODE_STRING* full_name;
    const UNICODE_STRING* base_name;
    PVOID base;
    ULONG size;
};
using Callback = void (CALLBACK*)(ULONG, const Notification*, void*);
using Register = NTSTATUS (NTAPI*)(ULONG, Callback, void*, void**);
using Unregister = NTSTATUS (NTAPI*)(void*);
static ReadMemory read_memory = nullptr;
static Unregister unregister = nullptr;
static void* cookie = nullptr;
static volatile LONG64 image = 0;
static volatile LONG64 initial_caps = 0;
static volatile LONG matched = 0;
static volatile LONG initial_read_status = 0;
static DWORD aslr = 0;
static DWORD aslr_error = 0;

// Release and SDK DLL SHA256 e55be2d89f2756f540a8f2acf18cd14bf275d1187a32d5189946ee5c896bcf03.
// Matching debuglink CRC32 8717764e: wincap.caps RVA 34f198,
// wincap_11 RVA 2bd9c0, wincap_10_2004 RVA 2bd9c8.
static constexpr SIZE_T caps_rva = 0x34f198;
static bool read(uintptr_t address, void* destination, SIZE_T size, NTSTATUS& status) {
    SIZE_T copied = 0;
    status = read_memory(reinterpret_cast<HANDLE>(static_cast<LONG_PTR>(-1)),
        reinterpret_cast<void*>(address), destination, size, &copied);
    return status >= 0 && copied == size;
}

static void observe(PVOID address) {
    uintptr_t base = reinterpret_cast<uintptr_t>(address);
    IMAGE_DOS_HEADER dos;
    IMAGE_NT_HEADERS64 nt;
    BYTE instruction[4];
    NTSTATUS status = 0;
    bool exact = read(base, &dos, sizeof(dos), status) &&
        dos.e_magic == IMAGE_DOS_SIGNATURE && dos.e_lfanew > 0 && dos.e_lfanew < 4096 &&
        read(base + dos.e_lfanew, &nt, sizeof(nt), status) &&
        nt.Signature == IMAGE_NT_SIGNATURE && nt.FileHeader.Machine == IMAGE_FILE_MACHINE_AMD64 &&
        nt.FileHeader.TimeDateStamp == 0x6a687710 && nt.OptionalHeader.SizeOfImage == 0x362000 &&
        nt.OptionalHeader.CheckSum == 0x34082d &&
        read(base + 0x1f4dca, instruction, sizeof(instruction), status) &&
        instruction[0] == 0xf6 && instruction[1] == 0x42 && instruction[2] == 1 && instruction[3] == 8;
    uintptr_t caps = 0;
    if (exact) read(base + caps_rva, &caps, sizeof(caps), status);
    InterlockedExchange64(&initial_caps, static_cast<LONG64>(caps));
    InterlockedExchange(&initial_read_status, status);
    InterlockedExchange(&matched, exact);
    InterlockedExchange64(&image, static_cast<LONG64>(base));
}

static void CALLBACK loaded(ULONG reason, const Notification* data, void*) {
    // Loader callbacks must not call into other DLLs. Only the pre-resolved
    // ntdll memory-read routine and compiler interlocked intrinsics are used.
    static constexpr wchar_t expected[] = L"msys-2.0.dll";
    if (!data || !data->base_name || !data->base_name->Buffer ||
        data->base_name->Length != sizeof(expected) - sizeof(wchar_t)) return;
    for (unsigned i = 0; i < (sizeof(expected) / sizeof(wchar_t)) - 1; ++i) {
        wchar_t c = data->base_name->Buffer[i];
        if (c >= L'A' && c <= L'Z') c += L'a' - L'A';
        if (c != expected[i]) return;
    }
    if (reason == 1) observe(data->base);
    else if (reason == 2) InterlockedCompareExchange64(&image, 0, reinterpret_cast<LONG64>(data->base));
}

static void snapshot(const char* stage) {
    DWORD error = GetLastError();
    uintptr_t base = static_cast<uintptr_t>(InterlockedCompareExchange64(&image, 0, 0));
    bool exact = InterlockedCompareExchange(&matched, 0, 0) != 0;
    uintptr_t caps = 0;
    NTSTATUS status = 0;
    bool ok = base && exact && read(base + caps_rva, &caps, sizeof(caps), status);
    IMAGE_DOS_HEADER dos = {};
    IMAGE_NT_HEADERS64 nt = {};
    NTSTATUS header_status = 0;
    if (base && read(base, &dos, sizeof(dos), header_status) && dos.e_lfanew > 0 && dos.e_lfanew < 4096)
        read(base + dos.e_lfanew, &nt, sizeof(nt), header_status);
    if (base && !exact)
        diagnostic("MSYS_MAPPING_RVA_SKIPPED pid=%lu reason=runtime-mismatch machine=%04x timestamp=%08lx size=%08lx checksum=%08lx\n",
            GetCurrentProcessId(), nt.FileHeader.Machine, nt.FileHeader.TimeDateStamp,
            nt.OptionalHeader.SizeOfImage, nt.OptionalHeader.CheckSum);
    diagnostic(
        "MSYS_MAPPING stage=%s pid=%lu base=%llx preferred=%llx matched=%d initial_caps=%llx initial_status=%08lx caps=%llx caps_read=%d status=%08lx expected_server_caps=%llx expected_win11_caps=%llx aslr=%08lx aslr_error=%lu\n",
        stage, GetCurrentProcessId(), static_cast<unsigned long long>(base), nt.OptionalHeader.ImageBase, exact,
        static_cast<unsigned long long>(InterlockedCompareExchange64(&initial_caps, 0, 0)),
        static_cast<ULONG>(InterlockedCompareExchange(&initial_read_status, 0, 0)),
        static_cast<unsigned long long>(caps), ok, static_cast<ULONG>(status),
        static_cast<unsigned long long>(base + 0x2bd9c8), static_cast<unsigned long long>(base + 0x2bd9c0), aslr, aslr_error);
    // Region metadata can distinguish an occupied preferred base or a stale
    // cross-mapping pointer without reading or changing either allocation.
    const uintptr_t addresses[] = { static_cast<uintptr_t>(nt.OptionalHeader.ImageBase), caps };
    for (unsigned i = 0; i < 2; ++i) {
        if (!addresses[i]) continue;
        MEMORY_BASIC_INFORMATION region = {};
        SIZE_T result = VirtualQuery(reinterpret_cast<void*>(addresses[i]), &region, sizeof(region));
        DWORD query_error = result ? 0 : GetLastError();
        diagnostic("MSYS_MAPPING_REGION pid=%lu kind=%s address=%llx result=%llu allocation=%p region=%p size=%llx state=%08lx type=%08lx protection=%08lx error=%lu\n",
            GetCurrentProcessId(), i ? "caps" : "preferred", static_cast<unsigned long long>(addresses[i]),
            static_cast<unsigned long long>(result), region.AllocationBase, region.BaseAddress,
            static_cast<unsigned long long>(region.RegionSize), region.State, region.Type, region.Protect, query_error);
    }
    SetLastError(error);
}

template<typename T> static T resolve(HMODULE module, const char* name) {
    FARPROC address = GetProcAddress(module, name);
    T function = nullptr;
    static_assert(sizeof(function) == sizeof(address));
    std::memcpy(&function, &address, sizeof(function));
    return function;
}

static void start() {
    DWORD error = GetLastError();
    auto ntdll = GetModuleHandleW(L"ntdll.dll");
    read_memory = resolve<ReadMemory>(ntdll, "NtReadVirtualMemory");
    auto reg = resolve<Register>(ntdll, "LdrRegisterDllNotification");
    unregister = resolve<Unregister>(ntdll, "LdrUnregisterDllNotification");
    if (!GetProcessMitigationPolicy(GetCurrentProcess(), ProcessASLRPolicy, &aslr, sizeof(aslr)))
        aslr_error = GetLastError();
    if (read_memory && reg && unregister) {
        NTSTATUS status = reg(0, loaded, nullptr, &cookie);
        diagnostic("MSYS_MAPPING_REGISTER pid=%lu status=%08lx\n", GetCurrentProcessId(), static_cast<ULONG>(status));
        // Imports can already be mapped before this DLL's attach runs. Capture
        // that case as well as later loads, without loading MSYS ourselves.
        if (auto existing = GetModuleHandleW(L"msys-2.0.dll")) observe(existing);
        snapshot("attach");
    } else {
        diagnostic("MSYS_MAPPING_REGISTER pid=%lu unavailable=1\n", GetCurrentProcessId());
    }
    SetLastError(error);
}

static void stop() {
    DWORD error = GetLastError();
    if (cookie) unregister(cookie);
    cookie = nullptr;
    SetLastError(error);
}
} // namespace msys_mapping
