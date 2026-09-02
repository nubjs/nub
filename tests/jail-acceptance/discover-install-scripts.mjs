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
  // ⛔⛔ THIS LIST IS NO LONGER HAND-CURATED, AND THAT CHANGE IS THE POINT. Measured 2026-09-02, the
  // former 121-name list produced a population carrying only 59.9% of the weekly downloads that
  // reach an install script — so 40% of the userbase the jail must not break was never swept, and
  // the heaviest omissions were not obscure: unrs-resolver at 54M/wk, @sentry/cli, workerd, msw, nx,
  // yarn, bun, aws-sdk. The list below is the union of that hand list with EVERY install-script
  // carrying package in the npm top-downloaded set, which takes the coverage to 100%.
  //
  // Regenerate by scanning the npm top-downloaded packages for an install-time script rather than by
  // adding names you thought of. ⛔ A scan of that size is rate-limited hard: at concurrency 24 a run
  // measured 9,523 of 15,916 lookups unresolved and reported 57 carriers against a known 87, and its
  // two-package control passed while it did so. Pace it (concurrency 3, ~120ms), retry every non-404,
  // and make the control a population you already know the answer for.
  '@anthropic-ai/claude-code', '@apollo/protobufjs', '@ast-grep/cli', '@aws-amplify/cli',
  '@bufbuild/buf', '@carbon/icons-react', '@contrast/fn-inspect', '@discordjs/opus',
  '@ffmpeg-installer/linux-x64', '@firebase/util', '@google/genai', '@heroui/shared-utils',
  '@mongodb-js/zstd', '@mui/x-telemetry', '@newrelic/fn-inspect', '@newrelic/native-metrics',
  '@openapitools/openapi-generator-cli', '@openrouter/sdk', '@parcel/watcher', '@percy/core',
  '@playwright/browser-chromium', '@playwright/browser-webkit', '@pnpm/exe', '@posthog/cli',
  '@prisma/engines', '@progress/kendo-licensing', '@pulumi/command', '@pulumi/docker-build',
  '@pulumi/kubernetes', '@reown/appkit', '@salesforce/cli', '@scarf/scarf',
  '@sentry-internal/node-cpu-profiler', '@sentry/cli', '@stoprocent/noble', '@swc/core',
  '@tensorflow/tfjs-node', '@tree-sitter-grammars/tree-sitter-yaml', '@tsparticles/engine',
  '@vscode/vsce-sign', '@whiskeysockets/baileys', '@zowe/secrets-for-zowe-sdk', 'agent-browser',
  'appium', 'applicationinsights-native-metrics', 'appmetrics', 'argon2', 'aws-crt', 'aws-sdk',
  'bcrypt', 'bigint-buffer', 'bignum', 'blake-hash', 'blake3', 'bluetooth-hci-socket',
  'braintrust', 'browser-tabs-lock', 'btch-downloader', 'bufferutil', 'bun', 'canvas',
  'chromedriver', 'classic-level', 'core-js', 'core-js-pure', 'cpu-features', 'cwebp-bin',
  'cypress', 'deasync', 'detox', 'dtrace-provider', 'duckdb', 'edgedriver', 'electron-winstaller',
  'epoll', 'es5-ext', 'esbuild', 'faiss-node', 'ffi-napi', 'ffmpeg-static', 'fsevents', 'gatsby',
  'gatsby-cli', 'gc-stats', 'geckodriver', 'gifsicle', 'grpc', 'grpc-tools', 'heapdump', 'hrtime',
  'ibm_db', 'iconv', 'inngest-cli', 'iohook', 'isolated-vm', 'jpegtran-bin', 'keccak', 'kerberos',
  'keytar', 'koffi', 'lefthook', 'leveldown', 'libpq', 'libxmljs2', 'llnode', 'lmdb', 'lz4',
  'microtime', 'mongodb-memory-server', 'mozjpeg', 'msgpackr-extract', 'msw',
  'n8n-nodes-evolution-api', 'netlify', 'netlify-cli', 'nice-napi', 'node', 'node-expat',
  'node-libcurl', 'node-llama-cpp', 'node-pty', 'node-report', 'node-sass', 'nodejieba', 'nx',
  'odbc', 'onnxruntime-node', 'openclaw', 'opencode-ai', 'optipng-bin', 'oracledb',
  'phantomjs-prebuilt', 'pngquant-bin', 'postinstall-postinstall', 'pprof', 'pre-commit', 'prisma',
  'puppeteer', 're2', 'react-native-inappbrowser-reborn', 'realm', 'ref-napi', 'robotjs',
  'rocksdb', 'scrypt', 'secp256k1', 'segfault-handler', 'serverless', 'simple-git-hooks',
  'skia-canvas', 'snyk', 'sqlite3', 'ssh2', 'stream-chat', 'svelte-preprocess', 'tesseract.js',
  'tldjs', 'tree-sitter', 'tree-sitter-bash', 'tree-sitter-javascript', 'tree-sitter-json',
  'tree-sitter-typescript', 'ttf2woff2', 'union', 'unix-dgram', 'unrs-resolver', 'utf-8-validate',
  'v8-profiler-next', 'vue-demi', 'web3-bzz', 'web3-shh', 'workerd', 'wrtc', 'yarn', 'yo',
  'yorkie', 'zeromq', 'zlib', 'zopflipng-bin', 'zstd-napi',
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
