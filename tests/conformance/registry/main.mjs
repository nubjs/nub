import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import { runTool, summarize, tmpdir, nextPort } from './diff.mjs';
import { cells, writeTree } from './cells.mjs';

const TOOLS = ['nub', 'npm', 'pnpm', 'bun', 'yarn'];
const onlyCell = process.argv[2]; // optional filter

const results = {};

for (const cell of cells) {
  if (onlyCell && cell.id !== onlyCell) continue;
  results[cell.id] = { desc: cell.desc, tools: {} };
  const toolList = ['nub', ...cell.tools];
  for (const tool of toolList) {
    // fresh ports per (cell, tool) run so logs don't collide
    const portA = nextPort();
    const portB = cell.twoPort ? nextPort() : null;
    const spec = cell.twoPort ? cell.build(portA, portB) : cell.build(portA);
    const fix = tmpdir(`rd-${cell.id}-${tool}-`);
    const home = tmpdir(`rdh-${cell.id}-${tool}-`);
    fs.mkdirSync(path.join(home, '.config'), { recursive: true });
    writeTree(fix, spec.project);
    writeTree(home, spec.home);
    // The mock server in diff.runTool listens on portA; for twoPort cells we need
    // BOTH ports up. Handle here: start a second server for portB inline.
    let result;
    if (cell.twoPort) {
      // start server for portB (the "other" registry) manually
      const http = await import('node:http');
      const logB = [];
      const srvB = await new Promise((resolve) => {
        const s = http.createServer((req, res) => {
          logB.push({ method: req.method, url: req.url, host: req.headers.host, authorization: req.headers.authorization || '' });
          res.statusCode = 404; res.end('{}');
        });
        s.on('error', () => resolve(null));
        s.listen(portB, '127.0.0.1', () => resolve(s));
      });
      result = await runTool(tool, fix, home, portA, spec.env || {});
      if (srvB) await new Promise((r) => srvB.close(r));
      result.requestsB = logB.map((r) => ({ url: `http://${r.host}${r.url}`, auth: r.authorization }));
    } else {
      result = await runTool(tool, fix, home, portA, spec.env || {});
    }
    results[cell.id].tools[tool] = {
      portA, portB,
      requests: summarize(result.requests || []),
      requestsB: result.requestsB || [],
    };
    results[cell.id].expectPort = spec.expectPort;
    fs.rmSync(fix, { recursive: true, force: true });
    fs.rmSync(home, { recursive: true, force: true });
  }
}

console.log(JSON.stringify(results, null, 2));
