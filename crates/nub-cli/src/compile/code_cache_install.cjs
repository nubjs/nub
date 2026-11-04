// A generated bootstrap suffix supplies the pack's identity and target. This is
// a cache only: unsupported settings or any publication failure leave Node's
// ordinary source-loading path intact.
(function (packName, packId, version, arch, tag) {
  try {
    if (process.version !== version || process.arch !== arch ||
        process.env.NODE_COMPILE_CACHE_PORTABLE === "1" ||
        process.env.NODE_COMPILE_CACHE_READONLY === "1") return;
    const getBuiltin = process[Symbol.for("nub.compile.bootstrap")].getBuiltin;
    const cacheDir = getBuiltin("node:module").getCompileCacheDir?.();
    if (!cacheDir || getBuiltin("node:v8").cachedDataVersionTag() !== tag) return;
    const fs = getBuiltin("node:fs");
    const path = getBuiltin("node:path");
    const { crc32, zstdDecompressSync } = getBuiltin("node:zlib");
    const { pathToFileURL } = getBuiltin("node:url");
    const location = crc32(pathToFileURL(__dirname).href).toString(16);
    const marker = path.join(cacheDir, `.nub-${packId}-${location}`);
    if (fs.existsSync(marker)) return;
    const pack = zstdDecompressSync(fs.readFileSync(path.join(__dirname, packName)));
    const indexEnd = 4 + pack.readUInt32LE(0);
    const index = JSON.parse(pack.subarray(4, indexEnd));
    const temporary = fs.mkdtempSync(path.join(cacheDir, ".nub-cache-"));
    try {
      for (const [name, offset, length] of index.entries) {
        if (typeof name !== "string" || !Number.isSafeInteger(offset) || offset < 0 ||
            !Number.isSafeInteger(length) || length < 20 || indexEnd + offset + length > pack.length) {
          throw new Error("Invalid compile-cache entry");
        }
        const url = pathToFileURL(path.join(__dirname, name)).href;
        const key = crc32(url, crc32(Buffer.from([1]))).toString(16).padStart(8, "0");
        const file = path.join(temporary, key);
        fs.writeFileSync(file, pack.subarray(indexEnd + offset, indexEnd + offset + length), { mode: 0o600 });
        fs.renameSync(file, path.join(cacheDir, key));
      }
      const complete = path.join(temporary, "complete");
      fs.writeFileSync(complete, "", { mode: 0o600 });
      fs.renameSync(complete, marker);
    } finally {
      fs.rmSync(temporary, { recursive: true, force: true });
    }
  } catch {}
})
