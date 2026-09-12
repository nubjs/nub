// The PE `Subsystem` field, read independently of nub.
//
// nub's own reader runs inside `verify_artifact` on every compile, so using it
// here would only prove it agrees with itself. This walks the header the way
// Windows' loader does: e_lfanew at 0x3c, the COFF header, then the optional
// header, whose `Subsystem` sits at offset 68 in BOTH PE32 and PE32+ — PE32's
// extra BaseOfData and its narrower ImageBase cancel out.
//
// Prints `2` for a GUI image, `3` for a console one, or `none` when the file is
// not a placeable PE.
import { readFileSync } from "node:fs";

const image = readFileSync(process.argv[2]);
const pe = image.readUInt32LE(0x3c);
if (image.toString("latin1", pe, pe + 4) !== "PE\0\0") {
  console.log("none");
  process.exit(0);
}
const magic = image.readUInt16LE(pe + 24);
if (magic !== 0x10b && magic !== 0x20b) {
  console.log("none");
  process.exit(0);
}
console.log(String(image.readUInt16LE(pe + 24 + 68)));
