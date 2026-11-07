// Only these implemented language APIs may enter this bounded conformance job.
// Adding a feature requires an explicit code review here as well as config.json.
export const implementedSuites = Object.freeze([
  "test/built-ins/Math/sumPrecise",
  "test/intl402/Locale/prototype/variants",
  "test/intl402/Locale/constructor-getter-order.js",
  "test/intl402/Locale/constructor-locale-object.js",
  "test/intl402/Locale/constructor-newtarget-undefined.js",
  "test/intl402/Locale/constructor-options-canonicalized.js",
  "test/intl402/Locale/constructor-options-variants-invalid.js",
  "test/intl402/Locale/constructor-options-variants-valid.js",
  "test/intl402/Locale/constructor-tag.js",
  "test/intl402/Locale/constructor-tag-tostring.js",
  "test/intl402/Locale/function-prototype.js",
  "test/intl402/Locale/getters-missing.js",
  "test/intl402/Locale/instance.js",
  "test/intl402/Locale/instance-extensibility.js",
  "test/intl402/Locale/invalid-tag-throws.js",
  "test/intl402/Locale/length.js",
  "test/intl402/Locale/name.js",
  "test/intl402/Locale/prop-desc.js",
  "test/intl402/Locale/proto-from-ctor-realm.js",
  "test/intl402/Locale/prototype/constructor/prop-desc.js",
  "test/intl402/Locale/reject-duplicate-variants.js",
  "test/intl402/Locale/reject-duplicate-variants-in-tlang.js",
  "test/intl402/Locale/subclassing.js",
  "test/built-ins/Iterator/zip",
  "test/built-ins/Iterator/zipKeyed",
  "test/built-ins/Iterator/prototype/join",
  "test/built-ins/Iterator/prototype/includes",
  "test/built-ins/Iterator/prototype/take",
  "test/built-ins/Iterator/prototype/drop",
  "test/built-ins/Iterator/prototype/chunks",
  "test/built-ins/Iterator/prototype/windows",
  "test/built-ins/Promise/try",
  "test/built-ins/AsyncIteratorPrototype/Symbol.asyncDispose"
]);

export function validateSelection(config) {
  if (!/^[a-f0-9]{40}$/.test(config.revision)) throw new Error("Test262 requires an exact commit pin");
  if (!/^\d+\.\d+\.\d+$/.test(config.harnessVersion)) throw new Error("Harness requires an exact release pin");
  if (!Array.isArray(config.suites) || config.suites.length === 0) throw new Error("Empty Test262 selection");
  const seen = new Set();
  for (const suite of config.suites) {
    if (!implementedSuites.includes(suite)) throw new Error(`Unimplemented or broad Test262 suite: ${suite}`);
    if (seen.has(suite)) throw new Error(`Duplicate Test262 suite: ${suite}`);
    seen.add(suite);
  }
  for (const name of ["expectedFiles", "expectedScenarios"]) {
    if (!Number.isSafeInteger(config[name]) || config[name] <= 0 || config[name] > 2000) {
      throw new Error(`${name} exceeds the focused Test262 budget (1..2000)`);
    }
  }
}

export function parseRunnerOptions(args, parallelism) {
  const options = { workers: Math.min(Math.max(1, parallelism), 4), shard: { index: 1, count: 1 }, native: false, list: false };
  const seen = new Set();
  for (let i = 0; i < args.length; i++) {
    const name = args[i];
    if (seen.has(name)) throw new Error(`Duplicate argument: ${name}`);
    seen.add(name);
    if (name === "--native" || name === "--list") {
      options[name.slice(2)] = true;
      continue;
    }
    if (!["--checkout", "--node", "--harness", "--workers", "--shard"].includes(name)) {
      throw new Error(`Unknown argument: ${name}`);
    }
    const value = args[++i];
    if (!value || value.startsWith("--")) throw new Error(`${name} needs a value`);
    if (name === "--workers") {
      if (!/^[1-9]\d*$/.test(value) || Number(value) > 32) throw new Error("--workers must be an integer in 1..32");
      options.workers = Number(value);
    } else if (name === "--shard") {
      const match = /^([1-9]\d*)\/([1-9]\d*)$/.exec(value);
      if (!match) throw new Error("--shard must be index/count with positive integers");
      const index = Number(match[1]), count = Number(match[2]);
      if (!Number.isSafeInteger(index) || !Number.isSafeInteger(count) || index > count) {
        throw new Error("--shard index must be in 1..count");
      }
      options.shard = { index, count };
    } else {
      options[name.slice(2)] = value;
    }
  }
  return options;
}

export function selectShard(files, shard, expectedScenarios) {
  if (files.length === 0 || new Set(files).size !== files.length) throw new Error("Test262 file selection must be nonempty and unique");
  const { index, count } = shard;
  if (!Number.isSafeInteger(index) || !Number.isSafeInteger(count) || index < 1 || count < 1 || index > count || count > files.length) {
    throw new Error("--shard must select a nonempty partition of the available files");
  }
  // At the pinned revision every selected file has default and strict scenarios.
  // A pin update which changes that invariant must update this count policy too.
  if (expectedScenarios !== files.length * 2) throw new Error("Pinned scenario count must be two per selected file");
  const selected = [...files].sort().filter((_, position) => position % count === index - 1);
  return { files: selected, expectedScenarios: selected.length * 2 };
}

export function inspectHarnessResult(result, expectedScenarios, selectedFiles, knownFailures, nodeMajor, native) {
  const output = result.stdout || "";
  const summary = /Ran (\d+) tests\s+(\d+) passed\s+(\d+) failed/.exec(output);
  const known = native ? [] : knownFailures
    .filter((entry) => entry.nodeMajors.includes(nodeMajor))
    .flatMap((entry) => entry.files)
    .filter((file) => selectedFiles.includes(file));
  const failed = [...output.matchAll(/^FAIL (\S+\.js) \(/gm)].map((match) => match[1]);
  const unexpected = failed.filter((file) => !known.some((entry) => file.endsWith(entry)));
  const stale = !summary ? [] : known.filter((entry) => !failed.some((file) => file.endsWith(entry)));
  const errors = [];
  if (result.status !== 0 || result.signal !== null) {
    errors.push(`Test262 harness exited unsuccessfully (status ${result.status}, signal ${result.signal})`);
  }
  if (!summary) errors.push("Test262 harness did not print a complete summary");
  else if (+summary[1] !== expectedScenarios || +summary[3] !== failed.length) {
    errors.push(`Ran ${summary[1]} of ${expectedScenarios} expected scenarios; ${summary[3]} failed`);
  }
  for (const file of unexpected) errors.push(`Unexpected failure: ${file}`);
  for (const file of stale) errors.push(`Listed in knownFailures for Node ${nodeMajor}, but passed: ${file}`);
  return { errors, failed, summary };
}
