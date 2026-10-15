const nativeUtil = await import("node:util");
const aliasUtil = await import("util");

const debounced = nativeUtil.debounce((value) => value, 1);
const throttled = aliasUtil.throttle((value) => value, 1, 1);
console.log((await Promise.all([debounced("debounce"), throttled("throttle")])).join(","));
