// RegExp.escape is native on Node 24+ and polyfilled on the 22.x floor; both must
// produce byte-identical output. Covers the cases the reduced-fidelity polyfill
// got wrong: a leading digit/letter, whitespace, and "other punctuators".
const cases = ["a.b*c", "0xff", "a b\tc", "a,b-c", ".*+?()[]{}|^$\\", "😀x"];
for (const c of cases) console.log(JSON.stringify(RegExp.escape(c)));
console.log("type:" + typeof RegExp.escape);
