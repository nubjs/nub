const assert = require('node:assert/strict');

assert.equal(require.cache[__filename], module);
assert.equal(typeof require.extensions['.js'], 'function');
assert.ok(Array.isArray(require.resolve.paths('not-installed')));
module.exports = 'cjs-cache-ok';
