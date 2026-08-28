// Regression: nub wraps URL.revokeObjectURL to drop its captured blob source.
// The wrapper used to call the native function with a fixed arity of one, so a
// zero-arg call passed `undefined` through and Node's own ERR_MISSING_ARGS
// check never fired — nub silently returned where vanilla Node throws.
try {
  (URL.revokeObjectURL as () => void)();
  console.log("revoke-no-args:no-throw");
} catch (e) {
  console.log("revoke-no-args:" + (e as { code?: string }).code);
}

// The ordinary one-arg path must still revoke.
const url = URL.createObjectURL(new Blob(["hello"]));
URL.revokeObjectURL(url);
console.log("revoke-one-arg:ok");
