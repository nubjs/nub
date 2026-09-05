import { strict as assert } from 'node:assert';
import value from './foreign-target.cjs';

assert.equal(value.value, 42);
assert.equal(value.cacheType, 'undefined');
console.log('foreign-loader-ok');
