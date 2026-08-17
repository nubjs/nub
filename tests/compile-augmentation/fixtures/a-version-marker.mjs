// `process.versions.nub` says "nub's augmentation is active in THIS process" —
// that is the contract the runtime's own tests pin, and why `--node` and
// NODE_COMPAT both withhold it. A compiled artifact withholds it too, and the
// row below is expected to differ for that reason.
//
// It is a real fork rather than an oversight, so it is written down. The
// artifact's augmentations genuinely ARE active — the other fixtures here prove
// 22 polyfilled globals on Node 22 — so a library feature-gating on the marker
// takes the plain-Node branch inside a compiled binary and gives up capability
// that is present. The other reading wins anyway: a compiled artifact is a
// standalone program on stock Node, not a `nub` process, and the marker exists
// for detecting the latter. Anything gating on it should feature-detect the
// capability instead, which works identically either way.
console.log("ok:marker=" + (process.versions.nub ?? "absent"));
