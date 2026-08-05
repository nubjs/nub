// Under `nub` the .env beside the entry loads; a compiled artifact must ignore
// it, because a baked program's configuration cannot depend on the directory it
// happens to be started in. The two DIFFER on purpose — see the strict
// compilation policy in wiki/design/compiled-executables.md.
console.log("ok:dotenv=" + (process.env.FROM_DOTENV ?? "unset"));
