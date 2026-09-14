const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { createHash } = require('node:crypto');
const { getHeapStatistics } = require('node:v8');

async function main() {
  const retained = Array.from({ length: 350000 }, (_, i) => ({
    id: i, name: `retained-${i}`, tags: [i, i + 1, i + 2], payload: 'fixed-'.repeat(10) + i,
  }));
  const text = JSON.stringify(Array.from({ length: 150 }, (_, i) => ({
    id: i, name: `item-${i}`, tags: ['green', 'large', 'available'], price: i * 1.15,
    nested: { x: i, y: 'payload'.repeat(10) },
  })));
  const ring = Array(64);
  const hash = createHash('sha256');
  for (let n = 0; n < 8000; n++) {
    const items = JSON.parse(text);
    for (const item of items) item.price *= 1.1;
    hash.update(JSON.stringify(items));
    ring[n % ring.length] = items;
    if (n % 20 === 0) await new Promise(resolve => setImmediate(resolve));
  }
  globalThis.retained = retained;
  const membership = readFileSync('/proc/self/cgroup', 'utf8').split('\n').find(x => x.startsWith('0::')).slice(3);
  const directory = '/sys/fs/cgroup' + membership;
  const budgetMiB = Number(process.argv[2] || 512);
  assert.equal(readFileSync(directory + '/memory.max', 'utf8').trim(), String(budgetMiB * 2 ** 20));
  const events = Object.fromEntries(readFileSync(directory + '/memory.events', 'utf8').trim().split('\n').map(line => line.split(' ')));
  assert.equal(events.oom, '0');
  assert.equal(events.oom_kill, '0');
  console.log(JSON.stringify({
    node: process.version, mainHeap: getHeapStatistics().heap_size_limit / 2 ** 20,
    checksum: hash.digest('hex'), peakRssMiB: process.resourceUsage().maxRSS / 1024, memoryEvents: events,
  }));
}
main().catch(error => { console.error(error); process.exitCode = 1; });
