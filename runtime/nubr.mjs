#!/usr/bin/env node
// `nubr` — the standalone runner's command. Two jobs behind one name, matching
// what `aubr` (→ `aube run`) and `vpr` (→ `vp run`) already mean, plus the file
// run that `nub <file>` covers in the full CLI:
//
//   nubr app.ts        a FILE that exists  → run it with the hooks armed
//   nubr build         a package.json key  → run that script
//
// File beats script when both could match, because a path is the more specific
// intent and a script named after an existing file is vanishingly rare.
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
import { effectiveShell, spliceArgs } from "./nubr-escape.mjs";

const HERE = path.dirname(fileURLToPath(import.meta.url));
// Absolute, because a bare specifier never resolves out of a global install and
// `NODE_PATH` does not apply to ESM — an absolute URL is the only form that
// reaches a child process reliably.
const REGISTER_URL = pathToFileURL(path.join(HERE, "loader-register.mjs")).href;

const USAGE = `nubr — run TypeScript on Node

  nubr <file>              run a file
  nubr <script> [args...]  run a script from package.json
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

function runScript(name, manifest, rawExtraArgs, cwd) {
  const scripts = manifest.scripts ?? {};
  // `nubr build -- --watch` appends `--watch`, not `-- --watch`: npm consumes the
  // separator, and a shell would otherwise hand the literal `--` to the script.
  const extraArgs = rawExtraArgs[0] === "--" ? rawExtraArgs.slice(1) : rawExtraArgs;
  // npm's pre/post convention. Skipping it silently drops a `prebuild` the
  // author expects to run, which is the kind of wrong answer nobody notices.
  const phases = [`pre${name}`, name, `post${name}`].filter((p) => scripts[p]);
  const env = childEnv(cwd, manifest, path.join(cwd, "package.json"));
  // Name the shell explicitly instead of `shell: true`, so the shell that RUNS
  // the script and the escaping applied to its arguments come from one value.
  // Deriving the escape from `process.platform` instead got this wrong on a
  // Windows box whose ComSpec is not cmd, where Node invokes the shell with -c.
  const shell = effectiveShell();

  const step = (i) => {
    if (i >= phases.length) return;
    const phase = phases[i];
    // Extra args go to the named script only, never to its pre/post hooks —
    // matching npm, where `npm run build -- --watch` leaves `prebuild` alone.
    const body =
      phase === name ? spliceArgs(scripts[phase], extraArgs, shell) : scripts[phase];
    const child = spawn(body, {
      shell,
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

  if (kind === "dir") {
    runFile([asPath, ...rest]);
    return;
  }

  const names = Object.keys(manifest?.scripts ?? {});
  process.stderr.write(
    `nubr: no file at "${target}" and no such script in package.json\n` +
      (names.length ? `  scripts: ${names.join(", ")}\n` : ""),
  );
  process.exit(1);
}

await main();
