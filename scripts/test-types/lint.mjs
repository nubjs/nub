import { readdirSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const tooling = dirname(fileURLToPath(import.meta.url));
const scripts = dirname(tooling);
const files = readdirSync(scripts)
  .filter(name => name.endsWith(".test.mts"))
  .sort()
  .map(name => join(scripts, name));
if (files.length === 0) throw new Error("No typed script tests found");

const result = spawnSync(process.execPath, [
  join(tooling, "node_modules/oxlint/bin/oxlint"),
  "--type-aware", "--type-check", "--deny-warnings", ...files,
], {
  cwd: scripts,
  stdio: "inherit",
  env: {
    ...process.env,
    TSRS_FAST: "1",
    OXLINT_TSGOLINT_PATH: join(tooling, "node_modules/.bin", process.platform === "win32" ? "tsrs-lint.cmd" : "tsrs-lint"),
  },
});
if (result.error) throw result.error;
process.exitCode = result.status ?? 1;
