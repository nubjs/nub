// Print each received argument on its own line, bracketed, so the test can see
// exactly how many args arrived and whether any were split/expanded by the shell.
for (const a of process.argv.slice(2)) {
  console.log(`[${a}]`);
}
