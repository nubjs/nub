import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import { validateSelection, implementedSuites, parseRunnerOptions, selectShard, inspectHarnessResult } from "../tests/test262/selection.mjs";

interface SelectionConfig {
  revision: string;
  harnessVersion: string;
  expectedFiles: number;
  expectedScenarios: number;
  suites: string[];
}
const config: SelectionConfig = JSON.parse(readFileSync(new URL("../tests/test262/config.json", import.meta.url), "utf8"));
void test("Test262 selection stays pinned and limited to implemented polyfills", () => {
  assert.doesNotThrow(() => validateSelection(config));
  assert.equal(config.expectedFiles, 355);
  assert.equal(config.expectedScenarios, 710);
  assert.deepEqual(config.suites, implementedSuites);
});
void test("Test262 refuses full suites, wildcards, unrelated APIs and path escapes", () => {
  for (const suite of ["test", "test/built-ins", "test/built-ins/Iterator", "test/**/*", "**/*.js", "../test", "/tmp/test", "test/built-ins/Math/abs", "test/built-ins/Iterator/zip/..", "test\\built-ins\\Math\\sumPrecise"]) {
    assert.throws(() => validateSelection({ ...config, suites: [suite] }), /broad Test262 suite/);
  }
  assert.throws(() => validateSelection({ ...config, suites: [] }), /Empty/);
  assert.throws(() => validateSelection({ ...config, suites: [config.suites[0], config.suites[0]] }), /Duplicate/);
});
void test("Test262 refuses floating pins and oversized or invalid execution counts", () => {
  for (const revision of ["main", "HEAD", "abcdef", ""]) assert.throws(() => validateSelection({ ...config, revision }), /commit pin/);
  for (const harnessVersion of ["latest", "^10.0.0", "10", "10.0.0-beta.1"]) assert.throws(() => validateSelection({ ...config, harnessVersion }), /release pin/);
  for (const name of ["expectedFiles", "expectedScenarios"]) {
    for (const value of [0, -1, 2001, 1.5, Infinity, "504"]) assert.throws(() => validateSelection({ ...config, [name]: value }), /budget/);
  }
});

void test("Test262 rejects a matching success summary when the harness exits unsuccessfully", () => {
  const output = "Ran 710 tests\n710 passed\n0 failed\n";
  for (const result of [
    { status: 7, signal: null, stdout: output },
    { status: null, signal: "SIGTERM", stdout: output },
  ]) {
    const report = inspectHarnessResult(result, 710, ["test.js"], [], 24, false);
    assert.ok(report.errors.some((error: string) => error.includes("exited unsuccessfully")));
  }
  assert.deepEqual(inspectHarnessResult(
    { status: 0, signal: null, stdout: output }, 710, ["test.js"], [], 24, false,
  ).errors, []);
});


void test("Test262 worker defaults respect available CPUs and cap local parallelism", () => {
  assert.equal(parseRunnerOptions([], 1).workers, 1);
  assert.equal(parseRunnerOptions([], 2).workers, 2);
  assert.equal(parseRunnerOptions([], 64).workers, 4);
  assert.equal(parseRunnerOptions(["--workers", "8"], 2).workers, 8);
  for (const value of ["0", "-1", "1.5", "33", "Infinity", "1e2", "01"]) {
    assert.throws(() => parseRunnerOptions(["--workers", value], 4), /workers/);
  }
});

void test("Test262 CLI rejects ambiguous or malformed options before execution", () => {
  assert.deepEqual(parseRunnerOptions(["--shard", "2/4", "--list", "--native"], 2), {
    workers: 2, shard: { index: 2, count: 4 }, native: true, list: true,
  });
  for (const value of ["0/4", "1/0", "5/4", "1.5/4", "1", "1/4/5", "1/9007199254740992"]) {
    assert.throws(() => parseRunnerOptions(["--shard", value], 4), /shard/);
  }
  for (const args of [["--workers"], ["--shard", "--list"], ["--all"], ["--native", "--native"], ["--workers", "2", "--workers", "3"]]) {
    assert.throws(() => parseRunnerOptions(args, 4));
  }
});

void test("Test262 shards form deterministic balanced disjoint coverage", () => {
  const files = Array.from({ length: config.expectedFiles }, (_, i) => `test/fixture-${String(i).padStart(3, "0")}.js`);
  for (const count of [1, 2, 3, 4, 7, files.length]) {
    const shards = Array.from({ length: count }, (_, i) => selectShard([...files].reverse(), { index: i + 1, count }, config.expectedScenarios));
    const combined = shards.flatMap((shard) => shard.files);
    assert.equal(new Set(combined).size, files.length);
    assert.deepEqual([...combined].sort((a, b) => a < b ? -1 : a > b ? 1 : 0), files);
    assert.equal(shards.reduce((sum, shard) => sum + shard.expectedScenarios, 0), config.expectedScenarios);
    assert.ok(Math.max(...shards.map((shard) => shard.files.length)) - Math.min(...shards.map((shard) => shard.files.length)) <= 1);
    assert.deepEqual(shards[0], selectShard(files, { index: 1, count }, config.expectedScenarios));
  }
});

void test("Test262 refuses empty, duplicated, oversized or inconsistent shards", () => {
  for (const shard of [{ index: 0, count: 1 }, { index: 2, count: 1 }, { index: 1, count: 3 }, { index: 1.5, count: 2 }]) {
    assert.throws(() => selectShard(["a", "b"], shard, 4), /partition/);
  }
  assert.throws(() => selectShard([], { index: 1, count: 1 }, 0), /nonempty/);
  assert.throws(() => selectShard(["a", "a"], { index: 1, count: 1 }, 4), /unique/);
  assert.throws(() => selectShard(["a"], { index: 1, count: 1 }, 1), /two per/);
});
