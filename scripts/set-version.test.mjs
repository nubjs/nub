// Regression tests for release-version stamping. Run: node --test scripts/set-version.test.mjs
import assert from "node:assert/strict";
import { existsSync, mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

const SCRIPT = fileURLToPath(new URL("./set-version.mjs", import.meta.url));
const VERSION = "7.8.9";
const PACKAGE_FILES = [
  "npm/nub/package.json",
  "npm/nub-types/package.json",
  "npm/nub-darwin-arm64/package.json",
  "npm/nub-darwin-x64/package.json",
  "npm/nub-linux-x64/package.json",
  "npm/nub-linux-x64-musl/package.json",
  "npm/nub-linux-arm64/package.json",
  "npm/nub-linux-arm64-musl/package.json",
  "npm/nub-win32-x64/package.json",
  "npm/nub-win32-arm64/package.json",
];
const VERSION_SURFACES = [
  ...PACKAGE_FILES,
  "Cargo.toml",
  "crates/nub-native/Cargo.toml",
  "runtime/version.mjs",
];

function write(root, file, content) {
  mkdirSync(join(root, dirname(file)), { recursive: true });
  writeFileSync(join(root, file), content);
}

function fixture({ latest = false, corruptLatest = false } = {}) {
  const root = mkdtempSync(join(tmpdir(), "nub-set-version-"));
  for (const file of PACKAGE_FILES) write(root, file, JSON.stringify({ version: "0.0.0" }) + "\n");
  write(root, "Cargo.toml", '[workspace.package]\nversion = "0.0.0"\n');
  write(root, "crates/nub-native/Cargo.toml", '[package]\nversion = "0.0.0"\n');
  write(root, "runtime/version.mjs", 'export const NUB_VERSION = "0.0.0";\n');
  if (latest) {
    write(root, "site/public/schema/latest.json", JSON.stringify({ $id: "https://nubjs.com/schema/latest.json", title: "Nub config" }) + "\n");
  }
  if (corruptLatest) write(root, "site/public/schema/latest.json", "{ not json\n");
  return root;
}

function run(root) {
  return spawnSync(process.execPath, [SCRIPT, VERSION], { cwd: root, encoding: "utf8" });
}

test("stamps a pinned schema snapshot from latest", () => {
  const root = fixture({ latest: true });
  try {
    const result = run(root);
    assert.equal(result.status, 0, result.stderr);

    const latest = JSON.parse(readFileSync(join(root, "site/public/schema/latest.json"), "utf8"));
    const snapshot = JSON.parse(readFileSync(join(root, "site/public/schema/v7.8.json"), "utf8"));
    assert.equal(snapshot.$id, "https://nubjs.com/schema/v7.8.json");
    delete latest.$id;
    delete snapshot.$id;
    assert.deepEqual(snapshot, latest);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

// The site withdraws the published schema whenever the nub.jsonc config
// reference is hidden pending ship (3539b65db1), so an absent latest.json is a
// normal state and must not block a release bump. A latest.json that EXISTS but
// cannot be parsed still aborts before anything is stamped — the next test.
test("absent latest schema still stamps every version surface, writing no snapshot", () => {
  const root = fixture();
  try {
    const result = run(root);
    assert.equal(result.status, 0, result.stderr);
    for (const file of VERSION_SURFACES) {
      assert.match(readFileSync(join(root, file), "utf8"), new RegExp(VERSION), file);
    }
    assert.equal(existsSync(join(root, "site/public/schema/v7.8.json")), false);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("unreadable latest schema leaves every version surface unchanged", () => {
  const root = fixture({ corruptLatest: true });
  try {
    const before = new Map(VERSION_SURFACES.map((file) => [file, readFileSync(join(root, file), "utf8")]));
    const result = run(root);
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /ERROR: cannot prepare schema snapshot:/);
    for (const [file, content] of before) assert.equal(readFileSync(join(root, file), "utf8"), content, file);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
