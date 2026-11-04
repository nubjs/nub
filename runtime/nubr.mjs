#!/usr/bin/env node
// `nubr` — the standalone runner's command. One name over the three things the
// full CLI splits across `nub <file>`, `nub run` and `nubx`, because a package
// that ships a single bin has to unify them:
//
//   nubr app.ts        a FILE that exists        → run it with the hooks armed
//   nubr build         a package.json key        → run that script
//   nubr vitest        a node_modules/.bin entry → run that bin
//
// Resolution is most-specific-first, and each tier is rarer than the one above
// it. A path beats a script because a script named after an existing file is
// vanishingly rare; a script beats a bin because a script usually WRAPS the bin
// it is named after, which is npm's own precedence. A directory comes last, so
// that `build/` existing cannot shadow the `build` script — it did, and it died
// inside Node's resolver.
//
// A file run RE-EXECS Node with `--import` rather than arming the hooks in this
// process and calling `Module.runMain`. In-process is ~30 ms cheaper and was the
// first design, but it is not the same environment, and the differences are
// silent: `Module.runMain` routes the entry through the CommonJS loader, which
// cannot load an ES module below Node 22.15; and a worker thread inherits its
// preload from the option store rather than from `process.execArgv`, so
// `new Worker("./child.ts")` failed on every Node version until this changed
// (mutating `process.execArgv` does not reach the worker — measured). Both were
// found by the fixture matrix, one after the other, which is the tell that the
// class is open-ended. Re-execing makes the command identical to the documented
// `--import` form by construction instead of by enumeration, which is also why
// tsx does it.
import module from "node:module";
if (module.enableCompileCache) module.enableCompileCache();

import { existsSync, readFileSync, statSync } from "node:fs";
import { spawn } from "node:child_process";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import {
  binExts,
  commandLine,
  effectiveShell,
  isCmdShell,
  isPowerShell,
  spliceArgs,
} from "./nubr-escape.mjs";

const HERE = path.dirname(fileURLToPath(import.meta.url));
// Absolute, because a bare specifier never resolves out of a global install and
// `NODE_PATH` does not apply to ESM — an absolute URL is the only form that
// reaches a child process reliably.
const REGISTER_URL = pathToFileURL(path.join(HERE, "loader-register.mjs")).href;

// The shell that will run a script body or a resolved bin, resolved once. Three
// separate things read it — which shell `spawn` is given, which escaping the
// forwarded arguments get, and which `node_modules/.bin` shim is runnable — and
// they are only consistent if they read the SAME value. Deriving any of them
// from `process.platform` instead has now produced two defects in a row.
const SHELL = effectiveShell();

const USAGE = `nubr — run TypeScript on Node

  nubr <file>              run a file
  nubr <script> [args...]  run a script from package.json
  nubr <bin> [args...]     run an installed bin from node_modules/.bin
  nubr [node flags] <file> run a file under Node flags (--inspect, ...)

  -h, --help     show this message
  -v, --version  show the version
`;

function readManifest(dir) {
  const file = path.join(dir, "package.json");
  if (!existsSync(file)) return null;
  try {
    return JSON.parse(readFileSync(file, "utf8"));
  } catch {
    return null;
  }
}

// Follows symlinks, so a link to a file classifies as a file — the same thing
// Node does with the same argument. Returns null for anything absent, dangling
// or unreadable, all of which mean "not a path we can run".
function statKind(p) {
  try {
    const st = statSync(p, { throwIfNoEntry: false });
    return st ? (st.isDirectory() ? "dir" : "file") : null;
  } catch {
    return null;
  }
}

// Every `node_modules/.bin` from the cwd up to the filesystem root, nearest
// first — the same lookup npm gives a lifecycle script, so a script can call a
// dependency's bin by bare name.
function binPath(from) {
  const dirs = [];
  let dir = from;
  for (;;) {
    dirs.push(path.join(dir, "node_modules", ".bin"));
    const parent = path.dirname(dir);
    if (parent === dir) break;
    dir = parent;
  }
  return dirs;
}

// The hooks reach a script's child processes through NODE_OPTIONS, which Node
// inherits down the whole tree. Append rather than replace: a caller's own
// NODE_OPTIONS is theirs to keep. The npm_* values are the documented script
// environment; without them a script reading `npm_package_version` gets
// undefined unless an outer npm happened to launch us.
function childEnv(cwd, manifest, manifestPath) {
  const existing = process.env.NODE_OPTIONS ? `${process.env.NODE_OPTIONS} ` : "";
  const pathKey = Object.keys(process.env).find((k) => k.toLowerCase() === "path") ?? "PATH";
  const pkgConfig = {};
  for (const [k, v] of Object.entries(manifest.config ?? {})) {
    pkgConfig[`npm_package_config_${k}`] = String(v);
  }
  return {
    ...process.env,
    NODE_OPTIONS: `${existing}--import ${REGISTER_URL}`,
    [pathKey]: [...binPath(cwd), process.env[pathKey] ?? ""].join(path.delimiter),
    npm_package_name: manifest.name ?? "",
    npm_package_version: manifest.version ?? "",
    npm_package_json: manifestPath,
    npm_node_execpath: process.execPath,
    npm_execpath: fileURLToPath(import.meta.url),
    npm_command: "run-script",
    ...pkgConfig,
  };
}

