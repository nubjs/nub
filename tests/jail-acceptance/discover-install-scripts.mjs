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

const url = (name, suffix = '') =>
  `https://registry.npmjs.org/${encodeURIComponent(name).replace('%40', '@')}${suffix}`;

/** Fetch with retries, so a transient blip cannot masquerade as "no install script". */
async function getJson(u, accept) {
  let last;
  for (let attempt = 0; attempt < 3; attempt++) {
    if (attempt) await new Promise((r) => setTimeout(r, 400 * attempt));
    try {
      const res = await fetch(u, accept ? { headers: { accept } } : undefined);
      if (res.status === 404) return null; // A genuinely absent package, not a failure.
      if (!res.ok) { last = `http ${res.status}`; continue; }
      return await res.json();
    } catch (e) { last = String(e); }
  }
  throw new Error(last ?? 'unknown');
}

const hooks = (scripts) => !!(scripts?.install || scripts?.preinstall || scripts?.postinstall);

/**
 * Highest non-prerelease version key, by numeric semver order. `null` when every version is one.
 * Exported so its ordering can be pinned by a test — a comparator that silently picked the
 * lexically-largest key would answer `9.9.9` over `10.0.0` and quietly re-introduce the drop.
 */
export function highestStable(versions) {
  let best = null;
  let bestParts = null;
  for (const v of versions) {
    if (v.includes('-')) continue;
    const parts = v.split('.').map((n) => parseInt(n, 10));
    if (parts.length < 3 || parts.some(Number.isNaN)) continue;
    if (!bestParts || cmpSemver(parts, bestParts) > 0) {
      best = v;
      bestParts = parts;
    }
  }
  return best;
}
function cmpSemver(a, b) {
  for (let i = 0; i < 3; i++) if ((a[i] ?? 0) !== (b[i] ?? 0)) return (a[i] ?? 0) - (b[i] ?? 0);
  return 0;
}

const isMain = process.argv[1]
  && import.meta.url === (await import('node:url')).pathToFileURL(process.argv[1]).href;
if (isMain) {
const found = [];
const unresolved = [];
const prereleaseFallbacks = [];
for (const name of [...new Set(CANDIDATES)]) {
  let doc;
  try {
    doc = await getJson(url(name, '/latest'));
  } catch (e) {
    // ⛔ A FAILED LOOKUP IS NOT "NO SCRIPT". This used to `continue` silently, so a rate-limited or
    // flaky scan shrank the population and every downstream count read as better coverage — the
    // failure mode this file's own header warns about (57 carriers reported against a known 87).
    unresolved.push(`${name} (${e.message})`);
    continue;
  }
  if (!doc) continue; // 404: the package does not exist.

  // ⛔⛔ `latest` CAN BE A PRERELEASE, AND THEN IT DESCRIBES NOBODY'S INSTALL. Measured 2026-09-04:
  // `prisma`'s latest was `8.0.0-rc.12`, which dropped the `preinstall` that `7.9.1` — the version an
  // ordinary `npm install prisma` resolves — still carries. So a package the jail confines for every
  // real user fell out of the population, and three same-commit sweeps measured it nowhere. When
  // latest is a prerelease, judge by the newest STABLE version instead, which is what users get.
  if (doc.version?.includes('-')) {
    let packument;
    try {
      packument = await getJson(url(name), 'application/vnd.npm.install-v1+json');
    } catch (e) {
      unresolved.push(`${name} (prerelease latest, packument: ${e.message})`);
      continue;
    }
    const stable = highestStable(Object.keys(packument?.versions ?? {}));
    if (stable) {
      // The abbreviated packument carries `hasInstallScript`, which is the registry's own answer to
      // exactly this question and does not depend on `scripts` being present in the trimmed document.
      const v = packument.versions[stable];
      prereleaseFallbacks.push(`${name}: latest ${doc.version} -> stable ${stable}`);
      if (v.hasInstallScript || hooks(v.scripts)) found.push(`${name}\t${stable}`);
      continue;
    }
  }
  if (hooks(doc.scripts)) found.push(`${name}\t${doc.version}`);
}

const text = `${found.join('\n')}\n`;
if (out) {
  const { writeFileSync } = await import('node:fs');
  writeFileSync(out, text);
  console.error(`${found.length} of ${new Set(CANDIDATES).size} candidates still carry an install script -> ${out}`);
} else {
  process.stdout.write(text);
}
for (const f of prereleaseFallbacks) console.error(`  prerelease latest, judged by newest stable — ${f}`);
if (unresolved.length) {
  // ⛔ REFUSE RATHER THAN UNDER-REPORT. An incomplete population is a sweep that claims coverage it
  // does not have, and the caller treats a non-zero exit as fatal, which is the correct outcome.
  console.error(`⛔ ${unresolved.length} candidate(s) could not be resolved after retries; the population would be INCOMPLETE:`);
  for (const u of unresolved) console.error(`   ${u}`);
  process.exit(3);
}
}
