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
  // ⛔⛔ THIS LIST IS THE INPUT, AND THE RULE THAT READS IT IS SEPARATE — BOTH HAVE BEEN WRONG, IN
  // THAT ORDER. A name missing here is a package the sweep never measures, reported as nothing rather
  // than as a hole. Until 2026-09-05 the lookup also judged every name by `latest`, so a name present
  // here could still be missed; and until 2026-09-06 the scan that regenerated this list used a top-3
  // window, which found 244 carriers over the band where a floor-only scan finds 403.
  //
  // The list is now the union of the former hand-curated set with every carrier a floor-only scan
  // found over npm ranks 1-25,500 — the band above 100,000 weekly downloads. `pickInstalledVersion`
  // explains why that scan has no window.
  //
  // ⛔ THE COVERAGE FIGURE THIS PRODUCES IS SCOPED TO THAT BAND. The metric is measured against the
  // set this list is drawn from, so it would read the same wherever the download gate sat. It says the
  // band is covered, never that the ecosystem is. What lies below the gate is measured separately, by
  // scan-below-gate.mjs, and recorded in results/uncovered-carriers.tsv.
  //
  // Regenerate rather than hand-edit: a name added here without a measurement behind it is a claim
  // nothing rechecks.
  '@andrewstory18/is-real-odd', '@anthropic-ai/claude-code', '@apollo/protobufjs', '@apollo/rover',
  '@ast-grep/cli', '@astryxdesign/core', '@aws-amplify/cli', '@azure/mcp',
  '@azure/msal-node-extensions', '@badeball/cypress-cucumber-preprocessor', '@biomejs/biome',
  '@brave/n8n-nodes-brave-search', '@bufbuild/buf', '@bundled-es-modules/glob', '@carbon/colors',
  '@carbon/feature-flags', '@carbon/grid', '@carbon/icon-helpers', '@carbon/icons',
  '@carbon/icons-react', '@carbon/layout', '@carbon/motion', '@carbon/react', '@carbon/styles',
  '@carbon/themes', '@carbon/type', '@carbon/utilities', '@cdktf/node-pty-prebuilt-multiarch',
  '@central-icons-react/round-outlined-radius-3-stroke-2',
  '@central-icons-react/square-outlined-radius-0-stroke-2', '@ckeditor/ckeditor5-react',
  '@clerk/shared', '@compodoc/compodoc', '@confluentinc/kafka-javascript', '@contrast/fn-inspect',
  '@coreui/coreui', '@coreui/react', '@coze/cli', '@datadog/mobile-react-native',
  '@datadog/mobile-react-native-session-replay', '@datadog/native-appsec',
  '@datadog/native-iast-taint-tracking', '@datadog/native-metrics', '@datadog/pprof',
  '@deepseek-ai/dsh-subprocess-local', '@depot/cli', '@discordjs/opus', '@elastic/eui',
  '@embedded-postgres/linux-x64', '@evilmartians/lefthook', '@ffmpeg-installer/linux-arm64',
  '@ffmpeg-installer/linux-x64', '@ffprobe-installer/linux-x64', '@firebase/util',
  '@fission-ai/openspec', '@fortawesome/fontawesome-common-types', '@fortawesome/fontawesome-free',
  '@fortawesome/fontawesome-svg-core', '@fortawesome/free-brands-svg-icons',
  '@fortawesome/free-regular-svg-icons', '@fortawesome/free-solid-svg-icons', '@github/keytar',
  '@google/genai', '@googleworkspace/cli', '@hatchet-dev/typescript-sdk', '@heroui/shared-utils',
  '@hyperjump/json-pointer', '@hyperjump/json-schema', '@hyperjump/json-schema-core', '@ibm/plex',
  '@ibm/plex-sans', '@ibm/plex-sans-hebrew', '@ibm/plex-sans-thai', '@ibm/plex-sans-thai-looped',
  '@ibm/plex-serif', '@infisical/cli', '@journeyapps/wa-sqlite', '@larksuite/cli',
  '@launchql/protobufjs', '@lavamoat/preinstall-always-fail',
  '@matrix-org/matrix-sdk-crypto-nodejs', '@maxmind/geoip2-node', '@medusajs/telemetry',
  '@memlab/cli', '@microsoft/m365agentstoolkit-cli', '@modelcontextprotocol/ext-apps',
  '@modelcontextprotocol/inspector', '@mongodb-js/zstd', '@moonrepo/cli', '@mui/x-telemetry',
  '@napi-rs/simple-git-linux-x64-gnu', '@napi-rs/simple-git-linux-x64-musl', '@nestjs/core',
  '@newrelic/fn-inspect', '@newrelic/native-metrics', '@openapitools/openapi-generator-cli',
  '@opencode-ai/cli', '@openrouter/sdk', '@openuidev/lang-core', '@parcel/watcher', '@percy/core',
  '@playwright/browser-chromium', '@playwright/browser-firefox', '@playwright/browser-webkit',
  '@pnpm/exe', '@posthog/cli', '@prisma/client', '@prisma/engines', '@progress/kendo-licensing',
  '@pulumi/aws', '@pulumi/aws-native', '@pulumi/awsx', '@pulumi/azure-native', '@pulumi/command',
  '@pulumi/docker', '@pulumi/docker-build', '@pulumi/gcp', '@pulumi/kubernetes', '@railway/cli',
  '@react-hookz/deep-equal', '@reown/appkit', '@salesforce/cli', '@sap/hana-client',
  '@scarf/scarf', '@sentry-internal/node-cpu-profiler', '@sentry-internal/node-native-stacktrace',
  '@sentry/cli', '@sentry/node-cpu-profiler', '@sentry/profiling-node',
  '@shopify/react-native-skia', '@stacksjs/ts-webp', '@stdlib/math-base-assert-is-integer',
  '@stdlib/math-base-special-exp', '@stdlib/math-base-special-kernel-cos',
  '@stdlib/math-base-special-kernel-sin', '@stdlib/math-base-special-ldexp',
  '@stdlib/math-base-special-ln', '@stdlib/number-float64-base-exponent',
  '@stdlib/number-float64-base-normalize', '@stellar/stellar-sdk', '@stoprocent/noble',
  '@strapi/strapi', '@swc/core', '@tailwindcss/oxide', '@temporalio/core-bridge',
  '@tensorflow/tfjs-node', '@tloncorp/tlon-skill', '@toruslabs/eccrypto',
  '@tree-sitter-grammars/tree-sitter-yaml', '@trufflesuite/bigint-buffer', '@tsparticles/engine',
  '@turbodocx/html-to-docx', '@uirouter/core', '@vaadin/vaadin-usage-statistics',
  '@vercel/speed-insights', '@vscode/ripgrep', '@vscode/sqlite3', '@vscode/vsce-sign',
  '@vscode/windows-registry', '@whiskeysockets/baileys', '@zowe/secrets-for-zowe-sdk', 'admin-lte',
  'agent-browser', 'agentdb', 'altcha', 'ant-design-vue', 'appium', 'appium-chromedriver',
  'appium-ios-tuntap', 'applicationinsights-native-metrics', 'appmetrics', 'argon2', 'autoevals',
  'aws-crt', 'aws-sdk', 'azure-functions-core-tools', 'backport', 'baileys', 'bcrypt',
  'better-sqlite3', 'bigint-buffer', 'bignum', 'blake-hash', 'blake3', 'bluetooth-hci-socket',
  'bootstrap-vue', 'braintrust', 'browser-tabs-lock', 'btch-downloader', 'bufferutil', 'bun',
  'canvas', 'cbor-extract', 'ccxt', 'chrome-local-mcp', 'chromedriver', 'chromium',
  'classic-level', 'cline', 'cloudflared', 'comfyui-mcp', 'console-stamp', 'contentful', 'core-js',
  'core-js-pure', 'cpu-features', 'cucumber-expressions', 'cwebp-bin', 'cy2', 'cypress',
  'dd-trace', 'deasync', 'deno', 'detox', 'dprint', 'dtrace-provider', 'duckdb', 'edgedriver',
  'ejs', 'electron', 'electron-winstaller', 'epoll', 'es5-ext', 'esbuild', 'exifreader',
  'faiss-node', 'fallow', 'farmhash', 'fetch-mock', 'ffi-napi', 'ffmpeg-static', 'flag-icon-css',
  'flow-bin', 'free-email-domains', 'fs-xattr', 'fsevents', 'full-icu', 'gatsby', 'gatsby-cli',
  'gatsby-telemetry', 'gc-stats', 'geckodriver', 'gifsicle', 'graphql-shield', 'grpc',
  'grpc-tools', 'heapdump', 'highlight.js', 'hnswlib-node', 'hrtime', 'hugo-extended', 'husky',
  'ibm_db', 'iconv', 'iframe-resizer', 'iltorb', 'impit', 'inferno', 'inngest-cli', 'iohook',
  'ip-num', 'isolated-vm', 'javascript-obfuscator', 'jest-preview', 'jpegtran-bin', 'jss',
  'keccak', 'kerberos', 'keytar', 'koffi', 'lefthook', 'less', 'level', 'leveldown', 'libpg-query',
  'libpq', 'libxmljs', 'libxmljs2', 'llnode', 'lmdb', 'lz4', 'lzo', 'maplibre-gl', 'memlab',
  'microtime', 'mongodb-client-encryption', 'mongodb-memory-server', 'mozjpeg', 'msgpackr-extract',
  'msw', 'n8n-nodes-evolution-api', 'native-keymap', 'nestjs-pino', 'netlify', 'netlify-cli',
  'ngrok', 'ngx-infinite-scroll', 'nice-napi', 'node', 'node-expat', 'node-hid', 'node-jq',
  'node-libcurl', 'node-liblzma', 'node-llama-cpp', 'node-pty', 'node-report', 'node-sass',
  'nodejieba', 'nodemon', 'nodent-runtime', 'nuxt', 'nx', 'odbc', 'odiff-bin', 'onnxruntime-node',
  'openclaw', 'opencode-ai', 'optipng-bin', 'oracledb', 'oxc-resolver', 'paper', 'parcel',
  'parse-domain', 'phantomjs-prebuilt', 'playwright-chromium', 'playwright-webkit', 'pngquant-bin',
  'pnpm', 'postinstall-postinstall', 'postman-code-generators', 'pprof', 'pre-commit', 'prisma',
  'protobufjs', 'puppeteer', 'radium', 're2', 'react-final-form', 'react-grab', 'react-jsx-parser',
  'react-native-confirmation-code-field', 'react-native-enriched-markdown',
  'react-native-inappbrowser-reborn', 'react-native-mmkv', 'react-native-nitro-modules',
  'react-native-webrtc', 'realm', 'redis-memory-server', 'ref-napi', 'robotjs', 'rocksdb',
  'scrypt', 'secp256k1', 'segfault-handler', 'serialport', 'serverless', 'sharp',
  'simple-git-hooks', 'sinon', 'skia-canvas', 'sleep', 'snappy', 'snyk', 'sodium-native',
  'sonar-scanner', 'spawn-sync', 'sqlite3', 'squawk-cli', 'sse4_crc32', 'ssh2',
  'storybook-addon-remix-react-router', 'stream-chat', 'stream-chat-react',
  'stream-chat-react-native-core', 'style-dictionary', 'styled-components', 'summernote',
  'supabase', 'svelte-preprocess', 'swagger-ui', 'swiper', 'tesseract.js', 'tiny-secp256k1',
  'tldjs', 'tree-sitter', 'tree-sitter-bash', 'tree-sitter-c', 'tree-sitter-c-sharp',
  'tree-sitter-cli', 'tree-sitter-cpp', 'tree-sitter-go', 'tree-sitter-java',
  'tree-sitter-javascript', 'tree-sitter-json', 'tree-sitter-php', 'tree-sitter-python',
  'tree-sitter-ruby', 'tree-sitter-rust', 'tree-sitter-typescript', 'tsparticles', 'ttf2woff2',
  'type-graphql', 'typechecker', 'typesense-instantsearch-adapter', 'uglifyjs-webpack-plugin',
  'unicode-animations', 'union', 'unix-dgram', 'unrs-resolver', 'usb', 'utf-8-validate',
  'v8-profiler-next', 'vis-data', 'vis-network', 'vis-timeline', 'vnu-jar', 'vue-demi',
  'vue-echarts', 'wd', 'web3-bzz', 'web3-shh', 'websocket', 'win-ca', 'wix-style-react', 'workerd',
  'wrtc', 'yarn', 'yo', 'yorkie', 'zeromq', 'zlib', 'zopflipng-bin', 'zstd-napi',
];

