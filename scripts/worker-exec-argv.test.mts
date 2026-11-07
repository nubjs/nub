// The three Worker constructors that turn a parent's flags into an explicit
// `execArgv` drop the flags Node refuses there by matching Node's report against
// the list. Each file carries its own copy of the parser, so each is driven here
// with the refusals the running Node actually produces.
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import test, { describe } from "node:test";
import { Worker } from "node:worker_threads";

type Parse = (
  execArgv: string[],
  error: unknown,
) => { kept: string[]; refused: string[] } | null;

const runtime = new URL("../runtime/", import.meta.url);

async function parserIn(file: string): Promise<Parse> {
  const source = readFileSync(new URL(file, runtime), "utf8");
  const start = source.indexOf("function withoutRefusedExecArgv(");
  assert.notEqual(start, -1, `${file} carries no withoutRefusedExecArgv`);
  let depth = 0;
  for (let i = source.indexOf("{", start); i < source.length; i++) {
    if (source[i] === "{") depth++;
    else if (source[i] === "}" && --depth === 0) {
      const module = `export default ${source.slice(start, i + 1)}`;
      const loaded = (await import(`data:text/javascript,${encodeURIComponent(module)}`)) as {
        default: Parse;
      };
      return loaded.default;
    }
  }
  throw new Error(`${file}: unbalanced withoutRefusedExecArgv`);
}

function refusal(execArgv: string[]): unknown {
  try {
    void new Worker("0", { eval: true, execArgv }).terminate();
  } catch (error) {
    return error;
  }
  throw new Error(`Node accepted ${JSON.stringify(execArgv)}`);
}

// Drop what Node refuses until it accepts, as each constructor does.
function accepted(parse: Parse, execArgv: string[]): string[] {
  for (;;) {
    let error: unknown;
    try {
      void new Worker("0", { eval: true, execArgv }).terminate();
      return execArgv;
    } catch (caught) {
      error = caught;
    }
    const retry = parse(execArgv, error);
    assert.ok(retry, `no retry for ${JSON.stringify(execArgv)}: ${(error as Error).message}`);
    execArgv = retry.kept;
  }
}

function conditionsInWorker(execArgv: string[]): Promise<string[]> {
  const worker = new Worker(
    "require('node:worker_threads').parentPort.postMessage(process.execArgv)",
    { eval: true, execArgv },
  );
  return new Promise<string[]>((resolve, reject) => {
    worker.once("message", resolve);
    worker.once("error", reject);
  }).finally(() => worker.terminate());
}

// The global Worker merges the caller's list onto the parent's, so only the flags
// Node names may count against the caller, never a dropped value of the parent's.
void describe("the global Worker under a parent started with --title demo", () => {
  const polyfill = new URL("worker-polyfill.mjs", runtime).href;
  const start = (execArgv: string[]) => {
    const code =
      `const { installWorkerPolyfill } = await import(${JSON.stringify(polyfill)});\n` +
      `installWorkerPolyfill();\n` +
      `let out = "started";\n` +
      `try { new Worker("data:text/javascript,0", { execArgv: ${JSON.stringify(execArgv)} }).terminate(); }\n` +
      `catch (error) { out = error.code; }\n` +
      `console.log(out);\nprocess.exit(0);\n`;
    const child = spawnSync(
      process.execPath,
      ["--title", "demo", "--input-type=module", "-e", code],
      { stdio: ["ignore", "pipe", "pipe"], encoding: "utf8", timeout: 30_000 },
    );
    return `${child.stdout.trim()}${child.stderr ? ` (stderr: ${child.stderr.trim()})` : ""}`;
  };

  void test("starts when a caller's value equals the parent's dropped value", () => {
    assert.equal(start(["--conditions", "demo"]), "started");
  });

  void test("still refuses a flag the caller passed", () => {
    assert.equal(start(["--title=explicit"]), "ERR_WORKER_INVALID_EXEC_ARGV");
  });
});

for (const file of ["compile-bootstrap.cjs", "compile-sea-loader.cjs", "worker-polyfill.mjs"]) {
  const parse = await parserIn(file);
  void describe(file, () => {

    void test("drops a refused --flag=value and keeps the rest in order", () => {
      const list = ["--enable-source-maps", "--max-old-space-size=200", "--conditions=x"];
      assert.deepEqual(parse(list, refusal(list)), {
        kept: ["--enable-source-maps", "--conditions=x"],
        refused: ["--max-old-space-size=200"],
      });
    });

    void test("drops a refused flag's separate value, so later flags still reach the worker", async () => {
      const list = ["--title", "demo", "--conditions=x"];
      assert.deepEqual(parse(list, refusal(list))?.kept, ["--conditions=x"]);
      assert.ok((await conditionsInWorker(accepted(parse, list))).includes("--conditions=x"));
    });

    void test("matches a refused value that itself contains a comma and space", () => {
      const list = ["--title=one, two", "--conditions=x"];
      assert.deepEqual(parse(list, refusal(list))?.kept, ["--conditions=x"]);
    });

    void test("reaches flags Node only reports once an earlier stray value is gone", () => {
      const list = ["--stack-size=900", "--title", "a", "--expose-gc", "--conditions=x"];
      assert.deepEqual(accepted(parse, list), ["--conditions=x"]);
    });

    void test("returns null for a report that does not line up with the list", () => {
      const error = Object.assign(
        new Error("Initiated Worker with invalid execArgv flags: --not-in-the-list"),
        { code: "ERR_WORKER_INVALID_EXEC_ARGV" },
      );
      assert.equal(parse(["--max-old-space-size=200"], error), null);
      assert.equal(parse(["--max-old-space-size=200"], new Error("other")), null);
    });
  });
}
