#!/usr/bin/env node
// Independent RT_VERSION reader. Deliberately shares NO code with nub: nub's own
// find_version_resource now runs inside `verify_artifact`, so a compile that
// succeeds has already been checked by nub's parser. Using that same parser to
// confirm the result would only prove it agrees with itself.
//
// Usage: node read-versioninfo.mjs <file.exe>
import { readFileSync } from "node:fs";

const buf = readFileSync(process.argv[2] ?? (() => { throw new Error("usage: read-versioninfo.mjs <exe>"); })());
const u16 = (o) => buf.readUInt16LE(o);
const u32 = (o) => buf.readUInt32LE(o);

if (buf.readUInt16LE(0) !== 0x5a4d) throw new Error("not a PE: no MZ");
const pe = u32(0x3c);
if (buf.toString("ascii", pe, pe + 4) !== "PE\0\0") throw new Error("not a PE: no signature");
const nsec = u16(pe + 6);
const optSize = u16(pe + 20);
const opt = pe + 24;
const magic = u16(opt);
// Data directories sit after the optional header's fixed fields: 96 bytes for
// PE32, 112 for PE32+ (the extra 16 are the four fields widened to 64-bit).
const dirs = magic === 0x20b ? opt + 112 : magic === 0x10b ? opt + 96 : (() => { throw new Error(`bad optional-header magic 0x${magic.toString(16)}`); })();
const rsrcRva = u32(dirs + 2 * 8);
if (!rsrcRva) { console.log(JSON.stringify({ present: false })); process.exit(0); }

const secs = [];
for (let i = 0, s = opt + optSize; i < nsec; i++, s += 40) {
  secs.push({ va: u32(s + 12), vsize: u32(s + 8), raw: u32(s + 20), rsize: u32(s + 16) });
}
const toOff = (rva) => {
  for (const s of secs) if (rva >= s.va && rva < s.va + Math.max(s.vsize, s.rsize)) return s.raw + (rva - s.va);
  throw new Error(`RVA 0x${rva.toString(16)} is outside every section`);
};
const base = toOff(rsrcRva);

// A resource directory table: 16-byte header, then 8-byte entries (id/name, then
// an offset whose high bit marks a subdirectory).
const entries = (off) => {
  const named = u16(off + 12), ids = u16(off + 14), out = [];
  for (let i = 0; i < named + ids; i++) {
    const e = off + 16 + i * 8;
    out.push({ id: u32(e), off: u32(e + 4) });
  }
  return out;
};
const descend = (off, want) => {
  const hit = entries(off).find((e) => e.id === want);
  if (!hit) return null;
  if (!(hit.off & 0x8000_0000)) throw new Error("expected a subdirectory");
  return base + (hit.off & 0x7fff_ffff);
};

const RT_VERSION = 16, VS_VERSION_INFO = 1;
const typeDir = descend(base, RT_VERSION);
if (!typeDir) { console.log(JSON.stringify({ present: false })); process.exit(0); }
const nameDir = descend(typeDir, VS_VERSION_INFO);
if (!nameDir) { console.log(JSON.stringify({ present: false })); process.exit(0); }
const leaf = entries(nameDir)[0];
if (!leaf || leaf.off & 0x8000_0000) throw new Error("language level is not a data entry");
const de = base + leaf.off;
const data = buf.subarray(toOff(u32(de)), toOff(u32(de)) + u32(de + 4));

// VS_VERSIONINFO nodes: wLength, wValueLength, wType, a NUL-terminated UTF-16LE
// key, then value and/or children — every body aligned to 4 bytes. wValueLength
// counts WORDS for a string node and bytes everywhere else, which is the field
// that silently truncates when a writer gets it wrong.
const align4 = (n) => (n + 3) & ~3;
function node(off) {
  const len = data.readUInt16LE(off), vlen = data.readUInt16LE(off + 2), type = data.readUInt16LE(off + 4);
  let k = off + 6;
  while (data.readUInt16LE(k) !== 0) k += 2;
  const key = data.toString("utf16le", off + 6, k);
  const vStart = align4(k + 2 - off) + off;
  const vBytes = type === 1 ? vlen * 2 : vlen;
  return { len, vlen, type, key, value: data.subarray(vStart, vStart + vBytes), childStart: align4(vStart + vBytes - off) + off, end: off + len };
}

const root = node(0);
if (root.key !== "VS_VERSION_INFO") throw new Error(`root key is ${JSON.stringify(root.key)}`);
const ffi = root.value;
const ver = (hi, lo) => [ffi.readUInt16LE(hi + 2), ffi.readUInt16LE(hi), ffi.readUInt16LE(lo + 2), ffi.readUInt16LE(lo)];
const out = { present: true, bytes: data.length, fileVersion: ver(8, 12), productVersion: ver(16, 20), strings: {}, translations: [] };

for (let c = root.childStart; c < root.end; ) {
  const kid = node(c);
  if (kid.key === "StringFileInfo") {
    for (let t = kid.childStart; t < kid.end; ) {
      const table = node(t);
      for (let s = table.childStart; s < table.end; ) {
        const str = node(s);
        out.strings[str.key] = str.value.toString("utf16le").replace(/\0+$/, "");
        s = align4(str.end);
      }
      out.stringTable = table.key;
      t = align4(table.end);
    }
  } else if (kid.key === "VarFileInfo") {
    for (let v = kid.childStart; v < kid.end; ) {
      const varNode = node(v);
      for (let i = 0; i + 4 <= varNode.value.length; i += 4) {
        out.translations.push([varNode.value.readUInt16LE(i), varNode.value.readUInt16LE(i + 2)]);
      }
      v = align4(varNode.end);
    }
  }
  c = align4(kid.end);
}
console.log(JSON.stringify(out, null, 2));
