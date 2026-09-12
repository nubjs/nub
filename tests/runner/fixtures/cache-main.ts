import { strict as assert } from 'node:assert';
import cache from './cache.cjs';

assert.equal(cache, 'cjs-cache-ok');
assert.equal((await import('./cache.cjs')).default, cache);
console.log(cache);
