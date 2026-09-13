// A standalone capability discriminator, not production sandbox code.
// Its opt-in in-process hook mirrors Chromium's denied-open broker seam only
// to test whether the native loader accepts a parent-transferred original DLL.
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <winternl.h>
#include <userenv.h>
#include <securityappcontainer.h>
#include <sddl.h>
#include <aclapi.h>
#include "detours.h"
#include <cstdio>
#include <cstdint>
#include <cstring>
#include <cwchar>
#include <string>
#include <string_view>
#include <vector>

namespace {

constexpr NTSTATUS kStatusAccessDenied = static_cast<NTSTATUS>(0xC0000022L);
constexpr ULONG kSecImage = 0x01000000;
constexpr ULONG kViewUnmap = 2;
constexpr ACCESS_MASK kSectionMapRead = 0x0004;
constexpr ACCESS_MASK kSectionMapExecute = 0x0008;
constexpr ACCESS_MASK kSectionQuery = 0x0001;
constexpr ULONG kFileOpen = 1;
constexpr ULONG kFileNonDirectory = 0x40;
constexpr ULONG kFileSynchronousNonalert = 0x20;

// winternl.h exposes the corresponding information classes but not these
// undocumented wire structs in the hosted Windows SDK used by Actions.
struct NtFileBasicInformation {
  LARGE_INTEGER creation_time;
  LARGE_INTEGER last_access_time;
  LARGE_INTEGER last_write_time;
  LARGE_INTEGER change_time;
  ULONG file_attributes;
};

struct NtFileNetworkOpenInformation {
  LARGE_INTEGER creation_time;
  LARGE_INTEGER last_access_time;
  LARGE_INTEGER last_write_time;
  LARGE_INTEGER change_time;
  LARGE_INTEGER allocation_size;
  LARGE_INTEGER end_of_file;
  ULONG file_attributes;
};

struct Handle {
  HANDLE value = nullptr;
  Handle() = default;
  explicit Handle(HANDLE value) : value(value) {}
  ~Handle() { if (value && value != INVALID_HANDLE_VALUE) CloseHandle(value); }
  Handle(const Handle&) = delete;
  Handle& operator=(const Handle&) = delete;
  HANDLE release() { HANDLE result = value; value = nullptr; return result; }
};

struct LocalMem { HLOCAL value = nullptr; ~LocalMem() { if (value) LocalFree(value); } };

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

using NtCreateFileFn = NTSTATUS(NTAPI*)(PHANDLE, ACCESS_MASK, POBJECT_ATTRIBUTES, PIO_STATUS_BLOCK,
                                        PLARGE_INTEGER, ULONG, ULONG, ULONG, ULONG, PVOID, ULONG);
using NtOpenFileFn = NTSTATUS(NTAPI*)(PHANDLE, ACCESS_MASK, POBJECT_ATTRIBUTES, PIO_STATUS_BLOCK,
                                      ULONG, ULONG);
using NtQueryAttributesFileFn = NTSTATUS(NTAPI*)(POBJECT_ATTRIBUTES, NtFileBasicInformation*);
using NtQueryFullAttributesFileFn = NTSTATUS(NTAPI*)(POBJECT_ATTRIBUTES, NtFileNetworkOpenInformation*);
using NtCreateSectionFn = NTSTATUS(NTAPI*)(PHANDLE, ACCESS_MASK, POBJECT_ATTRIBUTES, PLARGE_INTEGER,
                                           ULONG, ULONG, HANDLE);
using NtMapViewOfSectionFn = NTSTATUS(NTAPI*)(HANDLE, HANDLE, PVOID*, ULONG_PTR, SIZE_T,
                                               PLARGE_INTEGER, PSIZE_T, ULONG, ULONG, ULONG);
using NtUnmapViewOfSectionFn = NTSTATUS(NTAPI*)(HANDLE, PVOID);

struct NtApi {
  NtCreateFileFn create_file = nullptr;
  NtQueryAttributesFileFn query_attributes_file = nullptr;
  NtCreateSectionFn create_section = nullptr;
  NtMapViewOfSectionFn map_view = nullptr;
  NtUnmapViewOfSectionFn unmap_view = nullptr;
  bool load() {
    HMODULE ntdll = GetModuleHandleW(L"ntdll.dll");
    create_file = reinterpret_cast<NtCreateFileFn>(GetProcAddress(ntdll, "NtCreateFile"));
    query_attributes_file = reinterpret_cast<NtQueryAttributesFileFn>(GetProcAddress(ntdll, "NtQueryAttributesFile"));
    create_section = reinterpret_cast<NtCreateSectionFn>(GetProcAddress(ntdll, "NtCreateSection"));
    map_view = reinterpret_cast<NtMapViewOfSectionFn>(GetProcAddress(ntdll, "NtMapViewOfSection"));
    unmap_view = reinterpret_cast<NtUnmapViewOfSectionFn>(GetProcAddress(ntdll, "NtUnmapViewOfSection"));
    return create_file && query_attributes_file && create_section && map_view && unmap_view;
  }
};

constexpr NTSTATUS kStatusSuccess = static_cast<NTSTATUS>(0);

std::wstring g_brokered_dll_path;
HANDLE g_brokered_dll = nullptr;
unsigned long g_create_broker_calls = 0;
unsigned long g_open_broker_calls = 0;
unsigned long g_query_broker_calls = 0;
unsigned long g_query_full_broker_calls = 0;
unsigned long g_create_forward_calls = 0;
unsigned long g_open_forward_calls = 0;
unsigned long g_query_forward_calls = 0;
unsigned long g_query_full_forward_calls = 0;

bool native_name_matches_brokered_dll(POBJECT_ATTRIBUTES attributes) {
  if (!attributes || !attributes->ObjectName || !attributes->ObjectName->Buffer ||
      attributes->ObjectName->Length == 0) return false;
  const std::wstring_view name(attributes->ObjectName->Buffer,
                               attributes->ObjectName->Length / sizeof(wchar_t));
  if (name.size() < g_brokered_dll_path.size()) return false;
  return CompareStringOrdinal(name.data() + name.size() - g_brokered_dll_path.size(),
                              static_cast<int>(g_brokered_dll_path.size()),
                              g_brokered_dll_path.data(),
                              static_cast<int>(g_brokered_dll_path.size()), TRUE) == CSTR_EQUAL;
}

NTSTATUS duplicate_brokered_dll(PHANDLE file, PIO_STATUS_BLOCK io_status) {
  HANDLE duplicate = nullptr;
  if (!file || !io_status || !g_brokered_dll ||
      !DuplicateHandle(GetCurrentProcess(), g_brokered_dll, GetCurrentProcess(), &duplicate,
                       0, FALSE, DUPLICATE_SAME_ACCESS)) return kStatusAccessDenied;
  *file = duplicate;
  io_status->Status = kStatusSuccess;
  io_status->Information = kFileOpen;
  return kStatusSuccess;
}

NTSTATUS describe_brokered_dll(NtFileBasicInformation* basic,
                               NtFileNetworkOpenInformation* full) {
  BY_HANDLE_FILE_INFORMATION info = {};
  FILE_STANDARD_INFO standard = {};
  if (!g_brokered_dll || !GetFileInformationByHandle(g_brokered_dll, &info) ||
      !GetFileInformationByHandleEx(g_brokered_dll, FileStandardInfo, &standard, sizeof(standard))) {
    return kStatusAccessDenied;
  }
  const auto from_filetime = [](FILETIME value) {
    LARGE_INTEGER result = {};
    result.LowPart = value.dwLowDateTime;
    result.HighPart = static_cast<LONG>(value.dwHighDateTime);
    return result;
  };
  if (basic) {
    std::memset(basic, 0, sizeof(*basic));
    basic->creation_time = from_filetime(info.ftCreationTime);
    basic->last_access_time = from_filetime(info.ftLastAccessTime);
    basic->last_write_time = from_filetime(info.ftLastWriteTime);
    basic->change_time = basic->last_write_time;
    basic->file_attributes = info.dwFileAttributes;
  }
  if (full) {
    std::memset(full, 0, sizeof(*full));
    full->creation_time = from_filetime(info.ftCreationTime);
    full->last_access_time = from_filetime(info.ftLastAccessTime);
    full->last_write_time = from_filetime(info.ftLastWriteTime);
    full->change_time = full->last_write_time;
    full->allocation_size = standard.AllocationSize;
    full->end_of_file = standard.EndOfFile;
    full->file_attributes = info.dwFileAttributes;
  }
  return kStatusSuccess;
}

struct NativeOpenHooks {
  NtCreateFileFn create_original = nullptr;
  NtOpenFileFn open_original = nullptr;
  NtQueryAttributesFileFn query_original = nullptr;
  NtQueryFullAttributesFileFn query_full_original = nullptr;
  bool active = false;