const out = process.argv.includes('--out') ? process.argv[process.argv.indexOf('--out') + 1] : null;
// No window by default — see pickInstalledVersion. The flag exists for sensitivity sweeps only.
const TOP_VERSIONS = process.argv.includes('--top-versions')
  ? Number(process.argv[process.argv.indexOf('--top-versions') + 1])
  : Infinity;

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
 * ⛔⛔ THE RULE IS A DOWNLOAD-SHARE FLOOR, WITH NO TOP-N WINDOW, AND THAT IS A DELIBERATE DEPARTURE
 * FROM scripts/npm-install-script-census.ts. The census ranks a package's versions and looks at the
 * top 3; `topN` here exists only so a band can be re-measured under a window for a sensitivity check.
 * The reason is that the window turned out to be doing far too much of the work to be chosen by hand.
 * Measured over the same 25,421 packages above the download gate:
 *
 *     top 3 → 244 carriers      top 5 → 362 (+48%)      no window → 403 (+11%)
 *
 * A count that moves 48% between two defensible window widths is a property of the instrument, not of
 * npm. The floor is not: a version holding at least 1% of a package's downloads is installed by a real
 * share of that package's users, whatever its rank, and a jail that breaks them is broken. So the
 * floor decides inclusion and nothing else does. The curve converging by the limit — +48% then +11% —
 * is what says the answer is stable rather than merely larger.
 *
 * The window was the census's COST bound, and it is not needed here: the abbreviated packument settles
 * every package that has never carried a script in one request, so the expensive lookup only happens
 * for packages that might qualify.
 *
 * Returns the most-downloaded version that carries a script, when carrying versions together
 * clear the floor; otherwise null.
 * Exported so the ordering can be pinned by a test rather than trusted.
 */
