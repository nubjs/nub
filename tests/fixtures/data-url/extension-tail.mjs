// A `data:` URL carries its payload INLINE, so the URL string can end in
// something extension-shaped that belongs to the SOURCE, not to a filename — a
// trailing comment, a `sourceMappingURL`, a path inside a string literal.
// Reading an extension off the whole URL sent nub's load hooks into the
// transpile/data branches, whose `fileURLToPath` then threw
// ERR_INVALID_URL_SCHEME on a module plain Node imports without complaint.
//
// Prints DATA-URL-OK when every tail loads and evaluates.
const tails = [".ts", ".tsx", ".jsx", ".mjs", ".cjs", ".yaml", ".json5", ".txt"];
const failures = [];

for (const tail of tails) {
  try {
    const mod = await import(`data:text/javascript,export default 1//x${tail}`);
    if (mod.default !== 1) failures.push(`${tail}: value=${mod.default}`);
  } catch (err) {
    failures.push(`${tail}: ${err.code ?? err.name}`);
  }
}

// A plain data: URL with no extension-shaped tail must keep working too.
try {
  const plain = await import("data:text/javascript,export default 2");
  if (plain.default !== 2) failures.push(`plain: value=${plain.default}`);
} catch (err) {
  failures.push(`plain: ${err.code ?? err.name}`);
}

console.log(failures.length === 0 ? "DATA-URL-OK" : `DATA-URL-FAIL ${failures.join(", ")}`);