  bool install();
  bool uninstall();
};

NativeOpenHooks g_native_open_hooks;

NTSTATUS NTAPI shim_nt_create_file(PHANDLE file, ACCESS_MASK desired_access,
                                   POBJECT_ATTRIBUTES object_attributes, PIO_STATUS_BLOCK io_status,
                                   PLARGE_INTEGER allocation_size, ULONG file_attributes, ULONG sharing,
                                   ULONG disposition, ULONG options, PVOID ea_buffer, ULONG ea_length) {
  const NTSTATUS status = g_native_open_hooks.create_original(file, desired_access, object_attributes,
      io_status, allocation_size, file_attributes, sharing, disposition, options, ea_buffer, ea_length);
  if (!native_name_matches_brokered_dll(object_attributes)) { ++g_create_forward_calls; return status; }
  if (status != kStatusAccessDenied) return status;
  ++g_create_broker_calls;
  return duplicate_brokered_dll(file, io_status);
}

NTSTATUS NTAPI shim_nt_open_file(PHANDLE file, ACCESS_MASK desired_access,
                                 POBJECT_ATTRIBUTES object_attributes, PIO_STATUS_BLOCK io_status,
                                 ULONG sharing, ULONG options) {
  const NTSTATUS status = g_native_open_hooks.open_original(file, desired_access, object_attributes,
      io_status, sharing, options);
  if (!native_name_matches_brokered_dll(object_attributes)) { ++g_open_forward_calls; return status; }
  if (status != kStatusAccessDenied) return status;
  ++g_open_broker_calls;
  return duplicate_brokered_dll(file, io_status);
}

NTSTATUS NTAPI shim_nt_query_attributes_file(POBJECT_ATTRIBUTES object_attributes,
                                             NtFileBasicInformation* file_attributes) {
  const NTSTATUS status = g_native_open_hooks.query_original(object_attributes, file_attributes);
  if (!native_name_matches_brokered_dll(object_attributes)) { ++g_query_forward_calls; return status; }
  if (status >= 0) return status;
  ++g_query_broker_calls;
  return describe_brokered_dll(file_attributes, nullptr);
}

NTSTATUS NTAPI shim_nt_query_full_attributes_file(POBJECT_ATTRIBUTES object_attributes,
                                                  NtFileNetworkOpenInformation* file_attributes) {
  const NTSTATUS status = g_native_open_hooks.query_full_original(object_attributes, file_attributes);
  if (!native_name_matches_brokered_dll(object_attributes)) { ++g_query_full_forward_calls; return status; }
  if (status >= 0) return status;
  ++g_query_full_broker_calls;
  return describe_brokered_dll(nullptr, file_attributes);
}

bool NativeOpenHooks::install() {
  HMODULE ntdll = GetModuleHandleW(L"ntdll.dll");
  create_original = reinterpret_cast<NtCreateFileFn>(GetProcAddress(ntdll, "NtCreateFile"));
  open_original = reinterpret_cast<NtOpenFileFn>(GetProcAddress(ntdll, "NtOpenFile"));
  query_original = reinterpret_cast<NtQueryAttributesFileFn>(GetProcAddress(ntdll, "NtQueryAttributesFile"));
  query_full_original = reinterpret_cast<NtQueryFullAttributesFileFn>(GetProcAddress(ntdll, "NtQueryFullAttributesFile"));
  if (!create_original || !open_original || !query_original || !query_full_original ||
      DetourTransactionBegin() != NO_ERROR || DetourUpdateThread(GetCurrentThread()) != NO_ERROR ||
      DetourAttach(&create_original, shim_nt_create_file) != NO_ERROR ||
      DetourAttach(&open_original, shim_nt_open_file) != NO_ERROR ||
      DetourAttach(&query_original, shim_nt_query_attributes_file) != NO_ERROR ||
      DetourAttach(&query_full_original, shim_nt_query_full_attributes_file) != NO_ERROR ||
      DetourTransactionCommit() != NO_ERROR) {
    DetourTransactionAbort();
    return false;
  }
  active = true;
  return true;
}

bool NativeOpenHooks::uninstall() {
  if (!active) return true;
  if (DetourTransactionBegin() != NO_ERROR || DetourUpdateThread(GetCurrentThread()) != NO_ERROR ||
      DetourDetach(&create_original, shim_nt_create_file) != NO_ERROR ||
      DetourDetach(&open_original, shim_nt_open_file) != NO_ERROR ||
      DetourDetach(&query_original, shim_nt_query_attributes_file) != NO_ERROR ||
      DetourDetach(&query_full_original, shim_nt_query_full_attributes_file) != NO_ERROR ||
      DetourTransactionCommit() != NO_ERROR) {
    DetourTransactionAbort();
    return false;
  }
  active = false;
  return true;
}

std::wstring join(const std::wstring& left, const wchar_t* right) { return left + L"\\" + right; }
std::wstring sibling(const std::wstring& path, const wchar_t* name) {
  const size_t slash = path.find_last_of(L"\\/");
  return slash == std::wstring::npos ? std::wstring(name) : path.substr(0, slash + 1) + name;
}

bool no_package_sid_ace(const std::wstring& path, PSID package_sid) {
  PSECURITY_DESCRIPTOR descriptor = nullptr;
  PACL dacl = nullptr;
  const DWORD status = GetNamedSecurityInfoW(path.c_str(), SE_FILE_OBJECT, DACL_SECURITY_INFORMATION,
                                              nullptr, nullptr, &dacl, nullptr, &descriptor);
  LocalMem cleanup{reinterpret_cast<HLOCAL>(descriptor)};
  if (status != ERROR_SUCCESS || !dacl) return false;
  for (DWORD index = 0; index < dacl->AceCount; ++index) {
    void* ace = nullptr;
    if (!GetAce(dacl, index, &ace)) return false;
    auto* header = static_cast<ACE_HEADER*>(ace);
    PSID sid = nullptr;
    if (header->AceType == ACCESS_ALLOWED_ACE_TYPE) sid = reinterpret_cast<PSID>(&static_cast<ACCESS_ALLOWED_ACE*>(ace)->SidStart);
    if (header->AceType == ACCESS_DENIED_ACE_TYPE) sid = reinterpret_cast<PSID>(&static_cast<ACCESS_DENIED_ACE*>(ace)->SidStart);
    if (sid && EqualSid(sid, package_sid)) return false;
  }
  return true;
}

bool grant_executable(PCWSTR executable, PSID package_sid, PSECURITY_DESCRIPTOR* original, PACL* original_dacl) {
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

bool raw_nt_open_denied(NtApi& api, const wchar_t* label, const std::wstring& dos_path) {
  const std::wstring nt_path = L"\\??\\" + dos_path;
  UNICODE_STRING name = {};
  name.Buffer = const_cast<PWSTR>(nt_path.c_str());
  name.Length = static_cast<USHORT>(nt_path.size() * sizeof(wchar_t));
  name.MaximumLength = name.Length;
  OBJECT_ATTRIBUTES attrs = {};
  InitializeObjectAttributes(&attrs, &name, OBJ_CASE_INSENSITIVE, nullptr, nullptr);
  IO_STATUS_BLOCK iosb = {};
  HANDLE file = INVALID_HANDLE_VALUE;
  const NTSTATUS status = api.create_file(&file, FILE_GENERIC_READ, &attrs, &iosb, nullptr,
                                          FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, kFileOpen,
                                          kFileNonDirectory | kFileSynchronousNonalert, nullptr, 0);
  if (file != INVALID_HANDLE_VALUE) CloseHandle(file);
  std::printf("%ls=%s status=0x%08lx\n", label, status == kStatusAccessDenied ? "DENIED" : "UNEXPECTED", static_cast<unsigned long>(status));
  return status == kStatusAccessDenied;
}

bool map_image(NtApi& api, HANDLE section, const wchar_t* label) {
  PVOID base = nullptr;
  SIZE_T bytes = 0;
  const NTSTATUS status = api.map_view(section, GetCurrentProcess(), &base, 0, 0, nullptr, &bytes, kViewUnmap, 0, PAGE_READONLY);
  const bool ok = status >= 0 && base && *static_cast<const unsigned short*>(base) == 0x5A4D;
  std::printf("%ls=%s status=0x%08lx base=%p\n", label, ok ? "OK" : "FAIL", static_cast<unsigned long>(status), base);
  std::fflush(stdout);
  if (base) api.unmap_view(GetCurrentProcess(), base);
  return ok;
}

bool image_section_from_file(NtApi& api, HANDLE file, const wchar_t* label) {
  Handle section;
  const NTSTATUS status = api.create_section(&section.value, kSectionMapRead | kSectionMapExecute | kSectionQuery, nullptr, nullptr, PAGE_READONLY, kSecImage, file);
  if (status < 0) { std::printf("%ls=FAIL status=0x%08lx\n", label, static_cast<unsigned long>(status)); std::fflush(stdout); return false; }
  return map_image(api, section.value, label);
}

bool path_create_process_denied(const std::wstring& path) {
  STARTUPINFOW startup = {}; startup.cb = sizeof(startup);
  PROCESS_INFORMATION process = {};
  std::vector<wchar_t> command(path.begin(), path.end()); command.push_back(L'\0');
  const BOOL started = CreateProcessW(path.c_str(), command.data(), nullptr, nullptr, FALSE, CREATE_NO_WINDOW, nullptr, nullptr, &startup, &process);
  const DWORD error = started ? ERROR_SUCCESS : GetLastError();
  if (started) { TerminateProcess(process.hProcess, 1); CloseHandle(process.hThread); CloseHandle(process.hProcess); }
  std::printf("PATH_CREATEPROCESS=%s error=%lu\n", !started && error == ERROR_ACCESS_DENIED ? "DENIED" : "UNEXPECTED", error);
  return !started && error == ERROR_ACCESS_DENIED;
}

bool path_loadlibrary_denied(const std::wstring& path) {
  HMODULE module = LoadLibraryW(path.c_str());
  const DWORD error = module ? ERROR_SUCCESS : GetLastError();
  if (module) FreeLibrary(module);
  std::printf("PATH_LOADLIBRARY=%s error=%lu\n", !module && error == ERROR_ACCESS_DENIED ? "DENIED" : "UNEXPECTED", error);
  return !module && error == ERROR_ACCESS_DENIED;
}

bool forwarded_allowed_control(NtApi& api, const std::wstring& allowed_path) {
  const std::wstring nt_path = L"\\??\\" + allowed_path;
  UNICODE_STRING name = {};
  name.Buffer = const_cast<PWSTR>(nt_path.c_str());
  name.Length = static_cast<USHORT>(nt_path.size() * sizeof(wchar_t));
  name.MaximumLength = name.Length;
  OBJECT_ATTRIBUTES attrs = {};
  InitializeObjectAttributes(&attrs, &name, OBJ_CASE_INSENSITIVE, nullptr, nullptr);
  IO_STATUS_BLOCK iosb = {};
  HANDLE file = INVALID_HANDLE_VALUE;
  const NTSTATUS open_status = api.create_file(&file, FILE_GENERIC_READ, &attrs, &iosb, nullptr,
      FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, kFileOpen,
      kFileNonDirectory | kFileSynchronousNonalert, nullptr, 0);
  if (file != INVALID_HANDLE_VALUE) CloseHandle(file);
  NtFileBasicInformation basic = {};
  const NTSTATUS query_status = api.query_attributes_file(&attrs, &basic);
  const bool ok = open_status >= 0 && query_status >= 0 && g_create_forward_calls > 0 &&
      g_query_forward_calls > 0;
  std::printf("HOOK_FORWARDED_ALLOWED_OPEN=%s open=0x%08lx query=0x%08lx create_forward=%lu query_forward=%lu\n",
      ok ? "OK" : "FAIL", static_cast<unsigned long>(open_status),
      static_cast<unsigned long>(query_status), g_create_forward_calls, g_query_forward_calls);
  std::fflush(stdout);
  return ok;
}

bool read_controls(unsigned long long* read_raw, unsigned long long* write_raw,
                   unsigned long long* exe_file_raw, unsigned long long* dll_file_raw,
                   unsigned long long* exe_section_raw, unsigned long long* dll_section_raw) {
  char controls[512] = {};
  DWORD bytes = 0;
  const HANDLE input = GetStdHandle(STD_INPUT_HANDLE);
  const DWORD type = GetFileType(input);
  if (!ReadFile(input, controls, sizeof(controls) - 1, &bytes, nullptr)) {
    char report[128] = {};
    const int length = sprintf_s(report, "CONTROL_READ=FAIL input=%p type=%lu error=%lu\n", input, type, GetLastError());
    DWORD written = 0;
    if (length > 0) WriteFile(GetStdHandle(STD_OUTPUT_HANDLE), report, static_cast<DWORD>(length), &written, nullptr);
    return false;
  }
  const int fields = sscanf_s(controls,
      "READ %llu\nWRITE %llu\nEXE_FILE %llu\nDLL_FILE %llu\nEXE_SECTION %llu\nDLL_SECTION %llu",
      read_raw, write_raw, exe_file_raw, dll_file_raw, exe_section_raw, dll_section_raw);
  if (fields != 6) {
    char report[640] = {};
    const int length = sprintf_s(report, "CONTROL_READ=FAIL fields=%d bytes=%lu text=%s\n", fields, bytes, controls);
    DWORD written = 0;
    if (length > 0) WriteFile(GetStdHandle(STD_OUTPUT_HANDLE), report, static_cast<DWORD>(length), &written, nullptr);
    return false;
  }
  std::printf("CONTROL_READ=OK bytes=%lu\n", bytes);
  std::fflush(stdout);
  return true;
}

bool child(const std::wstring& root, const std::wstring& image_exe, const std::wstring& image_dll, const std::wstring& source, const std::wstring& nonmatch) {
  NtApi api;
  if (!api.load() || !raw_nt_open_denied(api, L"RAW_NT_IMAGE_OPEN", image_exe) ||
      !raw_nt_open_denied(api, L"RAW_NT_EXISTING_READ_OPEN", source) ||
      !raw_nt_open_denied(api, L"RAW_NT_NEAREST_NONMATCH_OPEN", nonmatch) ||
      !path_create_process_denied(image_exe) || !path_loadlibrary_denied(image_dll)) return false;
  std::printf("READY pid=%lu\n", GetCurrentProcessId()); std::fflush(stdout);
  unsigned long long read_raw = 0, write_raw = 0, exe_file_raw = 0, dll_file_raw = 0, exe_section_raw = 0, dll_section_raw = 0;
  if (!read_controls(&read_raw, &write_raw, &exe_file_raw, &dll_file_raw, &exe_section_raw, &dll_section_raw)) return false;
  Handle read(reinterpret_cast<HANDLE>(static_cast<uintptr_t>(read_raw)));
  Handle write(reinterpret_cast<HANDLE>(static_cast<uintptr_t>(write_raw)));
  Handle exe_file(reinterpret_cast<HANDLE>(static_cast<uintptr_t>(exe_file_raw)));
  Handle dll_file(reinterpret_cast<HANDLE>(static_cast<uintptr_t>(dll_file_raw)));
  Handle exe_section(reinterpret_cast<HANDLE>(static_cast<uintptr_t>(exe_section_raw)));
  Handle dll_section(reinterpret_cast<HANDLE>(static_cast<uintptr_t>(dll_section_raw)));
  char read_text[64] = {}; DWORD read_count = 0;
  if (!ReadFile(read.value, read_text, sizeof(read_text) - 1, &read_count, nullptr) ||
      (std::strcmp(read_text, "existing-readable\n") != 0 && std::strcmp(read_text, "existing-readable\r\n") != 0)) {
    std::printf("TRANSFERRED_EXISTING_READ=FAIL error=%lu bytes=%lu text=%s\n", GetLastError(), read_count, read_text);
    std::fflush(stdout);
    return false;
  }
  std::printf("TRANSFERRED_EXISTING_READ=OK bytes=%lu\n", read_count);
  std::fflush(stdout);
  const std::wstring future = join(join(root, L"output"), L"future.json");
  if (!raw_nt_open_denied(api, L"RAW_NT_FUTURE_OPEN", future)) return false;
  const char payload[] = "brokered-write\n"; DWORD written = 0;
  if (!WriteFile(write.value, payload, sizeof(payload) - 1, &written, nullptr) || written != sizeof(payload) - 1 || !FlushFileBuffers(write.value)) {
    std::printf("TRANSFERRED_FUTURE_WRITE=FAIL error=%lu bytes=%lu\n", GetLastError(), written);
    std::fflush(stdout);
    return false;
  }
  std::printf("TRANSFERRED_FUTURE_WRITE=OK bytes=%lu\n", written);
  std::fflush(stdout);
  if (!image_section_from_file(api, exe_file.value, L"EXE_FILE_SEC_IMAGE") || !image_section_from_file(api, dll_file.value, L"DLL_FILE_SEC_IMAGE") ||
      !map_image(api, exe_section.value, L"EXE_SECTION_MAP") || !map_image(api, dll_section.value, L"DLL_SECTION_MAP")) return false;
  g_brokered_dll_path = image_dll;
  g_brokered_dll = dll_file.value;
  if (!g_native_open_hooks.install()) {
    std::printf("DLL_OPEN_SHIM=FAIL error=%lu\n", GetLastError());
    std::fflush(stdout);
    return false;
  }
  wchar_t allowed_path[MAX_PATH] = {};
  const DWORD allowed_size = GetModuleFileNameW(nullptr, allowed_path, MAX_PATH);
  if (allowed_size == 0 || allowed_size == MAX_PATH || !forwarded_allowed_control(api, allowed_path)) {
    g_native_open_hooks.uninstall();
    return false;
  }
  HMODULE brokered_module = LoadLibraryW(image_dll.c_str());
  const DWORD brokered_load_error = brokered_module ? ERROR_SUCCESS : GetLastError();
  auto brokered_value = brokered_module ? reinterpret_cast<int(*)()>(GetProcAddress(brokered_module, "image_fixture_value")) : nullptr;
  auto brokered_dllmain_calls = brokered_module ? reinterpret_cast<int(*)()>(GetProcAddress(brokered_module, "image_fixture_dllmain_calls")) : nullptr;
  const bool brokered_dll_ok = brokered_module && brokered_value && brokered_dllmain_calls &&
      brokered_value() == 0x472 && brokered_dllmain_calls() == 1 &&
      (g_create_broker_calls + g_open_broker_calls) > 0 &&
      (g_query_broker_calls + g_query_full_broker_calls) > 0;
  std::printf("SHIM_DLL_LOAD=%s error=%lu create_calls=%lu open_calls=%lu query_calls=%lu query_full_calls=%lu dllmain_calls=%d export=%d\n",
              brokered_dll_ok ? "OK" : "FAIL", brokered_load_error, g_create_broker_calls,
              g_open_broker_calls, g_query_broker_calls, g_query_full_broker_calls, brokered_dllmain_calls ? brokered_dllmain_calls() : -1,
              brokered_value ? brokered_value() : -1);
  std::fflush(stdout);
  if (brokered_module) FreeLibrary(brokered_module);
  const bool shim_restored = g_native_open_hooks.uninstall();
  g_brokered_dll = nullptr;
  g_brokered_dll_path.clear();
  if (!brokered_dll_ok || !shim_restored) return false;
  const bool rename_denied = !MoveFileExW(future.c_str(), join(join(root, L"outside"), L"moved.json").c_str(), 0) && GetLastError() == ERROR_ACCESS_DENIED;
  const bool hardlink_denied = !CreateHardLinkW(join(join(root, L"output"), L"hardlink.json").c_str(), future.c_str(), nullptr) && GetLastError() == ERROR_ACCESS_DENIED;
  std::printf("DIRECT_RENAME=%s DIRECT_HARDLINK=%s\n", rename_denied ? "DENIED" : "UNEXPECTED", hardlink_denied ? "DENIED" : "UNEXPECTED");
  if (!rename_denied || !hardlink_denied) return false;
  std::printf("CHILD_RESULT=PASS\n");
  return true;
}

bool unconfined_exe_control(const std::wstring& executable) {
  STARTUPINFOW startup = {}; startup.cb = sizeof(startup); PROCESS_INFORMATION process = {};
  std::vector<wchar_t> command(executable.begin(), executable.end()); command.push_back(L'\0');
  if (!CreateProcessW(executable.c_str(), command.data(), nullptr, nullptr, FALSE, CREATE_NO_WINDOW, nullptr, nullptr, &startup, &process)) return false;
  WaitForSingleObject(process.hProcess, 10000); DWORD exit_code = 0; GetExitCodeProcess(process.hProcess, &exit_code);
  CloseHandle(process.hThread); CloseHandle(process.hProcess); std::printf("UNCONFINED_EXE=%s exit=%lu\n", exit_code == 73 ? "PASS" : "FAIL", exit_code); return exit_code == 73;
}

bool unconfined_dll_control(const std::wstring& library) {
  HMODULE module = LoadLibraryW(library.c_str()); auto value = module ? reinterpret_cast<int(*)()>(GetProcAddress(module, "image_fixture_value")) : nullptr;
  const bool ok = value && value() == 0x472; std::printf("UNCONFINED_DLL=%s error=%lu\n", ok ? "PASS" : "FAIL", ok ? 0 : GetLastError()); if (module) FreeLibrary(module); return ok;
}

bool parent_appcontainer_image_control(const std::wstring& executable, PSID package_sid) {
  SECURITY_CAPABILITIES capabilities = {package_sid, nullptr, 0, 0};
  AttributeList attributes(1);
  if (!attributes.value || !UpdateProcThreadAttribute(attributes.value, 0,
      PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, &capabilities, sizeof(capabilities), nullptr, nullptr)) {
    std::printf("PARENT_APPCONTAINER_IMAGE_LAUNCH=FAIL stage=attributes error=%lu\n", GetLastError());
    return false;
  }
  STARTUPINFOEXW startup = {};
  startup.StartupInfo.cb = sizeof(startup);
  startup.lpAttributeList = attributes.value;
  PROCESS_INFORMATION process = {};
  std::vector<wchar_t> command(executable.begin(), executable.end());
  command.push_back(L'\0');
  const BOOL started = CreateProcessW(executable.c_str(), command.data(), nullptr, nullptr, FALSE,
      EXTENDED_STARTUPINFO_PRESENT | CREATE_NO_WINDOW, nullptr, nullptr, &startup.StartupInfo, &process);
  const DWORD launch_error = started ? ERROR_SUCCESS : GetLastError();
  BOOL appcontainer = FALSE;
  Handle token;
  if (started && OpenProcessToken(process.hProcess, TOKEN_QUERY, &token.value)) {
    DWORD bytes = 0;
    GetTokenInformation(token.value, TokenIsAppContainer, &appcontainer, sizeof(appcontainer), &bytes);
  }
  DWORD exit_code = 0;
  bool reaped = false;
  if (started) {
    if (WaitForSingleObject(process.hProcess, 10000) == WAIT_TIMEOUT) {
      TerminateProcess(process.hProcess, 1);
    }
    reaped = WaitForSingleObject(process.hProcess, 10000) == WAIT_OBJECT_0;
    if (reaped) GetExitCodeProcess(process.hProcess, &exit_code);
    CloseHandle(process.hThread);
    CloseHandle(process.hProcess);
  }
  const bool ok = started && appcontainer && reaped && exit_code == 73;
  std::printf("PARENT_APPCONTAINER_IMAGE_LAUNCH=%s error=%lu token_appcontainer=%d exit=%lu reaped=%d\n",
      ok ? "OK" : "FAIL", launch_error, appcontainer ? 1 : 0, exit_code, reaped ? 1 : 0);
  std::fflush(stdout);
  return ok;
}

bool create_image_section(NtApi& api, HANDLE file, Handle* section) { return api.create_section(&section->value, kSectionMapRead | kSectionMapExecute | kSectionQuery, nullptr, nullptr, PAGE_READONLY, kSecImage, file) >= 0; }
bool duplicate_to_child(HANDLE source, HANDLE process, ACCESS_MASK rights, HANDLE* target) { return DuplicateHandle(GetCurrentProcess(), source, process, target, rights, FALSE, 0) != FALSE; }

int parent(const std::wstring& executable) {
  const std::wstring profile = L"nub-live-image-" + std::to_wstring(GetCurrentProcessId()) + L"-" + std::to_wstring(GetTickCount64());
  const std::wstring image_exe = sibling(executable, L"image-fixture.exe"), image_dll = sibling(executable, L"image-fixture.dll"), source = sibling(executable, L"existing-readable.txt"), nonmatch = sibling(executable, L"image-nonmatch.txt");
  PSID package_sid = nullptr; bool profile_created = false; PSECURITY_DESCRIPTOR original_exe_dacl = nullptr; PACL original_exe_acl = nullptr; std::wstring root; PROCESS_INFORMATION process = {}; Handle stdin_read, stdin_write, stdout_read, stdout_write, job; bool child_reaped = false; int result = 1;
  do {
  NtApi api; if (!api.load() || !unconfined_exe_control(image_exe) || !unconfined_dll_control(image_dll)) break;
  const HRESULT profile_status = CreateAppContainerProfile(profile.c_str(), profile.c_str(), profile.c_str(), nullptr, 0, &package_sid);
  if (FAILED(profile_status) || !package_sid) { std::printf("PROFILE_CREATE=FAIL hr=0x%08lx\n", static_cast<unsigned long>(profile_status)); break; }
  profile_created = true; std::printf("PROFILE_CREATE=OK profile=%ls\n", profile.c_str());
  if (!grant_executable(executable.c_str(), package_sid, &original_exe_dacl, &original_exe_acl)) { std::printf("EXECUTABLE_GRANT=FAIL error=%lu\n", GetLastError()); break; }
  std::printf("EXECUTABLE_GRANT=FILE_ONLY CAPABILITIES=0\n");
  if (!no_package_sid_ace(image_exe, package_sid) || !no_package_sid_ace(image_dll, package_sid) || !no_package_sid_ace(source, package_sid) || !no_package_sid_ace(nonmatch, package_sid)) { std::printf("IMAGE_ACL=UNEXPECTED_PACKAGE_SID\n"); break; }
  std::printf("IMAGE_ACL=NO_PACKAGE_SID\n");
  if (!parent_appcontainer_image_control(image_exe, package_sid)) break;
  wchar_t temp[MAX_PATH] = {}; if (!GetTempPathW(MAX_PATH, temp)) break;
  root = std::wstring(temp) + L"nub-live-image-" + std::to_wstring(GetCurrentProcessId());
  if (!CreateDirectoryW(root.c_str(), nullptr) || !CreateDirectoryW(join(root, L"output").c_str(), nullptr) || !CreateDirectoryW(join(root, L"outside").c_str(), nullptr) || !no_package_sid_ace(root, package_sid)) { std::printf("TARGET_ROOT=FAIL error=%lu\n", GetLastError()); break; }
  std::printf("TARGET_ROOT_ACL=NO_PACKAGE_SID\n");
  job.value = CreateJobObjectW(nullptr, nullptr);
  JOBOBJECT_EXTENDED_LIMIT_INFORMATION limits = {};
  limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
  if (!job.value || !SetInformationJobObject(job.value, JobObjectExtendedLimitInformation, &limits, sizeof(limits))) {
    std::printf("CHILD_JOB=FAIL error=%lu\n", GetLastError());
    break;
  }
  std::printf("CHILD_JOB=OK\n");
  SECURITY_ATTRIBUTES inheritable = {sizeof(inheritable), nullptr, TRUE}; HANDLE raw_stdin_read = nullptr, raw_stdin_write = nullptr, raw_stdout_read = nullptr, raw_stdout_write = nullptr;
  if (!CreatePipe(&raw_stdin_read, &raw_stdin_write, &inheritable, 0) || !CreatePipe(&raw_stdout_read, &raw_stdout_write, &inheritable, 0) || !SetHandleInformation(raw_stdin_write, HANDLE_FLAG_INHERIT, 0) || !SetHandleInformation(raw_stdout_read, HANDLE_FLAG_INHERIT, 0)) break;
  stdin_read.value = raw_stdin_read; stdin_write.value = raw_stdin_write; stdout_read.value = raw_stdout_read; stdout_write.value = raw_stdout_write;
  HANDLE inherited[] = {stdin_read.value, stdout_write.value}; SECURITY_CAPABILITIES capabilities = {package_sid, nullptr, 0, 0}; AttributeList attributes(2);
  if (!attributes.value || !UpdateProcThreadAttribute(attributes.value, 0, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, &capabilities, sizeof(capabilities), nullptr, nullptr) || !UpdateProcThreadAttribute(attributes.value, 0, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, inherited, sizeof(inherited), nullptr, nullptr)) break;
  STARTUPINFOEXW startup = {}; startup.StartupInfo.cb = sizeof(startup); startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES; startup.StartupInfo.hStdInput = stdin_read.value; startup.StartupInfo.hStdOutput = stdout_write.value; startup.StartupInfo.hStdError = stdout_write.value; startup.lpAttributeList = attributes.value;
  std::wstring command = L"\"" + executable + L"\" --child \"" + root + L"\" \"" + image_exe + L"\" \"" + image_dll + L"\" \"" + source + L"\" \"" + nonmatch + L"\""; std::vector<wchar_t> command_line(command.begin(), command.end()); command_line.push_back(L'\0');
  if (!CreateProcessW(executable.c_str(), command_line.data(), nullptr, nullptr, TRUE, EXTENDED_STARTUPINFO_PRESENT | CREATE_NO_WINDOW, nullptr, nullptr, &startup.StartupInfo, &process)) { std::printf("APPCONTAINER_LAUNCH=FAIL error=%lu\n", GetLastError()); break; }
  if (!AssignProcessToJobObject(job.value, process.hProcess)) { std::printf("CHILD_JOB_ASSIGN=FAIL error=%lu\n", GetLastError()); break; }
  std::printf("CHILD_JOB_ASSIGN=OK\n");
  std::printf("APPCONTAINER_LAUNCH=OK pid=%lu\n", process.dwProcessId); CloseHandle(stdin_read.release()); CloseHandle(stdout_write.release());
  char ready[4096] = {}; DWORD count = 0; if (!ReadFile(stdout_read.value, ready, sizeof(ready) - 1, &count, nullptr) || std::strstr(ready, "READY pid=") == nullptr) { std::printf("CHILD_READY=FAIL text=%s\n", ready); break; }
  std::printf("CHILD_READY=OK %s", ready);
  const std::wstring future = join(join(root, L"output"), L"future.json"); Handle read_source(CreateFileW(source.c_str(), GENERIC_READ, FILE_SHARE_READ, nullptr, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, nullptr)); Handle future_file(CreateFileW(future.c_str(), GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE, nullptr, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, nullptr)); Handle exe_file(CreateFileW(image_exe.c_str(), GENERIC_READ | GENERIC_EXECUTE, FILE_SHARE_READ, nullptr, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, nullptr)); Handle dll_file(CreateFileW(image_dll.c_str(), GENERIC_READ | GENERIC_EXECUTE, FILE_SHARE_READ, nullptr, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, nullptr)); Handle exe_section, dll_section;
  if (read_source.value == INVALID_HANDLE_VALUE || future_file.value == INVALID_HANDLE_VALUE || exe_file.value == INVALID_HANDLE_VALUE || dll_file.value == INVALID_HANDLE_VALUE || !create_image_section(api, exe_file.value, &exe_section) || !create_image_section(api, dll_file.value, &dll_section)) { std::printf("PARENT_CAPABILITY_CREATE=FAIL error=%lu\n", GetLastError()); break; }
  HANDLE child_read = nullptr, child_write = nullptr, child_exe_file = nullptr, child_dll_file = nullptr, child_exe_section = nullptr, child_dll_section = nullptr;
  if (!duplicate_to_child(read_source.value, process.hProcess, GENERIC_READ, &child_read) || !duplicate_to_child(future_file.value, process.hProcess, GENERIC_WRITE, &child_write) || !duplicate_to_child(exe_file.value, process.hProcess, GENERIC_READ | GENERIC_EXECUTE, &child_exe_file) || !duplicate_to_child(dll_file.value, process.hProcess, GENERIC_READ | GENERIC_EXECUTE, &child_dll_file) || !duplicate_to_child(exe_section.value, process.hProcess, kSectionMapRead | kSectionMapExecute | kSectionQuery, &child_exe_section) || !duplicate_to_child(dll_section.value, process.hProcess, kSectionMapRead | kSectionMapExecute | kSectionQuery, &child_dll_section)) { std::printf("HANDLE_TRANSFER=FAIL error=%lu\n", GetLastError()); break; }
  CloseHandle(read_source.release()); CloseHandle(future_file.release()); CloseHandle(exe_file.release()); CloseHandle(dll_file.release()); CloseHandle(exe_section.release()); CloseHandle(dll_section.release());
  const std::string controls = "READ " + std::to_string(reinterpret_cast<uintptr_t>(child_read)) + "\nWRITE " + std::to_string(reinterpret_cast<uintptr_t>(child_write)) + "\nEXE_FILE " + std::to_string(reinterpret_cast<uintptr_t>(child_exe_file)) + "\nDLL_FILE " + std::to_string(reinterpret_cast<uintptr_t>(child_dll_file)) + "\nEXE_SECTION " + std::to_string(reinterpret_cast<uintptr_t>(child_exe_section)) + "\nDLL_SECTION " + std::to_string(reinterpret_cast<uintptr_t>(child_dll_section)) + "\n";
  DWORD sent = 0; if (!WriteFile(stdin_write.value, controls.data(), static_cast<DWORD>(controls.size()), &sent, nullptr) || sent != controls.size()) { std::printf("CONTROL_WRITE=FAIL error=%lu bytes=%lu\n", GetLastError(), sent); break; }
  std::printf("CONTROL_WRITE=OK bytes=%lu\n", sent);
  CloseHandle(stdin_write.release());
  if (WaitForSingleObject(process.hProcess, 30000) == WAIT_TIMEOUT) TerminateJobObject(job.value, 1);
  child_reaped = WaitForSingleObject(process.hProcess, 10000) == WAIT_OBJECT_0;
  DWORD child_exit = 1;
  if (child_reaped) GetExitCodeProcess(process.hProcess, &child_exit);
  std::printf("CHILD_TREE_REAP=%s exit=%lu\n", child_reaped ? "OK" : "FAIL", child_exit);
  if (!child_reaped) break;
  char output[8192] = {}; DWORD output_count = 0; ReadFile(stdout_read.value, output, sizeof(output) - 1, &output_count, nullptr); std::printf("CHILD_OUTPUT=%s", output);
  char contents[64] = {}; Handle verify(CreateFileW(future.c_str(), GENERIC_READ, FILE_SHARE_READ, nullptr, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, nullptr)); DWORD read = 0;
  if (verify.value == INVALID_HANDLE_VALUE || !ReadFile(verify.value, contents, sizeof(contents) - 1, &read, nullptr) || child_exit != 0 || std::strcmp(contents, "brokered-write\n") != 0 || std::strstr(output, "CHILD_RESULT=PASS") == nullptr) { std::printf("RESULT=FAIL child_exit=%lu bytes=%lu content=%s\n", child_exit, read, contents); break; }
  std::printf("RESULT=PASS child_exit=%lu\n", child_exit); result = 0;
  } while (false);
  if (process.hProcess) {
    if (!child_reaped) {
      TerminateJobObject(job.value, 1);
      child_reaped = WaitForSingleObject(process.hProcess, 10000) == WAIT_OBJECT_0;
      std::printf("CHILD_TREE_REAP=%s exit=forced\n", child_reaped ? "OK" : "FAIL");
    }
    CloseHandle(process.hThread);
    CloseHandle(process.hProcess);
  }
  bool cleanup_ok = child_reaped || !process.hProcess;
  if (original_exe_dacl) { const DWORD restore = SetNamedSecurityInfoW(const_cast<LPWSTR>(executable.c_str()), SE_FILE_OBJECT, DACL_SECURITY_INFORMATION, nullptr, nullptr, original_exe_acl, nullptr); std::printf("EXECUTABLE_ACL_RESTORE=%s error=%lu\n", restore == ERROR_SUCCESS ? "OK" : "FAIL", restore); cleanup_ok = cleanup_ok && restore == ERROR_SUCCESS; LocalFree(original_exe_dacl); }
  if (!root.empty()) {
    DeleteFileW(join(join(root, L"output"), L"future.json").c_str());
    RemoveDirectoryW(join(root, L"output").c_str());
    RemoveDirectoryW(join(root, L"outside").c_str());
    const BOOL removed = child_reaped && RemoveDirectoryW(root.c_str());
    const DWORD root_error = removed ? ERROR_SUCCESS : (child_reaped ? GetLastError() : ERROR_BUSY);
    std::printf("TEMP_ROOT_CLEANUP=%s error=%lu\n", removed ? "OK" : "FAIL", root_error);
    cleanup_ok = cleanup_ok && removed;
  }
  if (package_sid) FreeSid(package_sid); if (profile_created) { const HRESULT deleted = DeleteAppContainerProfile(profile.c_str()); std::printf("PROFILE_CLEANUP=%s hr=0x%08lx\n", SUCCEEDED(deleted) ? "OK" : "FAIL", static_cast<unsigned long>(deleted)); cleanup_ok = cleanup_ok && SUCCEEDED(deleted); }
  return cleanup_ok ? result : 1;
}

}  // namespace

int wmain(int argc, wchar_t** argv) {
  if (argc == 7 && std::wcscmp(argv[1], L"--child") == 0) return child(argv[2], argv[3], argv[4], argv[5], argv[6]) ? 0 : 2;
  if (argc != 1) return 64;
  wchar_t executable[MAX_PATH] = {}; const DWORD size = GetModuleFileNameW(nullptr, executable, MAX_PATH);
  return size == 0 || size == MAX_PATH ? 65 : parent(executable);
}
