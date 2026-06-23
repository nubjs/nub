#!/usr/bin/env node
// Mock npm web-login server used by test/login.bats to exercise
// `aube login --auth-type=web` without standing up a real registry.
//
// Usage: node web-login-server.mjs <port-file>
//
// Binds 127.0.0.1 on an ephemeral port, writes the chosen port to
// <port-file>, and then serves two endpoints that mirror the shape of
// `/-/v1/login` as implemented by the npm registry:
//
//   POST /-/v1/login       -> 200 { loginUrl, doneUrl }
//   GET  /-/v1/done?...    -> 202 once (with Retry-After: 1), then
//                             200 { token: "mock-web-token" }
//
// The first poll returns 202 on purpose so the aube client's polling
// loop is exercised, not just the happy-path 200.
import { createServer } from "node:http";
import { writeFileSync } from "node:fs";

const portFile = process.argv[2];
if (!portFile) {
	console.error("usage: web-login-server.mjs <port-file>");
	process.exit(2);
}

const TOKEN = "mock-web-token";
let polls = 0;

const server = createServer((req, res) => {
	if (req.method === "POST" && req.url === "/-/v1/login") {
		// Drain the body (hostname is echoed by real npm but we don't care).
		req.on("data", () => {});
		req.on("end", () => {
			const { port } = server.address();
			const base = `http://127.0.0.1:${port}`;
			const body = JSON.stringify({
				loginUrl: `${base}/login-page`,
				doneUrl: `${base}/-/v1/done?sessionId=abc`,
			});
			res.writeHead(200, {
				"content-type": "application/json",
				"content-length": Buffer.byteLength(body),
			});
			res.end(body);
		});
		return;
	}

	if (req.method === "GET" && req.url.startsWith("/-/v1/done")) {
		polls += 1;
		if (polls < 2) {
			res.writeHead(202, { "retry-after": "1" });
			res.end();
			return;
		}
		const body = JSON.stringify({ token: TOKEN });
		res.writeHead(200, {
			"content-type": "application/json",
			"content-length": Buffer.byteLength(body),
		});
		res.end(body);
		return;
	}

	res.writeHead(404);
	res.end();
});

server.listen(0, "127.0.0.1", () => {
	writeFileSync(portFile, String(server.address().port));
});

// Clean exit on SIGTERM so `kill $pid` in bats doesn't leave a zombie.
for (const sig of ["SIGTERM", "SIGINT"]) {
	process.on(sig, () => {
		server.close(() => process.exit(0));
	});
}
