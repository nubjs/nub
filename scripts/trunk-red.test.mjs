// Guards trunk-red's close rule by running the workflow's real shell step against a stub
// `gh`. trunk-red only ever runs from the default branch's copy, so no pull-request run can
// exercise a change to it, and a wrong close hides a red trunk from everyone. The stub
// applies the step's own --jq filters with real jq.
//
// Run: node --test scripts/trunk-red.test.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { chmodSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

// Line-oriented, like ci-optin.test.mjs: the repo has no YAML parser dependency.
function reportStep() {
  const lines = readFileSync(".github/workflows/trunk-red.yml", "utf8").split("\n");
  const start = lines.indexOf("        run: |") + 1;
  assert.ok(start > 0, "trunk-red.yml has no `run: |` step");
  const end = lines.findIndex((l, i) => i >= start && l !== "" && !l.startsWith(" ".repeat(10)));
  return lines.slice(start, end === -1 ? undefined : end).map((l) => l.slice(10)).join("\n");
}

const STUB_GH = String.raw`#!/usr/bin/env bash
d="$STUB_DIR"
jqexpr=""; wf=""; prev=""
for a in "$@"; do
  case "$prev" in --jq) jqexpr="$a" ;; --workflow) wf="$a" ;; esac
  prev="$a"
done
case "$1 $2" in
  "issue list") [[ -e "$d/body" ]] && echo 977; exit 0 ;;
  "issue view") [[ -e "$d/unreadable" ]] && exit 1; cat "$d/body"; exit 0 ;;
  "api --paginate") cat "$d/comments"; exit 0 ;;
  "run list") jq -r "$jqexpr" "$d/runs-$wf.json"; exit $? ;;
  "issue close"|"issue comment"|"issue create") echo "$2" >> "$d/actions"; exit 0 ;;
  "label create") exit 0 ;;
esac
echo "unexpected: gh $*" >&2; exit 99
`;

const report = ([workflow, runId]) =>
  `\`${workflow}\` failed on main at \`abc123456\`.\n\n` +
  `https://github.com/nubjs/nub/actions/runs/${runId}\n\nTrunk-only legs ...\n`;
const green = (databaseId, event = "push") => ({ conclusion: "success", databaseId, event });
const red = (databaseId, event = "push") => ({ conclusion: "failure", databaseId, event });
const cancelled = (databaseId) => ({ conclusion: "cancelled", databaseId, event: "push" });

