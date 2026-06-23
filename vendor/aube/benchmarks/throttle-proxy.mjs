#!/usr/bin/env node
// Bandwidth/latency-throttling HTTP proxy for hermetic Aube benchmarks.
//
// Sits between the package managers and the local Verdaccio instance
// and applies a fixed response latency plus token-bucket rate limit on
// the response body so we can measure install performance under a
// simulated internet link (e.g. 50 ms + 50 Mbit/s) without relying on
// OS-level traffic shaping (which would need root and would vary
// between macOS and Linux).
//
// Throttling is download-only on purpose: the request side of npm
// installs is tiny (a GET with some headers); all the bytes are on the
// response path. A symmetrical throttle would add complexity without
// changing the measurement meaningfully.
//
// A single process-wide token bucket is shared across every in-flight
// response. Package managers routinely open 10–50+ concurrent
// connections to download tarballs; a per-connection bucket would
// silently multiply the effective rate by the concurrency count and
// make cross-PM comparisons meaningless. One bucket = one pipe.
//
// Zero deps — uses node:http and node:stream. Node 24 is already a
// tools dependency of this repo.
//
// Usage:
//   node throttle-proxy.mjs --port 4875 --upstream http://127.0.0.1:4874 --rate 50mbit --latency 50ms
//
// --rate units:
//   <n>bit, <n>kbit, <n>mbit, <n>gbit   — bits/sec
//   <n>b,   <n>kb,   <n>mb,   <n>gb     — bytes/sec (SI kilo = 1000)
//   <n>                                  — bare integer, bytes/sec
//
// --latency units:
//   <n>ms, <n>s, <n>                     — milliseconds unless suffixed with s

import { createServer, request as httpRequest } from 'node:http';
import { Transform } from 'node:stream';

function parseArgs(argv) {
	const out = {};
	for (let i = 0; i < argv.length; i++) {
		const a = argv[i];
		if (a.startsWith('--')) {
			out[a.slice(2)] = argv[++i];
		}
	}
	return out;
}

// Returns bytes/sec.
function parseRate(raw) {
	if (!raw) throw new Error('missing --rate');
	const m = String(raw).trim().toLowerCase().match(/^(\d+(?:\.\d+)?)\s*([a-z]*)$/);
	if (!m) throw new Error(`cannot parse rate: ${raw}`);
	const n = Number(m[1]);
	const unit = m[2];
	switch (unit) {
		case '':
			return Math.floor(n); // bare integer = bytes/s
		case 'b':
			return Math.floor(n); // bytes/s
		case 'kb':
			return Math.floor(n * 1000);
		case 'mb':
			return Math.floor(n * 1_000_000);
		case 'gb':
			return Math.floor(n * 1_000_000_000);
		case 'bit':
			return Math.floor(n / 8); // bits/s → bytes/s
		case 'kbit':
			return Math.floor((n * 1000) / 8);
		case 'mbit':
			return Math.floor((n * 1_000_000) / 8);
		case 'gbit':
			return Math.floor((n * 1_000_000_000) / 8);
		default:
			throw new Error(`unknown rate unit: ${unit}`);
	}
}

// Returns milliseconds.
function parseLatency(raw) {
	if (!raw) return 0;
	const m = String(raw).trim().toLowerCase().match(/^(\d+(?:\.\d+)?)\s*(ms|s)?$/);
	if (!m) throw new Error(`cannot parse latency: ${raw}`);
	const n = Number(m[1]);
	return Math.floor(m[2] === 's' ? n * 1000 : n);
}

// Token-bucket shared across every response. Refills to `capacity`
// (not "adds up to") every REFILL_MS so idle periods can't be banked
// into bursts — we want a ceiling, not a credit.
//
// REFILL_MS trades off fidelity against timer overhead. 100ms gives
// ~10x smoothing over a 1-second average and is imperceptible for
// bench timing, which runs in hundreds of ms minimum.
const REFILL_MS = 100;

function createSharedBucket(bytesPerSec) {
	const capacity = Math.max(1, Math.floor(bytesPerSec / (1000 / REFILL_MS)));
	let tokens = capacity;
	const waiters = [];
	const timer = setInterval(() => {
		tokens = capacity;
		while (waiters.length && tokens > 0) {
			waiters.shift()();
		}
	}, REFILL_MS);
	timer.unref();

	return {
		take(n) {
			return new Promise((resolve) => {
				const tryTake = () => {
					if (tokens <= 0) {
						waiters.push(tryTake);
						return;
					}
					const grant = Math.min(n, tokens);
					tokens -= grant;
					resolve(grant);
				};
				tryTake();
			});
		},
		shutdown() {
			clearInterval(timer);
		},
	};
}

