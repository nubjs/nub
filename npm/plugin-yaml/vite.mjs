// Vite / Rollup plugin: the same YAML → `export default <value>` transform the
// Node loader hook performs, for the bundled build. `import yaml from
// "@nubjs/plugin-yaml/vite"` then `plugins: [yaml()]`.
import { parse } from "yaml";

const YAML = /\.ya?ml(?:\?.*)?$/;

export default function yaml() {
  return {
    name: "nub-plugin-yaml",
    transform(code, id) {
      if (!YAML.test(id)) return null;
      const value = parse(code);
      return {
        code: value === undefined ? "export default undefined;" : `export default ${JSON.stringify(value)};`,
        map: { mappings: "" },
      };
    },
  };
}
