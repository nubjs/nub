"""Opt-in AppContainer support for Python's private directory creation.

Execute from an embedder-owned sitecustomize module. This is compatibility code,
not the security boundary; omitting it leaves the OS policy in force.
"""


def _install_realpath_compat(os):
    original_realpath = os.path.realpath
    dot_paths = (".", ".\\", ".\\.", "./", "./.")
    byte_dot_paths = tuple(os.fsencode(value) for value in dot_paths)

    def realpath(path, *args, **kwargs):
        # Normalize PathLike once before forwarding. A stateful __fspath__ implementation must
        # see exactly the same path in CPython and in the narrow classifier below.
        path = os.fspath(path)
        result = original_realpath(path, *args, **kwargs)
        # Under an AppContainer, CPython's non-strict fallback can reach the final
        # NT name but cannot translate it to a DOS name. For a CURRENT-DIRECTORY
        # spelling it consequently returns the lexical `...\\.` rather than the
        # canonical directory. Repair only those five equivalent dot spellings;
        # paths containing links, a parent component, or a missing leaf retain
        # CPython's own fallback unchanged. Do not introduce `strict` for Python
        # 3.6-3.9: only the caller's arguments are forwarded. Explicit False is
        # the default non-strict behavior; strict and ALLOW_MISSING stay untouched.
        if args or kwargs.get("strict", False) is not False:
            return result
        if path not in dot_paths + byte_dot_paths:
            return result
        suffix = b"\\." if isinstance(result, bytes) else "\\."
        return result[:-len(suffix)] if result.endswith(suffix) else result

    realpath._appcontainer_compatible = True
    os.path.realpath = realpath


def _audit_mkdir(sys, path, mode):
    audit = getattr(sys, "audit", None)
    if audit is not None:
        audit("os.mkdir", path, mode, -1)


def _trace_gyp_realpaths(os):
    # Diagnostic-only: the jail deliberately strips ambient variables before this
    # interpreter starts, so an outer-process opt-in cannot reach this point.
    # Restrict the hook to the GYP entry point and the measured cpu-features path;
    # this function is removed with the diagnostic rather than shipped.
    import json
    import sys
    if not sys.argv or not sys.argv[0].lower().endswith("gyp_main.py"):
        return
    import gyp.common
    original_relative_path = gyp.common.RelativePath

    def relative_path(path, relative_to, follow_path_symlink=True):
        result = original_relative_path(path, relative_to, follow_path_symlink)
        if "cpu_features" in str(path):
            print(
                "NUB_GYP_REALPATH " + json.dumps({
                    "path": path,
                    "relative_to": relative_to,
                    "path_realpath": os.path.realpath(path),
                    "relative_to_realpath": os.path.realpath(relative_to),
                    "result": result,
                }, sort_keys=True),
                file=sys.stderr,
                flush=True,
            )
        return result

    gyp.common.RelativePath = relative_path


def _install():
    import os
    if os.name != "nt" or getattr(os.mkdir, "_appcontainer_compatible", False):
        return

    import ctypes
    from ctypes import wintypes
    import sys

    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    advapi = ctypes.WinDLL("advapi32", use_last_error=True)
    kernel.GetCurrentProcess.restype = wintypes.HANDLE
    kernel.CloseHandle.argtypes = [wintypes.HANDLE]
    kernel.LocalFree.argtypes = [ctypes.c_void_p]
    kernel.LocalFree.restype = ctypes.c_void_p
    advapi.OpenProcessToken.argtypes = [wintypes.HANDLE, wintypes.DWORD,
                                        ctypes.POINTER(wintypes.HANDLE)]
    advapi.GetTokenInformation.argtypes = [wintypes.HANDLE, ctypes.c_int,
                                           ctypes.c_void_p, wintypes.DWORD,
                                           ctypes.POINTER(wintypes.DWORD)]
    advapi.ConvertSidToStringSidW.argtypes = [ctypes.c_void_p,
                                            ctypes.POINTER(wintypes.LPWSTR)]
    advapi.ConvertStringSecurityDescriptorToSecurityDescriptorW.argtypes = [
        wintypes.LPCWSTR, wintypes.DWORD, ctypes.POINTER(ctypes.c_void_p),
        ctypes.POINTER(wintypes.DWORD)]

    token = wintypes.HANDLE()
    if not advapi.OpenProcessToken(kernel.GetCurrentProcess(), 8, ctypes.byref(token)):
        raise ctypes.WinError(ctypes.get_last_error())
    try:
        size = wintypes.DWORD()
        advapi.GetTokenInformation(token, 31, None, 0, ctypes.byref(size))
        if not size.value:
            raise ctypes.WinError(ctypes.get_last_error())
        info = ctypes.create_string_buffer(size.value)
        if not advapi.GetTokenInformation(token, 31, info, size, ctypes.byref(size)):
            raise ctypes.WinError(ctypes.get_last_error())
        sid = ctypes.cast(info, ctypes.POINTER(ctypes.c_void_p))[0]
        if not sid:
            return  # Ordinary Python must retain its original directory behavior.
        text = wintypes.LPWSTR()
        if not advapi.ConvertSidToStringSidW(sid, ctypes.byref(text)):
            raise ctypes.WinError(ctypes.get_last_error())
        try:
            package_sid = text.value
        finally:
            kernel.LocalFree(text)
    finally:
        kernel.CloseHandle(token)

    _install_realpath_compat(os)
    _trace_gyp_realpaths(os)

    class SecurityAttributes(ctypes.Structure):
        _fields_ = [("length", wintypes.DWORD), ("descriptor", ctypes.c_void_p),
                    ("inherit", wintypes.BOOL)]

    kernel.CreateDirectoryW.argtypes = [wintypes.LPCWSTR,
                                        ctypes.POINTER(SecurityAttributes)]
    original = os.mkdir

    def mkdir(path, mode=0o777, *, dir_fd=None):
        import operator
        mode = operator.index(mode)
        if mode != 0o700 or dir_fd is not None:
            return original(path, mode, dir_fd=dir_fd)
        path = os.fspath(path)
        decoded = os.fsdecode(path)
        if "\0" in decoded:
            raise ValueError("embedded null character")
        _audit_mkdir(sys, path, mode)
        # Preserve CPython's protected owner/admin/system ACL. Add only this
        # process's package SID, never All Application Packages or inherited ACEs.
        sddl = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;OW)"
        sddl += f"(A;OICI;FA;;;{package_sid})"
        descriptor = ctypes.c_void_p()
        if not advapi.ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl, 1, ctypes.byref(descriptor), None):
            raise ctypes.WinError(ctypes.get_last_error())
        try:
            attributes = SecurityAttributes(ctypes.sizeof(SecurityAttributes), descriptor, False)
            if not kernel.CreateDirectoryW(decoded, ctypes.byref(attributes)):
                error = ctypes.get_last_error()
                raise OSError(error, ctypes.FormatError(error), path, error)
        finally:
            kernel.LocalFree(descriptor)

    mkdir._appcontainer_compatible = True
    os.mkdir = mkdir


_install()
del _install
