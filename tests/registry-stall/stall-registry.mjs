// A self-contained registry that stalls on purpose. It serves one package,
// `stall-probe@1.0.0`, with a real packument and a real tarball built in
// memory, and stalls exactly one kind of request in one shape. No network.
//
//   node stall-registry.mjs --port 4999 --shape <shape> [--trickle-ms 1000]
//
// Shapes (every stalled socket is held open: a closed socket errors promptly
// on its own and proves nothing about the client's own bounds):
//
//   ok                   serve everything; the positive control
//   meta-silent          accept the connection, read the request, never answer
//   meta-partial-head    send a status line and one header, then nothing
//   meta-partial-body    send full headers and half the packument, then nothing
//   meta-trickle         send headers, then one packument byte per --trickle-ms
//   tarball-partial-body packument served normally; the tarball stops halfway
//   tarball-trickle      packument served normally; the tarball trickles
//
// Every request is logged with its arrival time, so a run's log shows how
// many attempts the client made and how far apart they were.
import http from "node:http";
import crypto from "node:crypto";
import zlib from "node:zlib";

const args = process.argv.slice(2);
const arg = (name, fallback) => {
  const i = args.indexOf(`--${name}`);
  return i === -1 ? fallback : args[i + 1];
};

const port = Number(arg("port", 4999));
const shape = arg("shape", "ok");
const trickleMs = Number(arg("trickle-ms", 1000));
const SHAPES = [
  "ok",
  "meta-silent",
  "meta-partial-head",
  "meta-partial-body",
  "meta-trickle",
  "tarball-partial-body",
  "tarball-trickle",
];
if (!SHAPES.includes(shape)) {
  console.error(`unknown --shape ${shape} (expected one of: ${SHAPES.join(", ")})`);
  process.exit(2);
}

const started = Date.now();
const stamp = () => `+${((Date.now() - started) / 1000).toFixed(1)}s`;

// A one-file tar (package/package.json), gzipped. Padding matters: a tarball
// the extractor rejects would fail the `ok` control for the wrong reason.
function tarball() {
  const body = Buffer.from(JSON.stringify({ name: "stall-probe", version: "1.0.0" }));
  const header = Buffer.alloc(512);
  const put = (text, offset, length) => header.write(text, offset, length, "ascii");
  put("package/package.json", 0, 100);
  put("0000644\0", 100, 8);
  put("0000000\0", 108, 8);
  put("0000000\0", 116, 8);
  put(`${body.length.toString(8).padStart(11, "0")}\0`, 124, 12);
  put("00000000000\0", 136, 12);
  put("        ", 148, 8);
  put("0", 156, 1);
  put("ustar\0", 257, 6);
  put("00", 263, 2);
  const sum = header.reduce((total, byte) => total + byte, 0);
  put(`${sum.toString(8).padStart(6, "0")}\0 `, 148, 8);
  const pad = Buffer.alloc((512 - (body.length % 512)) % 512);
  return zlib.gzipSync(Buffer.concat([header, body, pad, Buffer.alloc(1024)]));
}

const tgz = tarball();
const integrity = `sha512-${crypto.createHash("sha512").update(tgz).digest("base64")}`;
// Backdated so a minimum-release-age policy never filters the only version.
const published = "2020-01-01T00:00:00.000Z";
const packument = (origin) =>
  Buffer.from(
    JSON.stringify({
      name: "stall-probe",
      "dist-tags": { latest: "1.0.0" },
      modified: published,
      time: { created: published, modified: published, "1.0.0": published },
      versions: {
        "1.0.0": {
          name: "stall-probe",
          version: "1.0.0",
          dist: { tarball: `${origin}/stall-probe/-/stall-probe-1.0.0.tgz`, integrity },
        },
      },
    }),
  );

// Stalled responses are kept reachable so nothing is collected mid-run.
const held = [];

function trickle(res, bytes) {
  let i = 0;
  const timer = setInterval(() => {
    if (i >= bytes.length || res.destroyed) {
      clearInterval(timer);
      if (!res.destroyed) res.end();
      return;
    }
    res.write(bytes.subarray(i, i + 1));
    i += 1;
  }, trickleMs);
  res.on("close", () => clearInterval(timer));
}

function serve(req, res, bytes, type, stall) {
  if (stall === "silent") {
    held.push(res);
    return;
  }
  if (stall === "partial-head") {
    // Bypass the http module's buffering so the partial head reaches the wire.
    req.socket.write("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n");
    held.push(res);
    return;
  }
  res.writeHead(200, { "Content-Type": type, "Content-Length": bytes.length });
  if (stall === "partial-body") {
    res.write(bytes.subarray(0, Math.floor(bytes.length / 2)));
    held.push(res);
    return;
  }
  if (stall === "trickle") {
    held.push(res);
    trickle(res, bytes);
    return;
  }
  res.end(bytes);
}

let seen = 0;
http
  .createServer((req, res) => {
    const n = ++seen;
    const isTarball = req.url.endsWith(".tgz");
    const isMeta = !isTarball && /^\/stall-probe\/?$/.test(req.url.split("?")[0]);
    let stall = null;
    if (isMeta && shape.startsWith("meta-")) stall = shape.slice("meta-".length);
    if (isTarball && shape.startsWith("tarball-")) stall = shape.slice("tarball-".length);
    console.log(`${stamp()} REQ #${n} ${req.method} ${req.url}${stall ? ` STALL ${stall}` : ""}`);
    req.socket.on("close", () => console.log(`${stamp()} CLOSED #${n}`));
    if (isTarball) return serve(req, res, tgz, "application/octet-stream", stall);
    if (isMeta) return serve(req, res, packument(`http://127.0.0.1:${port}`), "application/json", stall);
    res.writeHead(404, { "Content-Type": "application/json" });
    res.end('{"error":"not found"}');
  })
  .listen(port, "127.0.0.1", () => console.log(`stall registry on 127.0.0.1:${port}, shape ${shape}`));
