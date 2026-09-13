// Disposable native loader fixture, built by the Windows acceptance harness.
// cl /LD /MT file_broker_fixture.c /link /OUT:file-broker-fixture.dll
#define WIN32_LEAN_AND_MEAN
#include <windows.h>

static DWORD attached;
BOOL WINAPI DllMain(HINSTANCE module, DWORD reason, LPVOID reserved) {
    (void)module;
    (void)reserved;
    if (reason == DLL_PROCESS_ATTACH) ++attached;
    return TRUE;
}
__declspec(dllexport) DWORD SandboxFileBrokerValue(void) {
    return attached == 1 ? 1138 : 0;
}
