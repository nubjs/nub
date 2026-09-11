import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { once } from "node:events";
import { mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

const root = dirname(dirname(fileURLToPath(import.meta.url)));
const read = (path) => readFileSync(join(root, path), "utf8");
const scripts = JSON.parse(read("package.json")).scripts;
const spec = scripts.lat.match(/^npx --yes (lat\.md@\d+\.\d+\.\d+)$/)?.[1];
const mcp = JSON.parse(read(".mcp.json")).mcpServers.lat;
const hook = "scripts/lat-prompt.mjs";

test("CLI and both MCP clients use the same exact Lat version", () => {
  assert.ok(spec, "lat must use an exact version outside root dependencies");
  assert.equal(scripts["lat:check"], `npx --yes ${spec} check`);
  assert.equal(scripts["lat:index"], `npx --yes ${spec} reindex --local --yes`);
  assert.deepEqual(mcp, { command: "npx", args: ["--yes", spec, "mcp"] });
  const codex = read(".codex/config.toml").split("[mcp_servers.lat]\n")[1]?.split(/\n\[/)[0];
  assert.ok(codex);
  assert.match(codex, /^command = "npx"$/m);
  assert.ok(codex.includes(`args = ["--yes", "${spec}", "mcp"]`));
  assert.match(codex, /^tool_timeout_sec = 600$/m);
});

test("both agents invoke the same fast, non-blocking prompt reminder", () => {
  for (const path of [".claude/settings.json", ".codex/hooks.json"]) {
    const hooks = JSON.parse(read(path)).hooks.UserPromptSubmit;
    assert.deepEqual(hooks, [{ hooks: [{
      type: "command",
      command: `node "$(git rev-parse --show-toplevel)/${hook}"`,
      timeout: 5,
    }] }]);
  }
  for (const input of [{ prompt: "Investigate cache behavior" }, { user_prompt: "Investigate cache behavior" }, {}]) {
    const result = spawnSync(process.execPath, [join(root, hook)], {
      input: JSON.stringify(input), encoding: "utf8", timeout: 5000,
    });
    assert.equal(result.status, 0, result.stderr);
    assert.equal(result.stderr, "");
    const output = JSON.parse(result.stdout);
    assert.equal(output.hookSpecificOutput.hookEventName, "UserPromptSubmit");
    assert.match(output.hookSpecificOutput.additionalContext, /lat_search/);
    assert.match(output.hookSpecificOutput.additionalContext, /nub run lat search/);
    assert.match(output.hookSpecificOutput.additionalContext, /nub run lat:index/);
    assert.equal(output.decision, undefined);
  }
});

test("the skill's navigation examples and prose section IDs resolve", {
  skip: process.env.LAT_SMOKE !== "1",
  timeout: 120000,
}, () => {
  const skill = read(".claude/skills/lat-md/SKILL.md");
  const examples = [...skill.matchAll(/^nub run lat .+$/gm)]
    .map(([line]) => line.replace(/\s{2,}#.*$/, "").trim());
  assert.equal(examples.length, 6);
  const run = (args) => {
    const result = spawnSync("npm", ["run", "--silent", "lat", "--", ...args], {
      cwd: root, encoding: "utf8", timeout: 30000,
    });
    assert.equal(result.status, 0, `${args.join(" ")}\n${result.error ?? ""}\n${result.stderr}\n${result.stdout}`);
  };
  for (const example of examples) {
    const match = example.match(/^nub run lat (\w+)(?: "([^"]*)")?$/);
    assert.ok(match, `unrecognized example: ${example}`);
    // The small fixture below covers search without rebuilding the whole wiki.
    if (match[1] !== "search") run([match[1], ...(match[2] ? [match[2]] : [])]);
  }
  const ids = [...new Set([...skill.matchAll(/`([^`\n]+)`/g)]
    .map(([, value]) => value)
    .filter((value) => value.includes("#") && !value.includes("<") && !value.includes("[[") && !value.startsWith("#[") && !value.includes("…")))];
  assert.ok(ids.length >= 2);
  for (const id of ids) run(["section", id]);
});

// Opt in where the pinned package can be fetched. Ordinary script tests stay offline.
test("local indexing, refresh, cache recovery, and MCP search", {
  skip: process.env.LAT_SMOKE !== "1",
  timeout: 180000,
}, async (t) => {
  const cwd = mkdtempSync(join(tmpdir(), "nub-lat-"));
  t.after(() => rmSync(cwd, { recursive: true, force: true }));
  const graph = join(cwd, "lat.md");
  mkdirSync(graph);
  // One searchable section tests indexing and transport, not approximate ranking.
  const document = join(graph, "lat.md");
  writeFileSync(document, "# Package cache\n\nOffline package installation reuses cached dependency archives.\n");
  const env = {
    ...process.env,
    XDG_CONFIG_HOME: join(cwd, "config"),
    LAT_LLM_KEY: "sk-invalid-local-search-must-not-use-this",
    LAT_LLM_KEY_FILE: "",
    LAT_LLM_KEY_HELPER: "",
    NO_COLOR: "1",
  };
  const run = (...args) => {
    const result = spawnSync("npx", ["--yes", spec, ...args], {
      cwd, env, encoding: "utf8", timeout: 60000,
    });
    assert.equal(result.status, 0, `${args.join(" ")}\n${result.error ?? ""}\n${result.stderr}\n${result.stdout}`);
    return result.stdout;
  };
  run("reindex", "--local", "--yes");
  assert.match(run("search", "offline package installation cache", "--limit", "1"), /\[\[lat\.md\/lat#Package cache\]\]/);

  writeFileSync(document, "# Dependency mirror\n\nOffline package installation reuses cached dependency archives from the mirror.\n");
  const refreshed = run("search", "offline package installation cache", "--limit", "1");
  assert.match(refreshed, /\[\[lat\.md\/lat#Dependency mirror\]\]/);
  assert.doesNotMatch(refreshed, /#Package cache/);
  rmSync(join(graph, ".cache"), { recursive: true });
  assert.match(run("search", "offline package installation cache", "--limit", "1"), /#Dependency mirror/);

  const server = spawn(mcp.command, mcp.args, { cwd, env, stdio: ["pipe", "pipe", "pipe"] });
  const exited = once(server, "exit");
  let errors = "";
  server.stderr.on("data", (data) => { errors += data; });
  const lines = createInterface({ input: server.stdout });
  let id = 0;
  const pending = new Map();
  const rejectPending = (error) => {
    for (const request of pending.values()) request.reject(error);
    pending.clear();
  };
  server.on("error", rejectPending);
  server.on("exit", (code, signal) => rejectPending(new Error(`MCP exited ${code ?? signal}: ${errors}`)));
  lines.on("line", (line) => {
    let message;
    try { message = JSON.parse(line); }
    catch { rejectPending(new Error(`Non-JSON MCP stdout: ${line}`)); return; }
    const request = pending.get(message.id);
    if (!request) return;
    pending.delete(message.id);
    if (message.error) request.reject(new Error(JSON.stringify(message.error)));
    else request.resolve(message.result);
  });
  const send = (method, params) => new Promise((resolve, reject) => {
    const requestId = ++id;
    const timer = setTimeout(() => {
      pending.delete(requestId);
      reject(new Error(`MCP ${method} timed out: ${errors}`));
    }, 30000);
    pending.set(requestId, {
      resolve: (value) => { clearTimeout(timer); resolve(value); },
      reject: (error) => { clearTimeout(timer); reject(error); },
    });
    server.stdin.write(JSON.stringify({ jsonrpc: "2.0", id: requestId, method, params }) + "\n");
  });
  try {
    await send("initialize", { protocolVersion: "2024-11-05", capabilities: {}, clientInfo: { name: "nub-lat-test", version: "1.0.0" } });
    server.stdin.write(JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" }) + "\n");
    const list = await send("tools/list", {});
    assert.deepEqual(list.tools.map((tool) => tool.name).sort(), ["lat_check", "lat_expand", "lat_locate", "lat_refs", "lat_search", "lat_section"]);
    const result = await send("tools/call", { name: "lat_search", arguments: { query: "offline package installation cache", limit: 1 } });
    assert.notEqual(result.isError, true, JSON.stringify(result));
    assert.match(result.content.map((item) => item.text ?? "").join("\n"), /#Dependency mirror/);
  } finally {
    server.stdin.end();
    const kill = setTimeout(() => server.kill("SIGTERM"), 5000);
    await exited;
    clearTimeout(kill);
    lines.close();
  }
});
