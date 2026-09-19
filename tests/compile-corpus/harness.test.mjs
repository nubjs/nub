import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { chmodSync, copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

const here = dirname(fileURLToPath(import.meta.url));

for (const scenario of ["matching", "control-failure", "wrong-prefix", "artifact-failure", "warm-failure"]) {
  test(`corpus evidence: ${scenario}`, () => {
    const root = mkdtempSync(join(tmpdir(), "compile-corpus-harness-"));
    try {
      mkdirSync(join(root, "fixtures"));
      mkdirSync(join(root, "work", "node_modules"), { recursive: true });
      copyFileSync(join(here, "run.sh"), join(root, "run.sh"));
      writeFileSync(join(root, "fixtures", "a-test.mjs"), `console.log("prefix"); console.log("ok:test"); process.exit(${scenario === "control-failure" ? 7 : 0});\n`);
      const compiler = join(root, "compiler");
      writeFileSync(compiler, `#!/usr/bin/env bash
set -eu
while [ "$1" != --out ]; do shift; done
out="$2"
cat > "$out" <<'APP'
#!/usr/bin/env bash
set -eu
if [ "$CORPUS_SCENARIO" = matching ]; then
  test ! -f "$CORPUS_WORK/a-test.mjs"
  test ! -d "$CORPUS_WORK/node_modules"
fi
if [ "$CORPUS_SCENARIO" = wrong-prefix ]; then echo wrong; else echo prefix; fi
echo ok:test
if [ "$CORPUS_SCENARIO" = artifact-failure ]; then exit 7; fi
if [ "$CORPUS_SCENARIO" = warm-failure ] && [ -e "$CORPUS_WORK/ran" ]; then exit 9; fi
touch "$CORPUS_WORK/ran"
APP
chmod +x "$out"
`);
      chmodSync(compiler, 0o755);
      const result = spawnSync("bash", [join(root, "run.sh"), join(root, "work")], {
        encoding: "utf8", timeout: 20_000,
        env: { ...process.env, NODE_OPTIONS: "", NODE_PATH: "", NODE_PIN: process.versions.node, NUB: compiler,
          CORPUS_WORK: join(root, "work"), CORPUS_SCENARIO: scenario },
      });
      assert.ifError(result.error);
      assert.equal(result.status, scenario === "matching" ? 0 : 1, `${result.stdout}\n${result.stderr}`);
      assert.ok(existsSync(join(root, "work", "a-test.mjs")), "source restored on failure");
      assert.ok(existsSync(join(root, "work", "node_modules")), "dependencies restored on failure");
      if (scenario === "control-failure") assert.ok(!existsSync(join(root, "work", "bin-a-test")), "failed controls must not reach compilation");
      if (scenario === "matching") assert.equal(readFileSync(join(root, "work", "warm-a-test.stdout"), "utf8"), "prefix\nok:test\n");
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });
}
