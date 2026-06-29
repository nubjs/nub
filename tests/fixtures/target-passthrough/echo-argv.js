// Echoes the args this process received (process.argv past argv[0]/argv[1]) so a
// passthrough test can assert exactly what a runner forwarded. Dual sink: when
// ARGV_OUT is set (the long-running `nub watch` case, whose own control output
// races stdout) it writes a sentinel file; otherwise it prints to stdout. One
// fixture serves the file runner, `nub run`, `nub exec`, and `nub watch`.
const line = "ARGV:" + JSON.stringify(process.argv.slice(2));
if (process.env.ARGV_OUT) require("fs").writeFileSync(process.env.ARGV_OUT, line);
else console.log(line);