// Per-response Transform that draws from the shared bucket. Chunks
// the incoming stream so backpressure is honored even for large
// tarball responses.
function createThrottleStream(bucket) {
	return new Transform({
		async transform(chunk, _enc, cb) {
			try {
				let offset = 0;
				while (offset < chunk.length) {
					const grant = await bucket.take(chunk.length - offset);
					this.push(chunk.subarray(offset, offset + grant));
					offset += grant;
				}
				cb();
			} catch (err) {
				cb(err);
			}
		},
	});
}

function main() {
	const args = parseArgs(process.argv.slice(2));
	const port = Number(args.port || 4875);
	const upstreamRaw = args.upstream;
	if (!upstreamRaw) {
		console.error('ERROR: --upstream required');
		process.exit(2);
	}
	const upstream = new URL(upstreamRaw);
	const bytesPerSec = parseRate(args.rate);
	const latencyMs = parseLatency(args.latency);
	const bucket = createSharedBucket(bytesPerSec);

	const server = createServer((clientReq, clientRes) => {
		// Build the upstream request. Preserve method, path, and all
		// headers — Verdaccio's packument-format negotiation depends on
		// `accept: application/vnd.npm.install-v1+json` for bun/pnpm,
		// and conditional fetches rely on `if-none-match`.
		//
		// Critically, we pass the client's original `host` header
		// (i.e. the proxy's own address) through unchanged. Verdaccio
		// uses Host to compute self-referential tarball URLs in
		// packument responses, so leaving it as the proxy's host means
		// every tarball download is also routed back through us and
		// therefore throttled. If we overwrote host to the upstream
		// address, PMs would fetch tarballs directly from Verdaccio
		// and silently bypass the bandwidth limit.
		const headers = { ...clientReq.headers };

		const upstreamReq = httpRequest(
			{
				protocol: upstream.protocol,
				hostname: upstream.hostname,
				port: upstream.port || 80,
				method: clientReq.method,
				path: clientReq.url,
				headers,
			},
			(upstreamRes) => {
				const forward = () => {
					if (clientRes.destroyed) {
						upstreamRes.destroy();
						return;
					}
					try {
						clientRes.writeHead(upstreamRes.statusCode, upstreamRes.headers);
						upstreamRes.pipe(createThrottleStream(bucket)).pipe(clientRes);
						upstreamRes.resume();
					} catch (err) {
						upstreamRes.destroy(err);
						clientRes.destroy(err);
					}
				};

				upstreamRes.pause();
				if (latencyMs > 0) {
					setTimeout(forward, latencyMs);
				} else {
					forward();
				}
			},
		);

		upstreamReq.on('error', (err) => {
			console.error(`upstream error: ${err.message}`);
			if (!clientRes.headersSent) {
				clientRes.writeHead(502, { 'content-type': 'text/plain' });
			}
			clientRes.end(`upstream error: ${err.message}`);
		});

		clientReq.pipe(upstreamReq);
	});

	server.on('clientError', (err, socket) => {
		// A package manager closing a keep-alive connection shouldn't
		// spam stderr. Swallow ECONNRESET / EPIPE quietly and let other
		// errors surface.
		if (!['ECONNRESET', 'EPIPE'].includes(err.code)) {
			console.error(`client error: ${err.message}`);
		}
		socket.destroy();
	});

	server.listen(port, '127.0.0.1', () => {
		const rateLabel = `${bytesPerSec} B/s (${((bytesPerSec * 8) / 1_000_000).toFixed(1)} Mbit/s)`;
		const latencyLabel = latencyMs > 0 ? ` + ${latencyMs} ms latency` : '';
		console.log(`throttle-proxy listening on 127.0.0.1:${port} → ${upstream.origin} @ ${rateLabel}${latencyLabel} (shared bucket)`);
	});

	const shutdown = () => {
		bucket.shutdown();
		server.close(() => process.exit(0));
		setTimeout(() => process.exit(0), 2000).unref();
	};
	process.on('SIGTERM', shutdown);
	process.on('SIGINT', shutdown);
}

main();
