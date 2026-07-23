#!/usr/bin/env node
import { cpSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs"
import { dirname, resolve } from "node:path"
import { fileURLToPath } from "node:url"
import { spawnSync } from "node:child_process"

const here = dirname(fileURLToPath(import.meta.url))
const version = process.env.VERSION
const out = resolve(process.env.OUT_DIR || resolve(here, "dist"))
const targets = [
  ["darwin", "arm64"], ["darwin", "x64"],
  ["linux", "arm64"], ["linux", "x64"],
  ["linux", "arm64", "musl"], ["linux", "x64", "musl"],
  ["win32", "arm64"], ["win32", "x64"],
]
if (!version || !/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/.test(version)) throw new Error("VERSION must be SemVer")
mkdirSync(out, { recursive: true })

function name(os, cpu, libc) {
  return `@jdxcode/aube-ffi-${os}-${cpu}${libc ? `-${libc}` : ""}`
}
function libraryName(os) {
  if (os === "darwin") return "libaube.dylib"
  if (os === "win32") return "aube.dll"
  return "libaube.so"
}
function pack(dir) {
  const args = ["pack", "--pack-destination", out]
  const npm = process.platform === "win32"
    ? resolve(dirname(process.execPath), "node_modules", "npm", "bin", "npm-cli.js")
    : "npm"
  if (process.platform === "win32") args.unshift(npm)
  const result = spawnSync(process.platform === "win32" ? process.execPath : npm, args, {
    cwd: dir,
    stdio: "inherit",
  })
  if (result.status !== 0) throw new Error(`npm pack failed: ${result.error?.message ?? result.status}`)
}

if (process.env.AUBE_FFI_BINARY) {
  const os = process.env.AUBE_FFI_OS
  const cpu = process.env.AUBE_FFI_CPU
  const libc = process.env.AUBE_FFI_LIBC || undefined
  const target = process.env.AUBE_FFI_TARGET
  if (!targets.some((item) => item[0] === os && item[1] === cpu && item[2] === libc)) throw new Error("invalid target")
  if (!target) throw new Error("AUBE_FFI_TARGET is required")
  const stage = resolve(out, "stage-platform")
  const library = libraryName(os)
  rmSync(stage, { recursive: true, force: true })
  mkdirSync(stage, { recursive: true })
  cpSync(process.env.AUBE_FFI_BINARY, resolve(stage, library))
  cpSync(resolve(here, "../README.md"), resolve(stage, "README.md"))
  writeFileSync(resolve(stage, "index.js"), [
    'const path = require("node:path")',
    `exports.libraryPath = path.join(__dirname, ${JSON.stringify(library)})`,
    `exports.target = ${JSON.stringify(target)}`,
    "",
  ].join("\n"))
  writeFileSync(resolve(stage, "package.json"), JSON.stringify({
    name: name(os, cpu, libc), version,
    description: "Platform library for @jdxcode/aube-ffi; install the root package instead",
    license: "MIT", repository: "https://github.com/jdx/aube", main: "index.js",
    files: ["index.js", library, "README.md"], preferUnplugged: true,
    os: [os], cpu: [cpu], ...(libc ? { libc: [libc] } : {}),
  }, null, 2) + "\n")
  pack(stage)
} else {
  const stage = resolve(out, "stage-root")
  rmSync(stage, { recursive: true, force: true })
  mkdirSync(stage, { recursive: true })
  cpSync(resolve(here, "index.js"), resolve(stage, "index.js"))
  cpSync(resolve(here, "index.d.ts"), resolve(stage, "index.d.ts"))
  cpSync(resolve(here, "../include/aube.h"), resolve(stage, "aube.h"))
  cpSync(resolve(here, "../README.md"), resolve(stage, "README.md"))
  const manifest = JSON.parse(readFileSync(resolve(here, "package.json"), "utf8"))
  manifest.version = version
  manifest.optionalDependencies = Object.fromEntries(targets.map((target) => [name(...target), version]))
  writeFileSync(resolve(stage, "package.json"), JSON.stringify(manifest, null, 2) + "\n")
  pack(stage)
}
