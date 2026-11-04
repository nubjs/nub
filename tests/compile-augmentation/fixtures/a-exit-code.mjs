// A compiled CLI's exit code is contract: a binary that returns 0 where the
// program returned 1 breaks every script that wraps it, silently. The launcher
// runs the user's Node as a CHILD, so forwarding the status is real work it
// could get wrong — and nothing else here would notice, since every other
// fixture exits 0 and a status of 0 is what a broken forwarder returns too.
//
// 7 rather than 1: an uncaught throw already produces 1, so 1 would still pass
// if the status were being invented rather than forwarded.
console.log("ok:exiting-with-7");
process.exit(7);
