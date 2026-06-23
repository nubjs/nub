// Differential registry/auth harness.
// For each CELL (a registry/auth config) and each TOOL, sets up a hermetic fixture
// + HOME, runs a mock registry on a fresh port (logging url+auth header), runs the
// tool's install, and records which (url, authorization) the tool attempted.
// A divergence between nub and the reference PM = a finding.

import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import { execFileSync, spawn } from 'node:child_process';

const BINS = {
  nub: process.env.NUB_BIN,
  npm: process.env.NPM_BIN || '/usr/local/bin/npm',
  pnpm: process.env.PNPM_BIN || '/opt/homebrew/bin/pnpm',
  yarn: process.env.YARN_BIN || '/usr/local/bin/yarn',
  bun: process.env.BUN_BIN || '/opt/homebrew/bin/bun',
};
const NODE = process.execPath;

let portCounter = 5200;
function nextPort() { return portCounter++; }

function startServer(port, logArr) {
  return new Promise((resolve) => {
    const srv = http.createServer((req, res) => {
      logArr.push({
        method: req.method,
        url: req.url,
        host: req.headers['host'] || '',
        authorization: req.headers['authorization'] || '',
      });
      res.statusCode = 404;
      res.setHeader('content-type', 'application/json');
      res.end('{"error":"not found"}');
    });
    srv.on('error', () => resolve(null));
    srv.listen(port, '127.0.0.1', () => resolve(srv));
  });
}

function tmpdir(prefix) {
  return fs.mkdtempSync(path.join(os.tmpdir(), prefix));
}

// cmd per tool — minimal install that triggers metadata fetch
function installCmd(tool) {
  switch (tool) {
    case 'nub': return ['install', '--no-frozen-lockfile'];
    case 'npm': return ['install', '--no-audit', '--no-fund'];
    case 'pnpm': return ['install', '--no-frozen-lockfile', '--ignore-scripts'];
    case 'yarn': return ['install', '--mode=update-lockfile'];
    case 'bun': return ['install', '--no-save'];
  }
}

async function runTool(tool, fixtureDir, homeDir, port, extraEnv) {
  const logArr = [];
  const srv = await startServer(port, logArr);
  if (!srv) return { error: 'port-in-use', requests: [] };
  const env = {
    ...process.env,
    HOME: homeDir,
    XDG_CONFIG_HOME: path.join(homeDir, '.config'),
    XDG_DATA_HOME: path.join(homeDir, '.local/share'),
    XDG_CACHE_HOME: path.join(homeDir, '.cache'),
    npm_config_cache: path.join(homeDir, '.npmcache'),
    NPM_CONFIG_CACHE: path.join(homeDir, '.npmcache'),
    // Avoid corepack interfering for yarn:
    COREPACK_ENABLE_DOWNLOAD_PROMPT: '0',
    ...extraEnv,
  };
  // strip our own host registry env that could leak
  delete env.NUB_CACHE_DIR;
  const [cmd, ...args] = [BINS[tool], ...installCmd(tool)];
  await new Promise((resolve) => {
    const child = spawn(cmd, args, { cwd: fixtureDir, env, stdio: 'ignore' });
    const killer = setTimeout(() => { try { child.kill('SIGKILL'); } catch {} }, 25000);
    child.on('exit', () => { clearTimeout(killer); resolve(); });
    child.on('error', () => { clearTimeout(killer); resolve(); });
  });
  await new Promise((r) => srv.close(r));
  return { requests: logArr };
}

// Summarize: which registry host:port+path did the tool hit, and what auth.
function summarize(requests) {
  // Filter to package-metadata-ish requests (ignore favicon etc). Keep all GETs.
  const out = [];
  for (const r of requests) {
    out.push({ url: `http://${r.host}${r.url}`, auth: r.authorization });
  }
  return out;
}

export { startServer, runTool, summarize, tmpdir, nextPort, BINS, NODE };
