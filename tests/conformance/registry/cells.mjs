import fs from 'node:fs';
import path from 'node:path';
import { runTool, summarize, tmpdir, nextPort } from './diff.mjs';

const DUMMY = 'dummy-token-abc';

// A cell describes a registry/auth scenario. Given a port + a tool, it produces
// a fixture dir + home dir with config written, and the package to resolve.
// `tools` lists which reference tools to compare nub against for this cell.
// `files(ctx)` returns { project: {name:content}, home: {relpath:content}, pkg }
// where {PORT} is substituted.

function writeTree(base, files) {
  for (const [rel, content] of Object.entries(files)) {
    const p = path.join(base, rel);
    fs.mkdirSync(path.dirname(p), { recursive: true });
    fs.writeFileSync(p, content);
  }
}

const PKG_UNSCOPED = 'is-odd';      // a real unscoped package name
const PKG_SCOPED = '@acme/widget';  // a scoped package name

function basePkgJson(deps) {
  return JSON.stringify({ name: 'fixture', version: '1.0.0', dependencies: deps }, null, 2);
}

export const cells = [
  {
    id: 'default-registry-project-npmrc',
    desc: 'project .npmrc registry= points at mock; unscoped dep should hit it',
    tools: ['npm', 'pnpm', 'yarn'],
    build: (port) => ({
      project: {
        'package.json': basePkgJson({ [PKG_UNSCOPED]: '^3.0.0' }),
        '.npmrc': `registry=http://127.0.0.1:${port}/\n`,
      },
      home: {},
      env: {},
    }),
  },
  {
    id: 'bun-incumbent-bunfig-registry',
    desc: 'bun project (bun.lock + bunfig.toml registry); nub must mirror bunfig, not .npmrc',
    tools: ['bun'],
    build: (port) => ({
      project: {
        'package.json': basePkgJson({ [PKG_UNSCOPED]: '^3.0.0' }),
        'bunfig.toml': `[install]\nregistry = "http://127.0.0.1:${port}/"\n`,
        'bun.lock': '',
      },
      home: {},
      env: {},
    }),
  },
  {
    id: 'user-npmrc-registry',
    desc: 'user ~/.npmrc registry= (no project .npmrc) — must be honored',
    tools: ['npm', 'pnpm'],
    build: (port) => ({
      project: { 'package.json': basePkgJson({ [PKG_UNSCOPED]: '^3.0.0' }) },
      home: { '.npmrc': `registry=http://127.0.0.1:${port}/\n` },
      env: {},
    }),
  },
  {
    id: 'precedence-project-over-user',
    desc: 'project registry (mock A) vs user registry (mock B): project must win',
    tools: ['npm', 'pnpm'],
    twoPort: true,
    build: (portA, portB) => ({
      project: {
        'package.json': basePkgJson({ [PKG_UNSCOPED]: '^3.0.0' }),
        '.npmrc': `registry=http://127.0.0.1:${portA}/\n`,
      },
      home: { '.npmrc': `registry=http://127.0.0.1:${portB}/\n` },
      env: {},
      expectPort: 'A',
    }),
  },
  {
    id: 'scoped-registry-split',
    desc: 'scoped @acme:registry=mock; both a scoped + unscoped dep; scoped→mock, unscoped→npmjs',
    tools: ['npm', 'pnpm'],
    build: (port) => ({
      project: {
        'package.json': basePkgJson({ [PKG_SCOPED]: '^1.0.0', [PKG_UNSCOPED]: '^3.0.0' }),
        '.npmrc': `@acme:registry=http://127.0.0.1:${port}/\n`,
      },
      home: {},
      env: {},
    }),
  },
  {
    id: 'host-bound-authtoken',
    desc: 'registry=mock + //mock/:_authToken=dummy; auth header must be sent to mock only',
    tools: ['npm', 'pnpm'],
    build: (port) => ({
      project: {
        'package.json': basePkgJson({ [PKG_UNSCOPED]: '^3.0.0' }),
        '.npmrc': `registry=http://127.0.0.1:${port}/\n//127.0.0.1:${port}/:_authToken=${DUMMY}\n//127.0.0.1:${port}/:always-auth=true\n`,
      },
      home: {},
      env: {},
    }),
  },
  {
    id: 'authtoken-env-interp',
    desc: 'user .npmrc _authToken=${MOCK_TOKEN} interpolation from env',
    tools: ['npm', 'pnpm'],
    build: (port) => ({
      project: {
        'package.json': basePkgJson({ [PKG_UNSCOPED]: '^3.0.0' }),
        '.npmrc': `registry=http://127.0.0.1:${port}/\n`,
      },
      home: { '.npmrc': `//127.0.0.1:${port}/:_authToken=\${MOCK_TOKEN}\n//127.0.0.1:${port}/:always-auth=true\n` },
      env: { MOCK_TOKEN: DUMMY },
    }),
  },
  {
    id: 'path-prefix-registry',
    desc: 'Artifactory-style registry with path prefix + matching path-bound _authToken',
    tools: ['npm', 'pnpm'],
    build: (port) => ({
      project: {
        'package.json': basePkgJson({ [PKG_UNSCOPED]: '^3.0.0' }),
        '.npmrc': `registry=http://127.0.0.1:${port}/artifactory/api/npm/repo/\n//127.0.0.1:${port}/artifactory/api/npm/repo/:_authToken=${DUMMY}\n//127.0.0.1:${port}/artifactory/api/npm/repo/:always-auth=true\n`,
      },
      home: {},
      env: {},
    }),
  },
  {
    id: 'env-registry-override',
    desc: 'npm_config_registry env overrides file registry',
    tools: ['npm', 'pnpm'],
    twoPort: true,
    build: (portA, portB) => ({
      project: {
        'package.json': basePkgJson({ [PKG_UNSCOPED]: '^3.0.0' }),
        '.npmrc': `registry=http://127.0.0.1:${portB}/\n`,
      },
      home: {},
      env: { npm_config_registry: `http://127.0.0.1:${portA}/` },
      expectPort: 'A',
    }),
  },
];

export { writeTree, PKG_UNSCOPED, PKG_SCOPED, DUMMY };
