// YAML → TypeScript source with a span map, for the content mapper.
//
// The emitted module is `export default <object literal>;` whose literal types are
// what `yaml`'s `parse()` returns at runtime, so the type the checker infers for
// `import config from "./config.yaml"` is the shape of the actual file. Every key
// and scalar carries a span back to its YAML token so diagnostics, hover, and
// go-to-definition land in the YAML file. Positions are UTF-16 code units — the
// `yaml` package's offsets are JS string indices, which is what TypeScript's
// "utf-16" position encoding means.
import { isAlias, isMap, isPair, isScalar, isSeq, parseDocument, stringify } from "yaml";

const flowText = (node) => {
  const clone = node.clone();
  clone.flow = true;
  return stringify(clone).trimEnd();
};

/** SpanMapKind from the protocol: Verbatim = same text, Atom = corresponding token. */
const VERBATIM = 0;
const ATOM = 1;

/** Diagnostic code for a YAML parse error; the source is reported as "yaml". */
export const PARSE_ERROR_CODE = 1;

/**
 * @param {string} content
 * @returns {{ text: string, mappings: number[][], diagnostics: { messageText: string, start: number, length: number, code: number }[] }}
 */
export function emit(content) {
  const doc = parseDocument(content, { keepSourceTokens: false });
  const out = [];
  const mappings = [];
  let pos = 0;
  const push = (s) => {
    out.push(s);
    pos += s.length;
  };
  const map = (node, text) => {
    const range = node?.range;
    if (!range) return push(text);
    const [start, end] = range;
    const original = content.slice(start, end);
    mappings.push([pos, text.length, start, end - start, original === text ? VERBATIM : ATOM]);
    push(text);
  };

  const scalar = (value) => {
    if (value === null || value === undefined) return "null";
    if (typeof value === "number") return Object.is(value, -0) ? "-0" : String(value);
    if (typeof value === "bigint") return `${value}n`;
    if (value instanceof Date) return `new Date(${JSON.stringify(value.toISOString())})`;
    return JSON.stringify(value);
  };

  const visit = (node, indent) => {
    if (isAlias(node)) return visit(node.resolve(doc), indent);
    if (isScalar(node)) return map(node, scalar(node.value));
    const pad = "  ".repeat(indent + 1);
    if (isSeq(node)) {
      if (node.items.length === 0) return push("[]");
      push("[\n");
      for (const item of node.items) {
        push(pad);
        visit(item, indent + 1);
        push(",\n");
      }
      return push(`${"  ".repeat(indent)}]`);
    }
    if (isMap(node)) {
      if (node.items.length === 0) return push("{}");
      push("{\n");
      for (const pair of node.items) {
        if (!isPair(pair)) continue;
        push(pad);
        // Same coercion `parse()` applies to a plain-object target: a scalar key is
        // its string form (null → ""), a collection key is its YAML flow text
        // ("[ a, b ]"). A node's bare toString() is JSON, so stringify a flow clone.
        const key = isScalar(pair.key) ? String(pair.key.value ?? "") : flowText(pair.key);
        // A quoted `"__proto__":` in an object literal sets the prototype; the
        // computed form defines an own property, which is what `parse()` returns.
        map(pair.key, key === "__proto__" ? '["__proto__"]' : JSON.stringify(key));
        push(": ");
        if (pair.value == null) push("null");
        else visit(pair.value, indent + 1);
        push(",\n");
      }
      return push(`${"  ".repeat(indent)}}`);
    }
    // A node kind this emitter does not model: fall back to its JS value, unmapped.
    push(scalar(node?.toJSON?.() ?? null));
  };

  push("export default ");
  if (doc.contents == null) push("undefined");
  else visit(doc.contents, 0);
  push(";\n");

  // TypeScript rejects the whole transform when a range runs past the file, and
  // the yaml package reports an unterminated construct at end-of-input.
  const diagnostics = doc.errors.map((error) => {
    const [rawStart, rawEnd] = error.pos ?? [0, 0];
    const start = Math.min(Math.max(rawStart, 0), content.length);
    const length = Math.min(Math.max(rawEnd - start, 0), content.length - start);
    return { messageText: error.message, start, length, code: PARSE_ERROR_CODE };
  });
  return { text: out.join(""), mappings, diagnostics };
}

/**
 * The module source the runtime hook and the bundler plugin evaluate. The same
 * emitter as the mapper, so `import x from "./x.yaml"` yields one value on every
 * path — `JSON.stringify` would turn `.nan`/`.inf` into `null` and `-0.0` into `0`.
 * Throws on a malformed document, as `parse()` does.
 * @param {string} content
 * @returns {string}
 */
export function toModule(content) {
  const { text, diagnostics } = emit(content);
  if (diagnostics.length > 0) throw new SyntaxError(diagnostics[0].messageText);
  return text;
}
