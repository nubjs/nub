// Node loader hook: `import config from "./config.yaml"` evaluates to the parsed
// document. Activated by `--import @nubjs/plugin-yaml/register` on plain Node, or
// by listing the package in nub.jsonc. Uses the synchronous hook API (Node >= 22.15),
// so the import resolves in-thread with no loader worker.
import { readFileSync } from "node:fs";
import { registerHooks } from "node:module";
import { fileURLToPath } from "node:url";
import { parse } from "yaml";

const YAML = /\.ya?ml$/;

registerHooks({
  load(url, context, nextLoad) {
    if (!url.startsWith("file:") || !YAML.test(new URL(url).pathname)) return nextLoad(url, context);
    const value = parse(readFileSync(fileURLToPath(url), "utf8"));
    const source = value === undefined ? "export default undefined;" : `export default ${JSON.stringify(value)};`;
    return { format: "module", source, shortCircuit: true };
  },
});
