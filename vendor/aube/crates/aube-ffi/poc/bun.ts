import { CString, dlopen, ptr } from "bun:ffi"
import { createHash } from "node:crypto"
import { access, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises"
import { tmpdir } from "node:os"
import path from "node:path"

const libraryPath = process.env.AUBE_FFI_LIBRARY
if (!libraryPath) throw new Error("AUBE_FFI_LIBRARY is required")

const library = dlopen(libraryPath, {
  aube_init: { args: ["ptr"], returns: "i32" },
  aube_install: { args: ["ptr", "ptr", "ptr"], returns: "u64" },
  aube_add: { args: ["ptr", "ptr", "ptr", "ptr", "ptr"], returns: "u64" },
  aube_wait: { args: ["u64"], returns: "ptr" },
  aube_events_next: { args: ["u64"], returns: "ptr" },
  aube_cancel: { args: ["u64"], returns: "i32" },
  aube_string_free: { args: ["ptr"], returns: "void" },
})

function cstring(value: string) {
  return Buffer.from(`${value}\0`, "utf8")
}

function wait(handle: bigint) {
  const resultPointer = library.symbols.aube_wait(handle)
  if (!resultPointer) throw new Error("aube_wait returned null")
  const result = JSON.parse(new CString(resultPointer).toString()) as {
    ok: boolean
    code?: string
    message?: string
  }
  library.symbols.aube_string_free(resultPointer)
  return result
}

const host = cstring(JSON.stringify({ name: "aube-ffi-bun", version: "1.0.0" }))
if (library.symbols.aube_init(ptr(host)) !== 0) throw new Error("aube_init failed")

const project = await mkdtemp(path.join(tmpdir(), "aube-ffi-bun-"))
const seedProject = await mkdtemp(path.join(tmpdir(), "aube-ffi-bun-seed-"))
const offlineProject = await mkdtemp(path.join(tmpdir(), "aube-ffi-bun-offline-"))
const registryDir = await mkdtemp(path.join(tmpdir(), "aube-ffi-bun-registry-"))
const cacheDir = await mkdtemp(path.join(tmpdir(), "aube-ffi-bun-cache-"))
let registry: ReturnType<typeof Bun.spawn> | undefined
try {
  const dependency = path.join(project, "local-dependency")
  await mkdir(dependency)
  await Promise.all([
    writeFile(path.join(dependency, "package.json"), JSON.stringify({ name: "local-dependency", version: "1.0.0" })),
    writeFile(path.join(project, "package.json"), JSON.stringify({ dependencies: { "local-dependency": "file:./local-dependency" } })),
  ])

  const installOptions = cstring(JSON.stringify({ projectDir: project, offline: true, ignoreScripts: true }))
  const installResult = wait(library.symbols.aube_install(ptr(installOptions), 0, 0))
  if (!installResult.ok) throw new Error(`offline install failed: ${JSON.stringify(installResult)}`)
  await access(path.join(project, "node_modules", "local-dependency"))

  const polledOptions = cstring(
    JSON.stringify({ projectDir: project, offline: true, ignoreScripts: true, force: true, bufferEvents: true }),
  )
  const polledHandle = library.symbols.aube_install(ptr(polledOptions), 0, 0)
  const polled: string[] = []
  for (let attempt = 0; attempt < 600; attempt++) {
    for (;;) {
      const event = library.symbols.aube_events_next(polledHandle)
      if (!event) break
      polled.push(new CString(event).toString())
      library.symbols.aube_string_free(event)
    }
    if (polled.some((event) => event.includes('"phase":"complete"'))) break
    await new Promise((resolve) => setTimeout(resolve, 10))
  }
  if (!polled.some((event) => event.includes('"phase":"complete"'))) {
    throw new Error(`polled events missing completion: ${JSON.stringify(polled)}`)
  }
  const polledResult = wait(polledHandle)
  if (!polledResult.ok) throw new Error(`polled install failed: ${JSON.stringify(polledResult)}`)
  if (library.symbols.aube_events_next(polledHandle)) {
    throw new Error("events_next should be null after wait consumed the handle")
  }

  const malformed = cstring("{")
  const malformedResult = wait(library.symbols.aube_install(ptr(malformed), 0, 0))
  if (malformedResult.code !== "ERR_AUBE_FFI_INVALID_ARGUMENT") {
    throw new Error(`missing structured FFI code: ${JSON.stringify(malformedResult)}`)
  }

  const packageDir = path.join(registryDir, "package")
  await mkdir(packageDir)
  await writeFile(
    path.join(packageDir, "package.json"),
    JSON.stringify({ name: "aube-ffi-cached-package", version: "1.0.0" }),
  )
  const packed = Bun.spawnSync(
    ["npm", "pack", "--pack-destination", registryDir],
    { cwd: packageDir, stdout: "pipe", stderr: "pipe" },
  )
  if (packed.exitCode !== 0) throw new Error(packed.stderr.toString())
  const tarballName = packed.stdout.toString().trim().split("\n").at(-1)
  if (!tarballName) throw new Error("npm pack did not return a tarball name")
  const tarballPath = path.join(registryDir, tarballName)
  const tarball = await readFile(tarballPath)
  const integrity = `sha512-${createHash("sha512").update(tarball).digest("base64")}`
  registry = Bun.spawn(["node", path.join(import.meta.dir, "registry.mjs")], {
    env: {
      ...process.env,
      AUBE_FFI_TARBALL: tarballPath,
      AUBE_FFI_INTEGRITY: integrity,
    },
    stdout: "pipe",
    stderr: "inherit",
  })
  const reader = registry.stdout.getReader()
  const portChunk = await reader.read()
  const port = new TextDecoder().decode(portChunk.value).trim()
  if (!port) throw new Error("registry fixture did not report a port")
  const npmrc = `registry=http://127.0.0.1:${port}/\ncache-dir=${cacheDir}\nminimum-release-age=0\n`
  await Promise.all([
    writeFile(path.join(seedProject, "package.json"), "{}\n"),
    writeFile(path.join(seedProject, ".npmrc"), npmrc),
    writeFile(path.join(offlineProject, "package.json"), "{}\n"),
    writeFile(path.join(offlineProject, ".npmrc"), npmrc),
  ])
  const packageList = cstring(JSON.stringify(["aube-ffi-cached-package@1.0.0"]))
  const exactAdd = cstring(JSON.stringify({ saveExact: true }))
  const seedProjectString = cstring(seedProject)
  const seedResult = wait(
    library.symbols.aube_add(ptr(seedProjectString), ptr(packageList), ptr(exactAdd), 0, 0),
  )
  if (!seedResult.ok) throw new Error(`cache seed failed: ${JSON.stringify(seedResult)}`)
  registry.kill()
  await registry.exited
  registry = undefined

  const offlineProjectString = cstring(offlineProject)
  const offlineAdd = cstring(JSON.stringify({ saveExact: true, offline: true }))
  const cachedResult = wait(
    library.symbols.aube_add(ptr(offlineProjectString), ptr(packageList), ptr(offlineAdd), 0, 0),
  )
  if (!cachedResult.ok) throw new Error(`cached offline add failed: ${JSON.stringify(cachedResult)}`)
  await access(path.join(offlineProject, "node_modules", "aube-ffi-cached-package"))

  await writeFile(path.join(project, ".npmrc"), "registry=http://10.255.255.1/\nfetch-timeout=60000\n")
  const projectString = cstring(project)
  const packages = cstring(JSON.stringify(["aube-ffi-bun-cancel-test"]))
  const addOptions = cstring("{}")
  const handle = library.symbols.aube_add(ptr(projectString), ptr(packages), ptr(addOptions), 0, 0)
  if (library.symbols.aube_cancel(handle) !== 0) throw new Error("aube_cancel failed")
  const cancelled = wait(handle)
  if (cancelled.code !== "ERR_AUBE_INSTALL_CANCELLED") {
    throw new Error(`unexpected cancellation result: ${JSON.stringify(cancelled)}`)
  }

  console.log("Bun FFI smoke passed")
} finally {
  registry?.kill()
  library.close()
  await Promise.all(
    [project, seedProject, offlineProject, registryDir, cacheDir].map((directory) =>
      rm(directory, { recursive: true, force: true }),
    ),
  )
}
