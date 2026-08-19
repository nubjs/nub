// Top-level side effect. Its POSITION in stdout is the whole assertion: under a
// working `import defer` it must appear AFTER the entry's "before-access" marker.
console.log("import-defer:dep-evaluated");

export const answer: number = 42;
