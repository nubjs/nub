// Which popular packages still HAVE an install script? Ask the registry, do not hardcode.
//
// ⛔⛔ WHY THIS IS DISCOVERED AND NOT A LIST. The population that matters for the build jail is exactly the
// packages whose lifecycle scripts the jail confines, and which version of a package that is has to be
// discovered rather than assumed. Prebuilt binaries have moved a lot of packages off install scripts on
// their CURRENT release — `sharp`, `better-sqlite3` and `playwright` all ship platform
// `optionalDependencies` today — but that is not the same as those packages no longer running a script,
// because installs do not concentrate on the current release. Measured 2026-09-05: `sharp`'s 0.34.5 runs
// `install` and holds 30.7M weekly downloads. So the question this asks is not "does latest carry a
// script" but "does the version people install carry one", which is what `pickInstalledVersion` decides.
//
// A hardcoded list therefore rots in the direction that makes the sweep LOOK fine while measuring nothing:
// a package that drops its script keeps passing, as a row where the jail was never exercised. The first
// version of the sweep hardcoded 18 names, and the ecosystem sweep it replaced reported "12 OK" over rows
// where no script ran at all.
//
// Usage: node discover-install-scripts.mjs [--out <tsv>]   (prints `name<TAB>version` for the ones with a script)
const CANDIDATES = [
  // ⛔⛔ THIS LIST IS THE INPUT, AND THE RULE THAT READS IT IS SEPARATE — BOTH HAVE BEEN WRONG. A name
  // missing here is a package the sweep never measures, reported as nothing rather than as a hole; and
  // until 2026-09-05 the lookup judged every name by `latest`, so a name present here could still be
  // missed. Measured that day against a version-aware scan of the whole band this list is drawn from
  // (npm ranks 1-25,500, i.e. everything above 100,000 weekly downloads): 244 packages carry an install
  // script on a version people install, and the list then held 107 of them. Of the 137 missed, 56 were
  // invisible to any `latest` read and 81 were simply absent. Both halves are fixed — the lookup now
  // ranks versions by download count, and this list is the union of the former hand-curated set with
  // every carrier that scan found.
  //
  // ⛔ THE COVERAGE FIGURE THAT PRODUCES IS SCOPED TO THAT BAND. The metric is measured against the set
  // this list is drawn from, so it would read the same wherever the download gate sat. It says the band
  // is covered, never that the ecosystem is. What lies below the gate is measured separately, by
  // scan-below-gate.mjs, and recorded in results/uncovered-carriers.tsv.
  //
  // Regenerate rather than hand-edit: a name added here without a measurement behind it is a claim
  // nothing rechecks.
  '@andrewstory18/is-real-odd', '@anthropic-ai/claude-code', '@apollo/protobufjs', '@apollo/rover',
  '@ast-grep/cli', '@aws-amplify/cli', '@azure/mcp', '@azure/msal-node-extensions',
  '@biomejs/biome', '@brave/n8n-nodes-brave-search', '@bufbuild/buf', '@carbon/feature-flags',
  '@carbon/grid', '@carbon/icon-helpers', '@carbon/icons-react', '@carbon/layout',
  '@carbon/motion', '@carbon/react', '@carbon/styles', '@carbon/themes', '@carbon/type',
  '@central-icons-react/round-outlined-radius-3-stroke-2',
  '@central-icons-react/square-outlined-radius-0-stroke-2', '@clerk/shared', '@compodoc/compodoc',
  '@confluentinc/kafka-javascript', '@contrast/fn-inspect', '@coze/cli', '@datadog/native-appsec',
  '@datadog/native-iast-taint-tracking', '@datadog/pprof', '@discordjs/opus',
  '@evilmartians/lefthook', '@ffmpeg-installer/linux-x64', '@ffprobe-installer/linux-x64',
  '@firebase/util', '@fortawesome/fontawesome-free', '@fortawesome/fontawesome-svg-core',
  '@fortawesome/free-brands-svg-icons', '@fortawesome/free-regular-svg-icons',
  '@fortawesome/free-solid-svg-icons', '@google/genai', '@googleworkspace/cli',
  '@hatchet-dev/typescript-sdk', '@heroui/shared-utils', '@hyperjump/json-pointer', '@ibm/plex',
  '@ibm/plex-sans', '@infisical/cli', '@journeyapps/wa-sqlite', '@launchql/protobufjs',
  '@lavamoat/preinstall-always-fail', '@medusajs/telemetry', '@microsoft/m365agentstoolkit-cli',
  '@modelcontextprotocol/ext-apps', '@modelcontextprotocol/inspector', '@mongodb-js/zstd',
  '@mui/x-telemetry', '@newrelic/fn-inspect', '@newrelic/native-metrics',
  '@openapitools/openapi-generator-cli', '@opencode-ai/cli', '@openrouter/sdk',
  '@openuidev/lang-core', '@parcel/watcher', '@percy/core', '@playwright/browser-chromium',
  '@playwright/browser-firefox', '@playwright/browser-webkit', '@pnpm/exe', '@posthog/cli',
  '@prisma/engines', '@progress/kendo-licensing', '@pulumi/aws-native', '@pulumi/azure-native',
  '@pulumi/command', '@pulumi/docker', '@pulumi/docker-build', '@pulumi/gcp', '@pulumi/kubernetes',
  '@railway/cli', '@reown/appkit', '@salesforce/cli', '@sap/hana-client', '@scarf/scarf',
  '@sentry-internal/node-cpu-profiler', '@sentry-internal/node-native-stacktrace', '@sentry/cli',
  '@shopify/react-native-skia', '@stacksjs/ts-webp', '@stdlib/math-base-special-exp',
  '@stdlib/math-base-special-ln', '@stdlib/number-float64-base-exponent',
  '@stdlib/number-float64-base-normalize', '@stellar/stellar-sdk', '@stoprocent/noble',
  '@strapi/strapi', '@swc/core', '@temporalio/core-bridge', '@tensorflow/tfjs-node',
  '@tloncorp/tlon-skill', '@tree-sitter-grammars/tree-sitter-yaml', '@trufflesuite/bigint-buffer',
  '@tsparticles/engine', '@turbodocx/html-to-docx', '@vaadin/vaadin-usage-statistics',
  '@vscode/ripgrep', '@vscode/sqlite3', '@vscode/vsce-sign', '@whiskeysockets/baileys',
  '@zowe/secrets-for-zowe-sdk', 'admin-lte', 'agent-browser', 'agentdb', 'ant-design-vue',
  'appium', 'appium-chromedriver', 'appium-ios-tuntap', 'applicationinsights-native-metrics',
  'appmetrics', 'argon2', 'aws-crt', 'aws-sdk', 'azure-functions-core-tools', 'backport',
  'baileys', 'bcrypt', 'better-sqlite3', 'bigint-buffer', 'bignum', 'blake-hash', 'blake3',
  'bluetooth-hci-socket', 'bootstrap-vue', 'braintrust', 'browser-tabs-lock', 'btch-downloader',
  'bufferutil', 'bun', 'canvas', 'cbor-extract', 'ccxt', 'chrome-local-mcp', 'chromedriver',
  'chromium', 'classic-level', 'cline', 'cloudflared', 'console-stamp', 'contentful', 'core-js',
  'core-js-pure', 'cpu-features', 'cwebp-bin', 'cypress', 'dd-trace', 'deasync', 'deno', 'detox',
  'dprint', 'dtrace-provider', 'duckdb', 'edgedriver', 'electron-winstaller', 'epoll', 'es5-ext',
  'esbuild', 'faiss-node', 'farmhash', 'ffi-napi', 'ffmpeg-static', 'fs-xattr', 'fsevents',
  'gatsby', 'gatsby-cli', 'gc-stats', 'geckodriver', 'gifsicle', 'grpc', 'grpc-tools', 'heapdump',
  'hnswlib-node', 'hrtime', 'ibm_db', 'iconv', 'iframe-resizer', 'iltorb', 'inngest-cli', 'iohook',
  'isolated-vm', 'javascript-obfuscator', 'jest-preview', 'jpegtran-bin', 'jss', 'keccak',
  'kerberos', 'keytar', 'koffi', 'lefthook', 'less', 'leveldown', 'libpq', 'libxmljs', 'libxmljs2',
  'llnode', 'lmdb', 'lz4', 'memlab', 'microtime', 'mongodb-client-encryption',
  'mongodb-memory-server', 'mozjpeg', 'msgpackr-extract', 'msw', 'n8n-nodes-evolution-api',
  'native-keymap', 'nestjs-pino', 'netlify', 'netlify-cli', 'ngrok', 'nice-napi', 'node',
  'node-expat', 'node-hid', 'node-jq', 'node-libcurl', 'node-llama-cpp', 'node-pty', 'node-report',
  'node-sass', 'nodejieba', 'nodent-runtime', 'nx', 'odbc', 'odiff-bin', 'onnxruntime-node',
  'openclaw', 'opencode-ai', 'optipng-bin', 'oracledb', 'phantomjs-prebuilt',
  'playwright-chromium', 'playwright-webkit', 'pngquant-bin', 'postinstall-postinstall',
  'postman-code-generators', 'pprof', 'pre-commit', 'prisma', 'protobufjs', 'puppeteer', 're2',
  'react-jsx-parser', 'react-native-inappbrowser-reborn', 'react-native-webrtc', 'realm',
  'ref-napi', 'robotjs', 'rocksdb', 'scrypt', 'secp256k1', 'segfault-handler', 'serverless',
  'sharp', 'simple-git-hooks', 'skia-canvas', 'sleep', 'snyk', 'sonar-scanner', 'spawn-sync',
  'sqlite3', 'sse4_crc32', 'ssh2', 'storybook-addon-remix-react-router', 'stream-chat',
  'style-dictionary', 'summernote', 'svelte-preprocess', 'tesseract.js', 'tiny-secp256k1', 'tldjs',
  'tree-sitter', 'tree-sitter-bash', 'tree-sitter-c-sharp', 'tree-sitter-cli', 'tree-sitter-cpp',
  'tree-sitter-go', 'tree-sitter-javascript', 'tree-sitter-json', 'tree-sitter-php',
  'tree-sitter-python', 'tree-sitter-rust', 'tree-sitter-typescript', 'ttf2woff2', 'type-graphql',
  'uglifyjs-webpack-plugin', 'union', 'unix-dgram', 'unrs-resolver', 'utf-8-validate',
  'v8-profiler-next', 'vue-demi', 'vue-echarts', 'wd', 'web3-bzz', 'web3-shh', 'win-ca', 'workerd',
  'wrtc', 'yarn', 'yo', 'yorkie', 'zeromq', 'zlib', 'zopflipng-bin', 'zstd-napi',
];

