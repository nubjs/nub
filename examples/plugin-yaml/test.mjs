// node --test test.mjs
//
// Covers the three transforms that need no external compiler: the mapper's emit
// (text + span map + diagnostics), the runtime hook through a real `node --import`,
// and the Vite transform. The content-mapper protocol end to end needs
// `typescript@next` (7.1) and is exercised by hand against a fixture, not here.
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";
import { emit, PARSE_ERROR_CODE, toModule } from "./emit.mjs";
import vite from "./vite.mjs";

const here = fileURLToPath(new URL(".", import.meta.url));
const sample = "host: localhost\nport: 5432\ntags: [api, \"db\"]\n";
// Values JSON.stringify would not round-trip: non-finite numbers, negative zero,
// and a mapping key that an object literal would treat as the prototype.
const edges = "nan: .nan\ninf: .inf\nneg: -.inf\nzero: -0.0\n__proto__: { x: 1 }\n";
const evaluate = async (source) => (await import(`data:text/javascript,${encodeURIComponent(source)}`)).default;
const assertEdges = (value) => {
  assert.ok(Number.isNaN(value.nan));
  assert.equal(value.inf, Infinity);
  assert.equal(value.neg, -Infinity);
  assert.ok(Object.is(value.zero, -0));
  assert.ok(Object.hasOwn(value, "__proto__"), "__proto__ is an own property, not the prototype");
  assert.deepEqual(value.__proto__, { x: 1 });
};

test("emit produces an object literal whose evaluation equals the parsed document", async () => {
  const { text, diagnostics } = emit(sample);
  assert.equal(diagnostics.length, 0);
  assert.deepEqual(await evaluate(text), { host: "localhost", port: 5432, tags: ["api", "db"] });
});

test("the module keeps values JSON cannot: NaN, ±Infinity, -0, and an own __proto__ key", async () => {
  assertEdges(await evaluate(toModule(edges)));
  assertEdges(await evaluate(vite().transform(edges, "/p/edges.yaml").code));
  const dir = mkdtempSync(join(tmpdir(), "plugin-yaml-"));
  writeFileSync(join(dir, "edges.yaml"), edges);
  writeFileSync(
    join(dir, "app.mjs"),
    'import v from "./edges.yaml"; console.log(JSON.stringify([Number.isNaN(v.nan), v.inf === Infinity, v.neg === -Infinity, Object.is(v.zero, -0), Object.hasOwn(v, "__proto__") && v.__proto__.x]));',
  );
  const run = spawnSync(process.execPath, ["--import", join(here, "register.mjs"), "app.mjs"], { cwd: dir, encoding: "utf8" });
  assert.equal(run.status, 0, run.stderr);
  assert.equal(run.stdout.trim(), "[true,true,true,true,1]");
});

test("mapping keys are coerced the way parse() coerces them, collection keys included", async () => {
  const { parse } = await import("yaml");
  const keys = "? [a, b]\n: seq\n? { k: 1 }\n: map\n1: number\ntrue: boolean\n~: nothing\n";
  const expected = parse(keys, { logLevel: "silent" });
  assert.deepEqual(await evaluate(toModule(keys)), expected);
  assert.deepEqual(Object.keys(expected), ["1", "[ a, b ]", "{ k: 1 }", "true", "null"]);
});

test("toModule throws on a malformed document, as parse() does", () => {
  assert.throws(() => toModule("host: [unclosed\n"), SyntaxError);
});

