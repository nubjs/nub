#include "common.h"

int wmain(int argc, wchar_t** argv) {
    if (argc != 3) return 91;
    wprintf(L"READY label=%ls pid=%lu\n", argv[2], GetCurrentProcessId());
    fflush(stdout);
    HANDLE token = nullptr;
    require(OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &token), L"child token");
    token_facts(token, argv[2]);
    CloseHandle(token);
    native_opens(argv[1], argv[2]);
    wprintf(L"CHILD_COMPLETE label=%ls\n", argv[2]);
    return 0;
}
