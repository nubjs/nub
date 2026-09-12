#!/usr/bin/env node

import { createReadStream, readFileSync, statSync } from "node:fs";
import { createServer } from "node:http";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const storageDir = join(scriptDir, "..", "test", "registry", "storage");
const portArg = process.argv.indexOf("--port");
const port = portArg === -1 ? 4876 : Number(process.argv[portArg + 1]);

if (!Number.isInteger(port) || port < 1 || port > 65535) {
  console.error("offline-registry: --port must be an integer from 1 to 65535");
  process.exit(2);
}

const packagePattern = /^(?:@[a-zA-Z0-9._~-]+\/)?[a-zA-Z0-9._~-]+$/;

createServer((request, response) => {
  if (request.method !== "GET" && request.method !== "HEAD") {
    response.writeHead(405).end();
    return;
  }

  const requestUrl = new URL(
    request.url ?? "/",
    "http://" + request.headers.host,
  );
  let pathname: string;
  try {
    pathname = decodeURIComponent(requestUrl.pathname);
  } catch {
    response.writeHead(400).end();
    return;
  }

  if (pathname === "/") {
    response.writeHead(200, { "content-type": "application/json" });
    response.end('{"name":"aube-offline-registry"}');
    return;
  }

  const tarballMarker = "/-/";
  const markerIndex = pathname.indexOf(tarballMarker);
  const packageName =
    markerIndex === -1 ? pathname.slice(1) : pathname.slice(1, markerIndex);
  if (!packagePattern.test(packageName)) {
    response.writeHead(404).end();
    return;
  }

  try {
    if (markerIndex !== -1) {
      const tarballName = pathname.slice(markerIndex + tarballMarker.length);
      if (!/^[a-zA-Z0-9._~-]+\.tgz$/.test(tarballName)) {
        response.writeHead(404).end();
        return;
      }
      const tarballPath = join(storageDir, ...packageName.split("/"), tarballName);
      const size = statSync(tarballPath).size;
      response.writeHead(200, {
        "content-length": size,
        "content-type": "application/octet-stream",
      });
      if (request.method === "HEAD") {
        response.end();
      } else {
        createReadStream(tarballPath).pipe(response);
      }
      return;
    }

    const packumentPath = join(
      storageDir,
      ...packageName.split("/"),
      "package.json",
    );
    const packument = JSON.parse(readFileSync(packumentPath, "utf8"));
    const origin = "http://127.0.0.1:" + port;
    for (const [version, manifest] of Object.entries(
      packument.versions ?? {},
    ) as Array<[string, { dist?: { tarball?: string } }]>) {
      if (manifest.dist?.tarball) {
        const tarballName = manifest.dist.tarball.split("/").at(-1);
        const tarballPath = join(
          storageDir,
          ...packageName.split("/"),
          tarballName ?? "",
        );
        try {
          statSync(tarballPath);
        } catch {
          delete packument.versions[version];
          continue;
        }
        manifest.dist.tarball =
          origin + "/" + packageName + "/-/" + tarballName;
      }
    }
    for (const [tag, version] of Object.entries(packument["dist-tags"] ?? {})) {
      if (!((version as string) in packument.versions)) {
        delete packument["dist-tags"][tag];
      }
    }
    const body = JSON.stringify(packument);
    response.writeHead(200, {
      "content-length": Buffer.byteLength(body),
      "content-type": "application/json",
    });
    response.end(request.method === "HEAD" ? undefined : body);
  } catch {
    response.writeHead(404).end();
  }
}).listen(port, "127.0.0.1", () => {
  console.log("offline registry ready on http://127.0.0.1:" + port);
});
