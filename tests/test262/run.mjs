#!/usr/bin/env node
import { validateSelection, parseRunnerOptions, selectShard, inspectHarnessResult } from "./selection.mjs";
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, renameSync, rmSync } from "node:fs";
import { availableParallelism, tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../..");
const config = JSON.parse(readFileSync(join(here, "config.json"), "utf8"));
validateSelection(config);
const options = parseRunnerOptions(process.argv.slice(2), availableParallelism());
if (options.shard.count > config.expectedFiles) throw new Error("--shard count exceeds the selected file count");
const { native, workers, harness } = options;
const checkout = resolve(options.checkout ?? join(tmpdir(), `nub-test262-${config.revision}`));
const node = resolve(options.node ?? process.execPath);
function run(command, argv, options = {}) {
  const result = spawnSync(command, argv, { encoding: "utf8", maxBuffer: 16 * 1024 * 1024, ...options });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`${command} exited ${result.status}:\n${result.stdout || ""}${result.stderr || ""}`);
  return result.stdout;
}
if (!existsSync(checkout)) {
  // Publish only a complete checkout. Concurrent local shards can safely race
  // to initialize the cache; each validates the winning checkout below.
  mkdirSync(dirname(checkout), { recursive: true });
  const staging = mkdtempSync(join(dirname(checkout), ".nub-test262-"));
  try {
    run("git", ["init", "--quiet", staging]);
    run("git", ["-C", staging, "remote", "add", "origin", "https://github.com/tc39/test262.git"]);
    run("git", ["-C", staging, "sparse-checkout", "set", "--no-cone", "/package.json", "/harness/", ...config.suites.map((suite) => `/${suite}`)]);
    run("git", ["-C", staging, "fetch", "--filter=blob:none", "--depth=1", "origin", config.revision]);
    run("git", ["-C", staging, "checkout", "--detach", "FETCH_HEAD"]);
    try { renameSync(staging, checkout); }
    catch (error) {
      if (!["EEXIST", "ENOTEMPTY"].includes(error.code) || !existsSync(checkout)) throw error;
    }
  } finally { rmSync(staging, { recursive: true, force: true }); }
}
const revision = run("git", ["-C", checkout, "rev-parse", "HEAD"]).trim();
if (revision !== config.revision) throw new Error(`Test262 revision ${revision} differs from pinned ${config.revision}`);
if (run("git", ["-C", checkout, "status", "--porcelain", "--untracked-files=no"]).trim()) {
  throw new Error("Test262 checkout has modified tracked files");
}
// One tracked-file query avoids a subprocess per suite. Git paths use '/', so
// the deterministic partition is identical on Windows and POSIX hosts.
const files = run("git", ["-C", checkout, "ls-files", "-z", "--", ...config.suites]).split("\0")
  .filter((file) => file.endsWith(".js") && !file.endsWith("_FIXTURE.js"));
for (const suite of config.suites) {
  if (!files.some((file) => file === suite || file.startsWith(`${suite}/`))) throw new Error(`Empty Test262 suite: ${suite}`);
}
if (files.length !== config.expectedFiles) throw new Error(`Expected ${config.expectedFiles} files, found ${files.length}`);
// `--native` skips what no Node ships yet, so a missing method is not a failure.
const runnable = native
  ? files.filter((file) => !config.polyfillOnly.some((suite) => file.startsWith(`${suite}/`)))
  : files;
const selected = selectShard(runnable, options.shard, native ? runnable.length * 2 : config.expectedScenarios);
if (options.list) {
  console.log(JSON.stringify({
    revision, native, workers, shard: options.shard,
    totalFiles: files.length, totalScenarios: config.expectedScenarios,
    expectedScenarios: selected.expectedScenarios,
    files: selected.files,
  }, null, 2));
  process.exit(0);
}
const harnessArgs = [
  "--host-type", "node", "--host-path", node, "--test262-dir", checkout,
  "--threads", String(workers), "--timeout", "10000",
  ...(!native ? ["--prelude", join(here, "polyfill-prelude.cjs")] : []), ...selected.files.map((file) => join(checkout, file)),
];
console.log(`Test262 ${revision}: shard ${options.shard.index}/${options.shard.count}; ${selected.files.length}/${files.length} files; ${selected.expectedScenarios} scenarios; ${workers} workers; ${native ? "native" : "forced polyfills"}; ${run(node, ["--version"]).trim()}`);
const command = harness ? process.execPath : process.platform === "win32" ? "npx.cmd" : "npx";
const commandArgs = harness ? [resolve(harness), ...harnessArgs] : ["--yes", `--package=test262-harness@${config.harnessVersion}`, "test262-harness", ...harnessArgs];
const result = spawnSync(command, commandArgs, {
  cwd: root, encoding: "utf8", maxBuffer: 16 * 1024 * 1024,
  env: { ...process.env, NUB_TEST262_POLYFILLS: join(root, "runtime/polyfills.cjs") },
});
if (result.error) throw result.error;
process.stdout.write(result.stdout || "");
process.stderr.write(result.stderr || "");
const major = Number(/^v(\d+)/.exec(run(node, ["--version"]))[1]);
const report = inspectHarnessResult(result, selected.expectedScenarios, selected.files, config.knownFailures, major, native);
// Fail closed: a runner failure or zero/truncated test selection is never green.
if (report.errors.length) {
  for (const error of report.errors) console.error(error);
  process.exit(1);
}