const out = process.argv.includes('--out') ? process.argv[process.argv.indexOf('--out') + 1] : null;

// A scoped name is one path segment, so the `/` must be escaped but the `@` must not.
const enc = (name) => encodeURIComponent(name).replace('%40', '@');
const url = (name) => `https://registry.npmjs.org/${enc(name)}`;

/** Fetch with retries, so a transient blip cannot masquerade as "no install script". */
async function getJson(u, accept) {
  let last;
  for (let attempt = 0; attempt < 5; attempt++) {
    if (attempt) await new Promise((r) => setTimeout(r, 600 * attempt));
    try {
      // ⛔ A HUNG SOCKET IS NOT A SLOW ANSWER. Without a timeout one stalled request parks the whole
      // run silently — measured on a sibling scan: frozen for 40+ minutes while the registry answered
      // an unrelated probe in 0.3s.
      const res = await fetch(u, {
        headers: accept ? { accept } : undefined,
        signal: AbortSignal.timeout(20000),
      });
      if (res.status === 404) return null; // A genuinely absent package, not a failure.
      if (res.status === 429) {
        // Throttling is the expected failure at this request count, and it needs a real pause rather
        // than the ordinary backoff. Honour `retry-after` when the server states one.
        const wait = Math.min(30000, (Number(res.headers.get('retry-after')) || 5) * 1000);
        last = 'http 429';
        await new Promise((r) => setTimeout(r, wait));
        continue;
      }
      if (!res.ok) { last = `http ${res.status}`; continue; }
      return await res.json();
    } catch (e) { last = String(e); }
  }
  throw new Error(last ?? 'unknown');
}


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

