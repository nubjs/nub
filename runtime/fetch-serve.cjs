// The HTTP server behind nub's default-export `fetch` handler: an entry whose
// default export is an object with a `fetch` method is served over HTTP instead of
// merely evaluated. That is the handler shape Cloudflare Workers, Bun, Deno
// (`deno serve`) and Vercel all accept, so one file runs unchanged on each of them
// and under `nub <file>`.
//
// Detection lives in preload-common.cjs (`installServeEntry`), which requires THIS
// module only once it has a handler in hand — so an ordinary script never pays for
// node:http, and the module list at user code's first line is untouched either way.
//
// Contract (decided 2026-05-22): ONE argument,
// `fetch(request: Request) → Response | Promise<Response>` — the intersection of
// Workers' `(request, env, ctx)`, Bun's `(request, server)` and Deno's
// `(request, info)`. A second argument stays additive for whenever WinterTC's
// http-server proposal settles what belongs in one. Honored option keys on the
// default export are `port` and `hostname`, nothing else. Pure node:http plus
// WHATWG Request/Response: no N-API, no Rust listener, and the same adapter shape
// `srvx` and `@hono/node-server` use on plain Node, which is the escape hatch this
// feature is reversible through.
"use strict";

const http = require("node:http");
const { Readable, pipeline } = require("node:stream");
const { isIP } = require("node:net");

const DEFAULT_PORT = 3000;

// Bind a listener that routes every request through the handler. Called once, from
// the detection pass, on the main thread of a process the user launched as a file
// run — never for a child or a Worker.
function serve(handler) {
  let port;
  let host;
  try {
    ({ port, host } = listenOptions(handler, process.env));
  } catch (err) {
    fail(err.message);
  }
  const server = http.createServer((req, res) => {
    handle(handler, req, res);
  });
  // A failed bind is fatal, matching Bun and Deno: silently serving a port nobody
  // asked for is worse than stopping, and a dev loop retries in a second.
  server.on("error", (err) => {
    const where = host ? `${host}:${port}` : `port ${port}`;
    if (err.code === "EADDRINUSE") {
      fail(`${where} is already in use (set PORT to choose another port)`);
    }
    if (err.code === "EACCES") {
      fail(`${where} needs elevated privileges (set PORT to choose another port)`);
    }
    fail(err.message);
  });
  server.listen(port, host, () => {
    process.stderr.write(`Listening on ${displayUrl(host, server.address().port)}\n`);
  });
}

// `PORT` > the export's `port` > 3000, and `HOST` > the export's `hostname` > every
// interface. `PORT` is the one signal every platform-as-a-service agrees on, and
// Bun, Express and Next all read it, so an environment that sets it has to win over
// a port literal the source committed. Leaving the host undefined hands Node its own
// dual-stack default rather than pinning IPv4.
function listenOptions(handler, env) {
  const port = parsePort(env.PORT, "PORT")
    ?? parsePort(handler.port, "the default export's `port`")
    ?? DEFAULT_PORT;
  const host = nonEmpty(env.HOST) ?? nonEmpty(handler.hostname) ?? undefined;
  return { port, host };
}

function parsePort(value, source) {
  if (value === undefined || value === null || value === "") return undefined;
  const n = typeof value === "number" ? value : Number(String(value).trim());
  if (!Number.isInteger(n) || n < 0 || n > 65535) {
    throw new Error(`${source} must be an integer from 0 to 65535, got ${JSON.stringify(value)}`);
  }
  return n;
}

function nonEmpty(value) {
  return typeof value === "string" && value.trim() !== "" ? value.trim() : undefined;
}

// What to print for a host the user can actually open. A wildcard bind is reachable
// at localhost, and a bare IPv6 literal needs brackets to be a valid URL.
function displayUrl(host, port) {
  let shown = "localhost";
  if (host && host !== "0.0.0.0" && host !== "::") {
    shown = isIP(host) === 6 ? `[${host}]` : host;
  }
  return `http://${shown}:${port}`;
}

function fail(message) {
  process.stderr.write(`nub: ${message}\n`);
  process.exit(1);
}

