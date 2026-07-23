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
  return `@jdxcode/aube-node-${os}-${cpu}${libc ? `-${libc}` : ""}`
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

if (process.env.AUBE_NODE_BINARY) {
  const os = process.env.AUBE_NODE_OS
  const cpu = process.env.AUBE_NODE_CPU
  const libc = process.env.AUBE_NODE_LIBC || undefined
  if (!targets.some((item) => item[0] === os && item[1] === cpu && item[2] === libc)) throw new Error("invalid target")
  const stage = resolve(out, "stage-platform")
  rmSync(stage, { recursive: true, force: true })
  mkdirSync(stage, { recursive: true })
  cpSync(process.env.AUBE_NODE_BINARY, resolve(stage, "aube.node"))
  cpSync(resolve(here, "../README.md"), resolve(stage, "README.md"))
  writeFileSync(resolve(stage, "package.json"), JSON.stringify({
    name: name(os, cpu, libc), version,
    description: "Platform addon for @jdxcode/aube-node; install the root package instead",
    license: "MIT", repository: "https://github.com/jdx/aube", main: "aube.node",
    files: ["aube.node", "README.md"], preferUnplugged: true,
    os: [os], cpu: [cpu], ...(libc ? { libc: [libc] } : {}),
  }, null, 2) + "\n")
  pack(stage)
} else {
  const stage = resolve(out, "stage-root")
  rmSync(stage, { recursive: true, force: true })
  mkdirSync(stage, { recursive: true })
  cpSync(resolve(here, "index.js"), resolve(stage, "index.js"))
  cpSync(resolve(here, "index.d.ts"), resolve(stage, "index.d.ts"))
  cpSync(resolve(here, "bun-plugin.js"), resolve(stage, "bun-plugin.js"))
  cpSync(resolve(here, "bun-plugin.d.ts"), resolve(stage, "bun-plugin.d.ts"))
  cpSync(resolve(here, "../README.md"), resolve(stage, "README.md"))
  const manifest = JSON.parse(readFileSync(resolve(here, "package.json"), "utf8"))
  manifest.version = version
  manifest.optionalDependencies = Object.fromEntries(targets.map((target) => [name(...target), version]))
  writeFileSync(resolve(stage, "package.json"), JSON.stringify(manifest, null, 2) + "\n")
  pack(stage)
}
