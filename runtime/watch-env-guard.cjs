// Node's watch supervisor skips preload modules and spawns each restarted child
// from its unchanged environment. Each child has already parsed the raw
// `--env-file` values when this first `NODE_OPTIONS` preload runs. Remove
// file-derived runtime-control spellings and restore the ambient values before
// any user preload or entry module can observe them. This stays separate from
// Nub's full preload so user/PnP preloads still patch the runtime before Nub
// captures any built-in functions.

const WATCH_ENV_GUARD = "__NUB_WATCH_ENV_GUARD";
const { isMainThread } = require("node:worker_threads");
const FALLBACK_DENYLIST = [
  "NODE_OPTIONS",
  "NODE_TLS_REJECT_UNAUTHORIZED",
  "NODE_EXTRA_CA_CERTS",
  "NODE_REPL_EXTERNAL_MODULE",
];
const asciiUpper = (value) => value.replace(
  /[a-z]/g,
  (character) => String.fromCharCode(character.charCodeAt(0) - 32),
);
const clearDenied = (denylist, ambientKeys = []) => {
  const denied = new Set(denylist.map(asciiUpper));
  const ambientKey = process.platform === "win32"
    ? asciiUpper
    : (key) => key;
  const ambient = new Set(ambientKeys.map(ambientKey));
  for (const key of Object.keys(process.env)) {
    if (denied.has(asciiUpper(key)) && !ambient.has(ambientKey(key))) {
      delete process.env[key];
    }
  }
};

const watchEnvGuardRaw = process.env[WATCH_ENV_GUARD];
delete process.env[WATCH_ENV_GUARD];

try {
  const state = JSON.parse(watchEnvGuardRaw);
  if (
    !state ||
    !Array.isArray(state.denylist) ||
    !state.denylist.every((key) => typeof key === "string") ||
    !Array.isArray(state.ambientKeys) ||
    !state.ambientKeys.every((key) => typeof key === "string") ||
    (state.nodeOptions !== null && typeof state.nodeOptions !== "string")
  ) {
    throw new TypeError("invalid watch env guard state");
  }
  const denied = new Set(state.denylist.map(asciiUpper));
  if (!FALLBACK_DENYLIST.every((key) => denied.has(key))) {
    throw new TypeError("incomplete watch env guard denylist");
  }
  clearDenied(state.denylist, state.ambientKeys);
  if (typeof state.nodeOptions === "string") {
    process.env.NODE_OPTIONS = state.nodeOptions;
  } else {
    delete process.env.NODE_OPTIONS;
  }
} catch (cause) {
  // Compat-tier `--import` creates a loader worker that re-runs NODE_OPTIONS
  // preloads after the main child consumed this marker. Missing state is that
  // legitimate re-entry only off the main thread, where the inherited env was
  // already sanitized. Preserve its ambient values. Missing main-thread state
  // or any present-but-invalid state is corruption: clear and abort before user
  // preloads or the entry module can observe an unfiltered value.
  if (watchEnvGuardRaw === undefined && !isMainThread) {
    // The main child already removed file-derived values and restored ambient.
  } else {
    clearDenied(FALLBACK_DENYLIST);
    const detail = watchEnvGuardRaw === undefined ? "missing" : "invalid";
    const error = new Error(`Nub watch env-file guard state is ${detail}`, { cause });
    error.code = "ERR_NUB_WATCH_ENV_GUARD";
    throw error;
  }
}
