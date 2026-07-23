import { createServer } from "node:http"
import { readFileSync } from "node:fs"

const tarballPath = process.env.AUBE_FFI_TARBALL
const integrity = process.env.AUBE_FFI_INTEGRITY
if (!tarballPath || !integrity) throw new Error("registry fixture environment is incomplete")
const tarball = readFileSync(tarballPath)
const tarballName = tarballPath.split(/[\\/]/).at(-1)

const server = createServer((request, response) => {
  if (request.url?.endsWith(".tgz")) {
    response.writeHead(200, { "content-type": "application/octet-stream" })
    response.end(tarball)
    return
  }
  if (request.url === "/aube-ffi-cached-package") {
    const address = server.address()
    if (!address || typeof address === "string") throw new Error("registry address unavailable")
    response.writeHead(200, { "content-type": "application/json" })
    response.end(JSON.stringify({
      name: "aube-ffi-cached-package",
      "dist-tags": { latest: "1.0.0" },
      versions: {
        "1.0.0": {
          name: "aube-ffi-cached-package",
          version: "1.0.0",
          dist: {
            tarball: `http://127.0.0.1:${address.port}/${tarballName}`,
            integrity,
          },
        },
      },
    }))
    return
  }
  response.writeHead(404)
  response.end("not found")
})

server.listen(0, "127.0.0.1", () => {
  const address = server.address()
  if (!address || typeof address === "string") throw new Error("registry address unavailable")
  process.stdout.write(`${address.port}\n`)
})

process.on("SIGTERM", () => server.close(() => process.exit(0)))
