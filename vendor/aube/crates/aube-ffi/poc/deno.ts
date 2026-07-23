const libraryPath = Deno.env.get("AUBE_FFI_LIBRARY")
if (!libraryPath) throw new Error("AUBE_FFI_LIBRARY is required")

const library = Deno.dlopen(libraryPath, {
  aube_init: { parameters: ["buffer"], result: "i32" },
  aube_install: { parameters: ["buffer", "function", "pointer"], result: "u64" },
  aube_add: { parameters: ["buffer", "buffer", "buffer", "function", "pointer"], result: "u64" },
  aube_wait: { parameters: ["u64"], result: "pointer", nonblocking: true },
  aube_cancel: { parameters: ["u64"], result: "i32" },
  aube_string_free: { parameters: ["pointer"], result: "void" },
} as const)

const encoder = new TextEncoder()
function cstring(value: string) {
  return encoder.encode(`${value}\0`)
}

async function wait(handle: bigint) {
  const resultPointer = await library.symbols.aube_wait(handle)
  if (!resultPointer) throw new Error("aube_wait returned null")
  const result = JSON.parse(new Deno.UnsafePointerView(resultPointer).getCString()) as {
    ok: boolean
    code?: string
    message?: string
  }
  library.symbols.aube_string_free(resultPointer)
  return result
}

const host = cstring(JSON.stringify({ name: "aube-ffi-deno", version: "1.0.0" }))
if (library.symbols.aube_init(host) !== 0) throw new Error("aube_init failed")

const project = await Deno.makeTempDir({ prefix: "aube-ffi-deno-" })
const events: unknown[] = []
const callback = Deno.UnsafeCallback.threadSafe(
  { parameters: ["pointer", "pointer"], result: "void" } as const,
  (event) => {
    if (event) events.push(JSON.parse(new Deno.UnsafePointerView(event).getCString()))
  },
)

try {
  const dependency = `${project}/local-dependency`
  await Deno.mkdir(dependency)
  await Promise.all([
    Deno.writeTextFile(`${dependency}/package.json`, JSON.stringify({ name: "local-dependency", version: "1.0.0" })),
    Deno.writeTextFile(`${project}/package.json`, JSON.stringify({ dependencies: { "local-dependency": "file:./local-dependency" } })),
  ])

  const installOptions = cstring(JSON.stringify({ projectDir: project, offline: true, ignoreScripts: true }))
  const installResult = await wait(library.symbols.aube_install(installOptions, callback.pointer, null))
  if (!installResult.ok) throw new Error(`offline install failed: ${JSON.stringify(installResult)}`)
  await Deno.stat(`${project}/node_modules/local-dependency`)
  if (!events.some((event) => (event as { kind?: string; phase?: string }).kind === "phase" && (event as { phase?: string }).phase === "complete")) {
    throw new Error(`completion event missing: ${JSON.stringify(events)}`)
  }

  await Deno.writeTextFile(`${project}/.npmrc`, "registry=http://10.255.255.1/\nfetch-timeout=60000\n")
  const handle = library.symbols.aube_add(
    cstring(project),
    cstring(JSON.stringify(["aube-ffi-deno-cancel-test"])),
    cstring("{}"),
    null,
    null,
  )
  if (library.symbols.aube_cancel(handle) !== 0) throw new Error("aube_cancel failed")
  const cancelled = await wait(handle)
  if (cancelled.code !== "ERR_AUBE_INSTALL_CANCELLED") {
    throw new Error(`unexpected cancellation result: ${JSON.stringify(cancelled)}`)
  }

  console.log("Deno FFI smoke passed")
} finally {
  callback.close()
  library.close()
  await Deno.remove(project, { recursive: true })
}
