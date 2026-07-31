// Runs as a jailed dependency lifecycle script. Reports, for the PATH the jail actually
// handed this process, which entries return an errno libuv's PATH walk cannot survive.
//
// libuv resolves a bare program name by calling posix_spawn() on each PATH candidate in
// turn and continuing ONLY on ENOENT/ENOTDIR/EACCES; every other errno is returned as the
// result of the whole resolution (deps/uv/src/unix/process.c, uv__spawn_resolve_and_spawn).
// Seatbelt answers a refused symlink read with EPERM, which is not in that continue set,
// so one ungranted entry holding a symlink of the right name aborts the search before any
// later granted directory is tried.
const cp = require("child_process");

const TOOL = process.env.SPAWN_PROBE_TOOL || "git";
const dirs = (process.env.PATH || "").split(":").filter(Boolean);
const CONTINUES = ["ENOENT", "ENOTDIR", "EACCES"];

const poison = [];
for (const dir of dirs) {
  const r = cp.spawnSync(`${dir}/${TOOL}`, ["--version"]);
  if (r.error && !CONTINUES.includes(r.error.code)) poison.push([dir, r.error.code]);
}

console.log(`PROBE tool=${TOOL} entries=${dirs.length} poison=${JSON.stringify(poison)}`);

// The differential. Both arms are one bare-name spawn; the only variable is whether the
// poison entries are still on PATH, so a difference is attributable to them alone.
const clean = dirs.filter((d) => !poison.some(([p]) => p === d)).join(":");
const asIs = cp.spawnSync(TOOL, ["--version"], { encoding: "utf8" });
const filtered = cp.spawnSync(TOOL, ["--version"], {
  encoding: "utf8",
  env: { ...process.env, PATH: clean },
});
const say = (r) => (r.error ? `FAIL ${r.error.code}` : `OK rc=${r.status} ${r.stdout.trim()}`);
console.log(`PROBE jailed PATH verbatim : ${say(asIs)}`);
console.log(`PROBE poison dropped       : ${say(filtered)}`);
