// A child that opts out with `--no-js-defer-import-eval` on its own argv must not be
// re-armed by the signal it inherits from this parent, which runs with the flag armed
// for its preload. Two launch paths, because two different guards cover them: `node`
// through PATH re-enters nub, whose launch decision replaces the inherited signal;
// `process.execPath` makes no nub launch decision and inherits the env verbatim, so
// the preload itself has to honor the polarity on its own execArgv.
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const child = fileURLToPath(new URL("./nested-child.mjs", import.meta.url));
for (const [label, bin] of [["path", "node"], ["abs", process.execPath]]) {
  const r = spawnSync(bin, ["--no-js-defer-import-eval", child], { encoding: "utf8" });
  for (const line of r.stdout.split("\n").filter(Boolean)) console.log(`${label}:${line}`);
  console.log(`${label}:nested:status=${r.status}`);
}
