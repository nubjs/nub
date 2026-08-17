// The contract at the process boundary, which the launcher sits astride: a
// compiled binary runs the user's Node as a CHILD, so argv, the exit code and
// the two output streams all have to survive a hop that plain Node never makes.
//
// Every other fixture here checks that a package RESOLVES. This one checks that
// the program behaves like a program. It is one fixture rather than four because
// the harness compares a single last line, and because these fail together — a
// launcher that loses the exit code usually lost the streams too.
//
// The assertions run in a CHILD of this fixture, spawned from `process.execPath`,
// which is the artifact itself once compiled. That is the point: it exercises the
// artifact's own re-entry, and a binary that cannot start a copy of itself is one
// no CLI could use to re-exec.
import { spawnSync } from "node:child_process";

// `execPath` is `node` when this runs as a script and the artifact when it runs
// compiled, so the script path is an argument in the first case and absent in the
// second — the binary already carries its entry.
const selfPrefix = /node(\.exe)?$/.test(process.execPath) ? [process.argv[1]] : [];
const runSelf = (...args) =>
  spawnSync(process.execPath, [...selfPrefix, ...args], { encoding: "utf8" });

if (process.argv.includes("--probe")) {
  // A space inside an argument, because quoting is where argv passing breaks.
  const tail = process.argv[process.argv.indexOf("--probe") + 1];
  process.stderr.write("E:" + tail + "\n");
  process.stdout.write("O:" + tail + "\n");
  process.exit(7);
}

const r = runSelf("--probe", "a b");
const checks = [
  ["exit", r.status === 7],
  // Separate streams: a launcher that merges them, or writes its own noise to
  // either, shows up here rather than as a mysterious diff in another fixture.
  ["stdout", r.stdout.trim() === "O:a b"],
  ["stderr", r.stderr.trim() === "E:a b"],
];
const failed = checks.filter(([, ok]) => !ok).map(([name]) => name);
console.log("ok:" + (failed.length ? failed.join(",") : "process"));
