// A registry that stalls on purpose, in the two shapes that actually break nub.
//
//   --mode blackhole
//       Accepts the TCP connection, reads the request, and never writes a byte
//       back. This is the "connection accepted, then silence" case: no response
//       head ever arrives, so only a bound on the *head* wait can end it.
//
//   --mode proxy --stall-at N
//       Forwards every request to the real registry, except the Nth, where it
//       sends the real status and headers plus ~4 KB of the real body and then
//       stops forever without closing. This is the "stream dies mid-body" case,
//       which is what issue #715's reporter was hitting; it surfaces elsewhere
//       as `error decoding response body`.
//
// Both keep the socket open, which is the point: a closed socket produces a
// prompt error on its own and proves nothing about nub's own timeouts.
import http from "node:http";
import https from "node:https";
import net from "node:net";

const args = process.argv.slice(2);
const arg = (name, fallback) => {
  const i = args.indexOf(`--${name}`);
  return i === -1 ? fallback : args[i + 1];
};

const port = Number(arg("port", 4999));
const mode = arg("mode", "blackhole");
const stallAt = Number(arg("stall-at", 800));
const upstream = arg("upstream", "registry.npmjs.org");

const stamp = () => new Date().toISOString();

if (mode === "blackhole") {
  net
    .createServer((sock) => {
      console.log(`${stamp()} ACCEPTED ${sock.remoteAddress}:${sock.remotePort}`);
      sock.on("data", (d) =>
        console.log(`${stamp()} REQ ${d.toString("utf8").split("\r\n")[0]}`),
      );
      sock.on("close", () => console.log(`${stamp()} CLOSED (client gave up)`));
      sock.on("error", (e) => console.log(`${stamp()} ERR ${e.code}`));
      // Deliberately never respond.
    })
    .listen(port, "127.0.0.1", () =>
      console.log(`blackhole registry on 127.0.0.1:${port}`),
    );
} else if (mode === "proxy") {
  let seen = 0;
  // Holds the stalled response objects so nothing is garbage collected and the
  // socket stays open for the whole run.
  const held = [];
  http
    .createServer((req, res) => {
      const n = ++seen;
      const headers = { ...req.headers, host: upstream };
      const preq = https.request(
        { host: upstream, path: req.url, method: req.method, headers },
        (pres) => {
          res.writeHead(pres.statusCode, pres.headers);
          if (n !== stallAt) {
            pres.pipe(res);
            return;
          }
          console.log(`${stamp()} STALLING request #${n} ${req.url}`);
          let sent = 0;
          pres.on("data", (chunk) => {
            if (sent < 4096) {
              res.write(chunk);
              sent += chunk.length;
            } else {
              pres.pause(); // never deliver the rest, never end the response
            }
          });
          held.push({ req, res, pres });
        },
      );
      preq.on("error", () => {
        try {
          res.writeHead(502);
          res.end();
        } catch {
          // client already gone
        }
      });
      req.pipe(preq);
    })
    .listen(port, "127.0.0.1", () =>
      console.log(
        `stall proxy on 127.0.0.1:${port} -> ${upstream}, stalling request #${stallAt}`,
      ),
    );
} else {
  console.error(`unknown --mode ${mode} (expected "blackhole" or "proxy")`);
  process.exit(2);
}
