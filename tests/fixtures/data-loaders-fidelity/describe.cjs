// A rendering JSON.stringify cannot give: it keeps NaN, -0, BigInt, Date and an
// own `__proto__` key distinct, which are exactly the values under test.
exports.describe = function describe(v) {
  if (v === null) return "null";
  switch (typeof v) {
    case "number": return "n:" + (Object.is(v, -0) ? "-0" : String(v));
    case "bigint": return "b:" + v;
    case "string": return "s:" + JSON.stringify(v);
    case "boolean": return String(v);
  }
  if (v instanceof Date) return "d:" + v.toISOString();
  if (Array.isArray(v)) return "[" + v.map(describe).join(",") + "]";
  return "{" + Reflect.ownKeys(v).map((k) => JSON.stringify(k) + ":" + describe(v[k])).join(",") + "}";
};