// `reports` are [workflow, failed run id] pairs, or raw text; the first is the issue body and
// the rest its comments. `runs` maps a workflow to its completed runs, newest first; a workflow
// with no entry makes `gh run list` fail. Omitting `reports` means no issue is open.
function trigger({ event, reports, runs = {}, unreadable = false }) {
  const dir = mkdtempSync(join(tmpdir(), "trunk-red-"));
  try {
    writeFileSync(join(dir, "gh"), STUB_GH);
    chmodSync(join(dir, "gh"), 0o755);
    if (reports) {
      const [body, ...comments] = reports.map((r) => (typeof r === "string" ? r : report(r)));
      writeFileSync(join(dir, "body"), body);
      writeFileSync(join(dir, "comments"), comments.join("\n"));
    }
    if (unreadable) writeFileSync(join(dir, "unreadable"), "");
    for (const [wf, list] of Object.entries(runs)) {
      writeFileSync(join(dir, `runs-${wf}.json`), JSON.stringify(list));
    }
    const [WORKFLOW, CONCLUSION] = event;
    const result = spawnSync("bash", ["-c", reportStep()], {
      encoding: "utf8",
      env: {
        ...process.env,
        PATH: `${dir}:${process.env.PATH}`,
        STUB_DIR: dir,
        REPO: "nubjs/nub",
        TITLE: "CI is failing on main",
        WORKFLOW,
        CONCLUSION,
        RUN_URL: "https://example.test/run",
        SHA: "0123456789abcdef",
      },
    });
    const log = `${result.stdout}${result.stderr}`;
    assert.equal(result.status, 0, `the step must not fail the notifier:\n${log}`);
    let actions = [];
    try {
      actions = readFileSync(join(dir, "actions"), "utf8").trim().split("\n");
    } catch {}
    return { actions, log };
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

const CI_GREEN = ["CI", "success"];

test("a green from another workflow does not clear a reported red", () => {
  const { actions, log } = trigger({
    event: CI_GREEN,
    reports: [["Aube parity", 200]],
    runs: { "Aube parity": [red(200)] },
  });
  assert.deepEqual(actions, [], log);
  assert.match(log, /still red: Aube parity/);
});

test("closes once every reported workflow has a green verdict newer than its failure", () => {
  const { actions, log } = trigger({
    event: CI_GREEN,
    reports: [["CI", 200], ["Node matrix", 210]],
    runs: {
      CI: [cancelled(330), green(300)],
      "Node matrix": [cancelled(340), green(310, "schedule")],
    },
  });
  assert.deepEqual(actions, ["close"], log);
});

test("a re-run of the failed run never falls back to the green before the failure", () => {
  // A re-run keeps its run id: in progress it leaves the completed list, cancelled it is skipped.
  for (const completed of [[green(100)], [cancelled(200), green(100)]]) {
    const { actions, log } = trigger({
      event: CI_GREEN,
      reports: [["Node matrix", 200]],
      runs: { "Node matrix": completed },
    });
    assert.deepEqual(actions, [], log);
  }
});

test("a successful re-run of the failed run clears it", () => {
  const { actions, log } = trigger({
    event: ["Node matrix", "success"],
    reports: [["Node matrix", 200]],
    runs: { "Node matrix": [green(200), green(100)] },
  });
  assert.deepEqual(actions, ["close"], log);
});

test("a workflow reported twice must recover from its newest failure, compared numerically", () => {
  // As strings "9999999999" sorts above "41636887252", which would anchor on the older failure.
  const { actions, log } = trigger({
    event: CI_GREEN,
    reports: [["Node matrix", 9999999999], ["Node matrix", 41636887252]],
    runs: { "Node matrix": [green(20000000000)] },
  });
  assert.deepEqual(actions, [], log);
});

test("a pull-request run from a fork's main branch counts neither way", () => {
  const held = trigger({
    event: CI_GREEN,
    reports: [["Node matrix", 200]],
    runs: { "Node matrix": [green(400, "pull_request"), red(200)] },
  });
  assert.deepEqual(held.actions, [], held.log);

  const closed = trigger({
    event: CI_GREEN,
    reports: [["Node matrix", 200]],
    runs: { "Node matrix": [red(400, "pull_request"), green(300)] },
  });
  assert.deepEqual(closed.actions, ["close"], closed.log);
});

test("anything unreadable leaves the issue open", () => {
  const cases = {
    "issue unreadable": { reports: [["CI", 200]], runs: { CI: [green(300)] }, unreadable: true },
    "run list failing": { reports: [["CI", 200], ["WPT Worker", 210]], runs: { CI: [green(300)] } },
    "no report lines": { reports: ["filed by hand\n"], runs: { CI: [green(300)] } },
    "report without a run URL": {
      reports: ["`CI` failed on main at `abc123456`.\n\nno link\n"],
      runs: { CI: [green(300)] },
    },
    "only cancelled runs": { reports: [["CI", 200]], runs: { CI: [cancelled(300)] } },
  };
  for (const [name, scenario] of Object.entries(cases)) {
    const { actions, log } = trigger({ event: CI_GREEN, ...scenario });
    assert.deepEqual(actions, [], `${name}:\n${log}`);
  }
});

test("a failure comments on the open issue, or files one when none is open", () => {
  const event = ["Node matrix", "failure"];
  assert.deepEqual(trigger({ event, reports: [["CI", 200]] }).actions, ["comment"]);
  assert.deepEqual(trigger({ event }).actions, ["create"]);
});

test("a green with no open issue does nothing", () => {
  assert.deepEqual(trigger({ event: CI_GREEN }).actions, []);
});
