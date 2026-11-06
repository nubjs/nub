import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import test from "node:test";

const root = fileURLToPath(new URL("../", import.meta.url));

function softLimitUnder(setup: string) {
  const result = spawnSync("sh", ["-c", `${setup}; exec sh scripts/exec-hook.sh sh -c 'ulimit -Sn'`], {
    cwd: root,
    encoding: "utf8",
  });
  assert.equal(result.status, 0, result.stderr);
  return Number(result.stdout.trim());
}

void test("Codex command hooks raise the soft descriptor limit before Node", () => {
  const config: { hooks: Record<string, { hooks: { command: string }[] }[]> } = JSON.parse(readFileSync(new URL("../.codex/hooks.json", import.meta.url), "utf8"));
  const commands = Object.values(config.hooks).flatMap((events) =>
    events.flatMap((event) => event.hooks.map((hook) => hook.command)),
  );
  assert.equal(commands.length, 2);
  for (const command of commands) {
    assert.match(command, /scripts\/exec-hook\.sh/);
  }

  const raised = softLimitUnder("ulimit -Sn 64");
  assert.ok(raised > 64, `expected a soft limit above 64, got ${raised}`);
});

void test("hook raises the soft limit no further than the inherited hard limit", () => {
  for (const [soft, hard, expected] of [[64, 4096, 4096], [64, 64, 64], [4096, 4096, 4096]]) {
    const got = softLimitUnder(`ulimit -Sn ${soft}; ulimit -Hn ${hard}`);
    assert.equal(got, expected, `soft=${soft} hard=${hard}: expected ${expected}, got ${got}`);
  }
});
