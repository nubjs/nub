// Produce an ablated copy of runtime/compile-preamble.mjs on stdout.
//
// The preamble's static import graph is evaluated on every run of every artifact,
// whether or not anything calls into it, so the only way to price a member of that
// graph is to remove the IMPORT — not the call. Each ablation below deletes one
// import plus every use that would then be an undefined binding, so the result is
// still a preamble that runs; the delta against `full` is that module's evaluation
// cost. `-call` variants delete only the call, which separates evaluation from work.
//
// Deliberately a throwaway measurement instrument, not a build step: it edits by
// source anchors and asserts every anchor matched, so a drifted preamble fails loudly
// rather than silently measuring nothing.
import { readFileSync } from "node:fs";

const [, , file, ...drops] = process.argv;
let src = readFileSync(file, "utf8");

function cut(re, what) {
  const before = src;
  src = src.replace(re, "");
  if (src === before) throw new Error(`ablate: anchor did not match: ${what}`);
}

// Delete `head` through the balanced `close` that matches the `open` inside it,
// following an `else` onto the next block so an if/else pair goes as one unit.
function cutBalanced(head, open, close, what) {
  const at = src.indexOf(head);
  if (at < 0) throw new Error(`ablate: anchor did not match: ${what}`);
  let end = at;
  for (;;) {
    let depth = 0;
    let i = end === at ? at : end;
    for (; i < src.length; i++) {
      if (src[i] === open) depth++;
      else if (src[i] === close) {
        depth--;
        if (depth === 0) break;
      }
    }
    if (depth !== 0) throw new Error(`ablate: unbalanced ${open}${close} after ${what}`);
    end = i + 1;
    if (src.slice(end).trimStart().startsWith("else")) continue;
    break;
  }
  if (src[end] === ";") end++;
  while (src[end] === "\n") end++;
  src = src.slice(0, at) + src.slice(end);
}

for (const drop of drops) {
  switch (drop) {
    case "worker":
      cut(/^import \{\n(?:  .*\n)*?\} from "\.\/worker-polyfill\.mjs";\n/m, "worker-polyfill import");
      cut(/^import \{ blobUrlSource, installBlobUrlSupport \} from "\.\/worker-blob-url\.cjs";\n/m, "worker-blob-url import");
      cut(/^ *setWorkerCreateRequire\(.*\n/m, "setWorkerCreateRequire");
      cut(/^ *setBlobUrlModule\(.*\n/m, "setBlobUrlModule");
      cut(/^ *setCompiledBootstrapRequireArg\(.*\n/m, "setCompiledBootstrapRequireArg");
      cutBalanced("if (bootstrap.needsWorker) {", "{", "}", "worker install/accessor block");
      break;
    case "childprocess":
      cut(/^import \{\n(?:  .*\n)*?\} from "\.\/preload-common\.cjs";\n/m, "preload-common import");
      cut(/^ *if \(bootstrap\.needsChildProcess\) installCompiledChildProcess\(\);\n/m, "installCompiledChildProcess call");
      // Stripped by strip_native_polyfills for any target >= 26 anyway, so removing
      // it here is a no-op for the Node 26 measurement and only keeps the ablated
      // source syntactically whole for a lower target.
      cut(/^ *installTemporalGlobal\(\{ Temporal, toTemporalInstant \}\);\n/m, "installTemporalGlobal call");
      break;
    case "syncpolyfills":
      cut(/^import \{ installSyncPolyfills \} from "\.\/polyfills\.cjs";\n/m, "polyfills import");
      cutBalanced("installSyncPolyfills(", "(", ")", "installSyncPolyfills call");
      break;
    case "syncpolyfills-call":
      // Import kept, so the module is still evaluated; only the work is removed.
      cutBalanced("installSyncPolyfills(", "(", ")", "installSyncPolyfills call");
      src = src.replace(
        /^import \{ installSyncPolyfills \} from "\.\/polyfills\.cjs";$/m,
        'import { installSyncPolyfills } from "./polyfills.cjs";\nif (globalThis.__never) installSyncPolyfills({});',
      );
      break;
    case "empty":
      src = "export function installCompilePreamble() {}\ninstallCompilePreamble();\n";
      break;
    default:
      throw new Error(`ablate: unknown drop ${drop}`);
  }
}

process.stdout.write(src);
