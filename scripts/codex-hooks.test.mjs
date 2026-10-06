import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import test from "node:test";

const root = fileURLToPath(new URL("../", import.meta.url));

test("Codex command hooks raise the soft descriptor limit before Node", () => {
	const config = JSON.parse(readFileSync(new URL("../.codex/hooks.json", import.meta.url), "utf8"));
	const commands = Object.values(config.hooks).flatMap((events) =>
		events.flatMap((event) => event.hooks.map((hook) => hook.command)),
	);
	assert.equal(commands.length, 2);
	for (const command of commands) {
		assert.match(command, /scripts\/exec-hook\.sh/);
	}

	const result = spawnSync("sh", ["-c", "ulimit -Sn 64; exec sh scripts/exec-hook.sh sh -c 'ulimit -Sn'"], {
		cwd: root,
		encoding: "utf8",
	});
	assert.equal(result.status, 0, result.stderr);
	assert.ok(Number(result.stdout.trim()) > 64, `expected raised limit, got ${result.stdout.trim()}`);
});

// A restrictive inherited hard limit must still allow raising the soft limit.
test("hook uses the available hard limit and preserves an already higher soft limit", () => {
  for (const [soft, hard, expected] of [[64, 4096, 4096], [64, 64, 64], [4096, 4096, 4096]]) {
    const result = spawnSync("sh", ["-c",
      `ulimit -Sn ${soft}; ulimit -Hn ${hard}; exec sh scripts/exec-hook.sh sh -c 'ulimit -Sn'`],
      { cwd: root, encoding: "utf8" });
    assert.equal(result.status, 0, result.stderr);
    assert.equal(Number(result.stdout.trim()), expected, result.stderr);
  }
});
