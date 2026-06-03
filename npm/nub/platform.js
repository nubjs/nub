"use strict";
// Platform → @nubjs/nub-<platform> package selection, shared by postinstall.js
// (install-time symlink/copy of the native binary) and bin/launch.js (runtime
// spawn). Keep the musl detection in ONE place — it shipped wrong in 0.0.10
// duplicated across both files (see isMusl below).

const PLATFORMS = {
  "darwin-arm64": "@nubjs/nub-darwin-arm64",
  "darwin-x64": "@nubjs/nub-darwin-x64",
  "linux-x64": "@nubjs/nub-linux-x64",
  "linux-x64-musl": "@nubjs/nub-linux-x64-musl",
  "linux-arm64": "@nubjs/nub-linux-arm64",
  "linux-arm64-musl": "@nubjs/nub-linux-arm64-musl",
  "win32-x64": "@nubjs/nub-win32-x64",
  "win32-arm64": "@nubjs/nub-win32-arm64",
};

// True on a musl Linux (Alpine, etc.), where the `-musl` platform package is the
// one npm installs and the one the binary must come from.
//
// Primary signal: Node's own diagnostic report. `header.glibcVersionRuntime` is
// present on glibc and ABSENT on musl — no subprocess, and what detect-libc keys
// on. Fallback: `ldd --version`, which on musl exits NON-ZERO and prints
// "musl libc …". 0.0.10 shipped a detection that, in that throw path, checked only
// `e.stderr` — but with `2>&1` the text lands in `e.stdout`, so musl read as glibc
// and the launcher resolved the wrong (uninstalled) package. Check the merged output.
function isMusl() {
  if (process.platform !== "linux") return false;
  try {
    const report = process.report.getReport();
    const header = (typeof report === "string" ? JSON.parse(report) : report).header;
    if (header && "glibcVersionRuntime" in header) {
      return !header.glibcVersionRuntime;
    }
  } catch {
    // process.report unavailable — fall through to ldd.
  }
  try {
    const out = require("child_process").execSync("ldd --version 2>&1", { encoding: "utf8" });
    return out.includes("musl");
  } catch (e) {
    return (((e && e.stdout) || "") + ((e && e.stderr) || "")).includes("musl");
  }
}

// { key, pkg }. `pkg` is undefined for an unsupported platform.
function platformPackage() {
  const key = `${process.platform}-${process.arch}${isMusl() ? "-musl" : ""}`;
  return { key, pkg: PLATFORMS[key] };
}

module.exports = { PLATFORMS, isMusl, platformPackage };
