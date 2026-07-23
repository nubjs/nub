const fs = require("node:fs")
const path = require("node:path")
const childProcess = require("node:child_process")

function isMusl() {
  if (fs.existsSync("/etc/alpine-release")) return true
  try {
    if (process.report.getReport().header.glibcVersionRuntime) return false
  } catch (_) {}
  try {
    return childProcess.execFileSync("ldd", ["--version"], { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] })
      .toLowerCase().includes("musl")
  } catch (error) {
    return `${error.stdout || ""}\n${error.stderr || ""}`.toLowerCase().includes("musl")
  }
}

function platformPackage() {
  const key = `${process.platform}-${process.arch}`
  if (key === "darwin-arm64") return "@jdxcode/aube-ffi-darwin-arm64"
  if (key === "darwin-x64") return "@jdxcode/aube-ffi-darwin-x64"
  if (key === "win32-arm64") return "@jdxcode/aube-ffi-win32-arm64"
  if (key === "win32-x64") return "@jdxcode/aube-ffi-win32-x64"
  if (key === "linux-arm64" && isMusl()) return "@jdxcode/aube-ffi-linux-arm64-musl"
  if (key === "linux-arm64") return "@jdxcode/aube-ffi-linux-arm64"
  if (key === "linux-x64" && isMusl()) return "@jdxcode/aube-ffi-linux-x64-musl"
  if (key === "linux-x64") return "@jdxcode/aube-ffi-linux-x64"
  throw new Error(`Unsupported aube FFI platform: ${key}`)
}

const platform = require(platformPackage())
exports.libraryPath = platform.libraryPath
exports.headerPath = path.join(__dirname, "aube.h")
exports.target = platform.target