// A path we resolved, spelled the way the effective shell reads it. A POSIX-like
// shell on Windows re-parses its own Windows command line with MSYS rules, where
// a backslash is an ESCAPE rather than a separator — so Git Bash received a
// correctly single-quoted `'C:\Users\...\whichshim'` and still reported
// `C:UsersRUNNER~1AppData...: command not found`, every separator eaten
// (measured on the Windows CI leg). Forward slashes are what that shell wants and
// Windows accepts them everywhere, so only the SEPARATORS of a path we produced
// are rewritten. A forwarded argument is the user's data and is never touched.
function shellPath(p) {
  if (process.platform !== "win32" || isCmdShell(SHELL)) return p;
  return p.replace(/\\/g, "/");
}

// Hand a finished command line to the effective shell, assembled the way npm
// assembles one: `['/d', '/s', '/c', script]` with the script bare and
// `windowsVerbatimArguments` set, so libuv does not re-quote the string cmd.exe
// is meant to parse itself. Node's `shell` option is equivalent — it wraps the
// script in one more pair of quotes and sets the same flag — and a Windows probe
// measured the two delivering identical argv across nine adversarial argument
// vectors. This spelling is kept because the escaping in nubr-escape.mjs is a
// byte-exact port of npm's and is computed against THIS assembly; keeping both
// halves from one source is what stops them drifting apart.
function shellSpawn(commandLine, opts) {
  if (isCmdShell(SHELL)) {
    return spawn(SHELL, ["/d", "/s", "/c", commandLine], {
      ...opts,
      windowsVerbatimArguments: true,
    });
  }
  return spawn(SHELL, ["-c", commandLine], opts);
}

function runScript(name, manifest, rawExtraArgs, cwd) {
  const scripts = manifest.scripts ?? {};
  // `nubr build -- --watch` appends `--watch`, not `-- --watch`: npm consumes the
  // separator, and a shell would otherwise hand the literal `--` to the script.
  const extraArgs = rawExtraArgs[0] === "--" ? rawExtraArgs.slice(1) : rawExtraArgs;
  // npm's pre/post convention. Skipping it silently drops a `prebuild` the
  // author expects to run, which is the kind of wrong answer nobody notices.
  const phases = [`pre${name}`, name, `post${name}`].filter((p) => scripts[p]);
  const env = childEnv(cwd, manifest, path.join(cwd, "package.json"));

  const step = (i) => {
    if (i >= phases.length) return;
    const phase = phases[i];
    // Extra args go to the named script only, never to its pre/post hooks —
    // matching npm, where `npm run build -- --watch` leaves `prebuild` alone.
    const body =
      phase === name ? spliceArgs(scripts[phase], extraArgs, SHELL) : scripts[phase];
    const child = shellSpawn(body, {
      cwd,
      stdio: "inherit",
      env: { ...env, npm_lifecycle_event: phase, npm_lifecycle_script: scripts[phase] },
    });
    child.on("exit", (code, signal) => {
      if (signal) process.kill(process.pid, signal);
      else if (code !== 0) process.exit(code ?? 1);
      else step(i + 1);
    });
    child.on("error", (err) => {
      process.stderr.write(`nubr: could not run script "${phase}": ${err.message}\n`);
      process.exit(1);
    });
  };
  step(0);
}

// A bin from the `node_modules/.bin` chain, run ad hoc. It goes through the
// shell — the same one, with the same environment, a script body gets — so the
// argument escaping is the code the three-OS leg already exercises rather than a
// second spawn implementation.
//
// What is passed to that shell is the RESOLVED PATH, never the bare name.
// Handing back the name reintroduces the shell's own lookup, which does not
// agree with ours: `sh -c "test …"` runs the BUILTIN, not
// `node_modules/.bin/test`, and reports exit 1 with no output — the user sees a
// plausible failure and never learns their bin did not run (reproduced). Naming
// the path also makes the Windows `.cmd` shim visible to the batch-file test, so
// forwarded arguments get the second caret pass cmd.exe's re-parse needs.
function runBin(name, binPathAbs, rawExtraArgs, manifest, cwd) {
  const extraArgs = rawExtraArgs[0] === "--" ? rawExtraArgs.slice(1) : rawExtraArgs;
  const child = shellSpawn(commandLine(shellPath(binPathAbs), extraArgs, SHELL), {
    cwd,
    stdio: "inherit",
    // No npm_lifecycle_* here: nothing in package.json declared this run, so
    // reporting a lifecycle event would be a lie a tool could branch on.
    env: { ...childEnv(cwd, manifest ?? {}, path.join(cwd, "package.json")), npm_command: "exec" },
  });
  child.on("exit", (code, signal) => {
    if (signal) process.kill(process.pid, signal);
    else process.exit(code ?? 1);
  });
  child.on("error", (err) => {
    process.stderr.write(`nubr: could not run "${name}": ${err.message}\n`);
    process.exit(1);
  });
}