/**
 * Which version should the sweep measure, judged by what people actually install?
 *
 * ⛔⛔ THE RULE THIS REPLACES WAS `latest`, AND IT WAS STRUCTURALLY BLIND. Downloads pile up on the
 * TERMINAL release of each major/minor line, because that is where lockfiles pin. So a package whose
 * current release dropped its install script still runs one for most of its users. Measured
 * 2026-09-05: `sharp` has no install script on `latest` and one on 0.34.5, which carries 30.7M weekly
 * downloads — a `latest`-only judgement scored it a clean negative, and it was one of 56 such
 * packages holding 286.7M weekly between them that the population could not see.
 *
 * The rule is the census's (scripts/npm-install-script-census.ts): rank a package's versions by
 * ACTUAL weekly downloads and ask whether any of the top N runs a script. That lands on terminal
 * versions empirically, with no semver modelling.
 *
 * The 1% floor exists because top-N by rank is meaningless for a package with few versions —
 * `environment` would otherwise qualify on a 0.0.1 release holding 14 weekly downloads against 36M
 * on its current one.
 *
 * Returns the most-downloaded script-carrying version among the top N, or null if none qualifies.
 * Exported so the ordering can be pinned by a test rather than trusted.
 */
export function pickInstalledVersion(versions, downloads, topN = 3) {
  const total = Object.values(downloads).reduce((a, b) => a + (b || 0), 0);
  if (!total) return null;
  const ranked = Object.keys(versions)
    .sort((a, b) => (downloads[b] || 0) - (downloads[a] || 0))
    .slice(0, topN);
  for (const v of ranked) {
    if (versions[v]?.hasInstallScript && (downloads[v] || 0) / total >= 0.01) return v;
  }
  return null;
}

