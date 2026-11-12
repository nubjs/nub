import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import test from "node:test";

const file = fileURLToPath(new URL("../npm/nub/platform.js", import.meta.url));

// Each case runs in its own process: it replaces process.platform and
// process.report, which must not leak into node:test or a sibling case.
function isolated(name: string, body: string): void {
  void test(name, () => {
    const script = `
      const assert = require("node:assert/strict");
      const { isMusl } = require(${JSON.stringify(file)});
      const asLinux = () => Object.defineProperty(process, "platform", { value: "linux", configurable: true });
      const fakeReport = (getReport, withExclude, initial) => {
        const seen = [];
        let exclude = initial;
        const report = { getReport() { seen.push(exclude); return getReport(); } };
        if (withExclude) {
          Object.defineProperty(report, "excludeNetwork", {
            get: () => exclude, set: (value) => { exclude = value; }, enumerable: true, configurable: true,
          });
        }
        Object.defineProperty(process, "report", { value: report, configurable: true });
        return { report, seen, current: () => exclude };
      };
      ${body}
    `;
    const result = spawnSync(process.execPath, ["-e", script], { encoding: "utf8", timeout: 10000 });
    assert.ifError(result.error);
    assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`);
  });
}

isolated("isMusl answers false off Linux without touching the report setting", `
  Object.defineProperty(process, "platform", { value: "darwin", configurable: true });
  const before = process.report.excludeNetwork;
  assert.equal(isMusl(), false);
  assert.equal(process.report.excludeNetwork, before);
`);

isolated("isMusl excludes network data while reading the report and restores the caller's setting", `
  asLinux();
  for (const initial of [false, true]) {
    const fake = fakeReport(() => ({ header: { glibcVersionRuntime: "2.39" } }), true, initial);
    assert.equal(isMusl(), false);
    assert.deepEqual(fake.seen, [true]);
    assert.equal(fake.current(), initial);
  }
`);

isolated("isMusl restores the report setting when getReport throws", `
  asLinux();
  const fake = fakeReport(() => { throw new Error("report failed"); }, true, false);
  isMusl();
  assert.deepEqual(fake.seen, [true]);
  assert.equal(fake.current(), false);
`);

isolated("isMusl adds no excludeNetwork setting on a Node that lacks one", `
  asLinux();
  const fake = fakeReport(() => ({ header: { glibcVersionRuntime: "2.17" } }), false, undefined);
  assert.equal(isMusl(), false);
  assert.equal(Object.hasOwn(fake.report, "excludeNetwork"), false);
`);

isolated("isMusl leaves the real report setting as it found it", `
  asLinux();
  for (const initial of [false, true]) {
    process.report.excludeNetwork = initial;
    isMusl();
    assert.equal(process.report.excludeNetwork, initial);
  }
`);