async function handle(handler, req, res) {
  // The handler observes a disconnect through `request.signal`, the only channel the
  // one-argument contract has for it. Firing on `close` before the response finished
  // covers both a client that went away and a response we destroyed.
  const controller = new AbortController();
  res.on("close", () => {
    if (!res.writableFinished) controller.abort();
  });
  let request;
  try {
    request = toRequest(req, controller.signal);
  } catch {
    // A request line or Host header no WHATWG URL can express, or a method `Request`
    // forbids (CONNECT, TRACE, TRACK). Never reaches the handler.
    plain(res, 400, "Bad Request");
    return;
  }
  let response;
  try {
    response = await handler.fetch(request);
    if (!isResponse(response)) {
      throw new TypeError(`fetch handler must return a Response, got ${describe(response)}`);
    }
  } catch (err) {
    if (controller.signal.aborted) return;
    // The handler's bug, reported the way an uncaught error in user code is, and
    // answered rather than left to hang. Headers already on the wire mean the status
    // is spent, so the only honest signal left is an incomplete response.
    console.error(err);
    if (res.headersSent) res.destroy();
    else plain(res, 500, "Internal Server Error");
    return;
  }
  try {
    writeResponse(res, response, req.method === "HEAD");
  } catch (err) {
    console.error(err);
    res.destroy();
  }
}

// IncomingMessage → Request. `rawHeaders` keeps a repeated header as separate
// entries, which `req.headers` has already joined; the body rides the request stream
// itself, and `duplex: "half"` is what undici requires of any streamed body. GET and
// HEAD get none because `Request` rejects a body on either.
function toRequest(req, signal) {
  const url = new URL(req.url, `http://${req.headers.host ?? "localhost"}`);
  const headers = new Headers();
  const raw = req.rawHeaders;
  for (let i = 0; i < raw.length; i += 2) headers.append(raw[i], raw[i + 1]);
  const method = req.method;
  if (method === "GET" || method === "HEAD") {
    return new Request(url, { method, headers, signal });
  }
  // `duplex` is only meaningful alongside a body, and passing it without one is an
  // option undici has no reason to keep accepting.
  return new Request(url, { method, headers, body: Readable.toWeb(req), duplex: "half", signal });
}

// A Response from another realm — a Worker's, or a polyfill a dependency installed —
// fails `instanceof`, so accept anything carrying the shape the writer below reads.
function isResponse(value) {
  if (value instanceof Response) return true;
  return isObject(value)
    && typeof value.status === "number"
    && isObject(value.headers)
    && typeof value.headers.forEach === "function";
}

function isObject(value) {
  return typeof value === "object" && value !== null;
}

function describe(value) {
  if (value === null) return "null";
  if (typeof value !== "object") return typeof value;
  const name = value.constructor && value.constructor.name;
  return name ? `an instance of ${name}` : "an object";
}

// Response → ServerResponse. Cookies come from `getSetCookie()` so each one keeps
// its own header line — the Headers iterator joins them with a comma, which breaks
// any cookie carrying an `Expires` date. A HEAD, 204 or 304 response sends no body,
// and a body nothing will read is cancelled rather than left for the collector.
function writeResponse(res, response, isHead) {
  const { status, headers } = response;
  for (const [name, value] of headers) {
    if (name === "set-cookie") continue;
    res.setHeader(name, value);
  }
  const cookies = typeof headers.getSetCookie === "function"
    ? headers.getSetCookie()
    : headers.has("set-cookie")
      ? [headers.get("set-cookie")]
      : [];
  if (cookies.length > 0) res.setHeader("set-cookie", cookies);
  res.writeHead(status, response.statusText || undefined);
  const body = response.body;
  if (body === null || isHead || status === 204 || status === 304) {
    if (body !== null) body.cancel().catch(() => {});
    res.end();
    return;
  }
  // pipeline owns backpressure and teardown in both directions: a client that goes
  // away destroys the source, which cancels the web stream, and a source that fails
  // destroys the response instead of leaving the request open.
  pipeline(Readable.fromWeb(body), res, () => {});
}

function plain(res, status, text) {
  res.statusCode = status;
  res.setHeader("content-type", "text/plain;charset=UTF-8");
  res.end(text);
}

module.exports = { serve, listenOptions, displayUrl, isResponse };
