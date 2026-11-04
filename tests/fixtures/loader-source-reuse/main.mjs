import assert from 'node:assert/strict';
import module, {createRequire} from 'node:module';
import {spawnSync} from 'node:child_process';
import {fileURLToPath} from 'node:url';
import * as core from '../../../runtime/transform-core.mjs';

core.setBootstrapCreateRequire(createRequire);
const url = new URL('./absent.mjs', import.meta.url).href;
const lowered = core.maybeTranspilePlainJs(url, '.mjs', 'using resource = null;');
assert.equal(lowered.format, 'module');
assert.match(lowered.source, /usingCtx/);
assert.equal(core.maybeTranspilePlainJs(url, '.mjs', ''), null);
assert.match(core.loadTranspile(url, '.mts', 'export const answer: number = 42;').source, /answer = 42/);

if (typeof module.registerHooks === 'function') {
  const hook = fileURLToPath(new URL('./early-hook.cjs', import.meta.url));
  const child = spawnSync(process.execPath, ['--input-type=module', '-e',
    'import {disposals} from "./state.mjs"; console.log(disposals);'], {
    cwd: fileURLToPath(new URL('.', import.meta.url)),
    env: {...process.env, NODE_OPTIONS: `--require=${JSON.stringify(hook)} ${process.env.NODE_OPTIONS || ''}`},
    encoding: 'utf8',
  });
  assert.equal(child.status, 0, child.stderr);
  assert.equal(child.stdout.trim(), '1');
}

// Separate native consumers must still share one module instance.
const imported = await import('./state.mjs');
assert.equal(imported.disposals, 1);
try {
  const required = createRequire(import.meta.url)('./state.mjs');
  imported.increment();
  assert.equal(required.value, 42);
  assert.equal(required.Token, imported.Token);
} catch (e) {
  if (e.code !== 'ERR_REQUIRE_ESM') throw e;
}
console.log('source-reuse:ok');
