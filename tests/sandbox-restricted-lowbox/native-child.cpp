#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <winternl.h>
#include <stddef.h>

extern "C" __declspec(dllimport) NTSTATUS NTAPI NtWriteFile(
    HANDLE, HANDLE, PIO_APC_ROUTINE, PVOID, PIO_STATUS_BLOCK, PVOID, ULONG, PLARGE_INTEGER, PULONG);
extern "C" __declspec(dllimport) NTSTATUS NTAPI NtTerminateProcess(HANDLE, NTSTATUS);

// This prefix is identical on native x64 and ARM64. No CRT or Win32 imports.
struct Parameters {
    ULONG maximum_length, length, flags, debug_flags;
    HANDLE console;
    ULONG console_flags;
    HANDLE input, output, error;
};
static_assert(offsetof(Parameters, output) == 0x28);

extern "C" void entry() {
    auto parameters = reinterpret_cast<Parameters*>(NtCurrentTeb()->ProcessEnvironmentBlock->ProcessParameters);
    static char marker[] = "NATIVE_READY\n";
    IO_STATUS_BLOCK ios{};
    NTSTATUS status = NtWriteFile(parameters->output, nullptr, nullptr, nullptr, &ios,
        marker, sizeof(marker) - 1, nullptr, nullptr);
    NtTerminateProcess(reinterpret_cast<HANDLE>(static_cast<LONG_PTR>(-1)), status);
}
