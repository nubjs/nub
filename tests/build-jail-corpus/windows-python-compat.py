"""Callable contract checks for the Windows Python startup adapter.

These use stand-in os.path implementations so one current interpreter verifies the
legacy (one-argument) and modern realpath signatures without requiring a mutable
installed Python or an AppContainer token.
"""

import os
from pathlib import Path
import runpy
from types import SimpleNamespace


SOURCE = Path(__file__).resolve().parents[2] / "crates/nub-sandbox/src/backend/windows_python_compat.py"
ADAPTER = runpy.run_path(str(SOURCE))


def install_realpath(realpath):
    fake_os = SimpleNamespace(
        path=SimpleNamespace(realpath=realpath),
        fspath=os.fspath,
        fsencode=os.fsencode,
    )
    ADAPTER["_install_realpath_compat"](fake_os)
    return fake_os.path.realpath


class DotPath:
    def __init__(self):
        self.calls = 0

    def __fspath__(self):
        self.calls += 1
        return "."


def old_python_and_pathlike_call_once():
    received = []

    def old_realpath(path):
        received.append(path)
        return r"C:\cwd\."

    realpath = install_realpath(old_realpath)
    path = DotPath()
    assert realpath(path) == r"C:\cwd"
    assert path.calls == 1
    assert received == ["."]


def strict_and_bytes_contract():
    strict_calls = []

    def modern_realpath(path, *, strict=False):
        strict_calls.append(strict)
        if strict is True:
            raise FileNotFoundError(path)
        return b"C:\\cwd\\." if isinstance(path, bytes) else r"C:\cwd\."

    realpath = install_realpath(modern_realpath)
    assert realpath(".", strict=False) == r"C:\cwd"
    assert realpath(b".") == b"C:\\cwd"
    marker = object()
    assert realpath(".", strict=marker) == r"C:\cwd\."
    try:
        realpath(".", strict=True)
    except FileNotFoundError:
        pass
    else:
        raise AssertionError("strict=True must retain the original error")
    assert strict_calls == [False, False, marker, True]


def audit_is_optional_on_pre_38_python():
    ADAPTER["_audit_mkdir"](SimpleNamespace(), "private", 0o700)
    calls = []
    ADAPTER["_audit_mkdir"](
        SimpleNamespace(audit=lambda *args: calls.append(args)), "private", 0o700
    )
    assert calls == [("os.mkdir", "private", 0o700, -1)]


def gyp_relative_path(path, relative_to, realpath):
    path = realpath(path)
    relative_to = realpath(relative_to)
    path_split = path.split("\\")
    relative_to_split = relative_to.split("\\")
    prefix_len = len(os.path.commonprefix([path_split, relative_to_split]))
    relative_split = [".."] * (len(relative_to_split) - prefix_len)
    relative_split.extend(path_split[prefix_len:])
    return "\\".join(relative_split)


def gyp_empty_current_directory_base_is_canonicalized():
    root = r"C:\store\cpu-features"

    def fallback_realpath(path):
        if path == "":
            return root + r"\."
        if path == r"deps\cpu_features":
            return root + r"\deps\cpu_features"
        raise AssertionError(path)

    realpath = install_realpath(fallback_realpath)
    assert gyp_relative_path(r"deps\cpu_features", "", realpath) == r"deps\cpu_features"


def drive_root_fallback_retains_its_separator():
    def fallback_realpath(path):
        return b"C:\\." if isinstance(path, bytes) else r"C:\."

    realpath = install_realpath(fallback_realpath)
    assert realpath("") == "C:\\"
    assert realpath(b"") == b"C:\\"


old_python_and_pathlike_call_once()
strict_and_bytes_contract()
audit_is_optional_on_pre_38_python()
gyp_empty_current_directory_base_is_canonicalized()
drive_root_fallback_retains_its_separator()
