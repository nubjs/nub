// Runtime globals compiled artifacts need without Nub's loader or installed runtime.
const bootstrap = process[Symbol.for("nub.compile.bootstrap")];
const COMPILED_WORKER_STATE = "nub.compile.worker-state";
// Loading node:worker_threads costs ~1.4 ms on every run of the artifact, so it is
// skipped when the payload's sealed graph never reaches Worker or worker_threads.
// The bootstrap made that call at build time — see its `nub:compile:worker` region
// — and a null here means no Worker can exist in this process, so there is no
// state to carry and nothing to publish.
const workerThreads = bootstrap.needsWorker
  ? bootstrap.getBuiltin("node:worker_threads")
  : null;

function readCompiledWorkerState() {
  if (workerThreads === null) return null;
  let state;
  try {
    state = workerThreads.getEnvironmentData(COMPILED_WORKER_STATE);
    if (state === null || typeof state !== "object" || Array.isArray(state)) return null;
    const fields = Reflect.ownKeys(state);
    if (
      fields.length !== 2 ||
      !Object.hasOwn(state, "compiledExecPath") ||
      !Object.hasOwn(state, "neutralizeLocalStorage") ||
      typeof state.compiledExecPath !== "string" ||
      state.compiledExecPath.length === 0 ||
      typeof state.neutralizeLocalStorage !== "boolean"
    ) {
      return null;
    }
  } catch {
    return null;
  }
  return state;
}

const carriedWorkerState = readCompiledWorkerState();
const compiledExecPathFromEnv = process.env.__NUB_COMPILED_EXEC_PATH;
const compiledExecPath =
  typeof compiledExecPathFromEnv === "string" && compiledExecPathFromEnv.length !== 0
    ? compiledExecPathFromEnv
    : compiledExecPathFromEnv === undefined
      ? carriedWorkerState?.compiledExecPath
      : undefined;
const neutralizeLocalStorage =
  process.env.__NUB_NEUTRALIZE_LOCALSTORAGE !== undefined ||
  carriedWorkerState?.neutralizeLocalStorage === true;

// options.env can omit the launcher's private env, but Worker environment data
// is cloned independently. Restore the polyfill signal before it is consumed and
// republish validated state for native, global, and nested Workers.
if (
  neutralizeLocalStorage &&
  process.env.__NUB_NEUTRALIZE_LOCALSTORAGE === undefined
) {
  process.env.__NUB_NEUTRALIZE_LOCALSTORAGE = "1";
}
if (
  workerThreads !== null &&
  typeof compiledExecPath === "string" &&
  compiledExecPath.length !== 0
) {
  workerThreads.setEnvironmentData(COMPILED_WORKER_STATE, {
    compiledExecPath,
    neutralizeLocalStorage,
  });
}

// Each polyfill below sits in a `nub:polyfill:<name>` region. When `nub compile`
// probes the Node it is embedding and finds the global already native, it strips
// the matching regions from this source before bundling — so the polyfill is never
// pulled into the graph at all. Measured on Node 26, where Temporal, URLPattern,
// Float16Array and navigator are all native: ~195 KB of the 206 KB bundle was dead
// weight, and constructing it cost ~18 ms on every launch.
//
// Keep an import and its use in regions of the SAME name, and keep every region
// independently removable — stripping one must leave valid syntax behind.
// #region nub:polyfill:urlpattern
import { URLPattern } from "urlpattern-polyfill/urlpattern";
// #endregion
// #region nub:polyfill:float16
import * as float16 from "@petamoriken/float16";
// #endregion
// #region nub:polyfill:temporal
import { Temporal, toTemporalInstant } from "@js-temporal/polyfill";
// #endregion
import { installSyncPolyfills } from "./polyfills.cjs";
import {
  installCompiledChildProcess,
  // #region nub:polyfill:temporal
  installTemporalGlobal,
  // #endregion
} from "./preload-common.cjs";
// #region nub:polyfill:navigator
import {
  installNavigatorShim,
  setBootstrapCreateRequire as setNavigatorCreateRequire,
} from "./navigator-shim.mjs";
// #endregion
// #region nub:polyfill:navigatorlocks
import { installNavigatorLocks } from "./navigator-locks.mjs";
// #endregion
import {
  installWorkerPolyfill,
  setBootstrapCreateRequire as setWorkerCreateRequire,
  setBlobUrlModule,
  setCompiledBootstrapRequireArg,
} from "./worker-polyfill.mjs";
// Statically imported so it is BUNDLED rather than resolved beside the emitted
// chunk. worker-polyfill reaches this module through
// `createRequire(import.meta.url)("./worker-blob-url.cjs")` — correct for an
// ordinary nub run, where the eager CommonJS preload must share the one registry
// instance, but in a compiled artifact it is the only reason the payload has to
// ship a sibling file at all. Importing it here keeps the whole preamble inside
// the bundle, which is what lets a pure-JavaScript payload run straight from the
// executable with nothing on disk.
import { blobUrlSource, installBlobUrlSupport } from "./worker-blob-url.cjs";

export function installCompilePreamble() {
  // Node 18/20 lack process.getBuiltinModule, so keep builtin lookup tied to the
  // early fixed-root bootstrap rather than the bundle or an installed runtime path.
  // #region nub:polyfill:navigator
  setNavigatorCreateRequire(bootstrap.createRequire);
  // #endregion
  setWorkerCreateRequire(bootstrap.createRequire);
  setBlobUrlModule({ blobUrlSource, installBlobUrlSupport });
  setCompiledBootstrapRequireArg(bootstrap.requireArg);

  // Eagerly loads and patches node:child_process — the one place that cost is
  // unavoidable when the payload uses it, because an ESM `import { spawn }`
  // bypasses Module._load and so cannot be intercepted lazily. A payload whose
  // sealed graph never names child_process or cluster skips it and the fork
  // identity fix-up in the bootstrap together; neither has anything to correct.
  if (bootstrap.needsChildProcess) installCompiledChildProcess();
  if (compiledExecPath !== undefined) {
    process.execPath = compiledExecPath;
    process.argv[0] = compiledExecPath;
  }

  installSyncPolyfills({
    // #region nub:polyfill:urlpattern
    urlpattern: { URLPattern },
    // #endregion
    // #region nub:polyfill:float16
    float16,
    // #endregion
  });
  // #region nub:polyfill:navigator
  installNavigatorShim();
  // #endregion
  // #region nub:polyfill:navigatorlocks
  installNavigatorLocks();
  // #endregion
  if (bootstrap.needsWorker) installWorkerPolyfill();
  // #region nub:polyfill:temporal
  installTemporalGlobal({ Temporal, toTemporalInstant });
  // #endregion
}

installCompilePreamble();
