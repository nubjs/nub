// Compile `app.mjs` with a freshly built nub and prove the result is a Node
// single-executable application that still carries nub's augmentation.
//
// Written as one Node script rather than per-OS shell so the five runners assert
// exactly the same things. The only per-OS knob is `--run-with`, for a musl
// artifact that has to execute inside an Alpine container.
//
// usage:
//   node probe.mjs --nub <path> --out <path> [--platform <target>]
//                  [--run-with "docker run --rm -v {dir}:{dir} -w {dir} alpine:3.20"]
//                  [--expect sea|launcher] [--target <node-version>]
import { execFileSync, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const args = new Map();
for (let i = 2; i < process.argv.length; i += 2) args.set(process.argv[i].replace(/^--/, ""), process.argv[i + 1]);
const nub = resolve(args.get("nub"));
let out = resolve(args.get("out"));
const expect = args.get("expect") ?? "sea";
const targetNode = args.get("target") ?? "26.7.0";

const fail = (msg) => { console.error(`FAIL: ${msg}`); process.exit(1); };
const say = (msg) => console.log(`  ${msg}`);

writeFileSync(join(here, "package.json"), '{ "name": "sea-probe", "version": "1.0.0", "type": "module" }\n');
writeFileSync(join(here, ".node-version"), targetNode);
mkdirSync(dirname(out), { recursive: true });

// The control is `nub app.mjs`, not `node app.mjs`: what a compiled artifact owes
// its author is the behaviour of running the same file through nub, augmentation
// included. Plain node is the wrong answer here — it prints `worker:undefined`,
// which is exactly the difference the artifact is supposed to erase.
const env = { ...process.env, NUB_SEA_PROBE: "live" };
const control = execFileSync(nub, [join(here, "app.mjs"), "a", "b"], { encoding: "utf8", env })
  .trim().split("\n").pop();
say(`control: ${control}`);
if (!control.includes("worker:function")) fail(`the control ran unaugmented or not at all: ${control}`);

const compile = [ "compile", join(here, "app.mjs"), "--out", out, "--target", targetNode ];
if (args.get("platform")) compile.push("--platform", args.get("platform"));
const built = spawnSync(nub, compile, { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] });
process.stdout.write(built.stdout ?? "");
process.stderr.write(built.stderr ?? "");
if (built.status !== 0) fail(`nub compile exited ${built.status}`);
// A Windows target gets `.exe` appended, and cross-compiling one from a POSIX
// host does too, so the path asked for is not always the path written.
if (!existsSync(out) && existsSync(`${out}.exe`)) out = `${out}.exe`;
if (!existsSync(out)) fail(`nub compile produced no ${out}`);

// Which container did it actually pick? Both markers have to agree, because
// either alone can be true of the wrong shape: the blob magic can sit in a file
// whose fuse was never flipped (Node would ignore it and open a REPL), and a
// flipped fuse with no blob is a Node that looks for one and finds nothing.
const image = readFileSync(out);
const fusePrefix = Buffer.from("NODE_SEA_FUSE_fce680ab2cc467b6e072b8b5df1996b2:");
const fuseAt = image.indexOf(fusePrefix);
const blobAt = image.indexOf(Buffer.from("NODE_SEA_BLOB"));
const magicAt = image.indexOf(Buffer.from([0x20, 0xda, 0x43, 0x01]));
const isSea = fuseAt >= 0 && image[fuseAt + fusePrefix.length] === 0x31 && blobAt >= 0 && magicAt >= 0;
say(`shape: ${isSea ? "single-executable application" : "launcher"} (${(image.length / 1e6).toFixed(1)} MB)`);
if (expect === "sea" && !isSea) fail("expected a single-executable application, got a launcher");
if (expect === "launcher" && isSea) fail("expected the launcher shape, got a single-executable application");

// A fresh HOME/TMPDIR, so "it extracted nothing" is a statement about this run
// rather than about a directory some earlier run had already filled.
const home = mkdtempSync(join(tmpdir(), "nub-sea-probe-"));
mkdirSync(join(home, "tmp"));
const runEnv = { ...env, HOME: home, USERPROFILE: home, XDG_CACHE_HOME: join(home, "cache"), TMPDIR: join(home, "tmp") };

let cmd = out, argv = ["a", "b"];
if (args.get("run-with")) {
  const parts = args.get("run-with").replaceAll("{dir}", dirname(out)).split(" ").filter(Boolean);
  [cmd, ...argv] = [...parts, out, "a", "b"];
}
const ran = spawnSync(cmd, argv, { encoding: "utf8", env: runEnv });
process.stderr.write(ran.stderr ?? "");
if (ran.status !== 0) fail(`the artifact exited ${ran.status}: ${(ran.stdout ?? "").trim()}`);
const got = (ran.stdout ?? "").trim().split("\n").pop();
say(`artifact: ${got}`);
if (got !== control) fail(`artifact printed '${got}', control printed '${control}'`);

// Nothing under the fresh cache root may be a compile extraction. Skipped for a
// containerised run, whose writes land in the container's own filesystem.
if (isSea && !args.get("run-with")) {
  const cache = join(home, "cache", "nub");
  const extracted = existsSync(cache) ? readdirSync(cache).filter((d) => d.startsWith("compile-app")) : [];
  if (extracted.length > 0) fail(`a single-executable application still extracted: ${extracted.join(", ")}`);
  say("extracted nothing");
}

console.log(`OK ${process.platform}-${process.arch}${args.get("platform") ? ` (${args.get("platform")})` : ""}`);
