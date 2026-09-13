#include "common.h"
#include <userenv.h>

using CreateAppContainerTokenFn = BOOL(WINAPI*)(HANDLE, PSECURITY_CAPABILITIES, PHANDLE);
using NtCreateLowBoxTokenFn = NTSTATUS(NTAPI*)(PHANDLE, HANDLE, ACCESS_MASK, POBJECT_ATTRIBUTES,
    PSID, ULONG, PSID_AND_ATTRIBUTES, ULONG, PHANDLE);

void acl(const std::wstring& path, const std::wstring& sddl) {
    PSECURITY_DESCRIPTOR sd = nullptr;
    require(ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.c_str(), SDDL_REVISION_1, &sd, nullptr), L"parse SD");
    PACL dacl = nullptr, sacl = nullptr; BOOL present, def;
    require(GetSecurityDescriptorDacl(sd, &present, &dacl, &def), L"get DACL");
    require(GetSecurityDescriptorSacl(sd, &present, &sacl, &def), L"get SACL");
    DWORD error = SetNamedSecurityInfoW(const_cast<LPWSTR>(path.c_str()), SE_FILE_OBJECT,
        DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION | LABEL_SECURITY_INFORMATION,
        nullptr, nullptr, dacl, sacl);
    wprintf(L"ACL path=%ls error=%lu sddl=%ls\n", path.c_str(), error, sddl.c_str());
    require(error == ERROR_SUCCESS, L"set fixture ACL");
    LocalFree(sd);
}

void check_sd(HANDLE token, const std::wstring& path, const wchar_t* label) {
    PSECURITY_DESCRIPTOR sd = nullptr;
    require(GetNamedSecurityInfoW(path.c_str(), SE_FILE_OBJECT,
        OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
        nullptr, nullptr, nullptr, nullptr, &sd) == ERROR_SUCCESS, L"read SD");
    for (DWORD desired : {FILE_GENERIC_READ, FILE_GENERIC_WRITE}) {
        GENERIC_MAPPING mapping{FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_GENERIC_EXECUTE, FILE_ALL_ACCESS};
        BYTE privileges[2048]{}; DWORD length = sizeof(privileges), granted = 0; BOOL allowed = FALSE;
        BOOL ok = AccessCheck(sd, token, desired, &mapping, reinterpret_cast<PRIVILEGE_SET*>(privileges),
            &length, &granted, &allowed);
        wprintf(L"ACCESS_CHECK label=%ls path=%ls desired=%08lx api=%d error=%lu allowed=%d granted=%08lx\n",
            label, path.c_str(), desired, ok, ok ? 0 : GetLastError(), allowed, granted);
    }
    LocalFree(sd);
}

void launch(HANDLE token, const std::wstring& root, const wchar_t* label) {
    std::wstring exe = root + L"\\child.exe";
    std::wstring args = L"\"" + exe + L"\" \"" + root + L"\" " + label;
    SECURITY_ATTRIBUTES sa{sizeof(sa), nullptr, TRUE};
    std::wstring log_path = root + L"\\" + label + L".log";
    HANDLE log = CreateFileW(log_path.c_str(), GENERIC_WRITE, FILE_SHARE_READ, &sa, CREATE_NEW, 0, nullptr);
    require(log != INVALID_HANDLE_VALUE, L"child log");
    STARTUPINFOEXW si{}; si.StartupInfo.cb = sizeof(si);
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    si.StartupInfo.hStdOutput = log; si.StartupInfo.hStdError = log;
    SIZE_T size = 0;
    InitializeProcThreadAttributeList(nullptr, 1, 0, &size);
    std::vector<BYTE> storage(size);
    si.lpAttributeList = reinterpret_cast<LPPROC_THREAD_ATTRIBUTE_LIST>(storage.data());
    require(InitializeProcThreadAttributeList(si.lpAttributeList, 1, 0, &size), L"attribute list");
    require(UpdateProcThreadAttribute(si.lpAttributeList, 0, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
        &log, sizeof(log), nullptr, nullptr), L"child output handle list");
    PROCESS_INFORMATION pi{};
    wprintf(L"LAUNCH_BEGIN label=%ls impersonation=none\n", label); fflush(stdout);
    DWORD flags = CREATE_SUSPENDED | CREATE_NO_WINDOW | EXTENDED_STARTUPINFO_PRESENT;
    bool ordinary = wcscmp(label, L"ordinary") == 0;
    BOOL ok = ordinary
        ? CreateProcessW(exe.c_str(), args.data(), nullptr, nullptr, TRUE, flags, nullptr, root.c_str(), &si.StartupInfo, &pi)
        : CreateProcessAsUserW(token, exe.c_str(), args.data(), nullptr, nullptr, TRUE, flags, nullptr, root.c_str(), &si.StartupInfo, &pi);
    wprintf(L"LAUNCH label=%ls function=%ls api=%d error=%lu\n", label,
        ordinary ? L"CreateProcessW" : L"CreateProcessAsUserW", ok, ok ? 0 : GetLastError());
    DeleteProcThreadAttributeList(si.lpAttributeList); CloseHandle(log);
    if (!ok) return;
    HANDLE actual = nullptr;
    BOOL queried = OpenProcessToken(pi.hProcess, TOKEN_QUERY, &actual);
    wprintf(L"CHILD_TOKEN_QUERY label=%ls api=%d error=%lu\n", label, queried, queried ? 0 : GetLastError());
    if (queried) { token_facts(actual, label); CloseHandle(actual); }
    DWORD resumed = ResumeThread(pi.hThread);
    wprintf(L"RESUME label=%ls previous=%lu error=%lu\n", label, resumed, resumed == -1u ? GetLastError() : 0);
    DWORD wait = WaitForSingleObject(pi.hProcess, 15000), code = STILL_ACTIVE;
    if (wait != WAIT_OBJECT_0) {
        require(TerminateProcess(pi.hProcess, 92), L"terminate timeout child");
        require(WaitForSingleObject(pi.hProcess, 5000) == WAIT_OBJECT_0, L"wait terminated child");
    }
    require(GetExitCodeProcess(pi.hProcess, &code), L"child exit code");
    wprintf(L"EXIT label=%ls wait=%lu code=%08lx\n", label, wait, code);
    CloseHandle(pi.hThread); CloseHandle(pi.hProcess); fflush(stdout);
}

