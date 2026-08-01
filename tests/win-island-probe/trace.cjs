// Preloaded into a compiled artifact's Node via NODE_OPTIONS so the CJS
// resolution candidates are visible. `process._rawDebug` writes to stderr
// synchronously, so nothing is lost when the process dies mid-resolution.
const Module = require("node:module");
const path = require("node:path");

Error.stackTraceLimit = 100;

process._rawDebug("[trace] execPath=" + process.execPath);
process._rawDebug("[trace] argv=" + JSON.stringify(process.argv));
process._rawDebug("[trace] cwd=" + process.cwd());
process._rawDebug("[trace] =C:=" + JSON.stringify(process.env["=C:"]));
process._rawDebug("[trace] =D:=" + JSON.stringify(process.env["=D:"]));

const originalFindPath = Module._findPath;
Module._findPath = function (request, paths, isMain) {
  try {
    const resolved = Array.isArray(paths)
      ? paths.map((p) => `${JSON.stringify(p)} -> ${JSON.stringify(path.resolve(p, request))}`)
      : String(paths);
    process._rawDebug(
      `[findPath] ${JSON.stringify(request)} isMain=${isMain} :: ${JSON.stringify(resolved)}`,
    );
  } catch (error) {
    process._rawDebug(`[findPath] trace error ${error.message}`);
  }
  return originalFindPath.call(this, request, paths, isMain);
};

const originalResolveFilename = Module._resolveFilename;
Module._resolveFilename = function (request, parent, ...rest) {
  try {
    process._rawDebug(
      `[resolveFilename] ${JSON.stringify(request)} parent=${JSON.stringify(parent && parent.filename)}`,
    );
  } catch {
    // tracing must never change resolution
  }
  return originalResolveFilename.call(this, request, parent, ...rest);
};
