#include <windows.h>

extern "C" __declspec(dllexport) int image_fixture_value() {
  return 0x472;
}
