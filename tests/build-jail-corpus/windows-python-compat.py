"""Callable contract checks for the Windows Python startup adapter.

These use stand-in os.path implementations so one current interpreter verifies the
legacy (one-argument) and modern realpath signatures without requiring a mutable
installed Python or an AppContainer token.
"""

import io
import json
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


def gyp_current_directory_base_is_canonicalized():
    root = r"C:\store\cpu-features"

    def fallback_realpath(path):
        if path == ".":
            return root + r"\."
        if path == "deps\cpu_features":
            return root + r"\deps\cpu_features"
        raise AssertionError(path)

    realpath = install_realpath(fallback_realpath)
    assert gyp_relative_path("deps\cpu_features", ".", realpath) == r"deps\cpu_features"


def gyp_trace_is_unconditional_for_the_entrypoint():
    output = io.StringIO()
    calls = []

    def original_relative_path(path, relative_to, follow_path_symlink=True):
        calls.append((path, relative_to, follow_path_symlink))
        return r"..\.."

    gyp_common = SimpleNamespace(RelativePath=original_relative_path)
    fake_os = SimpleNamespace(path=SimpleNamespace(realpath=lambda path: path))
    fake_sys = SimpleNamespace(
        argv=[r"C:\node-gyp\gyp_main.py"], stderr=output,
    )

    ADAPTER["_trace_gyp_realpaths"](fake_os, fake_sys, gyp_common)
    assert gyp_common.RelativePath(
        r"C:\project\cpu_features", r"C:\project\build\."
    ) == r"..\.."
    assert calls == [
        (r"C:\project\cpu_features", r"C:\project\build\.", True),
    ]
    trace = json.loads(output.getvalue().removeprefix("NUB_GYP_REALPATH "))
    assert trace == {
        "path": r"C:\project\cpu_features",
        "path_realpath": r"C:\project\cpu_features",
        "relative_to": r"C:\project\build\.",
        "relative_to_realpath": r"C:\project\build\.",
        "result": r"..\..",
    }


old_python_and_pathlike_call_once()
strict_and_bytes_contract()
audit_is_optional_on_pre_38_python()
gyp_current_directory_base_is_canonicalized()
gyp_trace_is_unconditional_for_the_entrypoint()
