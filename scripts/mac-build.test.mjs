// Tests for scripts/mac-build.ts — the run-selection race and arg handling.
// Run: node --test scripts/mac-build.test.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import { parseArgs, artifactName, pickDispatchedRun } from "./mac-build.ts";

test("parseArgs defaults to a release build on Depot", () => {
  const a = parseArgs([]);
  assert.equal(a.profile, "release");
  assert.equal(a.runner, "depot");
  assert.equal(a.push, true);
  assert.equal(a.stage, true);
});

test("parseArgs reads profile, runner and ref", () => {
  const a = parseArgs(["--profile", "fast", "--runner", "github", "--ref", "my-branch", "--no-push", "--no-stage"]);
  assert.deepEqual(
    { p: a.profile, r: a.runner, ref: a.ref, push: a.push, stage: a.stage },
    { p: "fast", r: "github", ref: "my-branch", push: false, stage: false },
  );
});

test("artifact name matches what the workflow uploads", () => {
  assert.equal(artifactName("release"), "macos-arm64-release");
  assert.equal(artifactName("fast"), "macos-arm64-fast");
});

// `gh workflow run` returns no run id, so the run must be found by listing. Picking "newest
// for this branch" races against a run already in flight — it would attach to someone else's
// build and download THEIR artifact, reporting success for a binary we never asked for.
test("run selection ignores runs that predate the dispatch", () => {
  const now = Date.parse("2026-07-29T12:00:00Z");
  const runs = [
    { databaseId: 1, createdAt: "2026-07-29T11:59:00Z", headBranch: "main" }, // in flight already
    { databaseId: 2, createdAt: "2026-07-29T12:00:03Z", headBranch: "main" }, // ours
  ];
  assert.equal(pickDispatchedRun(runs, "main", now), 2);
});

test("run selection ignores other branches", () => {
  const now = Date.parse("2026-07-29T12:00:00Z");
  const runs = [{ databaseId: 9, createdAt: "2026-07-29T12:00:05Z", headBranch: "other" }];
  assert.equal(pickDispatchedRun(runs, "main", now), null);
});

// Clock skew between this machine and GitHub can stamp our own run a fraction before the
// dispatch instant; without slack the tool would wait out its find-deadline and give up.
test("run selection tolerates slight clock skew", () => {
  const now = Date.parse("2026-07-29T12:00:00Z");
  const runs = [{ databaseId: 3, createdAt: "2026-07-29T11:59:59.5Z", headBranch: "main" }];
  assert.equal(pickDispatchedRun(runs, "main", now), 3);
});

test("run selection takes the OLDEST qualifying run, not the newest", () => {
  const now = Date.parse("2026-07-29T12:00:00Z");
  const runs = [
    { databaseId: 5, createdAt: "2026-07-29T12:00:30Z", headBranch: "main" },
    { databaseId: 4, createdAt: "2026-07-29T12:00:02Z", headBranch: "main" }, // ours: fired first
  ];
  assert.equal(pickDispatchedRun(runs, "main", now), 4);
});