const isMain = process.argv[1]
  && import.meta.url === (await import('node:url')).pathToFileURL(process.argv[1]).href;
if (isMain) {
const found = [];
const unresolved = [];
const downloadFallbacks = [];
let paced = false;
for (const name of [...new Set(CANDIDATES)]) {
  // Pace it. The registry throttles hard at this request count, and a throttled lookup is
  // indistinguishable from "no install script" until the refusal at the end fires — so the cheap
  // fix is to not provoke it. Measured 2026-09-05: an unpaced run of this list drew HTTP 429 from
  // api.npmjs.org within a couple of minutes.
  if (paced) await new Promise((r) => setTimeout(r, 120));
  paced = true;

  // The abbreviated ("corgi") packument carries `hasInstallScript` per version, which is the
  // registry's own answer to the question this script asks. Measured by the census against the full
  // documents: exactly equivalent over 6,351 versions, zero disagreements, and 38 `prepare`-only
  // versions all correctly flagged false. One request settles every package that has never carried a
  // script on any version, which is most of them.
  let packument;
  try {
    packument = await getJson(url(name), 'application/vnd.npm.install-v1+json');
  } catch (e) {
    // ⛔ A FAILED LOOKUP IS NOT "NO SCRIPT". This used to `continue` silently, so a rate-limited or
    // flaky scan shrank the population and every downstream count read as better coverage — the
    // failure mode this file's own header warns about (57 carriers reported against a known 87).
    unresolved.push(`${name} (${e.message})`);
    continue;
  }
  if (!packument?.versions) continue; // 404: the package does not exist.
  if (!Object.values(packument.versions).some((e) => e.hasInstallScript)) continue;

  // ⛔ A FAILED DOWNLOAD FETCH IS NOT "NOBODY INSTALLS THIS". Swallowing it would drop the package
  // onto the no-data fallback below, which judges by `latest` — silently reinstating the exact method
  // this rewrite replaced, for whichever packages happened to be throttled. Let it throw.
  let dlDoc;
  try {
    dlDoc = await getJson(`https://api.npmjs.org/versions/${enc(name)}/last-week`);
  } catch (e) {
    unresolved.push(`${name} (per-version downloads: ${e.message})`);
    continue;
  }
  const downloads = dlDoc?.downloads ?? {}; // 404 here is a package with no download record at all.

  const pick = pickInstalledVersion(packument.versions, downloads);
  if (pick) {
    found.push(`${name}\t${pick}`);
    continue;
  }
  if (Object.values(downloads).some((n) => n > 0)) continue; // ranked, and no top version carries one.

  // No usable download data — the registry reports nothing for packages nobody installs, and the
  // hand-curated half of the candidate list is deliberately full of those (iohook sits near 1,100
  // weekly). Ranking is meaningless there, so fall back to the newest STABLE release, which is what
  // `npm install <name>` resolves. ⛔ Never fall back to whatever order the packument happens to be
  // in: with no download data the ranking comparator returns 0 for every pair, the sort becomes a
  // no-op, and the top N is the OLDEST releases — which for a native package almost always carry an
  // install script, manufacturing a carrier out of a transient fetch failure.
  const stable = highestStable(Object.keys(packument.versions));
  if (!stable) continue;
  downloadFallbacks.push(`${name} -> ${stable}`);
  if (packument.versions[stable]?.hasInstallScript) found.push(`${name}\t${stable}`);
}

const text = `${found.join('\n')}\n`;
if (out) {
  const { writeFileSync } = await import('node:fs');
  writeFileSync(out, text);
  console.error(`${found.length} of ${new Set(CANDIDATES).size} candidates still carry an install script -> ${out}`);
} else {
  process.stdout.write(text);
}
for (const f of downloadFallbacks) console.error(`  no per-version download data, judged by newest stable — ${f}`);
if (unresolved.length) {
  // ⛔ REFUSE RATHER THAN UNDER-REPORT. An incomplete population is a sweep that claims coverage it
  // does not have, and the caller treats a non-zero exit as fatal, which is the correct outcome.
  console.error(`⛔ ${unresolved.length} candidate(s) could not be resolved after retries; the population would be INCOMPLETE:`);
  for (const u of unresolved) console.error(`   ${u}`);
  process.exit(3);
}
}
