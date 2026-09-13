// A standalone primitive discriminator, not production sandbox code.
// It asks whether an ordinary parent can create one future glob-matching file
// and transfer only that file's write handle to a zero-capability AppContainer.
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <userenv.h>
#include <securityappcontainer.h>
#include <sddl.h>
#include <aclapi.h>
#include <cstdio>
#include <cstdint>
#include <cstring>
#include <cwchar>
#include <string>
#include <vector>

namespace {

struct Handle {
  HANDLE value = nullptr;
  Handle() = default;
  explicit Handle(HANDLE value) : value(value) {}
  ~Handle() { if (value && value != INVALID_HANDLE_VALUE) CloseHandle(value); }
  Handle(const Handle&) = delete;
  Handle& operator=(const Handle&) = delete;
  HANDLE release() { HANDLE result = value; value = nullptr; return result; }
};

struct LocalMem {
  HLOCAL value = nullptr;
  ~LocalMem() { if (value) LocalFree(value); }
};

struct AttributeList {
  std::vector<unsigned char> buffer;
  LPPROC_THREAD_ATTRIBUTE_LIST value = nullptr;
  explicit AttributeList(DWORD count) {
    SIZE_T bytes = 0;
    InitializeProcThreadAttributeList(nullptr, count, 0, &bytes);
    buffer.resize(bytes);
    value = reinterpret_cast<LPPROC_THREAD_ATTRIBUTE_LIST>(buffer.data());
    if (!InitializeProcThreadAttributeList(value, count, 0, &bytes)) value = nullptr;
  }
  ~AttributeList() { if (value) DeleteProcThreadAttributeList(value); }
};

std::wstring join(const std::wstring& left, const wchar_t* right) {
  return left + L"\\" + right;
}

bool denied(const wchar_t* label, const std::wstring& path, DWORD disposition) {
  Handle file(CreateFileW(path.c_str(), GENERIC_WRITE, 0, nullptr, disposition,
                          FILE_ATTRIBUTE_NORMAL, nullptr));
  const DWORD error = file.value == INVALID_HANDLE_VALUE ? GetLastError() : ERROR_SUCCESS;
  if (file.value != INVALID_HANDLE_VALUE) {
    std::printf("%ls=UNEXPECTED_SUCCESS\n", label);
    return false;
  }
  std::printf("%ls=DENIED error=%lu\n", label, error);
  return error == ERROR_ACCESS_DENIED;
}

bool no_package_sid_ace(const std::wstring& path, PSID package_sid) {
  PSECURITY_DESCRIPTOR descriptor = nullptr;
  PACL dacl = nullptr;
  const DWORD status = GetNamedSecurityInfoW(
      path.c_str(), SE_FILE_OBJECT, DACL_SECURITY_INFORMATION, nullptr, nullptr, &dacl,
      nullptr, &descriptor);
  LocalMem cleanup{reinterpret_cast<HLOCAL>(descriptor)};
  if (status != ERROR_SUCCESS || !dacl) return false;
  for (DWORD index = 0; index < dacl->AceCount; ++index) {
    void* ace = nullptr;
    if (!GetAce(dacl, index, &ace)) return false;
    auto* header = static_cast<ACE_HEADER*>(ace);
    PSID sid = nullptr;
    if (header->AceType == ACCESS_ALLOWED_ACE_TYPE) {
      sid = reinterpret_cast<PSID>(&static_cast<ACCESS_ALLOWED_ACE*>(ace)->SidStart);
    } else if (header->AceType == ACCESS_DENIED_ACE_TYPE) {
      sid = reinterpret_cast<PSID>(&static_cast<ACCESS_DENIED_ACE*>(ace)->SidStart);
    }
    if (sid && EqualSid(sid, package_sid)) return false;
  }
  return true;
}

bool child(const std::wstring& root) {
  const std::wstring output = join(root, L"output");
  const std::wstring outside = join(root, L"outside");
  if (!denied(L"DIRECT_APPROVED_CREATE", join(output, L"direct.json"), CREATE_NEW) ||
      !denied(L"DIRECT_NEAREST_NONMATCHING_CREATE", join(output, L"not-json.txt"), CREATE_NEW) ||
      !denied(L"DIRECT_OUTSIDE_CREATE", join(outside, L"outside.json"), CREATE_NEW)) {
    return false;
  }
  std::printf("READY pid=%lu\n", GetCurrentProcessId());
  std::fflush(stdout);

  unsigned long long raw_handle = 0;
  if (scanf_s("HANDLE %llu", &raw_handle) != 1) {
    std::printf("CONTROL_READ=FAIL error=%lu\n", GetLastError());
    return false;
  }
  HANDLE allowed = reinterpret_cast<HANDLE>(static_cast<uintptr_t>(raw_handle));
  const std::wstring future = join(output, L"future.json");
  if (!denied(L"DIRECT_FUTURE_OPEN", future, OPEN_EXISTING)) return false;

  const char payload[] = "brokered-write\n";
  DWORD written = 0;
  if (!WriteFile(allowed, payload, sizeof(payload) - 1, &written, nullptr) ||
      written != sizeof(payload) - 1 || !FlushFileBuffers(allowed)) {
    std::printf("TRANSFERRED_HANDLE_WRITE=FAIL error=%lu\n", GetLastError());
    return false;
  }
  CloseHandle(allowed);
  std::printf("TRANSFERRED_HANDLE_WRITE=OK bytes=%lu\n", written);

  const bool rename_denied = !MoveFileExW(future.c_str(), join(outside, L"moved.json").c_str(), 0) &&
                             GetLastError() == ERROR_ACCESS_DENIED;
  std::printf("DIRECT_RENAME=%s error=%lu\n", rename_denied ? "DENIED" : "UNEXPECTED", GetLastError());
  if (!rename_denied) return false;
  const bool hardlink_denied = !CreateHardLinkW(join(output, L"hardlink.json").c_str(), future.c_str(), nullptr) &&
                               GetLastError() == ERROR_ACCESS_DENIED;
  std::printf("DIRECT_HARDLINK=%s error=%lu\n", hardlink_denied ? "DENIED" : "UNEXPECTED", GetLastError());
  if (!hardlink_denied) return false;

  const std::wstring reparse = join(output, L"future-link.json");
  if (GetFileAttributesW(reparse.c_str()) != INVALID_FILE_ATTRIBUTES) {
    if (!denied(L"DIRECT_REPARSE_OPEN", reparse, OPEN_EXISTING)) return false;
  } else {
    std::printf("DIRECT_REPARSE_OPEN=SETUP_SKIPPED\n");
  }
  std::printf("CHILD_RESULT=PASS\n");
  return true;
}

bool grant_executable(PCWSTR executable, PSID package_sid, PSECURITY_DESCRIPTOR* original,
                      PACL* original_dacl) {
  *original = nullptr;
  PACL dacl = nullptr;
  const DWORD status = GetNamedSecurityInfoW(executable, SE_FILE_OBJECT, DACL_SECURITY_INFORMATION,
                                              nullptr, nullptr, &dacl, nullptr, original);
  if (status != ERROR_SUCCESS) return false;
  BOOL present = FALSE, defaulted = FALSE;
  if (!GetSecurityDescriptorDacl(*original, &present, original_dacl, &defaulted)) return false;
  EXPLICIT_ACCESSW access = {};
  access.grfAccessPermissions = GENERIC_READ | GENERIC_EXECUTE;
  access.grfAccessMode = GRANT_ACCESS;
  access.grfInheritance = NO_INHERITANCE;
  access.Trustee.TrusteeForm = TRUSTEE_IS_SID;
  access.Trustee.TrusteeType = TRUSTEE_IS_WELL_KNOWN_GROUP;
  access.Trustee.ptstrName = static_cast<LPWSTR>(package_sid);
  PACL replacement = nullptr;
  const DWORD acl_status = SetEntriesInAclW(1, &access, dacl, &replacement);
  LocalMem acl_cleanup{reinterpret_cast<HLOCAL>(replacement)};
  return acl_status == ERROR_SUCCESS &&
         SetNamedSecurityInfoW(const_cast<LPWSTR>(executable), SE_FILE_OBJECT,
                               DACL_SECURITY_INFORMATION, nullptr, nullptr, replacement, nullptr) == ERROR_SUCCESS;
}

int parent(const std::wstring& executable) {
  const std::wstring profile = L"nub-live-fs-" + std::to_wstring(GetCurrentProcessId()) +
                               L"-" + std::to_wstring(GetTickCount64());
  PSID package_sid = nullptr;
  bool profile_created = false;
  PSECURITY_DESCRIPTOR original_exe_dacl = nullptr;
  PACL original_exe_acl = nullptr;
  std::wstring root;
  PROCESS_INFORMATION process = {};
  Handle stdin_read, stdin_write, stdout_read, stdout_write;
  int result = 1;

  do {
  const HRESULT profile_status = CreateAppContainerProfile(profile.c_str(), profile.c_str(), profile.c_str(), nullptr, 0, &package_sid);
  if (FAILED(profile_status) || !package_sid) {
    std::printf("PROFILE_CREATE=FAIL hr=0x%08lx\n", static_cast<unsigned long>(profile_status));
    break;
  }
  profile_created = true;
  std::printf("PROFILE_CREATE=OK profile=%ls\n", profile.c_str());
  LPWSTR sid_text = nullptr;
  PWSTR package_folder = nullptr;
  if (ConvertSidToStringSidW(package_sid, &sid_text) &&
      SUCCEEDED(GetAppContainerFolderPath(sid_text, &package_folder))) {
    const DWORD attributes = GetFileAttributesW(package_folder);
    std::printf("PACKAGE_STORAGE=CREATED path=%ls exists=%d\n", package_folder,
                attributes != INVALID_FILE_ATTRIBUTES);
  } else {
    std::printf("PACKAGE_STORAGE=LOOKUP_FAILED error=%lu\n", GetLastError());
  }
  if (package_folder) CoTaskMemFree(package_folder);
  if (sid_text) LocalFree(sid_text);
  if (!grant_executable(executable.c_str(), package_sid, &original_exe_dacl, &original_exe_acl)) {
    std::printf("EXECUTABLE_GRANT=FAIL error=%lu\n", GetLastError());
    break;
  }
  std::printf("EXECUTABLE_GRANT=FILE_ONLY CAPABILITIES=0\n");

  wchar_t temp[MAX_PATH] = {};
  if (!GetTempPathW(MAX_PATH, temp)) break;
  root = std::wstring(temp) + L"nub-live-fs-" + std::to_wstring(GetCurrentProcessId());
  if (!CreateDirectoryW(root.c_str(), nullptr) || !CreateDirectoryW(join(root, L"output").c_str(), nullptr) ||
      !CreateDirectoryW(join(root, L"outside").c_str(), nullptr)) {
    std::printf("ROOT_CREATE=FAIL error=%lu\n", GetLastError());
    break;
  }
  if (!no_package_sid_ace(root, package_sid)) {
    std::printf("TARGET_ROOT_ACL=UNEXPECTED_PACKAGE_SID\n");
    break;
  }
  std::printf("TARGET_ROOT_ACL=NO_PACKAGE_SID\n");
  const std::wstring reparse = join(join(root, L"output"), L"future-link.json");
  const std::wstring reparse_target = join(join(root, L"outside"), L"reparse-target.json");
  if (CreateSymbolicLinkW(reparse.c_str(), reparse_target.c_str(), 0)) {
    std::printf("REPARSE_SETUP=OK\n");
  } else {
    std::printf("REPARSE_SETUP=SKIPPED error=%lu\n", GetLastError());
  }

  SECURITY_ATTRIBUTES inheritable = {sizeof(inheritable), nullptr, TRUE};
  HANDLE raw_stdin_read = nullptr, raw_stdin_write = nullptr, raw_stdout_read = nullptr, raw_stdout_write = nullptr;
  if (!CreatePipe(&raw_stdin_read, &raw_stdin_write, &inheritable, 0) ||
      !CreatePipe(&raw_stdout_read, &raw_stdout_write, &inheritable, 0) ||
      !SetHandleInformation(raw_stdin_write, HANDLE_FLAG_INHERIT, 0) ||
      !SetHandleInformation(raw_stdout_read, HANDLE_FLAG_INHERIT, 0)) {
    std::printf("PIPE_SETUP=FAIL error=%lu\n", GetLastError());
    break;
  }
  stdin_read.value = raw_stdin_read; stdin_write.value = raw_stdin_write;
  stdout_read.value = raw_stdout_read; stdout_write.value = raw_stdout_write;
  HANDLE inherited[] = {stdin_read.value, stdout_write.value};
  SECURITY_CAPABILITIES capabilities = {package_sid, nullptr, 0, 0};
  AttributeList attributes(2);
  if (!attributes.value ||
      !UpdateProcThreadAttribute(attributes.value, 0, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
                                  &capabilities, sizeof(capabilities), nullptr, nullptr) ||
      !UpdateProcThreadAttribute(attributes.value, 0, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, inherited,
                                  sizeof(inherited), nullptr, nullptr)) {
    std::printf("ATTRIBUTE_SETUP=FAIL error=%lu\n", GetLastError());
    break;
  }
  STARTUPINFOEXW startup = {};
  startup.StartupInfo.cb = sizeof(startup);
  startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
  startup.StartupInfo.hStdInput = stdin_read.value;
  startup.StartupInfo.hStdOutput = stdout_write.value;
  startup.StartupInfo.hStdError = stdout_write.value;
  startup.lpAttributeList = attributes.value;
  std::wstring command = L"\"" + executable + L"\" --child \"" + root + L"\"";
  std::vector<wchar_t> command_line(command.begin(), command.end());
  command_line.push_back(L'\0');
  if (!CreateProcessW(executable.c_str(), command_line.data(), nullptr, nullptr, TRUE,
                      EXTENDED_STARTUPINFO_PRESENT | CREATE_NO_WINDOW, nullptr, nullptr,
                      &startup.StartupInfo, &process)) {
    std::printf("APPCONTAINER_LAUNCH=FAIL error=%lu\n", GetLastError());
    break;
  }
  std::printf("APPCONTAINER_LAUNCH=OK pid=%lu\n", process.dwProcessId);
  CloseHandle(stdin_read.release());
  CloseHandle(stdout_write.release());

  char ready[256] = {};
  DWORD count = 0;
  if (!ReadFile(stdout_read.value, ready, sizeof(ready) - 1, &count, nullptr) ||
      std::strstr(ready, "READY pid=") == nullptr) {
    std::printf("CHILD_READY=FAIL bytes=%lu error=%lu text=%s\n", count, GetLastError(), ready);
    break;
  }
  std::printf("CHILD_READY=OK %s", ready);
  const std::wstring future = join(join(root, L"output"), L"future.json");
  Handle future_file(CreateFileW(future.c_str(), GENERIC_WRITE, FILE_SHARE_READ, nullptr, CREATE_NEW,
                                 FILE_ATTRIBUTE_NORMAL, nullptr));
  if (future_file.value == INVALID_HANDLE_VALUE) {
    std::printf("PARENT_FUTURE_GLOB_CREATE=FAIL error=%lu\n", GetLastError());
    break;
  }
  std::printf("PARENT_FUTURE_GLOB_CREATE=OK path=%ls\n", future.c_str());
  HANDLE child_file = nullptr;
  if (!DuplicateHandle(GetCurrentProcess(), future_file.value, process.hProcess, &child_file,
                       GENERIC_WRITE, FALSE, 0)) {
    std::printf("HANDLE_TRANSFER=FAIL error=%lu\n", GetLastError());
    break;
  }
  // The broker relinquishes its source capability after creating the child's
  // narrower duplicate; it must not retain a second live path to the object.
  CloseHandle(future_file.release());
  const std::string control = "HANDLE " + std::to_string(reinterpret_cast<uintptr_t>(child_file)) + "\n";
  DWORD sent = 0;
  if (!WriteFile(stdin_write.value, control.data(), static_cast<DWORD>(control.size()), &sent, nullptr) || sent != control.size()) {
    std::printf("CONTROL_WRITE=FAIL error=%lu\n", GetLastError());
    break;
  }
  CloseHandle(stdin_write.release());
  WaitForSingleObject(process.hProcess, 30000);
  DWORD child_exit = 1;
  GetExitCodeProcess(process.hProcess, &child_exit);
  char output[4096] = {};
  DWORD output_count = 0;
  ReadFile(stdout_read.value, output, sizeof(output) - 1, &output_count, nullptr);
  std::printf("CHILD_OUTPUT=%s", output);
  char contents[64] = {};
  Handle verify(CreateFileW(future.c_str(), GENERIC_READ, FILE_SHARE_READ, nullptr, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, nullptr));
  DWORD read = 0;
  if (verify.value == INVALID_HANDLE_VALUE || !ReadFile(verify.value, contents, sizeof(contents) - 1, &read, nullptr) ||
      child_exit != 0 || std::strcmp(contents, "brokered-write\n") != 0 || std::strstr(output, "CHILD_RESULT=PASS") == nullptr) {
    std::printf("RESULT=FAIL child_exit=%lu verify_error=%lu bytes=%lu content=%s\n", child_exit,
                verify.value == INVALID_HANDLE_VALUE ? GetLastError() : ERROR_SUCCESS, read, contents);
    break;
  }
  std::printf("RESULT=PASS child_exit=%lu\n", child_exit);
  result = 0;
  } while (false);

  if (process.hProcess) {
    if (WaitForSingleObject(process.hProcess, 0) == WAIT_TIMEOUT) TerminateProcess(process.hProcess, 1);
    CloseHandle(process.hThread); CloseHandle(process.hProcess);
  }
  if (original_exe_dacl) {
    const DWORD restore = SetNamedSecurityInfoW(const_cast<LPWSTR>(executable.c_str()), SE_FILE_OBJECT,
                                                DACL_SECURITY_INFORMATION, nullptr, nullptr, original_exe_acl, nullptr);
    std::printf("EXECUTABLE_ACL_RESTORE=%s error=%lu\n", restore == ERROR_SUCCESS ? "OK" : "FAIL", restore);
    LocalFree(original_exe_dacl);
  }
  if (!root.empty()) {
    DeleteFileW(join(join(root, L"output"), L"future-link.json").c_str());
    DeleteFileW(join(join(root, L"output"), L"future.json").c_str());
    RemoveDirectoryW(join(root, L"output").c_str()); RemoveDirectoryW(join(root, L"outside").c_str());
    const BOOL removed = RemoveDirectoryW(root.c_str());
    std::printf("TEMP_CLEANUP=%s error=%lu\n", removed ? "OK" : "FAIL", removed ? 0 : GetLastError());
  }
  if (package_sid) FreeSid(package_sid);
  if (profile_created) {
    const HRESULT deleted = DeleteAppContainerProfile(profile.c_str());
    std::printf("PROFILE_CLEANUP=%s hr=0x%08lx\n", SUCCEEDED(deleted) ? "OK" : "FAIL", static_cast<unsigned long>(deleted));
  }
  return result;
}

}  // namespace

int wmain(int argc, wchar_t** argv) {
  if (argc == 3 && std::wcscmp(argv[1], L"--child") == 0) return child(argv[2]) ? 0 : 2;
  if (argc != 1) return 64;
  wchar_t executable[MAX_PATH] = {};
  const DWORD size = GetModuleFileNameW(nullptr, executable, MAX_PATH);
  return size == 0 || size == MAX_PATH ? 65 : parent(executable);
}
