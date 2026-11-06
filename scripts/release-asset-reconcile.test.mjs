// Runs the two "Reconcile immutable … assets" steps of release.yml, exactly as written,
// against a fake `gh`. They are the last gate before a release or a nightly canary goes
// out, so a transient GitHub 5xx must not fail them, and retrying must not let a real
// outage or a byte mismatch through.
//
// Run: node --test scripts/release-asset-reconcile.test.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { delimiter, join } from "node:path";

const STEPS = ["Reconcile immutable stable assets", "Reconcile immutable canary assets"];

// The steps use `mapfile`, which macOS's bash 3.2 lacks. CI's script tests run on Linux.
const bashMajor = Number(spawnSync("bash", ["-c", "echo ${BASH_VERSINFO[0]}"], { encoding: "utf8" }).stdout);
const skip = bashMajor >= 4 ? false : `needs bash 4 or later, found ${bashMajor || "none"}`;

function stepScript(name) {
  const lines = readFileSync(".github/workflows/release.yml", "utf8").split("\n");
  const start = lines.findIndex((l) => l.trim() === `- name: ${name}`);
  assert.notEqual(start, -1, `release.yml has no step named "${name}"`);
  const run = lines.findIndex((l, i) => i > start && l.trim() === "run: |");
  const indent = lines[run].search(/\S/);
  const body = [];
  for (const line of lines.slice(run + 1)) {
    if (line.trim() !== "" && line.search(/\S/) <= indent) break;
    body.push(line.slice(indent + 2));
  }
  return body.join("\n");
}

// Each entry in GH_FAIL is one upcoming failure: `view`, `download`, or `download-always`.
// A failing download leaves a partial file behind, as the real gh does when a body dies.
const FAKE_GH = `#!/usr/bin/env bash
verb="$2"
if [ "$verb" = view ] && grep -qxF view "$GH_FAIL"; then
  sed -i '0,/^view$/{/^view$/d}' "$GH_FAIL"
  echo "HTTP 500" >&2; exit 1
fi
case "$verb" in
  view) ls "$RELEASE_DIR" ;;
  download)
    asset="$5"; dir="$7"
    if grep -qxF download-always "$GH_FAIL" || grep -qxF download "$GH_FAIL"; then
      sed -i '0,/^download$/{/^download$/d}' "$GH_FAIL"
      printf partial > "$dir/$asset"
      echo "HTTP 500 (https://api.github.com/repos/nubjs/nub/releases/assets/1)" >&2; exit 1
    fi
    if [ -e "$dir/$asset" ] && [ "$8" != --clobber ]; then
      echo "$dir/$asset already exists (use --clobber to overwrite)" >&2; exit 1
    fi
    cp "$RELEASE_DIR/$asset" "$dir/$asset" ;;
  upload) cp "$4" "$RELEASE_DIR/" ;;
esac
`;

function runStep(name, { fail = [], tamper } = {}) {
  const dir = mkdtempSync(join(tmpdir(), "release-reconcile-"));
  try {
    const script = stepScript(name);
    const assets = script.match(/EXPECTED=\(([\s\S]*?)\n\s*\)/)[1].trim().split(/\s+/);
    assert.equal(assets.length, 32, "the step should list 32 assets");

    const release = join(dir, "release");
    const bin = join(dir, "bin");
    for (const d of [release, bin, join(dir, "release-archives")]) mkdirSync(d);
    for (const a of assets) {
      writeFileSync(join(dir, "release-archives", a), a);
      writeFileSync(join(release, a), a);
    }
    if (tamper) writeFileSync(join(release, tamper), "other bytes");

    writeFileSync(join(bin, "gh"), FAKE_GH);
    writeFileSync(join(bin, "sleep"), "#!/bin/sh\nexit 0\n");
    chmodSync(join(bin, "gh"), 0o755);
    chmodSync(join(bin, "sleep"), 0o755);
    writeFileSync(join(dir, "fail"), fail.map((f) => `${f}\n`).join(""));
    writeFileSync(join(dir, "step.sh"), script);

    return spawnSync("bash", ["--noprofile", "--norc", "-eo", "pipefail", "step.sh"], {
      cwd: dir,
      encoding: "utf8",
      env: {
        PATH: `${bin}${delimiter}${process.env.PATH}`,
        TAG: "v1.2.3",
        VERSION: "1.2.3",
        RELEASE_DIR: release,
        GH_FAIL: join(dir, "fail"),
      },
    });
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

for (const name of STEPS) {
  test(`${name}: passes through a transient 5xx on a listing and on a download`, { skip }, () => {
    const r = runStep(name, { fail: ["view", "download"] });
    assert.equal(r.status, 0, `the step should recover:\n${r.stdout}\n${r.stderr}`);
    assert.match(r.stderr, /failed on attempt 1 of 4/, "the fault should have been injected and retried");
    assert.match(r.stdout, /✓ all 32 /);
  });

  test(`${name}: fails when downloads never recover`, { skip }, () => {
    const r = runStep(name, { fail: ["download-always"] });
    assert.notEqual(r.status, 0, `the step should fail:\n${r.stdout}`);
    assert.match(r.stderr, /failed on attempt 4 of 4/);
    assert.doesNotMatch(r.stdout, /✓ all 32 /);
  });

  test(`${name}: fails on an asset whose bytes differ`, { skip }, () => {
    const r = runStep(name, { tamper: "nub-linux-x64.tar.gz" });
    assert.notEqual(r.status, 0, `the step should fail:\n${r.stdout}`);
    assert.match(r.stdout, /::error::.*nub-linux-x64\.tar\.gz differs/);
  });
}
