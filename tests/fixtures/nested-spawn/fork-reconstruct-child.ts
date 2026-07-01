// Loading + running (the enum transpiles) proves the reconstructed single
// `--require` still carries nub's preload into the fork.
enum ReTag { Ok = "fork-reconstruct-child-ok" }
console.log(ReTag.Ok);
