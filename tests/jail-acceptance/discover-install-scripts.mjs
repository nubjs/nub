// Which popular packages still HAVE an install script? Ask the registry, do not hardcode.
//
// ⛔⛔ WHY THIS IS DISCOVERED AND NOT A LIST. The population that matters for the build jail is exactly the
// packages whose lifecycle scripts the jail confines — and that set is SHRINKING fast. `sharp`,
// `better-sqlite3`, `playwright` and even `electron@43` have no install script at all any more; their
// prebuilt binaries arrive as platform `optionalDependencies` instead. Measured: 61 of 148 popular
// native/tooling candidates no longer carry one.
//
// A hardcoded list therefore rots in the direction that makes the sweep LOOK fine while measuring nothing:
// a package that drops its script keeps passing, as a row where the jail was never exercised. The first
// version of the sweep hardcoded 18 names, and the ecosystem sweep it replaced reported "12 OK" over rows
// where no script ran at all.
//
// Usage: node discover-install-scripts.mjs [--out <tsv>]   (prints `name<TAB>version` for the ones with a script)
const CANDIDATES = [
  // Native addons and toolchains people actually install. Kept broad on purpose: the point is to catch the
  // ones that STILL confine, and a candidate that has dropped its script simply falls out of the output.
  'esbuild', 'bcrypt', 'canvas', 'puppeteer', 'prisma', '@swc/core', 'cypress', 'sqlite3', 'bufferutil',
  'utf-8-validate', 're2', 'ssh2', 'zeromq', 'node-pty', 'keytar', 'deasync', '@parcel/watcher', 'lmdb',
  'msgpackr-extract', 'nodejieba', 'node-expat', 'iconv', 'dtrace-provider', 'epoll', 'ffi-napi',
  'ref-napi', 'segfault-handler', 'heapdump', 'v8-profiler-next', 'gc-stats', 'node-libcurl', 'libpq',
  'oracledb', 'ibm_db', 'odbc', 'snappy', 'lz4', 'zstd-napi', 'blake3', 'argon2', 'sodium-native',
  'secp256k1', 'keccak', 'blake-hash', 'scrypt', 'sharp', 'node-sass', 'sass-embedded', 'playwright',
  'electron', 'turbo', '@biomejs/biome', 'rollup', 'webpack', 'terser', 'core-js', 'protobufjs', 'grpc',
  '@grpc/grpc-js', 'husky', 'patch-package', 'node-gyp', 'prebuild-install', 'nan', 'node-addon-api',
  'bindings', 'fsevents', 'chokidar', 'watchpack', 'imagemin', 'pngquant-bin', 'optipng-bin',
  'jpegtran-bin', 'gifsicle', 'mozjpeg', 'cwebp-bin', 'zopflipng-bin', 'svgo', 'puppeteer-core',
  'chromedriver', 'geckodriver', 'edgedriver', 'selenium-webdriver', 'appium', 'detox', 'realm',
  'better-sqlite3', 'level', 'classic-level', 'leveldown', 'rocksdb', 'duckdb', 'isolated-vm',
  'tree-sitter', 'node-datachannel', 'wrtc', '@discordjs/opus', 'ffmpeg-static', 'pdfjs-dist',
  'onnxruntime-node', '@tensorflow/tfjs-node', 'usb', 'serialport', 'robotjs', 'iohook',
  'systeminformation', 'microtime', 'bigint-buffer', 'kerberos', 'node-forge', 'libxmljs2',
  'fast-xml-parser', 'jsdom', 'playwright-core', 'pprof', 'applicationinsights-native-metrics',
  'appmetrics', 'node-report', 'llnode', 'hrtime', 'bluetooth-hci-socket', 'faiss-node', 'tesseract.js',
];

const out = process.argv.includes('--out') ? process.argv[process.argv.indexOf('--out') + 1] : null;
const found = [];
for (const name of [...new Set(CANDIDATES)]) {
  try {
    const res = await fetch(`https://registry.npmjs.org/${encodeURIComponent(name).replace('%40', '@')}/latest`);
    if (!res.ok) continue;
    const doc = await res.json();
    const s = doc.scripts ?? {};
    // `install`, `preinstall` or `postinstall` — the three the PM runs at install time, which is exactly
    // what the jail confines. A `prepare` script runs only for a git dep or the root, so it is out of scope.
    if (s.install || s.preinstall || s.postinstall) found.push(`${name}\t${doc.version}`);
  } catch {
    // A candidate that cannot be resolved is silently absent rather than fatal: this list is deliberately
    // broad, and one unreachable name must not stop the sweep from being built.
  }
}
const text = `${found.join('\n')}\n`;
if (out) {
  const { writeFileSync } = await import('node:fs');
  writeFileSync(out, text);
  console.error(`${found.length} of ${new Set(CANDIDATES).size} candidates still carry an install script -> ${out}`);
} else {
  process.stdout.write(text);
}
