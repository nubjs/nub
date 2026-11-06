// Tests for scripts/merge-cascade.ts — the mergeDecision gate that drives the
// queue-drain auto-merge. Focus: the #327 ghost carve-out (a nameless/non-
// required non-terminal check must not park the merger forever) AND the
// never-mis-merge invariants (a real required pending/failed check always holds/
// blocks; only MERGEABLE proceeds). Pure decision logic — no gh, no real merge.
// Run: node --test scripts/merge-cascade.test.mts
import { test } from "node:test";
import assert from "node:assert/strict";
import { mergeDecision, REQUIRED_GATE } from "./merge-cascade.ts";

const check = (name: string, conclusion: string) => ({ name, status: "COMPLETED", conclusion });
const running = (name: string) => ({ name, status: "IN_PROGRESS" });
const ghost = () => ({ status: "IN_PROGRESS" }); // nameless, never-terminating (the #327 shape)
const gateGreen = () => check(REQUIRED_GATE, "SUCCESS");

const decide = (over = {}) => mergeDecision({ state: "OPEN", mergeable: "MERGEABLE", rollup: [], ...over });

void test("#327 shape: required gate green + named checks green + 1 nameless ghost → MERGE (not park-forever)", () => {
  const rollup = [gateGreen(), check("build", "SUCCESS"), check("test", "SUCCESS"), ghost()];
  const d = decide({ rollup });
  assert.equal(d.verdict, "merge", "a nameless ghost must not block a mergeable PR whose required gate is green");
});

void test("#327 at scale: 51 green named checks + gate green + 1 ghost → MERGE", () => {
  const rollup = [gateGreen(), ...Array.from({ length: 51 }, (_, i) => check(`check ${i}`, "SUCCESS")), ghost()];
  assert.equal(decide({ rollup }).verdict, "merge");
});

void test("never mis-merge: required gate still RUNNING (with a nameless ghost) → WAIT, never merge", () => {
  const rollup = [running(REQUIRED_GATE), check("build", "SUCCESS"), ghost()];
  const d = decide({ rollup });
  assert.equal(d.verdict, "wait", "a real in-flight required gate must hold");
  assert.match(d.reason, /CI gate/);
});

void test("never mis-merge: required gate FAILED → BLOCK", () => {
  const d = decide({ rollup: [check(REQUIRED_GATE, "FAILURE"), check("build", "SUCCESS")] });
  assert.equal(d.verdict, "block");
  assert.match(d.reason, /CI gate/);
});

void test("never mis-merge: required gate ABSENT (partial rollup, only fast jobs green) → WAIT", () => {
  const d = decide({ rollup: [check("fast", "SUCCESS")] });
  assert.equal(d.verdict, "wait", "the aggregator gate not yet registered is not done");
  assert.match(d.reason, /CI gate/);
});

void test("never mis-merge: duplicate required name (one green, one still running) → WAIT (every occurrence must be green)", () => {
  const rollup = [gateGreen(), running(REQUIRED_GATE)];
  assert.equal(decide({ rollup }).verdict, "wait");
});

void test("branch-protection faithful: a FAILING non-required check does NOT block (gate green + mergeable → MERGE)", () => {
  const rollup = [gateGreen(), check("optional lint", "FAILURE"), ghost()];
  assert.equal(decide({ rollup }).verdict, "merge", "an optional red check must not refuse to merge a mergeable PR");
});

void test("mergeability is gated POSITIVELY: gate green but mergeable UNKNOWN → WAIT (not merge)", () => {
  const rollup = [gateGreen(), ghost()];
  assert.equal(decide({ rollup, mergeable: "UNKNOWN" }).verdict, "wait");
  assert.equal(decide({ rollup, mergeable: "" }).verdict, "wait");
});

void test("mergeability CONFLICTING → BLOCK even with the gate green", () => {
  const d = decide({ rollup: [gateGreen()], mergeable: "CONFLICTING" });
  assert.equal(d.verdict, "block");
  assert.match(d.reason, /CONFLICTING/);
});

void test("a non-OPEN PR → BLOCK regardless of checks", () => {
  assert.equal(decide({ state: "MERGED", rollup: [gateGreen()] }).verdict, "block");
});

void test("empty rollup (no checks yet) → WAIT (wait-for-existence preserved)", () => {
  const d = decide({ rollup: [] });
  assert.equal(d.verdict, "wait");
  assert.match(d.reason, /no checks/);
});

void test("clean happy path: gate + all named green, no ghost, mergeable → MERGE", () => {
  const rollup = [gateGreen(), check("build", "SUCCESS"), check("test", "NEUTRAL")];
  assert.equal(decide({ rollup }).verdict, "merge");
});
