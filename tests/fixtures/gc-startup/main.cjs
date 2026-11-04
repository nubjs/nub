const assert = require('node:assert/strict');
const { Worker } = require('node:worker_threads');
const { fork } = require('node:child_process');
const { getHeapStatistics } = require('node:v8');

const workerSource = `
  const { parentPort, resourceLimits } = require('node:worker_threads');
  parentPort.postMessage({
    heap: require('node:v8').getHeapStatistics().heap_size_limit / 2 ** 20,
    limits: resourceLimits,
    argv: process.execArgv,
  });
`;
function message(child) {
  return new Promise((resolve, reject) => {
    child.once('message', resolve);
    child.once('error', reject);
    child.once('exit', code => reject(new Error(`child exited before its message: ${code}`)));
  });
}
async function workers() {
  const results = [];
  for (const extra of [{}, { execArgv: [] }, { execArgv: process.execArgv }, { env: {} }]) {
    // Explicit user V8 flags stay visible, and Node rejects forwarding them.
    if (extra.execArgv?.includes('--max-semi-space-size=4')) {
      assert.throws(() => new Worker(workerSource, { eval: true, ...extra }),
        { code: 'ERR_WORKER_INVALID_EXEC_ARGV' });
      continue;
    }
    const worker = new Worker(workerSource, {
      eval: true,
      resourceLimits: { maxYoungGenerationSizeMb: 8, maxOldGenerationSizeMb: 128 },
      ...extra,
    });
    const result = await message(worker);
    assert.equal(result.heap, 140);
    assert.equal(result.limits.maxYoungGenerationSizeMb, 8);
    assert.equal(result.limits.maxOldGenerationSizeMb, 128);
    assert(!result.argv.includes('--max-semi-space-size=16'), JSON.stringify({ extra, result }));
    assert(!result.argv.some(arg => arg.includes('gc-startup.cjs')), JSON.stringify({ extra, result }));
    results.push(result);
  }
  return results;
}
async function main() {
  assert.equal(process.env.__NUB_GC_STARTUP, undefined);
  const result = {
    node: process.version,
    mainHeap: getHeapStatistics().heap_size_limit / 2 ** 20,
    argv: process.execArgv,
    workers: await workers(),
  };
  assert(!result.argv.includes('--max-semi-space-size=16'));
  if (process.argv[2] === 'child') {
    process.send(result);
    return;
  }
  result.fork = await message(fork(__filename, ['child'], { stdio: ['ignore', 'ignore', 'inherit', 'ipc'] }));
  console.log(JSON.stringify(result));
}
main().catch(error => { console.error(error); process.exitCode = 1; });
