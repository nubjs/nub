#!/usr/bin/env node
// Print the DLLs a Windows PE actually imports, one per line.
//
// Written because the obvious shortcut is wrong. Searching a binary for
// `VCRUNTIME140.dll` looks like it answers "what does this import", and it does not: a
// DLL name is an ordinary ASCII string, so any copy anywhere in the file matches —
// embedded payloads, a dependency's string table, a mitigation blocklist. Measured on
// Microsoft's own `node.exe` 26.8.1, a whole-file search reports `msvcr100.dll`,
// `msvcr110.dll`, `msvcp110.dll`, `msvcp120.dll` and `mfc42u.dll`, none of which it
// imports. A gate built on that would fail honest binaries and, worse, would be trusted.
//
// So walk the import directory. Both the normal (index 1) and delay-load (index 13)
// tables count: a delay-loaded DLL is still required, just later, and "later" for a CRT
// means the first call rather than process start.
//
// `--check` additionally fails the file if it imports anything from the Visual C++
// redistributable, which is what makes a shipped binary refuse to start on a machine
// that has not installed it — 0xC0000135, with empty stderr, before any of our code
// runs.
import { readFileSync } from "node:fs";

const args = process.argv.slice(2);
const check = args.includes("--check");
const file = args.find((a) => !a.startsWith("--"));
if (!file) {
  console.error("usage: pe-imports.mjs [--check] <file.exe|file.dll>");
  process.exit(2);
}

const buf = readFileSync(file);
const fail = (msg) => {
  console.error(`${file}: ${msg}`);
  process.exit(2);
};

if (buf.length < 0x40 || buf.readUInt16LE(0) !== 0x5a4d) fail("not a PE (no MZ header)");
const peOff = buf.readUInt32LE(0x3c);
if (buf.length < peOff + 24 || buf.readUInt32LE(peOff) !== 0x00004550) fail("not a PE (no PE signature)");

const sectionCount = buf.readUInt16LE(peOff + 6);
const optSize = buf.readUInt16LE(peOff + 20);
const optOff = peOff + 24;
const magic = buf.readUInt16LE(optOff);
// PE32 keeps the data directories at +96; PE32+ at +112, the 16 bytes of difference
// being the four fields widened to 64-bit. Getting this wrong silently reads garbage
// RVAs rather than failing, which is why it is asserted rather than assumed.
const dirOff = magic === 0x20b ? optOff + 112 : magic === 0x10b ? optOff + 96 : fail(`unknown optional header magic 0x${magic.toString(16)}`);

const sections = [];
const secOff = optOff + optSize;
for (let i = 0; i < sectionCount; i++) {
  const s = secOff + i * 40;
  if (s + 40 > buf.length) fail("truncated section table");
  sections.push({
    virtualSize: buf.readUInt32LE(s + 8),
    virtualAddress: buf.readUInt32LE(s + 12),
    rawSize: buf.readUInt32LE(s + 16),
    rawPointer: buf.readUInt32LE(s + 20),
  });
}

// A section's in-memory span can exceed its on-disk span (bss-like padding), so accept
// the larger of the two when locating the RVA and then bound the read to the file.
const toOffset = (rva) => {
  for (const s of sections) {
    const span = Math.max(s.virtualSize, s.rawSize);
    if (rva >= s.virtualAddress && rva < s.virtualAddress + span) {
      const off = rva - s.virtualAddress + s.rawPointer;
      return off < buf.length ? off : null;
    }
  }
  return null;
};

const cstring = (rva) => {
  const off = toOffset(rva);
  if (off === null) return null;
  const end = buf.indexOf(0, off);
  return buf.toString("latin1", off, end === -1 ? buf.length : end);
};

const names = new Set();

// Normal imports: 20-byte descriptors, name RVA at +12, terminated by an all-zero entry.
const readTable = (dirIndex, entrySize, nameFieldOffset) => {
  const rva = buf.readUInt32LE(dirOff + dirIndex * 8);
  if (rva === 0) return;
  let off = toOffset(rva);
  if (off === null) return;
  for (; off + entrySize <= buf.length; off += entrySize) {
    const entry = buf.subarray(off, off + entrySize);
    if (entry.every((b) => b === 0)) break;
    const nameRva = buf.readUInt32LE(off + nameFieldOffset);
    if (nameRva === 0) continue;
    const name = cstring(nameRva);
    if (name) names.add(name);
  }
};

readTable(1, 20, 12);
// Delay-load descriptors are 32 bytes with the name RVA at +4. Older linkers wrote
// absolute addresses here rather than RVAs; those resolve to nothing and are skipped
// rather than guessed at.
readTable(13, 32, 4);

const sorted = [...names].sort((a, b) => a.toLowerCase().localeCompare(b.toLowerCase()));
for (const n of sorted) console.log(n);

if (!check) process.exit(0);

// Every Windows PE imports kernel32, so its absence means the walk above found nothing
// and the "no redistributable dependency" verdict below would be vacuous. This is the
// control, and it has to fail loudly rather than pass quietly.
if (!sorted.some((n) => n.toLowerCase() === "kernel32.dll")) {
  console.error(`${file}: no kernel32.dll import — the import table was not read, so this check proves nothing`);
  process.exit(2);
}

// The redistributable families, per Microsoft's "Determining which DLLs to
// redistribute". Deliberately NOT here, because Windows itself provides them:
// `msvcrt.dll` (the legacy CRT), `ucrtbase.dll`, and the `api-ms-win-crt-*` API sets
// that a statically linked binary still forwards through on Windows 10 and later.
// `msvcrt.dll` is why the digit is required after `msvcr` — busybox imports it and is fine.
const REDIST = /^(vcruntime\d|msvcp\d|msvcr\d|concrt\d|vcomp\d|vcamp\d|mfc\d|mfcm\d)/i;
const offenders = sorted.filter((n) => REDIST.test(n));
if (offenders.length > 0) {
  console.error(
    `${file}: imports the Visual C++ redistributable: ${offenders.join(", ")}\n` +
      `  It will fail at 0xC0000135 (STATUS_DLL_NOT_FOUND) on a Windows machine without\n` +
      `  the redistributable installed, silently — the loader kills the process before\n` +
      `  any code can report it. Link the CRT statically for this target.`,
  );
  process.exit(1);
}
console.error(`${file}: no Visual C++ redistributable dependency (${sorted.length} imports, kernel32.dll present)`);
