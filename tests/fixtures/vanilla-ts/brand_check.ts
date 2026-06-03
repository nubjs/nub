console.log("nub-global:" + (typeof (globalThis as any).nub));
console.log("nub-env:" + Object.keys(process.env).filter(k => k.startsWith("NUB_")).length);