void measure(HANDLE token, const std::wstring& root, const wchar_t* label) {
    token_facts(token, label);
    HANDLE imp = nullptr;
    require(DuplicateTokenEx(token, TOKEN_QUERY | TOKEN_IMPERSONATE, nullptr, SecurityImpersonation,
        TokenImpersonation, &imp), L"duplicate measurement token");
    for (auto file : files) check_sd(imp, root + L"\\" + file, label);
    for (auto file : {L"ntdll.dll", L"kernel32.dll", L"KernelBase.dll", L"advapi32.dll"}) {
        wchar_t system[MAX_PATH]; require(GetSystemDirectoryW(system, MAX_PATH) != 0, L"system directory");
        check_sd(imp, std::wstring(system) + L"\\" + file, label);
    }
    BOOL impersonated = SetThreadToken(nullptr, imp);
    wprintf(L"IMPERSONATE label=%ls api=%d error=%lu\n", label, impersonated, impersonated ? 0 : GetLastError());
    if (impersonated) {
        // Filesystem checks only; no other process, helper, or broker is accessed.
        native_opens(root, (std::wstring(label) + L"-thread").c_str());
        require(RevertToSelf(), L"revert measurement token");
    }
    CloseHandle(imp);
    launch(token, root, label);
}

int wmain(int argc, wchar_t** argv) {
    if (argc != 2) return 91;
    setvbuf(stdout, nullptr, _IONBF, 0);
    SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX);
    std::wstring root = argv[1];
    HANDLE own = nullptr;
    require(OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &own), L"own token");
    token_facts(own, L"ordinary-parent");
    auto elevated = info(own, TokenElevation);
    require(!reinterpret_cast<TOKEN_ELEVATION*>(elevated.data())->TokenIsElevated, L"ordinary user required");
    auto user = info(own, TokenUser);
    std::wstring user_sid = sid_text(reinterpret_cast<TOKEN_USER*>(user.data())->User.Sid);
    ULONG random[4]{};
    auto rng = reinterpret_cast<BOOLEAN(WINAPI*)(PVOID, ULONG)>(GetProcAddress(GetModuleHandleW(L"advapi32.dll"), "SystemFunction036"));
    require(rng && rng(random, sizeof(random)), L"random restricting SID");
    std::wstring r = L"S-1-0";
    for (ULONG value : random) r += L"-" + std::to_wstring(value);
    PSID restricting = nullptr, cap = nullptr, package = nullptr;
    require(ConvertStringSidToSidW(r.c_str(), &restricting), L"restricting SID");
    require(ConvertStringSidToSidW(L"S-1-15-3-4096-111-222-333-444-555-666-777-888", &cap), L"capability SID");
    std::wstring profile = L"RestrictedLowBoxProbe." + std::to_wstring(random[0]) + L"." + std::to_wstring(random[1]);
    HRESULT hr = CreateAppContainerProfile(profile.c_str(), L"Disposable token probe", L"Disposable token probe", nullptr, 0, &package);
    wprintf(L"PROFILE create=%08lx name=%ls restricting=%ls\n", hr, profile.c_str(), r.c_str());
    require(SUCCEEDED(hr), L"create disposable profile");
    std::wstring pkg = sid_text(package), capability = sid_text(cap);
    const std::wstring base = L"D:P(A;;FA;;;" + user_sid + L")";
    const std::wstring low = L"S:(ML;;NW;;;LW)";
    auto grant = [](const std::wstring& sid) { return L"(A;;FRFX;;;" + sid + L")"; };
    std::vector<std::wstring> grants{L"", grant(L"S-1-15-2-1"), grant(L"S-1-15-2-2"), grant(capability),
        grant(pkg), grant(r), grant(L"S-1-15-2-1") + grant(r), grant(L"S-1-15-2-2") + grant(r),
        grant(capability) + grant(r), grant(pkg) + grant(r)};
    for (size_t i = 0; i < 11; ++i) {
        std::wstring path = root + L"\\" + files[i];
        HANDLE file = CreateFileW(path.c_str(), GENERIC_WRITE, 0, nullptr, CREATE_NEW, 0, nullptr);
        require(file != INVALID_HANDLE_VALUE, L"create controlled file");
        const char canary[] = "CONTROLLED_CANARY"; DWORD written = 0;
        require(WriteFile(file, canary, sizeof(canary), &written, nullptr) && written == sizeof(canary), L"write canary");
        CloseHandle(file);
        acl(path, i == 10 ? L"D:NO_ACCESS_CONTROL" + low : base + grants[i] + low);
    }
    acl(root + L"\\child.exe", base + grant(pkg) + grant(r) + low);
    require(CreateHardLinkW((root + L"\\allowed-looking.txt").c_str(), (root + L"\\normal.txt").c_str(), nullptr), L"data alias");
    require(CreateHardLinkW((root + L"\\bootstrap-alias.exe").c_str(), (root + L"\\child.exe").c_str(), nullptr), L"bootstrap alias");
    SID_AND_ATTRIBUTES restriction{restricting, 0}, capability_group{cap, SE_GROUP_ENABLED};
    SECURITY_CAPABILITIES caps{}; caps.AppContainerSid = package;
    caps.Capabilities = &capability_group; caps.CapabilityCount = 1;
    HANDLE restricted = nullptr;
    BOOL made = CreateRestrictedToken(own, DISABLE_MAX_PRIVILEGE, 0, nullptr, 0, nullptr, 1, &restriction, &restricted);
    wprintf(L"RESTRICT api=%d error=%lu\n", made, made ? 0 : GetLastError());
    require(made, L"create restricted primary");
    measure(own, root, L"ordinary");
    measure(restricted, root, L"restricted");
    auto win32 = reinterpret_cast<CreateAppContainerTokenFn>(GetProcAddress(GetModuleHandleW(L"kernelbase.dll"), "CreateAppContainerToken"));
    auto native = reinterpret_cast<NtCreateLowBoxTokenFn>(GetProcAddress(GetModuleHandleW(L"ntdll.dll"), "NtCreateLowBoxToken"));
    for (bool restrict : {false, true}) {
        HANDLE input = restrict ? restricted : own;
        for (bool nt : {false, true}) {
            const wchar_t* label = restrict ? (nt ? L"restricted-lowbox-nt" : L"restricted-lowbox-win32")
                                           : (nt ? L"lowbox-nt" : L"lowbox-win32");
            HANDLE output = nullptr;
            if (nt && native) {
                OBJECT_ATTRIBUTES oa{}; oa.Length = sizeof(oa);
                NTSTATUS status = native(&output, input, TOKEN_ALL_ACCESS, &oa, package, 1, &capability_group, 0, nullptr);
                wprintf(L"CONVERT label=%ls status=%08lx\n", label, static_cast<ULONG>(status));
            } else if (!nt && win32) {
                BOOL ok = win32(input, &caps, &output);
                wprintf(L"CONVERT label=%ls api=%d error=%lu\n", label, ok, ok ? 0 : GetLastError());
            } else wprintf(L"CONVERT label=%ls export_missing=1\n", label);
            if (output) { measure(output, root, label); CloseHandle(output); }
        }
    }
    CloseHandle(restricted); CloseHandle(own);
    for (size_t i = 0; i < 11; ++i) {
        HANDLE file = CreateFileW((root + L"\\" + files[i]).c_str(), GENERIC_READ, 7, nullptr, OPEN_EXISTING, 0, nullptr);
        require(file != INVALID_HANDLE_VALUE, L"host canary open");
        char bytes[32]{}; DWORD read = 0;
        require(ReadFile(file, bytes, sizeof(bytes), &read, nullptr) && read == 18 && strcmp(bytes, "CONTROLLED_CANARY") == 0, L"unchanged canary");
        CloseHandle(file);
        wprintf(L"HOST_CANARY file=%ls unchanged=1\n", files[i]);
    }
    FreeSid(package); LocalFree(cap); LocalFree(restricting);
    hr = DeleteAppContainerProfile(profile.c_str());
    wprintf(L"PROFILE delete=%08lx\n", hr);
    require(SUCCEEDED(hr), L"delete disposable profile");
    wprintf(L"PROBE_COMPLETE\n");
    return 0;
}
