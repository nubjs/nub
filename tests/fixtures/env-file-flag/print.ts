// Prints the A19 env var so the test can assert how --env-file reaches the child
// (and that shell env wins). No .env in this dir, so nothing else perturbs it.
console.log("VAR=" + (process.env.A19 ?? "unset"));
