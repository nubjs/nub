// Records what TypeScript's CommonJS emit observes for every case in cases.mjs, with
// `esModuleInterop` on and off, into corpus.json. The Rust test
// crates/nub-cli/tests/ts_commonjs_corpus.rs runs the same cases through Nub and
// compares, so CI needs no TypeScript.
//
//   node tests/ts-commonjs/record.mjs --typescript <dir of the typescript 5.9.3 package>
//
// Record with a Node that has `require(esm)` (20.19+, 22.12+), so tsc's own output
// runs for the cases that import an ES module.

import { execFileSync } from "node:child_process";
import { copyFileSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { dirname, join, relative } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";
import { cases, show } from "./cases.mjs";

const VERSION = "5.9.3";
const { values } = parseArgs({ options: { typescript: { type: "string" } } });
if (!values.typescript) throw new Error("pass --typescript <dir of the typescript package>");
const ts = createRequire(join(values.typescript, "package.json"))("./lib/typescript.js");
if (ts.version !== VERSION) throw new Error(`need TypeScript ${VERSION}, got ${ts.version}`);

function walk(dir) {
  return readdirSync(dir, { withFileTypes: true }).flatMap((entry) =>
    entry.isDirectory() ? walk(join(dir, entry.name)) : [join(dir, entry.name)],
  );
}

function rename(text, renames) {
  for (const [from, to] of Object.entries(renames ?? {})) {
    text = text.replace(new RegExp(`\\b${from}\\b`, "g"), to);
  }
  return text;
}

function observe(testCase, esModuleInterop) {
  const root = mkdtempSync(join(tmpdir(), "ts-commonjs-"));
  try {
    const src = join(root, "src");
    const out = join(root, "out");
    for (const [name, text] of Object.entries({ ...testCase.files, "show.cjs": show })) {
      mkdirSync(dirname(join(src, name)), { recursive: true });
      writeFileSync(join(src, name), rename(text, testCase.tscRename));
    }
    const files = walk(src);
    const program = ts.createProgram(
      files.filter((f) => f.endsWith(".ts")),
      {
        module: ts.ModuleKind.CommonJS,
        target: ts.ScriptTarget.ES2022,
        esModuleInterop,
        rootDir: src,
        outDir: out,
        types: [],
      },
    );
    const emitted = program.emit();
    if (emitted.emitSkipped) throw new Error(`${testCase.name}: tsc emitted nothing`);
    for (const file of files.filter((f) => !f.endsWith(".ts"))) {
      const dest = join(out, relative(src, file));
      mkdirSync(dirname(dest), { recursive: true });
      copyFileSync(file, dest);
    }
    try {
      const stdout = execFileSync(process.execPath, ["main.js"], {
        cwd: out,
        encoding: "utf8",
        stdio: ["ignore", "pipe", "pipe"],
      });
      return stdout
        .split("\n")
        .map((line) => {
          const label = line.slice(0, line.indexOf(" = "));
          const esm = testCase.esm?.[label];
          if (esm === undefined) return line;
          if (esm === line) throw new Error(`${testCase.name}: tsc already observes ${line}`);
          return esm;
        })
        .join("\n");
    } catch (error) {
      if (!error.stderr) throw error;
      throw new Error(`${testCase.name} (esModuleInterop ${esModuleInterop}) failed:\n${error.stderr}`);
    }
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
}

const corpus = {
  typescript: VERSION,
  node: process.version,
  shared: { "show.cjs": show },
  cases: cases.map((testCase) => ({
    name: testCase.name,
    files: testCase.files,
    expected: {
      esModuleInterop: observe(testCase, true),
      noEsModuleInterop: observe(testCase, false),
    },
  })),
};
const here = dirname(fileURLToPath(import.meta.url));
writeFileSync(join(here, "corpus.json"), `${JSON.stringify(corpus, null, 2)}\n`);
console.log(`recorded ${corpus.cases.length} cases with TypeScript ${VERSION} on Node ${process.version}`);