export function pickInstalledVersion(versions, downloads, topN = TOP_VERSIONS, minShare = 0.01) {
  const total = Object.values(downloads).reduce((a, b) => a + (b || 0), 0);
  if (!total) return null;
  const ranked = Object.keys(versions)
    .sort((a, b) => (downloads[b] || 0) - (downloads[a] || 0))
    .slice(0, topN);
  const carriers = ranked.filter((v) => versions[v]?.hasInstallScript);
  if (!carriers.length) return null;
  // ⛔ THE SHARE IS SUMMED ACROSS EVERY CARRYING VERSION, NOT TAKEN FROM THE BEST ONE. The question is
  // what fraction of a package's installs run an install script, and for a package with a long release
  // history that fraction is spread thin rather than concentrated. Measured 2026-09-06:
  // `@pulumi/azure-native` has 1,318 versions of which 719 carry a script, its best single one holds
  // 0.77% of downloads, and they sum to 1.82% — so a per-version floor excluded a package where nearly
  // one install in fifty runs a script. Summing subsumes the per-version test, since a single version
  // over the floor puts the sum over it too.
  const share = carriers.reduce((a, v) => a + (downloads[v] || 0), 0) / total;
  if (share < minShare) return null;
  // Sweep the most-installed carrying version — `ranked` is already in download order.
  return carriers[0];
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
