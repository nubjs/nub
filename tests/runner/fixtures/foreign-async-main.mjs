import { strict as assert } from 'node:assert';
import value from './foreign-target.cjs';

assert.equal(value.value, 42);
// Reported rather than asserted: whether imported CommonJS keeps a live
// `require.cache` is Node's own call and it changed across majors, so the
// harness scores this line against the same run with no preload instead of
// against a pinned value. The contract is that the two agree.
console.log(`foreign-loader-ok cacheType=${value.cacheType}`);
