#pragma once
#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <windows.h>
#include <winternl.h>
#include <sddl.h>
#include <aclapi.h>
#include <cstdio>
#include <cstring>
#include <string>
#include <vector>

inline void require(bool ok, const wchar_t* operation) {
    if (!ok) {
        wprintf(L"FATAL operation=%ls error=%lu\n", operation, GetLastError());
        fflush(stdout);
        ExitProcess(90);
    }
}

inline std::vector<BYTE> info(HANDLE token, TOKEN_INFORMATION_CLASS kind) {
    DWORD size = 0;
    GetTokenInformation(token, kind, nullptr, 0, &size);
    std::vector<BYTE> data(size);
    require(size && GetTokenInformation(token, kind, data.data(), size, &size), L"GetTokenInformation");
    return data;
}

inline std::wstring sid_text(PSID sid) {
    LPWSTR text = nullptr;
    require(ConvertSidToStringSidW(sid, &text), L"ConvertSidToStringSidW");
    std::wstring result(text);
    LocalFree(text);
    return result;
}

inline void token_facts(HANDLE token, const wchar_t* label) {
    auto user = info(token, TokenUser);
    auto elevation = info(token, TokenElevation);
    auto il = info(token, TokenIntegrityLevel);
    auto ac = info(token, TokenIsAppContainer);
    wprintf(L"TOKEN label=%ls user=%ls elevated=%lu appcontainer=%lu restricted=%d il=%ls\n",
            label, sid_text(reinterpret_cast<TOKEN_USER*>(user.data())->User.Sid).c_str(),
            reinterpret_cast<TOKEN_ELEVATION*>(elevation.data())->TokenIsElevated,
            *reinterpret_cast<DWORD*>(ac.data()), IsTokenRestricted(token),
            sid_text(reinterpret_cast<TOKEN_MANDATORY_LABEL*>(il.data())->Label.Sid).c_str());
    for (auto kind : {TokenRestrictedSids, TokenCapabilities, TokenGroups}) {
        auto data = info(token, kind);
        auto groups = reinterpret_cast<TOKEN_GROUPS*>(data.data());
        wprintf(L"SIDS label=%ls class=%u count=%lu\n", label, kind, groups->GroupCount);
        for (DWORD i = 0; i < groups->GroupCount; ++i)
            wprintf(L"SID label=%ls class=%u sid=%ls attributes=%08lx\n", label, kind,
                    sid_text(groups->Groups[i].Sid).c_str(), groups->Groups[i].Attributes);
    }
    auto privileges = info(token, TokenPrivileges);
    auto list = reinterpret_cast<TOKEN_PRIVILEGES*>(privileges.data());
    for (DWORD i = 0; i < list->PrivilegeCount; ++i) {
        wprintf(L"PRIVILEGE label=%ls luid=%08lx:%08lx attributes=%08lx\n", label,
                list->Privileges[i].Luid.HighPart, list->Privileges[i].Luid.LowPart, list->Privileges[i].Attributes);
    }
    if (*reinterpret_cast<DWORD*>(ac.data())) {
        auto package = info(token, TokenAppContainerSid);
        wprintf(L"PACKAGE label=%ls sid=%ls\n", label,
                sid_text(reinterpret_cast<TOKEN_APPCONTAINER_INFORMATION*>(package.data())->TokenAppContainer).c_str());
    }
    DWORD lpac = 0, size = 0;
    BOOL got = GetTokenInformation(token, TokenIsLessPrivilegedAppContainer, &lpac, sizeof(lpac), &size);
    wprintf(L"LPAC label=%ls query=%d value=%lu error=%lu\n", label, got, lpac, got ? 0 : GetLastError());
    fflush(stdout);
}

inline const wchar_t* files[] = {L"normal.txt", L"aap.txt", L"arap.txt", L"cap.txt", L"package.txt",
    L"r.txt", L"aap-r.txt", L"arap-r.txt", L"cap-r.txt", L"package-r.txt", L"null.txt",
    L"allowed-looking.txt", L"child.exe", L"bootstrap-alias.exe", L"native-child.exe"};

using NtOpenFileFn = NTSTATUS(NTAPI*)(PHANDLE, ACCESS_MASK, POBJECT_ATTRIBUTES, PIO_STATUS_BLOCK, ULONG, ULONG);

inline void native_opens(const std::wstring& root, const wchar_t* label, bool system = false) {
    auto open = reinterpret_cast<NtOpenFileFn>(GetProcAddress(GetModuleHandleW(L"ntdll.dll"), "NtOpenFile"));
    require(open != nullptr, L"NtOpenFile export");
    const wchar_t* dlls[] = {L"ntdll.dll", L"kernel32.dll", L"KernelBase.dll", L"advapi32.dll"};
    std::vector<const wchar_t*> targets;
    if (system) targets.assign(std::begin(dlls), std::end(dlls));
    else targets.assign(std::begin(files), std::end(files));
    for (auto file : targets) {
        std::wstring name = L"\\??\\" + root + L"\\" + file;
        UNICODE_STRING path{};
        path.Buffer = name.data();
        path.Length = static_cast<USHORT>(name.size() * sizeof(wchar_t));
        path.MaximumLength = path.Length;
        OBJECT_ATTRIBUTES oa{};
        oa.Length = sizeof(oa); oa.ObjectName = &path; oa.Attributes = 0x40;
        for (ACCESS_MASK access : {FILE_GENERIC_READ, FILE_GENERIC_WRITE}) {
            HANDLE handle = nullptr; IO_STATUS_BLOCK ios{};
            NTSTATUS status = open(&handle, access, &oa, &ios, 7, 0x60);
            wprintf(L"OPEN label=%ls file=%ls desired=%08lx share=7 options=00000060 status=%08lx",
                    label, file, access, static_cast<ULONG>(status));
            if (status >= 0) {
                if (access == FILE_GENERIC_READ) {
                    char bytes[16]{}; DWORD read = 0;
                    BOOL ok = ReadFile(handle, bytes, sizeof(bytes), &read, nullptr);
                    wprintf(L" read_ok=%d bytes=%lu first=%02x", ok, read, static_cast<unsigned char>(bytes[0]));
                }
                CloseHandle(handle);
            }
            wprintf(L"\n");
        }
    }
    fflush(stdout);
}
