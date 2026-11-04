/**
 * Compile/run corpus driver. This is deliberately dependency-free: it is invoked
 * by the candidate nub, creates an npm-compatible project, and uses that same
 * candidate to install and compile it. See README.md for the proof contract.
 */
import { createHash } from "node:crypto";
import { existsSync } from "node:fs";
import { cp, mkdir, mkdtemp, readdir, readFile, realpath, rename, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { basename, dirname, extname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";
import process from "node:process";

const here = dirname(fileURLToPath(import.meta.url));
const defaults = {
  nub: process.env.NUB_NATIVE_ISLAND_NUB,
  launcher: process.env.NUB_NATIVE_ISLAND_LAUNCHER,
  target: process.env.NUB_NATIVE_ISLAND_TARGET,
  platform: process.env.NUB_NATIVE_ISLAND_PLATFORM,
  keep: process.env.KEEP === "1",
  workspace: process.env.NUB_NATIVE_ISLAND_WORKSPACE,
  mode: process.env.NUB_NATIVE_ISLAND_MODE || "prebuilt",
  verifyCompanionDllRename: process.env.NUB_NATIVE_ISLAND_VERIFY_COMPANION_DLL_RENAME === "1",
  logDir: process.env.COMPILE_NATIVE_LOG_DIR,
};
const commandLogs = [];
const compactLogLimit = 32 * 1024;

function usage(message) {
  if (message) console.error(`error: ${message}`);
  console.error("usage: run.sh --nub <candidate> --launcher <matching-launcher> --target <node-version> [--platform <triple>] [--mode prebuilt] [--verify-companion-dll-rename] [--keep] [--workspace <path>]");
  process.exit(2);
}

function args(argv) {
  const options = { ...defaults };
  const positional = [];
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (["--nub", "--launcher", "--target", "--platform", "--workspace", "--mode"].includes(arg)) {
      if (!argv[i + 1]) usage(`missing value for ${arg}`);
      options[arg.slice(2)] = argv[++i];
    } else if (arg === "--keep") {
      options.keep = true;
    } else if (arg === "--verify-companion-dll-rename") {
      options.verifyCompanionDllRename = true;
    } else if (arg.startsWith("-")) {
      usage(`unknown option ${arg}`);
    } else {
      positional.push(arg);
    }
  }
  if (!options.nub && positional.length) options.nub = positional.shift();
  if (!options.launcher && positional.length) options.launcher = positional.shift();
  if (positional.length) usage(`unexpected positional argument ${positional[0]}`);
  if (!options.nub || !options.launcher || !options.target) usage("--nub, --launcher, and --target are required");
  if (options.mode !== "prebuilt") usage(`unsupported --mode ${options.mode}; this corpus only accepts prebuilt packages`);
  if (options.verifyCompanionDllRename && options.platform && !options.platform.startsWith("win32-")) {
    usage("--verify-companion-dll-rename is only valid for a win32 target");
  }
  return { ...options, nub: resolve(options.nub), launcher: resolve(options.launcher) };
}

function fail(message) {
  throw new Error(message);
}

function compact(value) {
  const text = value || "";
  return text.length <= compactLogLimit ? text : `…[truncated ${text.length - compactLogLimit} bytes]…\n${text.slice(-compactLogLimit)}`;
}

function command(label, file, commandArgs, options) {
  const result = spawnSync(file, commandArgs, { encoding: "utf8", ...options });
  commandLogs.push({
    label,
    command: [file, ...commandArgs],
    cwd: options.cwd,
    status: result.status,
    error: result.error?.message,
    stdout: compact(result.stdout),
    stderr: compact(result.stderr),
  });
  if (result.error) fail(`${file} ${commandArgs.join(" ")}: ${result.error.message}`);
  if (result.status !== 0) {
    fail(`${file} ${commandArgs.join(" ")} exited ${result.status}\nstdout:\n${result.stdout}\nstderr:\n${result.stderr}`);
  }
  return result;
}

async function persistFailureLogs(logDir) {
  if (!logDir) return;
  await mkdir(logDir, { recursive: true });
  const rendered = commandLogs.map((entry) => [
    `== ${entry.label} ==`,
    `cwd: ${entry.cwd || ""}`,
    `command: ${entry.command.join(" ")}`,
    `exit: ${entry.status ?? "spawn-error"}${entry.error ? ` (${entry.error})` : ""}`,
    "-- stdout --",
    entry.stdout,
    "-- stderr --",
    entry.stderr,
    "",
  ].join("\n")).join("\n");
  await writeFile(join(logDir, "compile-native-islands-failure.log"), rendered);
}

// Classify by STAT, not by dirent type. Nub's default isolated linker leaves
// `node_modules/<pkg>` a symlink into `.store/`, and the `.node` files inside
// that store are themselves symlinks into the content-addressed cache — so a
// dirent-typed walk (where a symlink is neither isFile() nor isDirectory())
// finds zero native files and the whole fixture fails to see its own inputs.
// Real paths are deduped so a symlinked cycle, or the same store entry reached
// twice, cannot loop or double-count.
async function walk(root) {
  const found = [];
  const seenDirs = new Set();
  const seenFiles = new Set();
  async function visit(dir) {
    let here;
    try {
      here = await realpath(dir);
    } catch {
      return;
    }
    if (seenDirs.has(here)) return;
    seenDirs.add(here);
    for (const entry of await readdir(dir, { withFileTypes: true })) {
      const path = join(dir, entry.name);
      let info;
      try {
        info = await stat(path); // follows symlinks
      } catch {
        continue; // dangling link: not an input this fixture can hash
      }
      if (info.isDirectory()) {
        await visit(path);
      } else if (info.isFile()) {
        const real = await realpath(path).catch(() => path);
        if (seenFiles.has(real)) continue;
        seenFiles.add(real);
        found.push(path);
      }
    }
  }
  await visit(root);
  return found;
}

async function digest(path) {
  return createHash("sha256").update(await readFile(path)).digest("hex");
}

function isNative(path) {
  return [".node", ".dylib", ".dll", ".so"].includes(extname(path)) || basename(path).includes(".so.");
}

function platformLibrarySuffix(platform) {
  if (!platform) return process.platform === "darwin" ? ".dylib" : process.platform === "win32" ? ".dll" : ".so";
  if (platform.startsWith("darwin-")) return ".dylib";
  if (platform.startsWith("win32-")) return ".dll";
  return ".so";
}

async function records(root, source) {
  const files = (await walk(root)).filter(isNative);
  return Promise.all(files.map(async (path) => ({
    source,
    path: path.slice(root.length + 1).replaceAll("\\", "/"),
    sha256: await digest(path),
    bytes: (await stat(path)).size,
  })));
}

function selectExpected(records, platform) {
  const sharp = records.filter((item) =>
    (item.path.includes("sharp/") || item.path.includes("@img/sharp-")) && item.path.endsWith(".node"),
  );
  const suffix = platformLibrarySuffix(platform);
  // Windows ships libvips inside the platform addon package itself
  // (`@img/sharp-win32-x64/lib/libvips-42.dll`); every other platform splits it
  // into a separate `@img/sharp-libvips-*` package. sharp 0.35.3 publishes no
  // `@img/sharp-libvips-win32-*` optional dependency at all, so scoping the
  // Windows lookup to `@img/sharp-libvips-` matches nothing and fails here
  // before compile ever runs.
  const companionScope = suffix === ".dll" ? "@img/sharp-win32-" : "@img/sharp-libvips-";
  const companion = records.filter((item) =>
    item.path.includes(companionScope) && (item.path.endsWith(suffix) || item.path.includes(`${suffix}.`)),
  );
  if (!sharp.length) fail("expected sharp native prebuild (.node) is absent after install");
  if (!companion.length) fail(`expected sharp libvips companion ${suffix} is absent after install`);
  return [...sharp, ...companion];
}

async function rejectSourceBuildResidue(nodeModules) {
  const residue = (await walk(nodeModules)).map((path) => path.slice(nodeModules.length + 1).replaceAll("\\", "/"))
    // Matched at a package boundary, never anchored at the tree root: nub's
    // default isolated linker puts the real files under the `.nub` virtual
    // store, and `walk` does not descend the top-level symlinks, so every path
    // here begins `.nub/...`. A `^`-anchored pattern silently matches nothing.
    .filter((path) => /(?:^|\/)sharp\/(?:build\/(?:Makefile|binding\.sln|config\.gypi)|(?:build\/)?(?:obj|obj\.target)\/)/.test(path));
  if (residue.length) fail(`--mode prebuilt forbids node-gyp source-build residue: ${residue.join(", ")}`);
}

function runtimeEnvironment(workspace, cache) {
  const home = join(workspace, "home");
  const inherited = { ...process.env };
  // run.sh drives this file THROUGH the candidate nub, so `process.env` already
  // carries the host nub's injected NODE_OPTIONS — version-gated Node flags plus
  // a `--require` of the host's runtime tree. Handing that to the artifact is
  // both a false pass (the run under test is preloaded by an outside runtime, so
  // "self-contained" is never actually proven) and a false failure (targeting a
  // Node older than the host injects a flag that Node rejects outright: Node 24
  // exits 9 on the `--experimental-import-text` the host's Node 26 wants). Only
  // invisible on CI, where the runner's Node happens to equal `--target`.
  delete inherited.NODE_OPTIONS;
  delete inherited.NODE_REPL_EXTERNAL_MODULE;
  return {
    ...inherited,
    HOME: home,
    USERPROFILE: home,
    XDG_CACHE_HOME: join(home, "xdg-cache"),
    XDG_CONFIG_HOME: join(home, "xdg-config"),
    XDG_DATA_HOME: join(home, "xdg-data"),
    __NUB_COMPILE_CACHE_DIR: cache,
  };
}

function parseProof(output, phase) {
  const line = output.trim().split(/\r?\n/).find((candidate) => candidate.includes("COMPILE_NATIVE_ISLANDS_OK"));
  if (!line) fail(`${phase} artifact run did not print its proof marker\n${output}`);
  const proof = JSON.parse(line);
  if (proof.input?.join(",") !== "1,1" || proof.output?.slice(0, 2).join(",") !== "2,3") {
    fail(`${phase} artifact proof has unexpected values: ${line}`);
  }
  return proof;
}

async function main() {
  const options = args(process.argv.slice(2));
  if (!existsSync(options.nub)) usage(`candidate does not exist: ${options.nub}`);
  if (!existsSync(options.launcher)) usage(`launcher does not exist: ${options.launcher}`);
  const workspace = options.workspace ? resolve(options.workspace) : await mkdtemp(join(tmpdir(), "nub-compile-native-islands-"));
  const project = join(workspace, "project");
  const foreign = join(workspace, "foreign-cwd");
  const output = join(workspace, process.platform === "win32" ? "native-islands.exe" : "native-islands");
  const cache = join(workspace, "fresh-compile-runtime-cache");
  const proofPath = join(workspace, "native-islands-proof.json");
  const env = runtimeEnvironment(workspace, cache);

  try {
    await mkdir(project, { recursive: true });
    await cp(join(here, "package.json"), join(project, "package.json"));
    await cp(join(here, "app.cjs"), join(project, "app.cjs"));

    // Default npm-compatible install first: no package-manager-specific flags.
    const install = command("install", options.nub, ["install"], { cwd: project, env });
    if (/\b(?:node-gyp|gyp info)\b/i.test(`${install.stdout}\n${install.stderr}`)) {
      fail("--mode prebuilt forbids a node-gyp fallback; expected native prebuild was unavailable");
    }
    const lockNames = ["package-lock.json", "npm-shrinkwrap.json", "nub.lock", "lock.yaml", "aube-lock.yaml", "pnpm-lock.yaml"];
    const lockPath = lockNames.map((name) => join(project, name)).find(existsSync);
    if (!lockPath) fail("default install produced no pinned lock input");
    const lockInput = { path: basename(lockPath), sha256: await digest(lockPath), bytes: (await stat(lockPath)).size };
    await rejectSourceBuildResidue(join(project, "node_modules"));
    const installed = await records(join(project, "node_modules"), "installed");
    const expected = selectExpected(installed, options.platform);

    const compileArgs = ["compile", "--target", options.target, "--out", output];
    if (options.platform) compileArgs.push("--platform", options.platform);
    compileArgs.push(join(project, "app.cjs"));
    command("compile", options.nub, compileArgs, {
      cwd: project,
      env: { ...env, __NUB_LAUNCHER_TEMPLATE: options.launcher },
    });
    if (!existsSync(output)) fail(`compile reported success but artifact is absent: ${output}`);
    // On every platform, not just Windows: the launcher must depend on neither
    // its own basename nor its directory, so the artifact is relocated out of
    // the compile-output tree before it is ever executed. Left in place, a
    // launcher keying off a sibling of its build path would pass here.
    if (options.verifyCompanionDllRename && extname(output).toLowerCase() !== ".exe") {
      fail("--verify-companion-dll-rename requires a Windows .exe artifact");
    }
    const beforeSha256 = await digest(output);
    const relocated = join(workspace, "relocated");
    await mkdir(relocated, { recursive: true });
    const artifact = join(relocated, `native-islands-renamed${extname(output)}`);
    await rename(output, artifact);
    const afterSha256 = await digest(artifact);
    if (beforeSha256 !== afterSha256) fail("relocating the outer artifact changed its bytes");
    const artifactRename = { before: basename(output), after: basename(artifact), sha256Before: beforeSha256, sha256After: afterSha256 };

    // The executable must be self-contained. No source or install tree remains
    // available when either run begins, and both run from a foreign cwd.
    await rename(project, join(workspace, "source-hidden"));
    await rm(join(workspace, "source-hidden", "node_modules"), { recursive: true, force: true });
    await mkdir(foreign, { recursive: true });

    const cold = command("cold artifact run", artifact, [], { cwd: foreign, env });
    const coldProof = parseProof(cold.stdout, "cold");
    const extracted = await records(cache, "extracted-after-cold");
    const extractedByHash = new Map();
    for (const item of extracted) extractedByHash.set(item.sha256, [...(extractedByHash.get(item.sha256) || []), item.path]);
    const preservation = expected.map((item) => ({ ...item, extractedPaths: extractedByHash.get(item.sha256) || [] }));
    for (const item of preservation) {
      if (!item.extractedPaths.length) fail(`artifact did not extract byte-identical native file ${item.path} (${item.sha256})`);
    }
    if (options.verifyCompanionDllRename && !preservation.some((item) => item.path.endsWith(".dll"))) {
      fail("--verify-companion-dll-rename requested but no sharp companion DLL was preserved");
    }

    const warm = command("warm artifact run", artifact, [], { cwd: foreign, env });
    const warmProof = parseProof(warm.stdout, "warm");
    await writeFile(proofPath, `${JSON.stringify({
      artifact: { path: artifact, sha256: await digest(artifact), bytes: (await stat(artifact)).size },
      artifactRename,
      target: options.target,
      platform: options.platform || "host-default",
      lockInput,
      cold: coldProof,
      warm: warmProof,
      nativeFiles: preservation,
      companionDllRenameCheck: options.verifyCompanionDllRename ? {
        requested: true,
        companionDlls: preservation.filter((item) => item.path.endsWith(".dll")).map((item) => item.path),
      } : undefined,
      cacheRoot: cache,
    }, null, 2)}\n`);
    if (options.logDir) {
      await mkdir(options.logDir, { recursive: true });
      await cp(proofPath, join(options.logDir, basename(proofPath)));
    }
    console.log(`COMPILE_NATIVE_ISLANDS_PROOF=${proofPath}`);
    console.log("COMPILE_NATIVE_ISLANDS_HARNESS_OK");
  } finally {
    if (!options.keep && !options.workspace) await discardWorkspace(workspace);
    else console.log(`COMPILE_NATIVE_ISLANDS_WORKSPACE=${workspace}`);
  }
}

// Removing the scratch workspace is housekeeping, not part of the contract under
// test — and on Windows it can fail for a reason that has nothing to do with the
// result. `unlink` there refuses a file any process still holds open, so a binary
// the harness JUST executed reports EBUSY while the handle is released, and an
// antivirus scanner reading the fresh executable widens that window. This runs in
// a `finally` AFTER the harness has already printed HARNESS_OK, so letting the
// error escape turns a passing run red; win32-arm64 failed exactly that way on the
// leg's first outing.
//
// `force: true` covers ENOENT and nothing else, hence the retry: a short backoff
// clears the ordinary lock, and anything still held after it is left for the OS to
// reclaim with the temp directory.
async function discardWorkspace(dir) {
  for (let attempt = 0; attempt < 5; attempt++) {
    try {
      await rm(dir, { recursive: true, force: true });
      return;
    } catch (error) {
      if (attempt === 4) {
        console.log(`note: left ${dir} behind (${error.code ?? error.message})`);
        return;
      }
      await new Promise((resolve) => setTimeout(resolve, 200 * (attempt + 1)));
    }
  }
}

main().catch(async (error) => {
  try {
    await persistFailureLogs(defaults.logDir);
  } catch (logError) {
    console.error(`FAIL: could not persist COMPILE_NATIVE_LOG_DIR logs: ${logError.message}`);
  }
  console.error(`FAIL: ${error.stack || error.message}`);
  process.exitCode = 1;
});