// The shim this shell can execute, from the same value that will execute it —
// see `binExts`. Selecting it from `process.platform` instead picked the `.cmd`
// on a Windows box whose ComSpec is a POSIX-like shell, which then cannot run a
// batch file: every ordinary dependency bin failed, under a configuration the
// escaping half already supports on purpose. Mirrors the resolution the full
// CLI's `nubx` performs.
function findBin(name, cwd) {
  // A name with a separator is a path, not a bin — it was already given its
  // chance as a file, and letting it match here would resolve `./x` off PATH.
  if (name.includes("/") || name.includes("\\")) return null;
  for (const dir of binPath(cwd)) {
    for (const ext of binExts(SHELL)) {
      const candidate = path.join(dir, name + ext);
      if (statKind(candidate) === "file") return candidate;
    }
  }
  return null;
}

// `args` is already a Node command line: either [file, ...rest] that we
// classified ourselves, or the caller's verbatim argv when it opened with a
// flag. Only the preload is inserted.
function runFile(args) {
  const child = spawn(process.execPath, ["--import", REGISTER_URL, ...args], {
    stdio: "inherit",
  });
  child.on("exit", (code, signal) => {
    if (signal) process.kill(process.pid, signal);
    else process.exit(code ?? 1);
  });
  child.on("error", (err) => {
    process.stderr.write(`nubr: could not start Node: ${err.message}\n`);
    process.exit(1);
  });
}

async function main() {
  const argv = process.argv.slice(2);
  if (argv.length === 0) {
    process.stdout.write(USAGE);
    process.exit(1);
  }

  if (argv[0] === "-h" || argv[0] === "--help") {
    process.stdout.write(USAGE);
    return;
  }
  if (argv[0] === "-v" || argv[0] === "--version") {
    const self = readManifest(HERE) ?? readManifest(path.join(HERE, ".."));
    process.stdout.write(`${self?.version ?? "unknown"}\n`);
    return;
  }

  // A leading flag means a file run under Node options, and Node is the only
  // thing that knows which of its options take a separate value — enumerating
  // them here would silently mis-split `--conditions development app.ts`,
  // treating the value as the target. So hand the whole argv to Node verbatim
  // and let it find its own entry point. Only a first token that is NOT a flag
  // has to be classified as file-or-script, and that case has no ambiguity.
  let i = 0;
  if (argv[0].startsWith("-") && argv[0] !== "--") {
    runFile(argv);
    return;
  }
  if (argv[0] === "--") i = 1;

  const target = argv[i];
  const rest = argv.slice(i + 1);
  if (target === undefined) {
    process.stderr.write("nubr: no file or script given\n");
    process.exit(1);
  }

  const cwd = process.cwd();
  const asPath = path.resolve(cwd, target);
  // A FILE outranks a script; a DIRECTORY does not. `build`, `dist`, `test`,
  // `docs` and `lib` are all ordinary directory names AND ordinary script
  // names, so keying this on mere existence pointed `nubr build` at the build
  // DIRECTORY and died in Node's resolver with ERR_UNSUPPORTED_DIR_IMPORT while
  // npm ran the script. A directory still runs as an entry point when no script
  // claims the name, which is what plain `node <dir>` does.
  const kind = statKind(asPath);
  if (kind === "file") {
    runFile([asPath, ...rest]);
    return;
  }

  const manifest = readManifest(cwd);
  if (manifest?.scripts?.[target]) {
    runScript(target, manifest, rest, cwd);
    return;
  }

  // A dependency's bin, run ad hoc — the thing a standalone install otherwise
  // cannot do without editing package.json first. A script of the same name
  // still wins, matching npm, where a script shadows the bin it usually wraps.
  const bin = findBin(target, cwd);
  if (bin) {
    runBin(target, bin, rest, manifest, cwd);
    return;
  }

  if (kind === "dir") {
    runFile([asPath, ...rest]);
    return;
  }

  const names = Object.keys(manifest?.scripts ?? {});
  process.stderr.write(
    `nubr: "${target}" is not a file, a package.json script, or an installed bin\n` +
      // Under PowerShell nothing can match, because `binExts` offers no candidate
      // there — so say so, rather than let an installed bin read as a typo.
      (isPowerShell(SHELL)
        ? `  ComSpec is ${SHELL}, and nubr cannot run an installed bin through PowerShell.\n` +
          `  Set ComSpec to cmd.exe, or call the bin from a package.json script.\n`
        : "") +
      (names.length ? `  scripts: ${names.join(", ")}\n` : ""),
  );
  process.exit(1);
}

await main();
