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
import { emit, PARSE_ERROR_CODE } from "./emit.mjs";
import vite from "./vite.mjs";

const here = fileURLToPath(new URL(".", import.meta.url));
const sample = "host: localhost\nport: 5432\ntags: [api, \"db\"]\n";

test("emit produces an object literal whose evaluation equals the parsed document", async () => {
  const { text, diagnostics } = emit(sample);
  assert.equal(diagnostics.length, 0);
  const value = (await import(`data:text/javascript,${encodeURIComponent(text)}`)).default;
  assert.deepEqual(value, { host: "localhost", port: 5432, tags: ["api", "db"] });
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

test("the Vite transform emits the same module and ignores other files", () => {
  const plugin = vite();
  assert.equal(plugin.transform("x", "/p/app.ts"), null);
  const out = plugin.transform(sample, "/p/config.yaml?import");
  assert.equal(out.code, 'export default {"host":"localhost","port":5432,"tags":["api","db"]};');
});
