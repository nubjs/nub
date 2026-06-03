// Float16Array + Math.f16round + DataView.{get,set}Float16. Native on Node 24+;
// on the 22.x floor these come from nub's @petamoriken/float16 polyfill (D5/A25).
// The test passes on both paths, locking the feature (incl. the TypedArray
// methods the old hand-rolled shim lacked) end-to-end through nub.
const a = new Float16Array([1.5, 2.5, 3.5]);
console.log("map:" + Array.from(a.map((x) => x * 2)).join(","));
console.log("filter:" + Array.from(a.filter((x) => x > 2)).join(","));
console.log("f16round:" + Math.f16round(1.5));

const dv = new DataView(new ArrayBuffer(2));
dv.setFloat16(0, 1.5, true);
console.log("dataview:" + dv.getFloat16(0, true));
