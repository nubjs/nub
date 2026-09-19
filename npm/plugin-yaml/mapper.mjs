#!/usr/bin/env node
// TypeScript content-mapper process for .yaml / .yml files.
//
// Spawned by `tsc --runExternalCode` (TypeScript >= 7.1) when a tsconfig lists this
// package under `contentMappers`. Speaks JSON-RPC 2.0 over stdio with LSP-style
// Content-Length framing; TypeScript sends every request, the mapper only answers.
// Protocol: https://github.com/microsoft/typescript-go/pull/4712
import { emit } from "./emit.mjs";

const projects = new Map();

const handlers = {
  initialize: () => ({ positionEncoding: "utf-16", diagnosticSource: "yaml" }),
  openProject: ({ projectHandle, options }) => {
    projects.set(projectHandle, options ?? {});
    return {};
  },
  closeProject: ({ projectHandle }) => {
    projects.delete(projectHandle);
    return null;
  },
  transform: ({ content }) => {
    const { text, mappings, diagnostics } = emit(content);
    return { text, extension: ".ts", mappings, diagnostics };
  },
};

function write(message) {
  const body = Buffer.from(JSON.stringify(message), "utf8");
  process.stdout.write(`Content-Length: ${body.length}\r\n\r\n`);
  process.stdout.write(body);
}

function dispatch(message) {
  if (message.id === undefined) return; // notification; none are defined
  const handler = handlers[message.method];
  if (!handler) {
    return write({ jsonrpc: "2.0", id: message.id, error: { code: -32601, message: `unknown method ${message.method}` } });
  }
  try {
    write({ jsonrpc: "2.0", id: message.id, result: handler(message.params ?? {}) });
  } catch (error) {
    write({ jsonrpc: "2.0", id: message.id, error: { code: -32603, message: String(error?.stack ?? error) } });
  }
}

let buffer = Buffer.alloc(0);
process.stdin.on("data", (chunk) => {
  buffer = Buffer.concat([buffer, chunk]);
  for (;;) {
    const headerEnd = buffer.indexOf("\r\n\r\n");
    if (headerEnd === -1) return;
    const header = buffer.subarray(0, headerEnd).toString("latin1");
    const length = Number(/Content-Length:\s*(\d+)/i.exec(header)?.[1]);
    if (!Number.isFinite(length)) {
      process.stderr.write(`plugin-yaml: bad frame header: ${header}\n`);
      process.exit(1);
    }
    const bodyStart = headerEnd + 4;
    if (buffer.length < bodyStart + length) return;
    const body = buffer.subarray(bodyStart, bodyStart + length).toString("utf8");
    buffer = buffer.subarray(bodyStart + length);
    dispatch(JSON.parse(body));
  }
});
process.stdin.on("end", () => process.exit(0));
