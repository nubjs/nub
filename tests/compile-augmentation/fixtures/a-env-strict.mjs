// Under `nub` the .env beside the entry loads; a compiled artifact must ignore
// it, because a baked program's configuration cannot depend on the directory it
// happens to be started in. The two DIFFER on purpose — see the strict
// compilation policy in wiki/design/compiled-executables.md.
//
// Nothing here probes for the .env itself: `new URL("./.env", import.meta.url)`
// is the very expression the bundler turns into an embedded asset, so asking
// whether the file is there would put it in the payload and destroy what is
// being tested.
console.log("ok:dotenv=" + (process.env.FROM_DOTENV ?? "unset"));
