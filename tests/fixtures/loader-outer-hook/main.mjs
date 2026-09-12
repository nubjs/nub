import assert from 'node:assert/strict';
import module from 'node:module';
import {spawnSync} from 'node:child_process';
import {fileURLToPath, pathToFileURL} from 'node:url';

// Each child registers a user hook set the way tsx would on this Node, appended
// to NODE_OPTIONS after nub's own preload — so its hooks run above nub's.
const here = new URL('.', import.meta.url);
const run = (nodeOptions, entry) => spawnSync(process.execPath, ['--input-type=module', '-e', `import ${JSON.stringify(entry)};`], {
  cwd: fileURLToPath(here),
  env: {...process.env, NODE_OPTIONS: `${process.env.NODE_OPTIONS || ''} ${nodeOptions}`},
  encoding: 'utf8',
});
const filePath = (name) => fileURLToPath(new URL(name, here));
const hasSyncHooks = typeof module.registerHooks === 'function';

// Whether this Node hands one sync hook's load result to the hook above it as
// is. Through 22.21, 24.11.0 and 25.0 the chain rebuilt `{ format, source }`
// between hooks and dropped `responseURL`, so an outer hook has nothing to see
// there whatever nub returns. Measured in-process: the pair below never reaches
// nub's hooks or the disk.
let passesResponseUrl = false;
if (hasSyncHooks) {
  const target = 'file:///outer-hook-probe/probe.mjs';
  module.registerHooks({
    resolve: (specifier, context, next) => specifier === 'outer-hook-probe' ? {url: target, shortCircuit: true} : next(specifier, context),
    load: (url, context, next) => url === target ? {format: 'module', source: 'export {}', responseURL: url, shortCircuit: true} : next(url, context),
  });
  module.registerHooks({
    load: (url, context, next) => {
      const loaded = next(url, context);
      if (url === target) passesResponseUrl = loaded.responseURL === url;
      return loaded;
    },
  });
  await import('outer-hook-probe');
}

// An outer LOAD hook keys on the responseURL nub's transpiled result carries.
if (passesResponseUrl) {
  const child = run(`--require=${filePath('outer-hook.cjs')}`, './cjs/entry.ts');
  assert.equal(child.status, 0, child.stderr);
  assert.match(child.stdout, /^cjs-ok function$/m);
}

// An outer RESOLVE hook that assigns a `.ts` file a bare `commonjs` format owns
// its transform: nub hands its load hook the raw source, and on the compat tier
// leaves the `require.extensions` handler it installed first in place.
const owner = hasSyncHooks
  ? `--require=${filePath('owner-hook.cjs')}`
  : `--require=${filePath('owner-cjs.cjs')} --import=${pathToFileURL(filePath('owner-register.mjs')).href}`;
const child = run(owner, './cjs/mixed.ts');
assert.equal(child.status, 0, child.stderr);
assert.match(child.stdout, /^mixed-ok function function 1$/m);

// The same label from a hook WITHOUT a load hook leaves nub as the transformer.
// Sync hooks only: a loader worker cannot be inspected for what it registered.
if (hasSyncHooks) {
  const child = run(`--require=${filePath('resolve-only-hook.cjs')}`, './cjs/typed.ts');
  assert.equal(child.status, 0, child.stderr);
  assert.match(child.stdout, /^typed-ok 1$/m);
}

// A user transpiler's `require.extensions['.ts']` also widens resolution inside
// dependencies, as it does under plain Node; nub's dependency retry keeps it —
// whether the handler was there before nub's preload or replaced nub's later.
for (const registration of [`--require=${filePath('owner-cjs.cjs')}`, `--import=${pathToFileURL(filePath('owner-late.mjs')).href}`]) {
  const child = run(registration, './cjs/dep-check.cjs');
  assert.equal(child.status, 0, `${registration}\n${child.stderr}`);
  assert.match(child.stdout, /^dep-ok 42$/m);
}

// The same for a key nub wraps rather than replaces: a user's `.cjs` handler.
{
  const child = run(`--require=${filePath('owner-cjs-ext.cjs')}`, './cjs/dep-check-cjs.cjs');
  assert.equal(child.status, 0, child.stderr);
  assert.match(child.stdout, /^dep-cjs-ok 7$/m);
}

console.log(`outer-hook:ok responseURL-check=${passesResponseUrl ? 'ran' : 'skipped'}`);
