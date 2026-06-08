'use client';

import { useState, type ReactNode } from 'react';
import { Terminal } from './code';

/* High-level overview of the toolchain, up front: four color-coded commands,
   one per accent, that recur throughout the page. Plaintext tabs over a
   horizontal panel that mirrors the band subsections below — the panel leads
   with the command pill as its accent header. No auto-cycling; the reader drives. */

function Mono({ children }: { children: ReactNode }) {
  return <span className="font-mono text-[0.84em] text-fd-foreground">{children}</span>;
}

function Chevron({ dir }: { dir: 'left' | 'right' }) {
  return (
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
      <path d={dir === 'left' ? 'm15 18-6-6 6-6' : 'm9 18 6-6-6-6'} />
    </svg>
  );
}

type Accent = 'ember' | 'acid' | 'sky' | 'orchid' | 'pink';

const PIECES: {
  accent: Accent;
  command: string;
  label: string;
  title: string;
  blurb: ReactNode;
  replaces: string[];
  lines: { cmd?: string; comment?: string; out?: string }[];
}[] = [
  {
    accent: 'ember',
    command: 'nub <file>',
    label: 'File runner',
    title: 'A TypeScript-first Node.js',
    blurb: (
      <>
        Run <Mono>.ts</Mono>, <Mono>.tsx</Mono>, and <Mono>.jsx</Mono> on stock Node with full{' '}
        <Mono>tsconfig.json</Mono> support, <Mono>.env</Mono> loading, and unflagged support
        for modern syntax and APIs.
      </>
    ),
    replaces: ['tsx', 'ts-node', 'tsconfig-paths', 'dotenv'],
    lines: [
      { out: '# run a TypeScript file' },
      { cmd: 'nub index.ts' },
      { out: '# restart on changes' },
      { cmd: 'nub watch src/server.ts' },
    ],
  },
  {
    accent: 'acid',
    command: 'nub run',
    label: 'Script runner',
    title: 'An 18× faster pnpm run',
    blurb: (
      <>
        A drop-in for <Mono>npm run</Mono> and <Mono>pnpm run</Mono> with lifecycle hooks, env
        vars, and workspaces, minus the per-call Node bootstrap.
      </>
    ),
    replaces: ['npm run', 'pnpm run'],
    lines: [
      { out: '# run a package.json script' },
      { cmd: 'nub run dev' },
      { out: '# every workspace, in dependency order' },
      { cmd: 'nub -r run build' },
    ],
  },
  {
    accent: 'sky',
    command: 'nubx',
    label: 'Package runner',
    title: 'A 20× faster npx',
    blurb: (
      <>
        Resolves <Mono>node_modules/.bin</Mono> in Rust and execs the binary directly — no
        Node process in the wrapper.
      </>
    ),
    replaces: ['npx', 'pnpm exec'],
    lines: [
      { out: '# run a local CLI, fast' },
      { cmd: 'nubx prisma generate' },
      { out: "# your install's exact binary" },
      { cmd: 'nubx eslint .' },
    ],
  },
  {
    accent: 'orchid',
    command: 'nub node',
    label: 'Node version manager',
    title: 'A built-in Node version manager',
    blurb: (
      <>
        Reads <Mono>.node-version</Mono> / <Mono>.nvmrc</Mono> and installs the right Node
        from nodejs.org. Replaces <Mono>nvm</Mono> and <Mono>fnm</Mono>.
      </>
    ),
    replaces: ['nvm', 'fnm'],
    lines: [
      { cmd: 'echo 26 > .node-version' },
      { cmd: 'nub hello.ts' },
      { out: 'Installing Node 26 from nodejs.org…' },
      { out: 'Hello world!' },
    ],
  },
  {
    accent: 'pink',
    command: 'nub install',
    label: 'Package hypermanager',
    title: 'A package hypermanager',
    blurb: (
      <>
        All package-management commands (<Mono>install</Mono>, <Mono>add</Mono>, <Mono>remove</Mono> …)
        delegate to your project&rsquo;s configured package manager. It <em>auto-detects</em>{' '}
        <Mono>package.json#packageManager</Mono> (or infers from an existing lockfile), installs it if
        needed, and delegates to it. No Corepack required.
      </>
    ),
    replaces: ['corepack'],
    lines: [
      { cmd: 'nub install' },
      { out: '» resolved package manager is pnpm @11' },
      { out: '» resolved from package.json#packageManager' },
      { out: '» found pnpm@11 in $PATH' },
      { out: '» pnpm install' },
      { out: 'Already up to date', bright: true },
      { out: 'Done in 0.4s', bright: true },
    ],
  },
];