test("the mapper process completes the content-mapper handshake and transforms a file", async () => {
  const { spawn } = await import("node:child_process");
  const child = spawn(process.execPath, [join(here, "mapper.mjs")], { stdio: ["pipe", "pipe", "inherit"] });
  let buffer = Buffer.alloc(0);
  const pending = new Map();
  child.stdout.on("data", (chunk) => {
    buffer = Buffer.concat([buffer, chunk]);
    for (;;) {
      const headerEnd = buffer.indexOf("\r\n\r\n");
      if (headerEnd === -1) return;
      const length = Number(/Content-Length:\s*(\d+)/i.exec(buffer.subarray(0, headerEnd).toString())[1]);
      if (buffer.length < headerEnd + 4 + length) return;
      const message = JSON.parse(buffer.subarray(headerEnd + 4, headerEnd + 4 + length).toString("utf8"));
      buffer = buffer.subarray(headerEnd + 4 + length);
      pending.get(message.id)(message);
      pending.delete(message.id);
    }
  });
  let nextId = 0;
  const request = (method, params) =>
    new Promise((resolve) => {
      const id = `t${++nextId}`;
      pending.set(id, resolve);
      const body = Buffer.from(JSON.stringify({ jsonrpc: "2.0", id, method, params }));
      child.stdin.write(`Content-Length: ${body.length}\r\n\r\n`);
      child.stdin.write(body);
    });
  try {
    const init = await request("initialize", { protocolVersion: 1, positionEncodings: ["utf-8", "utf-16"] });
    assert.deepEqual(init.result, { protocolVersion: 1, positionEncoding: "utf-16", diagnosticSource: "yaml" });
    const open = await request("openProject", { configFileName: "/p/tsconfig.json", projectHandle: "h0", compilerOptions: {} });
    assert.deepEqual(open.result, {});
    const transform = await request("transform", { fileName: "/p/config.yaml", content: sample, projectHandle: "h0" });
    assert.equal(transform.result.extension, ".ts");
    assert.equal(transform.result.text, emit(sample).text);
    assert.equal(transform.result.mappings.length, 7);
    assert.deepEqual(transform.result.diagnostics, []);
    const close = await request("closeProject", { projectHandle: "h0" });
    assert.equal(close.result, null);
    const unknown = await request("nope", {});
    assert.equal(unknown.error.code, -32601);
  } finally {
    child.stdin.end();
  }
});

test("every key and scalar carries a span back to its YAML token", () => {
  const { text, mappings } = emit(sample);
  const spans = mappings.map(([vs, vl, os, ol]) => [text.slice(vs, vs + vl), sample.slice(os, os + ol)]);
  assert.deepEqual(spans, [
    ['"host"', "host"],
    ['"localhost"', "localhost"],
    ['"port"', "port"],
    ["5432", "5432"],
    ['"tags"', "tags"],
    ['"api"', "api"],
    ['"db"', '"db"'],
  ]);
  // "5432" and "\"db\"" read the same in both texts: Verbatim (0); the rest are Atom (1).
  assert.deepEqual(mappings.map((m) => m[4]), [1, 1, 1, 0, 1, 1, 0]);
});

test("a parse error becomes a diagnostic inside the file, never past its end", () => {
  const broken = "host: [unclosed\n";
  const { diagnostics } = emit(broken);
  assert.equal(diagnostics.length, 1);
  const [d] = diagnostics;
  assert.equal(d.code, PARSE_ERROR_CODE);
  assert.ok(d.start + d.length <= broken.length, `range [${d.start}, ${d.start + d.length}) exceeds ${broken.length}`);
});

test("node --import <pkg>/register makes a YAML import evaluate to the parsed document", () => {
  const dir = mkdtempSync(join(tmpdir(), "plugin-yaml-"));
  writeFileSync(join(dir, "config.yaml"), sample);
  writeFileSync(join(dir, "app.mjs"), 'import c from "./config.yaml"; console.log(JSON.stringify(c));');
  const run = spawnSync(process.execPath, ["--import", join(here, "register.mjs"), "app.mjs"], { cwd: dir, encoding: "utf8" });
  assert.equal(run.status, 0, run.stderr);
  assert.equal(run.stdout.trim(), '{"host":"localhost","port":5432,"tags":["api","db"]}');
});

test("the Vite transform emits the same module and ignores other files", async () => {
  const plugin = vite();
  assert.equal(plugin.transform("x", "/p/app.ts"), null);
  const out = plugin.transform(sample, "/p/config.yaml?import");
  assert.equal(out.code, toModule(sample));
  assert.deepEqual(await evaluate(out.code), { host: "localhost", port: 5432, tags: ["api", "db"] });
});
