// Guards the opt-in PR CI invariant. PR CI is requested explicitly — every workflow
// that triggers on `pull_request` fires ONLY on `labeled`, and every job it defines
// re-checks the label NAME, because a `labeled` event fires for any label at all.
//
// The invariant is per-JOB rather than per-workflow on purpose: a job added later
// with no guard would run the whole matrix on every label, silently undoing the
// change. That is exactly the drift this file exists to catch, so it asserts over
// whatever is on disk instead of a hard-coded list.
//
// Two labels request a run. `ci-full` starts every job; `ci` starts every job except
// those of the FULL_ONLY workflows, which reach a pull request only through `ci-full`
// and otherwise run on main or the nightly.
//
// Run: node --test scripts/ci-optin.test.mts
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";

const DIR = ".github/workflows";
const CI_GUARD = "github.event.label.name == 'ci'";
const FULL_GUARD = "github.event.label.name == 'ci-full'";
const FULL_ONLY = [
  "compile-native.yml",
  "daily-driver.yml",
  "docker-smoke.yml",
  "lockfile-roundtrip.yml",
  "npm-corpus.yml",
  "verb-dispatch.yml",
  "wpt-worker.yml",
];
// ci-optin.yml IS the mechanism — it carries the label guard but must never require
// itself to be opted into, so it is checked separately below.
const MECHANISM = "ci-optin.yml";

// Line-oriented on purpose: the repo has no YAML parser dependency, and the two
// shapes this needs (the `on:` block, and top-level job keys) are unambiguous at
// fixed indents in every file here.
function parse(file: string) {
  const lines = readFileSync(`${DIR}/${file}`, "utf8").split("\n");
  const onIdx = lines.findIndex((l) => l.startsWith("on:"));
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

void test("the fixture finds the workflows it is meant to guard (instrument check)", () => {
  assert.ok(workflows.length > 20, `expected the full workflow set, saw ${workflows.length}`);
  assert.ok(prTriggered.length > 10, `expected many PR-triggered workflows, saw ${prTriggered.length}`);
  assert.ok(prTriggered.includes("ci.yml"), "ci.yml must be in scope");
  assert.ok(!prTriggered.includes("release.yml"), "release.yml is tag-driven and must NOT be in scope");
  for (const f of FULL_ONLY) assert.ok(prTriggered.includes(f), `${f} is listed as ci-full only but is not a PR-triggered workflow`);
});

for (const file of prTriggered.filter((f) => f !== MECHANISM)) {
  void test(`${file}: pull_request fires only on \`labeled\``, () => {
    const { onBlock } = parse(file);
    const start = onBlock.findIndex((l) => /^ {2}pull_request:/.test(l));
    const rest = onBlock.slice(start + 1);
    const end = rest.findIndex((l) => /^ {0,2}\S/.test(l));
    const block = (end === -1 ? rest : rest.slice(0, end)).join("\n");
    assert.match(block, /^ {4}types: \[labeled\]$/m, `${file} must declare \`types: [labeled]\` so pushes start nothing`);
  });

  void test(`${file}: every job answers exactly the labels meant to start it`, () => {
    const { jobs } = parse(file);
    assert.ok(jobs.length > 0, `${file} defines no jobs — the parser is broken`);
    const noFull = jobs.filter((j) => !j.body.includes(FULL_GUARD)).map((j) => j.name);
    assert.deepEqual(noFull, [], `${file}: job(s) missing the \`${FULL_GUARD}\` guard — ci-full would skip them, or they would run on ANY label`);
    const wrong = FULL_ONLY.includes(file)
      ? jobs.filter((j) => j.body.includes(CI_GUARD)).map((j) => j.name)
      : jobs.filter((j) => !j.body.includes(CI_GUARD)).map((j) => j.name);
    assert.deepEqual(wrong, [], FULL_ONLY.includes(file) ? `${file}: job(s) run on plain \`ci\`, but this workflow is ci-full only` : `${file}: job(s) missing the \`${CI_GUARD}\` guard — plain ci would skip them`);
  });
}

void test(`${MECHANISM}: scopes itself to the two request labels without gating itself`, () => {
  const { jobs } = parse(MECHANISM);
  assert.ok(jobs.length > 0);
  for (const j of jobs) {
    assert.ok(j.body.includes(CI_GUARD) && j.body.includes(FULL_GUARD), `${MECHANISM} job ${j.name} must scope to both request labels`);
  }
});

// A `labeled` run whose jobs all skip still takes its workflow's concurrency group, so
// with cancel-in-progress any label would cancel the run in progress. Confirmed on a live
// pull request: adding `bug` cancelled every running workflow.
for (const file of prTriggered.filter((f) => f !== MECHANISM)) {
  void test(`${file}: a label it ignores cannot cancel its running jobs`, () => {
    const lines = readFileSync(`${DIR}/${file}`, "utf8").split("\n");
    const start = lines.findIndex((l) => l.startsWith("concurrency:"));
    if (start < 0) return;
    const block = [];
    for (let i = start + 1; i < lines.length && /^\s/.test(lines[i]); i++) block.push(lines[i]);
    if (!block.some((l) => /^ {2}cancel-in-progress: true/.test(l))) return;
    const group = block.find((l) => /^ {2}group:/.test(l)) ?? "";
    const expected = FULL_ONLY.includes(file)
      ? "github.event.label.name != 'ci-full' && github.event.label.name || ''"
      : "github.event.label.name != 'ci' && github.event.label.name != 'ci-full' && github.event.label.name || ''";
    assert.ok(group.includes(expected), `${file}: its concurrency group must end with \`\${{ ${expected} }}\``);
  });
}

// Leaving plain `ci` moves these workflows' coverage later. It must not remove it.
for (const file of FULL_ONLY) {
  void test(`${file}: still runs without a label, on main or the nightly`, () => {
    const { onBlock } = parse(file);
    const nightly = onBlock.some((l) => /^ {2}schedule:/.test(l));
    const push = onBlock.findIndex((l) => /^ {2}push:\s*$/.test(l));
    const onMain = push >= 0 && /^ {4}branches: \[main\]$/.test(onBlock[push + 1] ?? "");
    assert.ok(nightly || onMain, `${file} would run nowhere once plain \`ci\` stops starting it`);
  });
}

// A `push:` trigger with no branch filter fires on EVERY branch, so it runs CI on a
// pull request's branch no matter what the label says — the opt-in gate walked around
// rather than removed. Both launcher.yml and verify-install.yml were doing exactly
// that. Tag-driven pushes (release.yml) are a different trigger and stay in scope.
void test("no workflow has a branch-unrestricted `push:` trigger", () => {
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
