// Compile `app.mjs` with a freshly built nub and prove the result is a Node
// single-executable application that still carries nub's augmentation.
//
// Written as one Node script rather than per-OS shell so the five runners assert
// exactly the same things. The only per-OS knob is `--docker`, for a musl
// artifact that has to execute inside an Alpine container.
//
// usage:
//   node probe.mjs --nub <path> --out <path> [--platform <target>]
//                  [--docker alpine:3.20]
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
// Windows will not execute a file whose name has no recognized extension, and it
// fails as a spawn error rather than a non-zero exit — so the artifact is asked
// for under the name the target can actually run.
const wantsExe = (args.get("platform") ?? process.platform).startsWith("win32");
let out = resolve(args.get("out")) + (wantsExe ? ".exe" : "");
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
if (args.get("docker")) {
  // `sh -c '… exec "$0" "$@"' <bin> a b` puts the artifact in $0 and its
  // arguments in $@, which is the only shape that survives without quoting the
  // path. The `apk add` is not incidental: a musl Node links against
  // `libgcc_s.so.1`, which a bare Alpine image does not ship, so the artifact
  // dies at relocation with exit 127 and says nothing about nub.
  const dir = dirname(out);
  cmd = "docker";
  argv = ["run", "--rm", "-e", "NUB_SEA_PROBE=live", "-v", `${dir}:${dir}`, "-w", dir,
    args.get("docker"), "/bin/sh", "-c",
    'apk add --no-cache libgcc libstdc++ >/dev/null 2>&1 || true; exec "$0" "$@"', out, "a", "b"];
}
// The fresh HOME isolates the ARTIFACT's cache, so it is wrong for the docker
// CLI, which reads its context out of `$HOME/.docker` and otherwise cannot find
// the daemon. Inside the container the artifact gets the container's own HOME.
const ran = spawnSync(cmd, argv, { encoding: "utf8", env: args.get("docker") ? env : runEnv });
process.stderr.write(ran.stderr ?? "");
if (ran.error) fail(`could not run ${cmd}: ${ran.error.message}`);
if (ran.status !== 0) {
  fail(`the artifact exited ${ran.status}${ran.signal ? ` on ${ran.signal}` : ""}: ${(ran.stdout ?? "").trim()}`);
}
const got = (ran.stdout ?? "").trim().split("\n").pop();
say(`artifact: ${got}`);

// A cross-target artifact cannot match the control field for field: it reports
// its own platform, and a path separator follows that platform. So the fields
// that must be identical are compared by name, and the platform is compared
// against the target that was asked for — which is the stronger assertion
// anyway, since it catches a build that resolved the wrong triple.
const fields = (line) => Object.fromEntries(line.split(" ").map((p) => {
  const at = p.indexOf(":");
  return [p.slice(0, at), p.slice(at + 1)];
}));
const wanted = fields(control);
const carried = fields(got);
const shared = args.get("platform") ? ["argv", "worker", "env"] : Object.keys(wanted);
for (const key of shared) {
  if (carried[key] !== wanted[key]) {
    fail(`artifact reported ${key}:${carried[key]}, control reported ${key}:${wanted[key]}`);
  }
}
if (args.get("platform")) {
  const expected = args.get("platform").replace(/-musl$/, "");
  if (carried.platform !== expected) {
    fail(`artifact reports platform ${carried.platform}, but was built for ${expected}`);
  }
}

// Nothing under the fresh cache root may be a compile extraction. Skipped for a
// containerised run, whose writes land in the container's own filesystem.
if (isSea && !args.get("docker")) {
  const cache = join(home, "cache", "nub");
  const extracted = existsSync(cache) ? readdirSync(cache).filter((d) => d.startsWith("compile-app")) : [];
  if (extracted.length > 0) fail(`a single-executable application still extracted: ${extracted.join(", ")}`);
  say("extracted nothing");
}

console.log(`OK ${process.platform}-${process.arch}${args.get("platform") ? ` (${args.get("platform")})` : ""}`);
