// Guards the opt-in PR CI invariant. PR CI is requested explicitly — every workflow
// that triggers on `pull_request` fires ONLY on `labeled`, and every job it defines
// re-checks the label NAME, because a `labeled` event fires for any label at all.
//
// The invariant is per-JOB rather than per-workflow on purpose: a job added later
// with no guard would run the whole matrix on every label, silently undoing the
// change. That is exactly the drift this file exists to catch, so it asserts over
// whatever is on disk instead of a hard-coded list.
//
// Run: node --test scripts/ci-optin.test.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";

const DIR = ".github/workflows";
const GUARD = "github.event.label.name == 'ci'";
// ci-optin.yml IS the mechanism — it carries the label guard but must never require
// itself to be opted into, so it is checked separately below.
const MECHANISM = "ci-optin.yml";

// Line-oriented on purpose: the repo has no YAML parser dependency, and the two
// shapes this needs (the `on:` block, and top-level job keys) are unambiguous at
// fixed indents in every file here.
function parse(file) {
  const lines = readFileSync(`${DIR}/${file}`, "utf8").split("\n");
  const onIdx = lines.findIndex((l) => /^on:/.test(l));
  const jobsIdx = lines.findIndex((l) => /^jobs:\s*$/.test(l));
  const onBlock = onIdx >= 0 && jobsIdx > onIdx ? lines.slice(onIdx, jobsIdx) : [];
  const jobs = [];
  for (let i = jobsIdx + 1; i < lines.length && jobsIdx >= 0; i++) {
    const m = lines[i].match(/^ {2}([A-Za-z0-9_-]+):\s*$/);
    if (!m) continue;
    let body = "";
    for (let k = i + 1; k < lines.length && !/^ {2}[A-Za-z0-9_-]+:\s*$/.test(lines[k]); k++) body += `${lines[k]}\n`;
    jobs.push({ name: m[1], body });
  }
  return { onBlock, jobs };
}

const workflows = readdirSync(DIR).filter((f) => f.endsWith(".yml"));
const prTriggered = workflows.filter((f) => parse(f).onBlock.some((l) => /^ {2}pull_request:/.test(l)));

test("the fixture finds the workflows it is meant to guard (instrument check)", () => {
  assert.ok(workflows.length > 20, `expected the full workflow set, saw ${workflows.length}`);
  assert.ok(prTriggered.length > 10, `expected many PR-triggered workflows, saw ${prTriggered.length}`);
  assert.ok(prTriggered.includes("ci.yml"), "ci.yml must be in scope");
  assert.ok(!prTriggered.includes("release.yml"), "release.yml is tag-driven and must NOT be in scope");
});

for (const file of prTriggered.filter((f) => f !== MECHANISM)) {
  test(`${file}: pull_request fires only on \`labeled\``, () => {
    const { onBlock } = parse(file);
    const start = onBlock.findIndex((l) => /^ {2}pull_request:/.test(l));
    const rest = onBlock.slice(start + 1);
    const end = rest.findIndex((l) => /^ {0,2}\S/.test(l));
    const block = (end === -1 ? rest : rest.slice(0, end)).join("\n");
    assert.match(block, /^ {4}types: \[labeled\]$/m, `${file} must declare \`types: [labeled]\` so pushes start nothing`);
  });

  test(`${file}: every job re-checks the ci label`, () => {
    const { jobs } = parse(file);
    assert.ok(jobs.length > 0, `${file} defines no jobs — the parser is broken`);
    const ungated = jobs.filter((j) => !j.body.includes(GUARD)).map((j) => j.name);
    assert.deepEqual(ungated, [], `${file}: job(s) missing the \`${GUARD}\` guard — they would run on ANY label`);
  });
}

test(`${MECHANISM}: scopes itself to the ci label without gating itself`, () => {
  const { jobs } = parse(MECHANISM);
  assert.ok(jobs.length > 0);
  for (const j of jobs) assert.ok(j.body.includes(GUARD), `${MECHANISM} job ${j.name} must scope to the ci label`);
});

// A `push:` trigger with no branch filter fires on EVERY branch, so it runs CI on a
// pull request's branch no matter what the label says — the opt-in gate walked around
// rather than removed. Both launcher.yml and verify-install.yml were doing exactly
// that. Tag-driven pushes (release.yml) are a different trigger and stay in scope.
test("no workflow has a branch-unrestricted `push:` trigger", () => {
  const offenders = [];
  for (const file of workflows) {
    const { onBlock } = parse(file);
    const p = onBlock.findIndex((l) => /^ {2}push:\s*$/.test(l));
    if (p < 0) continue;
    let filtered = false;
    for (let i = p + 1; i < onBlock.length && !/^ {2}\S/.test(onBlock[i]); i++) {
      if (/^ {4}(branches|tags):/.test(onBlock[i])) { filtered = true; break; }
    }
    if (!filtered) offenders.push(file);
  }
  assert.deepEqual(offenders, [], "an unfiltered `push:` runs on every branch and bypasses the ci label");
});
