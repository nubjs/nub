// Stamp one version across every version surface: the 10 npm package.jsons
// (root optionalDependencies pinned exact), both workspace Cargo.tomls, and
// runtime/version.mjs (NUB_VERSION — the transpile-cache key, which must stay in
// lockstep with the binary version or a stale cache would serve stale output
// after an upgrade). Shared by `make version V=<v>` (committed release bumps —
// the Makefile also refreshes the ROOT Cargo.lock through cargo) and release.yml's
// canary stamp (an UNCOMMITTED prerelease set on the build runners, where `make`
// isn't dependable on Windows). The two out-of-workspace lockfiles are stamped
// here, by BOTH paths, because they are consumed under `--locked` — see the note
// beside them below; only the root lock self-heals on the next cargo invocation.
// Run from the repo root: node scripts/set-version.mjs <version>
import fs from "node:fs";

const v = process.argv[2];
if (!v) {
  console.error("Usage: node scripts/set-version.mjs <version>");
  process.exit(1);
}

// Prepare the schema snapshot before touching any version surface. A release
// bump must not stamp packages and Cargo manifests when its public schema
// cannot be read or its destination cannot be prepared.
//
// An ABSENT latest.json is not that failure: the site withdraws the whole
// published schema whenever the nub.jsonc config reference is hidden pending
// ship (3539b65db1), and there is then nothing to snapshot. Only a latest.json
// that exists but cannot be read or parsed still aborts the bump — deliberate
// withdrawal removes the file, accidental damage leaves it there.
const schemaDir = "site/public/schema";
const pinned = `v${v.split(".").slice(0, 2).join(".")}.json`;
const latestPath = `${schemaDir}/latest.json`;
let schemaSnapshot;
if (fs.existsSync(latestPath)) {
  try {
    fs.mkdirSync(schemaDir, { recursive: true });
    const schema = JSON.parse(fs.readFileSync(latestPath, "utf8"));
    schema.$id = `https://nubjs.com/schema/${pinned}`;
    schemaSnapshot = JSON.stringify(schema, null, 2) + "\n";
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    console.error(`ERROR: cannot prepare schema snapshot: ${message}`);
    process.exit(1);
  }
}

const pkgs = [
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
for (const f of pkgs) {
  const p = JSON.parse(fs.readFileSync(f, "utf8"));
  p.version = v;
  if (p.optionalDependencies) {
    for (const k of Object.keys(p.optionalDependencies)) p.optionalDependencies[k] = v;
  }
  fs.writeFileSync(f, JSON.stringify(p, null, 2) + "\n");
}

const replaceOrDie = (file, re, replacement) => {
  const src = fs.readFileSync(file, "utf8");
  const next = src.replace(re, replacement);
  if (next === src) {
    console.error(`ERROR: version line not found in ${file}`);
    process.exit(1);
  }
  fs.writeFileSync(file, next);
};
replaceOrDie("Cargo.toml", /^version = .*/m, `version = "${v}"`);
replaceOrDie("crates/nub-native/Cargo.toml", /^version = .*/m, `version = "${v}"`);
// nub-core inlines its version instead of inheriting: it is a cross-workspace path
// dependency of crates/nub-launcher, whose release `cross` build mounts only that
// workspace, so a `.workspace = true` field here fails to parse (the #132 failure).
// The inlining is deliberate; keeping it in lockstep is this line's job. It is also
// load-bearing beyond packaging — spawn.rs hands `env!("CARGO_PKG_VERSION")` to the
// preload as `process.versions.nub`, so a stale value here silently makes the runtime
// misreport the binary's version, which is exactly how it was caught.
replaceOrDie("crates/nub-core/Cargo.toml", /^version = .*/m, `version = "${v}"`);
replaceOrDie(
  "runtime/version.mjs",
  /export const NUB_VERSION = .*/,
  `export const NUB_VERSION = "${v}";`,
);

// crates/nub-launcher and crates/nub-native are their OWN workspaces, so each
// carries its own Cargo.lock recording the version of a crate stamped above —
// the launcher's records nub-core, the addon's records itself. Stamping a
// manifest without these leaves that lock unsatisfiable, and both are built with
// `--locked`, which refuses to reconcile rather than doing it. Not hypothetical:
// release.yml's canary path stamps a version on the build runners and then builds
// the launcher `--locked` in the SAME job, so every push to main would fail the
// build on all eight targets. The root lock is not in this list because nothing
// consumes it under `--locked`; it self-heals on the next cargo invocation.
//
// Rewritten here rather than through `cargo update` because BOTH entry points
// reach this file while only `make version` can rely on cargo and a network, and
// these are path dependencies whose lock entry is a plain version string.
// `\r?\n`, not `\n`: this is the only MULTI-LINE pattern in the file, and the
// GitHub runner image ships `core.autocrlf=true` (see the mitigation in
// aube-parity.yml, and #290). Neither lock is normalized by .gitattributes, so
// on the two windows-latest release shards they check out CRLF — and a literal
// `\n` matches only 0x0A, so the stamp would exit 1 before either shard built
// anything. The single-line patterns above are CRLF-transparent, because `.*`
// absorbs the trailing `\r` and the replacement drops it.
const lockVersionOf = (crate) =>
  new RegExp(`(\\[\\[package\\]\\]\\r?\\nname = "${crate}"\\r?\\nversion = )"[^"]*"`);
replaceOrDie("crates/nub-launcher/Cargo.lock", lockVersionOf("nub-core"), `$1"${v}"`);
replaceOrDie("crates/nub-native/Cargo.lock", lockVersionOf("nub-native"), `$1"${v}"`);

// Freeze a copy of the nub.jsonc schema at this release. `latest.json` keeps
// tracking the newest release; a versioned file is what a project pins when it
// wants the schema to stop changing under it. Snapshotting on the version bump,
// rather than at tag time, keeps the published set in step with the version the
// tree claims — /schema lists whatever is on disk, so a gap would be visible.
// Patch releases reuse the minor's file: the schema describes a field surface,
// which does not move in a patch.
if (schemaSnapshot === undefined) {
  console.log(`· no published schema (${latestPath} absent) — snapshot skipped`);
} else {
  fs.writeFileSync(`${schemaDir}/${pinned}`, schemaSnapshot);
  console.log(`✓ schema snapshot ${schemaDir}/${pinned}`);
}

console.log(`✓ npm packages, Cargo manifests, and runtime/version.mjs set to ${v}`);
