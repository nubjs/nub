// The helper runs in a separate target-Node process and never links or evaluates
// a module. Restoring laziness before serialization retains the runtime's flag
// hash without making the runtime eagerly compile its builtins and other code.
// Only the build helper uses --predictable. V8 deliberately excludes it from
// the cache flag hash; the application's runtime retains its normal randomness.
const fs = require("node:fs");
const vm = require("node:vm");
const v8 = require("node:v8");
const { crc32 } = require("node:zlib");
const files = JSON.parse(fs.readFileSync(0, "utf8"));
const tag = v8.cachedDataVersionTag();
const entries = [];
const buffers = [];
let offset = 0;

for (const [name, source] of files) {
  v8.setFlagsFromString("--no-lazy");
  let module;
  try {
    module = new vm.SourceTextModule(source, { identifier: `file:///${name}` });
  } finally {
    v8.setFlagsFromString("--lazy");
  }
  if (v8.cachedDataVersionTag() !== tag) throw new Error("V8 flags were not restored");
  const cache = module.createCachedData();
  // Node's compile-cache envelope, src/compile_cache.cc. V8 generates its own
  // inner header and validates that header when the ordinary ESM loader reads it.
  const header = Buffer.alloc(20);
  header.writeUInt32LE(0x8adfdbb2, 0);
  header.writeUInt32LE(Buffer.byteLength(source), 4);
  header.writeUInt32LE(cache.length, 8);
  header.writeUInt32LE(crc32(source), 12);
  header.writeUInt32LE(crc32(cache), 16);
  const length = header.length + cache.length;
  entries.push([name, offset, length]);
  buffers.push(header, cache);
  offset += length;
}

const index = Buffer.from(JSON.stringify({ version: process.version, arch: process.arch, tag, entries }));
const length = Buffer.alloc(4);
length.writeUInt32LE(index.length);
process.stdout.write(Buffer.concat([length, index, ...buffers]));
