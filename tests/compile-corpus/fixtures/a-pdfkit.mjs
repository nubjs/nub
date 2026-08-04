// A package that reads DATA beside its own source rather than loading code.
// pdfkit resolves its built-in fonts through __dirname, so bundling it produces
// a binary that builds clean and dies on first use — the failure shape a build
// error cannot warn about, which is why it is on the curated list.
import PDFDocument from "pdfkit";
const d = new PDFDocument();
d.text("hi");
d.end();
console.log("ok:pdf");
