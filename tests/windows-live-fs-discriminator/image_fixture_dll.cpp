#include <windows.h>

namespace {

int g_dllmain_calls = 0;

}  // namespace

BOOL WINAPI DllMain(HINSTANCE, DWORD reason, LPVOID) {
  if (reason == DLL_PROCESS_ATTACH) {
    ++g_dllmain_calls;
  }
  return TRUE;
}

extern "C" __declspec(dllexport) int image_fixture_value() {
  return 0x472;
}

extern "C" __declspec(dllexport) int image_fixture_dllmain_calls() {
  return g_dllmain_calls;
}
