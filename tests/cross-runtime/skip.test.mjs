import { test } from "node:test";
import assert from "node:assert/strict";
import { classifySkip } from "./skip.mjs";

const guarded = `'use strict';
const common = require('../common');
if (!common.hasCrypto)
  common.skip('missing crypto');
if (!common.opensslCli)
  common.skip('node compiled without OpenSSL CLI.');
if (pair.skip) {
  common.printSkipMessage('Skipping unsupported test case');
}
common.printSkipMessage(\`Skipping unsupported \${file} test case\`);
common.printSkipMessage(
  'BoringSSL: skipping renegotiated ' +
  'client certificate verification case');
assert.ok(true);
`;

test("a terminating common.skip() is a whole-file skip", () => {
  assert.equal(classifySkip("1..0 # Skipped: missing crypto\n", guarded), "file");
  assert.equal(classifySkip("1..0 # Skipped: node compiled without OpenSSL CLI.\n", guarded), "file");
});

test("a skip announced from a helper the test requires is a whole-file skip", () => {
  const out = "(node:1) ExperimentalWarning: QUIC is experimental\n1..0 # Skipped: QUIC is not enabled\n";
  assert.equal(classifySkip(out, "'use strict';\nrequire('../common');\nrequire('../addons/x/test');\n"), "file");
});

test("a printSkipMessage() for one subcase leaves the file running", () => {
  assert.equal(classifySkip("1..0 # Skipped: Skipping unsupported test case\n", guarded), "partial");
  assert.equal(classifySkip("1..0 # Skipped: Skipping unsupported rsa_pss test case\n", guarded), "partial");
  assert.equal(classifySkip("1..0 # Skipped: BoringSSL: skipping renegotiated client certificate verification case\n", guarded), "partial");
});

test("output without the plan is not a skip", () => {
  assert.equal(classifySkip("ok 1 - runs\n1..1\n", guarded), null);
});
