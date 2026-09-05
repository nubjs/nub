// The second half of an inline (`no-extract`) compiled artifact's `-e` script.
//
// `nub compile` concatenates compile-bootstrap.cjs and this file, substitutes the
// two `__NUB_INLINE_*__` placeholders, and stores the result as the payload's
// bootstrap entry. The launcher decompresses it and hands it to Node as `-e`,
// which is what removes the last reason a qualifying artifact needed a writable
// directory: with no `--require` there is no file to require, and with the chunks
// served as `data:` URLs there is no file to import.
//
// It runs as CommonJS, before any ESM in the process, with the bootstrap's frozen
// builtin accessors already published — so every builtin here comes off that
// record rather than a bare `require`, for the same reason the bootstrap uses it:
// nothing the user can register has run yet, and keeping the one accessor means
// there is one thing to audit.

(() => {
  const boot = process[Symbol.for("nub.compile.bootstrap")];
  const fs = boot.getBuiltin("node:fs");
  const zlib = boot.getBuiltin("node:zlib");

  const ENTRY = "__NUB_INLINE_ENTRY__";
  // The virtual root every chunk reports as its own location, and the same string
  // `nub compile` bakes into each chunk's `import.meta.url` — see
  // `compile::inline::VIRTUAL_ROOT`, which carries the reason for the drive
  // letter. The short version: Node's Windows `fileURLToPath` rejects any path
  // that does not start with one, and `createRequire(import.meta.url)` converts,
  // so `file:///$nub/` crashed every inline artifact on Windows. `/N:/$nub/` is
  // an ordinary absolute path on POSIX, so one string serves both.
  const ROOT = "file:///N:/$nub/";
  // nub_core::compile::INLINE_LOCATOR_MAGIC.
  const MAGIC = Buffer.from("006e75622d696e6c696e652d61707000", "hex");
  // How much of the executable's tail to search before falling back to the whole
  // file. The locator is the last byte of the payload, and only the platform's
  // signature can follow it — a few hundred KB on macOS, nothing on Linux — so
  // this window is met on the first read and the fallback is a safety net rather
  // than a path anything is expected to take.
  const TAIL_WINDOW = 4 * 1024 * 1024;

  const selfPath = process.env.__NUB_COMPILED_EXEC_PATH;
  if (typeof selfPath !== "string" || selfPath.length === 0) {
    throw new Error("nub: the compiled executable's path was not published to its own process");
  }

  // Node's own argv under `-e` is [execPath, ...userArgs]: there is no script, so
  // the first USER argument lands where every program expects its own path. Splice
  // the artifact in, which is both the fix and an improvement on the extracted
  // shape, where argv[1] leaked the cache directory.
  process.argv.splice(1, 0, selfPath);

  // `-e` puts the whole script into execArgv. The launcher publishes what the flags
  // actually were, so a program that reads execArgv — or forwards it to a Worker,
  // which is where a bogus entry becomes a crash — sees the real set.
  const publishedExecArgv = process.env.__NUB_COMPILED_EXEC_ARGV;
  if (typeof publishedExecArgv === "string") {
    try {
      const parsed = JSON.parse(publishedExecArgv);
      if (Array.isArray(parsed) && parsed.every((a) => typeof a === "string")) {
        process.execArgv = parsed;
      }
    } catch {
      // A corrupt value leaves the `-e` spelling in place rather than throwing:
      // it is cosmetic, and failing the launch over it would be worse.
    }
    delete process.env.__NUB_COMPILED_EXEC_ARGV;
  }

  const fd = fs.openSync(selfPath, "r");
  let region;
  try {
    const size = fs.fstatSync(fd).size;
    const read = (length, position) => {
      const buf = Buffer.allocUnsafe(length);
      let got = 0;
      while (got < length) {
        const n = fs.readSync(fd, buf, got, length - got, position + got);
        if (n === 0) throw new Error("nub: the compiled executable ended early");
        got += n;
      }
      return buf;
    };

    let windowStart = Math.max(0, size - TAIL_WINDOW);
    let window = read(size - windowStart, windowStart);
    let at = window.lastIndexOf(MAGIC);
    if (at < 0 && windowStart > 0) {
      windowStart = 0;
      window = read(size, 0);
      at = window.lastIndexOf(MAGIC);
    }
    if (at < 0) {
      throw new Error("nub: this executable carries no inline compiled payload");
    }
    const locator = windowStart + at;
    const back = Number(window.readBigUInt64LE(at + 16));
    const length = Number(window.readBigUInt64LE(at + 24));
    region = read(length, locator - back);
  } finally {
    fs.closeSync(fd);
  }

  // The payload's V3 app region, which is the one container structure this file
  // knows: [u32 files][u32 records][per file: u16 nameLen, name, u32 dataIndex]
  // [per record: u64 len, u64 plainLen, bytes]. Bit 31 of dataIndex is the
  // executable flag, which an inline payload never sets — it ships no verbatim
  // file — and is masked off rather than asserted so the parse stays a pure reader.
  //
  // `plainLen` is the pre-compression size the warm-cache check compares against;
  // this reader decompresses, so it skips the field rather than using it. It MUST
  // still be skipped: the encoder writes V3 unconditionally, so reading V2's
  // two-field record here walks the pointer 8 bytes off at the first record and
  // every subsequent read is garbage.
  let p = 0;
  const u32 = () => {
    const v = region.readUInt32LE(p);
    p += 4;
    return v;
  };
  const fileCount = u32();
  const recordCount = u32();
  const entries = [];
  for (let i = 0; i < fileCount; i++) {
    const nameLen = region.readUInt16LE(p);
    p += 2;
    const name = region.toString("utf8", p, p + nameLen);
    p += nameLen;
    entries.push([name, u32() & 0x7fffffff]);
  }
  const records = [];
  for (let i = 0; i < recordCount; i++) {
    const len = Number(region.readBigUInt64LE(p));
    p += 8;
    p += 8; // plainLen, written by the encoder and unused here
    records.push(region.subarray(p, p + len));
    p += len;
  }

  // Brotli, not zstd: this is the one decompressor that has to run in JavaScript,
  // and `zlib.zstdDecompressSync` is absent on Node 23.5 and 23.6 while brotli is
  // present on every version nub supports.
  const sources = new Map();
  for (const [name, index] of entries) {
    if (name.endsWith(".mjs")) {
      sources.set(name, zlib.brotliDecompressSync(records[index]).toString("utf8"));
    }
  }

  // Each chunk becomes its own `data:` module, deepest first, with the
  // placeholders `nub compile` left in place of the cross-chunk specifiers
  // replaced by the URL of the chunk they named. Substitution rather than a
  // resolve hook is the whole point: a `data:` URL has no base, so a relative
  // specifier cannot resolve, and `module.registerHooks` would put a floor of
  // Node 22.15/23.5 on a mode that otherwise has none.
  const built = new Map();
  const building = new Set();
  const urlFor = (name) => {
    const cached = built.get(name);
    if (cached !== undefined) return cached;
    if (building.has(name)) {
      // The compiler proves the chunk graph acyclic before choosing this mode, so
      // reaching here means the payload does not match the manifest that selected
      // it. Fail loudly instead of recursing to a stack overflow.
      throw new Error(`nub: the compiled payload has a cyclic chunk graph at ${name}`);
    }
    building.add(name);
    let code = sources.get(name);
    if (code === undefined) throw new Error(`nub: the compiled payload is missing ${name}`);
    for (const [dep] of entries) {
      const token = `"nub-inline:${dep}"`;
      if (code.includes(token)) code = code.split(token).join(JSON.stringify(urlFor(dep)));
    }
    // Trailing, so it wins over anything the bundle already carries and so the
    // chunk's own line numbering — which the compiler kept intact when it prefixed
    // the `import.meta.url` assignment — is what stack frames report.
    code += `\n//# sourceURL=${ROOT}${name}\n`;
    const url = `data:text/javascript;base64,${Buffer.from(code, "utf8").toString("base64")}`;
    built.set(name, url);
    building.delete(name);
    return url;
  };

  const entryUrl = urlFor(ENTRY);
  // The promise has to be OBSERVED, because Node decides two things by watching
  // its own entry module's evaluation and this import is not that: an entry ending
  // in an unsettled top-level await exits 0 rather than 13, and a throwing entry
  // under `--unhandled-rejections=warn` exits 0 rather than 1. Both measured
  // against the same file run through `nub`.
  // The diagnostic goes on `beforeExit` and the exit code on `exit`, for the
  // reason the single-executable loader spells out: `process.emitWarning` queues
  // its write behind a tick, so one emitted from `exit` is composed and dropped,
  // while the exit code must be set at the only point nothing further can settle
  // the entry.
  let settled = false;
  let warned = false;
  let deferred = false;
  const warn = () => {
    if (settled || warned) return;
    // The first `beforeExit` is skipped for the reason the single-executable
    // loader spells out: this listener precedes every one the application
    // installs, and one of those may settle the entry.
    if (!deferred) {
      deferred = true;
      setImmediate(() => {});
      return;
    }
    warned = true;
    process.emitWarning(`Detected unsettled top-level await at ${ROOT}${ENTRY}`);
  };
  const unsettled = () => {
    if (settled || process.exitCode !== undefined) return;
    process.exitCode = 13;
  };
  const done = () => {
    settled = true;
    process.off("beforeExit", warn);
    process.off("exit", unsettled);
  };
  process.on("beforeExit", warn);
  process.on("exit", unsettled);
  import(entryUrl).then(
    () => {
      done();
    },
    (error) => {
      done();
      // Rethrown rather than reported, because a failed ESM ENTRY is an uncaught
      // exception in Node and not an unhandled rejection — so it must fail the
      // process whatever `--unhandled-rejections` says, and it must still reach an
      // `uncaughtException` handler the application installed. The `ROOT`-rooted
      // frames the `sourceURL` above establishes are preserved either way.
      process.nextTick(() => {
        throw error;
      });
    },
  );
})();