const TAB: Record<Accent, { text: string; active: string; hover: string; pill: string; dot: string }> = {
  ember: { text: 'text-ember', active: 'border-ember/50 bg-ember/10', hover: 'hover:bg-ember/10', pill: 'border-ember/40 text-ember', dot: 'bg-ember' },
  acid: { text: 'text-acid', active: 'border-acid/50 bg-acid/10', hover: 'hover:bg-acid/10', pill: 'border-acid/40 text-acid', dot: 'bg-acid' },
  sky: { text: 'text-sky', active: 'border-sky/50 bg-sky/10', hover: 'hover:bg-sky/10', pill: 'border-sky/40 text-sky', dot: 'bg-sky' },
  orchid: { text: 'text-orchid', active: 'border-orchid/50 bg-orchid/10', hover: 'hover:bg-orchid/10', pill: 'border-orchid/40 text-orchid', dot: 'bg-orchid' },
  pink: { text: 'text-pink', active: 'border-pink/50 bg-pink/10', hover: 'hover:bg-pink/10', pill: 'border-pink/40 text-pink', dot: 'bg-pink' },
};

export function ToolkitTabs() {
  const [active, setActive] = useState(0);
  const piece = PIECES[active];
  const tab = TAB[piece.accent];
  const go = (delta: number) => setActive((i) => (i + delta + PIECES.length) % PIECES.length);

  return (
    <div className="mx-auto max-w-5xl">
      {/* Plaintext segmented tabs: one outer border; active/hover lifts into a pill. */}
      <div className="flex justify-center">
        <div
          role="tablist"
          aria-label="Toolchain commands"
          className="inline-flex flex-wrap justify-center gap-1 rounded-xl border border-fd-border bg-fd-muted/30 p-1.5"
        >
          {PIECES.map((p, i) => {
            const t = TAB[p.accent];
            const on = i === active;
            return (
              <button
                key={p.command}
                role="tab"
                aria-selected={on}
                onClick={() => setActive(i)}
                className={`rounded-lg border px-4 py-1.5 text-sm font-normal ${t.text} ${
                  on ? t.active : `border-transparent ${t.hover}`
                }`}
              >
                {p.label}
              </button>
            );
          })}
        </div>
      </div>

      {/* Horizontal panel — same shape as the band subsections below. */}
      <div className="mt-12 grid items-center gap-10 lg:min-h-[244px] lg:grid-cols-2">
        <div className="min-w-0">
          <span
            className={`inline-flex items-center gap-2 rounded-full border bg-fd-card/50 px-3.5 py-1 font-mono text-sm ${tab.pill}`}
          >
            <span aria-hidden>$</span>
            <span>{piece.command}</span>
          </span>
          <h3 className="mt-4 text-balance font-display text-2xl font-medium leading-snug md:text-3xl">
            {piece.title}
          </h3>
          <p className="mt-4 text-pretty text-lg leading-relaxed text-fd-muted-foreground">
            {piece.blurb}
          </p>
          <div className="mt-5 flex flex-wrap items-center gap-2">
            <span className="text-sm text-fd-muted-foreground">Replaces</span>
            {piece.replaces.map((r) => (
              <span
                key={r}
                className="rounded-md border border-fd-border bg-fd-card/40 px-2.5 py-1 font-mono text-xs text-fd-muted-foreground"
              >
                {r}
              </span>
            ))}
          </div>
        </div>
        <div className="min-w-0">
          <Terminal lines={piece.lines} />
        </div>
      </div>

      {/* Carousel controls: chevrons flanking position dots. */}
      <div className="mt-10 flex items-center justify-center gap-4">
        <button
          type="button"
          aria-label="Previous"
          onClick={() => go(-1)}
          className="rounded-md border border-fd-border p-1.5 text-fd-muted-foreground hover:border-fd-foreground/30 hover:text-fd-foreground"
        >
          <Chevron dir="left" />
        </button>
        <div className="flex items-center gap-2">
          {PIECES.map((p, i) => (
            <button
              key={p.command}
              type="button"
              aria-label={`Show ${p.label}`}
              aria-current={i === active}
              onClick={() => setActive(i)}
              className={`h-2 w-2 rounded-full ${i === active ? TAB[p.accent].dot : 'bg-fd-border hover:bg-fd-muted-foreground'}`}
            />
          ))}
        </div>
        <button
          type="button"
          aria-label="Next"
          onClick={() => go(1)}
          className="rounded-md border border-fd-border p-1.5 text-fd-muted-foreground hover:border-fd-foreground/30 hover:text-fd-foreground"
        >
          <Chevron dir="right" />
        </button>
      </div>
    </div>
  );
}
