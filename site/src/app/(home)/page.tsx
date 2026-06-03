import Link from 'next/link';
import type { Metadata } from 'next';
import type { ReactNode } from 'react';
import { InstallTabs } from '@/components/install-tabs';
import { Terminal, Source, BenchBars } from '@/components/code';

export const metadata: Metadata = {
  title: 'Nub — a fast runner CLI for Node.js',
};

export default function HomePage() {
  return (
    <div className="relative w-full overflow-x-hidden">
      <Hero />
      <Pile />
      <RunFileBand />
      <RunScriptBand />
      <NubxBand />
      <NodeVersionBand />
      <Compatibility />
      <FinalCta />
      <Footer />
    </div>
  );
}

/* --------------------------------------------------------------- primitives */

function Container({ children, className = '' }: { children: ReactNode; className?: string }) {
  return <div className={`mx-auto w-full max-w-7xl px-6 ${className}`}>{children}</div>;
}

function Mono({ children }: { children: ReactNode }) {
  return <span className="font-mono text-[0.84em] text-fd-foreground">{children}</span>;
}

/* Inline code sized for a display heading: monospace, a touch smaller than the
   serif around it, with a faint tinted pill so a command reads as a command. */
function HeadingCode({ children }: { children: ReactNode }) {
  return (
    <code className="rounded-md border border-fd-border/70 bg-fd-muted/40 px-2 py-0.5 align-[0.1em] font-mono text-[0.66em] font-normal tracking-tight text-fd-foreground">
      {children}
    </code>
  );
}

type Accent = 'ember' | 'acid' | 'sky' | 'orchid';
const ACCENT_TEXT: Record<Accent, string> = {
  ember: 'text-ember',
  acid: 'text-acid',
  sky: 'text-sky',
  orchid: 'text-orchid',
};
const ACCENT_PILL: Record<Accent, string> = {
  ember: 'border-ember/40 text-ember',
  acid: 'border-acid/40 text-acid',
  sky: 'border-sky/40 text-sky',
  orchid: 'border-orchid/40 text-orchid',
};

/* The centered top-of-band header: a command pill + serif title + subhead. */
function BandHeader({
  command,
  title,
  subhead,
  accent,
  showDollar = true,
}: {
  command: string;
  title: ReactNode;
  subhead: ReactNode;
  accent: Accent;
  showDollar?: boolean;
}) {
  return (
    <div className="mx-auto max-w-3xl text-center">
      <div
        className={`inline-flex items-center gap-2 rounded-full border bg-fd-card/50 px-4 py-1.5 font-mono text-sm ${ACCENT_PILL[accent]}`}
      >
        {showDollar ? <span aria-hidden>$</span> : null}
        <span>{command}</span>
      </div>
      <h2 className="mt-6 text-balance font-display text-4xl font-medium leading-[1.05] tracking-tight md:text-5xl">
        {title}
      </h2>
      <p className="mx-auto mt-5 max-w-2xl text-balance text-lg leading-relaxed text-fd-muted-foreground">
        {subhead}
      </p>
    </div>
  );
}

/* A subsection inside a band: small prose column + a visual, alternating side. */
function Feature({
  eyebrow,
  title,
  body,
  visual,
  accent,
  reverse = false,
}: {
  eyebrow: string;
  title: ReactNode;
  body: ReactNode;
  visual: ReactNode;
  accent: Accent;
  reverse?: boolean;
}) {
  return (
    <div className="grid items-center gap-12 py-14 xl:grid-cols-2">
      <div className={`min-w-0 ${reverse ? 'xl:order-2' : ''}`}>
        <p className={`eyebrow ${ACCENT_TEXT[accent]}`}>{eyebrow}</p>
        <h3 className="mt-3 text-balance font-display text-2xl font-medium leading-snug md:text-3xl">
          {title}
        </h3>
        <p className="mt-4 text-pretty text-lg leading-relaxed text-fd-muted-foreground">
          {body}
        </p>
      </div>
      <div className={`min-w-0 ${reverse ? 'xl:order-1' : ''}`}>{visual}</div>
    </div>
  );
}

/* ---------------------------------------------------------------- Hero variants */

const HERO_LINES_LONG = [
  { cmd: 'nub index.ts', comment: 'run a TypeScript file' },
  { cmd: 'nub run dev', comment: 'run a package.json script' },
  { cmd: 'nub watch src/server.ts', comment: 'restart on changes' },
  { cmd: 'nubx prisma generate', comment: 'run a local CLI, fast' },
  { cmd: 'nub node install 26', comment: 'manage Node.js versions' },
];

function HeroPill() {
  return (
    <Link
      href="/blog/introducing-nub"
      className="group inline-flex items-center gap-2 rounded-full border border-fd-border bg-fd-card/50 py-1 pl-1 pr-3 text-sm leading-none text-fd-muted-foreground transition hover:border-ember/50"
    >
      <span className="rounded-full bg-ember px-2.5 py-0.5 font-mono text-[0.7rem] font-medium uppercase tracking-wider text-[#160c08]">
        New
      </span>
      <span className="translate-y-px text-fd-foreground">Introducing Nub</span>
      <span aria-hidden className="translate-y-px text-fd-muted-foreground transition group-hover:translate-x-0.5">
        →
      </span>
    </Link>
  );
}

function HeroH1({ className = '' }: { className?: string }) {
  return (
    <h1
      className={`text-balance font-display font-medium leading-[1.05] tracking-tight text-fd-foreground ${className}`}
    >
      The unified JavaScript toolkit that{' '}
      <span className="italic text-ember">augments</span> Node.js instead of trying
      to replace it.
    </h1>
  );
}

function HeroSub({ className = '' }: { className?: string }) {
  return (
    <p
      className={`text-balance text-lg leading-relaxed text-fd-muted-foreground md:text-xl ${className}`}
    >
      A TypeScript-first Node.js. Run TypeScript files, <Mono>package.json</Mono>{' '}
      scripts, and local CLIs on the <span className="text-fd-foreground">node</span>{' '}
      and package manager you already have — just faster, and with TypeScript built
      in. No new runtime, no lock-in.
    </p>
  );
}

function Hero() {
  return (
    <section className="relative border-b border-fd-border">
      <div
        aria-hidden
        className="pointer-events-none absolute inset-0 opacity-50"
        style={{
          background:
            'radial-gradient(55% 50% at 50% -5%, rgba(255,93,59,0.16), transparent 70%)',
        }}
      />
      {/* Wider than the rest of the page (smaller gutters) so the H1 has room
          and never breaks past 3 lines. Stacks to one column below xl. */}
      <div className="relative mx-auto grid w-full max-w-[88rem] items-center gap-12 px-6 py-24 sm:px-8 xl:grid-cols-[minmax(0,1fr)_minmax(0,1fr)]">
        <div className="min-w-0">
          <HeroPill />
          <HeroH1 className="mt-6 text-4xl md:text-5xl" />
          <HeroSub className="mt-6" />
          <div className="mt-9">
            <InstallTabs />
          </div>
        </div>
        <Terminal size="lg" className="w-full min-w-0 max-w-xl xl:max-w-none" lines={HERO_LINES_LONG} />
      </div>
    </section>
  );
}

/* ---------------------------------------------------------------------- Pile */

const PILE = [
  'node', 'npm run', 'npx', 'tsx', 'ts-node',
  'dotenv', 'cross-env', 'nodemon', 'tsconfig-paths',
];

function Pile() {
  return (
    <section className="border-b border-fd-border">
      <Container className="py-16">
        <div className="grid gap-8 lg:grid-cols-[0.8fr_1.2fr] lg:items-center">
          <div>
            <p className="eyebrow text-fd-muted-foreground">The pile</p>
            <h2 className="mt-3 text-balance font-display text-3xl font-medium leading-tight md:text-4xl">
              One binary instead of a drawer full.
            </h2>
            <p className="mt-4 text-balance text-lg leading-relaxed text-fd-muted-foreground">
              A typical Node project leans on half a dozen single-purpose CLIs to do
              what one tool should. Nub is that tool — without asking you to change
              your runtime.
            </p>
          </div>
          <div className="flex flex-wrap gap-3">
            {PILE.map((p) => (
              <span
                key={p}
                className="rounded-md border border-fd-border bg-fd-card/40 px-3.5 py-1.5 font-mono text-sm text-fd-muted-foreground line-through decoration-ember/70 decoration-2"
              >
                {p}
              </span>
            ))}
            <span className="rounded-md border border-ember/50 bg-ember/10 px-3.5 py-1.5 font-mono text-sm font-medium text-ember">
              nub
            </span>
          </div>
        </div>
      </Container>
    </section>
  );
}

/* ----------------------------------------------------------- Band: nub <file> */

function RunFileBand() {
  return (
    <section className="border-b border-fd-border">
      <Container className="py-20">
        <BandHeader
          command={'nub <file>'}
          title="A TypeScript-first Node.js"
          subhead="Run .ts, .tsx, and .js directly on the Node you already have. Nub transpiles on the fly with oxc and gets out of the way — no build step, no config, no separate runtime."
          accent="ember"
        />

        <div className="mt-10 divide-y divide-fd-border/60">
          <Feature
            accent="ember"
            eyebrow="Drop-in"
            title="A true drop-in for node"
            body={
              <>
                Running <Mono>nub app.ts</Mono>{' '}is flag-for-flag compatible with{' '}
                <Mono>node app.ts</Mono>{' '}— same argv, same flags, same behavior —
                because your code executes on the Node you already have. Nub adds the
                modern defaults and steps aside; if it vanished tomorrow, your code
                keeps running.
              </>
            }
            visual={
              <Terminal
                lines={[
                  { cmd: 'nub --inspect server.ts', comment: 'attach a debugger' },
                  { cmd: 'nub --import ./instrument.js app.ts' },
                  { cmd: 'nub app.ts --port 3000', comment: 'argv passes through' },
                  { cmd: 'echo "1+1" | nub -', comment: 'read from stdin' },
                ]}
              />
            }
          />

          <Feature
            accent="ember"
            reverse
            eyebrow="TypeScript-first"
            title="Full TypeScript support, not just type stripping"
            body={
              <>
                Vanilla node strips types and chokes on everything past that: enums
                error, parameter properties error, the path alias your editor resolves
                fine throws at runtime. Nub transpiles the whole TypeScript surface —
                so the code your IDE already understands runs with one{' '}
                <Mono>nub app.ts</Mono>.
              </>
            }
            visual={
              <Source
                lang="tsx"
                code={`// app.ts
import { logger } from "@/logger"   // tsconfig path alias
import { render } from "./invoice"  // extensionless → .ts

enum Status { Draft, Sent, Paid }   // node's stripper errors here

class Invoice {
  constructor(public id: string) {} // parameter property
}

logger.info(render(new Invoice("INV-1")))`}
              />
            }
          />

          <Feature
            accent="ember"
            eyebrow="tsconfig"
            title="Respects your tsconfig.json"
            body={
              <>
                Nub reads your <Mono>tsconfig.json</Mono>{' '}and applies it at runtime, so
                the imports your editor resolves actually resolve when you run:{' '}
                <Mono>paths</Mono>{' '}and <Mono>baseUrl</Mono>{' '}aliases like{' '}
                <Mono>@/db</Mono>, walked <Mono>extends</Mono>{' '}chains, and your{' '}
                <Mono>jsx</Mono>{' '}and decorator settings. No <Mono>tsconfig-paths</Mono>{' '}
                shim, no build step.
              </>
            }
            visual={
              <Source
                lang="json"
                code={`// tsconfig.json
{
  "compilerOptions": {
    "baseUrl": ".",
    "paths": {
      "@/*": ["src/*"],
      "@db": ["src/db/index.ts"]
    }
  }
}`}
              />
            }
          />

          <Feature
            accent="ember"
            reverse
            eyebrow="Environment"
            title="Loads .env files automatically"
            body={
              <>
                Nub reads <Mono>.env</Mono>, <Mono>.env.local</Mono>, and{' '}
                <Mono>.env.[NODE_ENV]</Mono>{' '}and injects them before Node starts — no{' '}
                <Mono>dotenv</Mono>, no <Mono>cross-env</Mono>. Same files and precedence
                as Vite and Next.js, with <Mono>{'${VAR}'}</Mono>{' '}expansion built in.
              </>
            }
            visual={
              <Source
                lang="bash"
                code={`# .env
APP=acme
DATABASE_URL=postgres://localhost/\${APP}_dev

# No dotenv. No cross-env. No import "dotenv/config".
$ nub server.ts`}
              />
            }
          />

          <Feature
            accent="ember"
            eyebrow="Modern syntax"
            title={<>Decorators, JSX, and <HeadingCode>using</HeadingCode></>}
            body={
              <>
                Legacy decorators with <Mono>emitDecoratorMetadata</Mono>, JSX for any
                runtime, and explicit resource management with <Mono>using</Mono> /{' '}
                <Mono>await using</Mono>{' '}— all compiled in memory by Nub&rsquo;s
                oxc-powered transpiler, with no flags or build step.
              </>
            }
            visual={
              <Source
                lang="tsx"
                code={`await using db = await connect()    // disposed at scope end

@sealed                             // legacy decorator
class User {}

const view = <Hello name="world" /> // JSX in .tsx`}
              />
            }
          />

          <Feature
            accent="ember"
            reverse
            eyebrow="Loaders"
            title="Import JSON, YAML, and TOML"
            body={
              <>
                Import a config or data file directly — no <Mono>yaml</Mono> /{' '}
                <Mono>json5</Mono>{' '}dependency to install. <Mono>.yaml</Mono>,{' '}
                <Mono>.toml</Mono>, <Mono>.json5</Mono>, <Mono>.jsonc</Mono>, and{' '}
                <Mono>.txt</Mono>{' '}all load the way <Mono>.json</Mono>{' '}already does.
              </>
            }
            visual={
              <Source
                lang="ts"
                code={`import config from "./config.yaml"   // parsed object
import flags  from "./feature.jsonc" // comments stripped
import pkg    from "./Cargo.toml"    // parsed object
import prompt from "./prompt.txt"    // string

import { host, port } from "./config.yaml" // named exports`}
              />
            }
          />

        </div>

        <ModernApis />
        <WatchBlock />
        <LockinBlock />
      </Container>
    </section>
  );
}

/* Modern web-platform + TC39 globals — "browser APIs on the server". */
const APIS: { name: string; label: string }[] = [
  { name: 'Temporal', label: 'Polyfilled < 26' },
  { name: 'URLPattern', label: 'Polyfilled < 24' },
  { name: 'WebSocket', label: 'Polyfilled < 22.5' },
  { name: 'Worker', label: 'Auto-polyfilled' },
  { name: 'navigator.locks', label: 'Auto-polyfilled' },
  { name: 'localStorage', label: 'Unflagged < 25' },
  { name: 'EventSource', label: 'Auto-unflagged' },
  { name: 'node:sqlite', label: 'Unflagged < 24' },
  { name: 'vm.Module', label: 'Auto-unflagged' },
  { name: 'RegExp.escape', label: 'Polyfilled < 24' },
  { name: 'Promise.try', label: 'Polyfilled < 24' },
  { name: 'Float16Array', label: 'Polyfilled < 24' },
];

function ModernApis() {
  return (
    <div className="mt-16 border-t border-fd-border/60 pt-14">
      <div className="mx-auto max-w-2xl text-center">
        <p className="eyebrow text-ember">Forward compatibility</p>
        <h3 className="mt-3 text-balance font-display text-2xl font-medium md:text-3xl">
          Modern APIs and syntax, fully supported
        </h3>
        <p className="mt-4 text-balance text-lg leading-relaxed text-fd-muted-foreground">
          All experimental Node.js features are unflagged, missing APIs like{' '}
          <Mono>Worker</Mono>{' '}are available globally, and new ECMAScript syntax
          (e.g. <Mono>using</Mono>) is supported via transpiler downleveling when
          possible.
        </p>
      </div>
      <div className="mt-10 grid grid-cols-2 gap-3 sm:grid-cols-3 lg:grid-cols-4">
        {APIS.map((api) => (
          <div
            key={api.name}
            className="rounded-lg border border-fd-border bg-fd-card/40 px-4 py-3.5 transition hover:border-ember/50"
          >
            <div className="font-mono text-sm text-fd-foreground">{api.name}</div>
            <div className="mt-1 font-mono text-[0.7rem] uppercase tracking-wider text-fd-muted-foreground">
              {api.label}
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}

/* ------------------------------------------------- Band: Node version mgmt */

function NodeVersionBand() {
  return (
    <section className="border-b border-fd-border">
      <Container className="py-20">
        <BandHeader
          command="nub node"
          title="A built-in Node version manager"
          subhead={
            <>
              Nub reads your <Mono>.node-version</Mono>{' '}or <Mono>.nvmrc</Mono>{' '}and, if
              that Node isn&rsquo;t on your machine, downloads the matching build from
              nodejs.org — verified and cached. The uv experience, for Node.
            </>
          }
          accent="orchid"
        />
        <div className="mt-10">
          <Feature
            accent="orchid"
            eyebrow="Version management"
            title="Pin a version, Nub fetches it"
            body={
              <>
                Drop a <Mono>.node-version</Mono>{' '}or <Mono>.nvmrc</Mono>{' '}in your
                project and run <Mono>nub</Mono>. If you don&rsquo;t have that Node,
                it downloads from nodejs.org, verifies the checksum, and caches it.
                No <Mono>nvm use</Mono>, no prompt, no second step.
              </>
            }
            visual={
              <Terminal
                lines={[
                  { cmd: 'echo 26 > .node-version' },
                  { cmd: 'nub hello.ts' },
                  { out: 'Installing Node 26 from nodejs.org…' },
                  { out: 'Hello world!' },
                ]}
              />
            }
          />
        </div>
      </Container>
    </section>
  );
}

/* ------------------------------------------------------------ Band: nub run */

function RunScriptBand() {
  return (
    <section className="border-b border-fd-border">
      <Container className="py-20">
        <BandHeader
          command="nub run"
          title={<>An 18× faster <HeadingCode>pnpm run</HeadingCode></>}
          subhead={
            <>
              Both <Mono>npm</Mono>{' '}and <Mono>pnpm</Mono>{' '}boot an entire Node process
              just to look up a script — roughly 150 ms of wrapper tax on every call. Nub
              is a Rust binary that skips it, so the same script runs an order of magnitude
              faster. It doesn&rsquo;t make your code faster — it gets the wrapper out of
              the way.
            </>
          }
          accent="acid"
        />

        <div className="mt-10 divide-y divide-fd-border/60">
          <Feature
            accent="acid"
            eyebrow="Performance"
            title="Run package.json scripts at the speed of Rust"
            body={
              <>
                A drop-in for <Mono>npm run</Mono>{' '}and <Mono>pnpm run</Mono>{' '}that keeps
                lifecycle hooks, <Mono>npm_*</Mono>{' '}env vars, arg forwarding, and
                pnpm&rsquo;s <Mono>--filter</Mono>{' '}grammar — without the Node bootstrap
                those tools pay on every call.
              </>
            }
            visual={
              <div className="rounded-xl border border-fd-border bg-[#0b0a08] p-6">
                <p className="mb-5 font-mono text-[0.7rem] uppercase tracking-[0.14em] text-fd-muted-foreground">
                  echo-hi script · hyperfine, 20 runs
                </p>
                <BenchBars
                  accent="acid"
                  max={161}
                  rows={[
                    { cmd: 'nub run echo-hi', ms: 9, us: true },
                    { cmd: 'npm run echo-hi', ms: 104, ratio: 11 },
                    { cmd: 'pnpm run echo-hi', ms: 161, ratio: 18 },
                  ]}
                />
              </div>
            }
          />

          <Feature
            accent="acid"
            reverse
            eyebrow="Monorepo"
            title="Monorepo workspace support"
            body={
              <>
                Nub implements pnpm&rsquo;s <Mono>--filter</Mono>{' '}grammar and{' '}
                <Mono>-r</Mono>, reading workspaces from either{' '}
                <Mono>package.json#workspaces</Mono>{' '}or <Mono>pnpm-workspace.yaml</Mono>.
                Packages run in dependency order — without the per-package bootstrap that
                pnpm pays on every one.
              </>
            }
            visual={
              <Terminal
                lines={[
                  { cmd: 'nub -r run build', comment: 'every package, topo-ordered' },
                  { cmd: 'nub --filter @org/api dev', comment: 'one package' },
                  { cmd: 'nub --filter ...@org/web build', comment: '+ its deps' },
                  { cmd: 'nub --filter "[main]" test', comment: 'changed since main' },
                ]}
              />
            }
          />
        </div>
      </Container>
    </section>
  );
}

/* --------------------------------------------------------------- Band: nubx */

function NubxBand() {
  return (
    <section className="border-b border-fd-border">
      <Container className="py-20">
        <BandHeader
          command="nubx"
          title={<>A 20× faster <HeadingCode>npx</HeadingCode></>}
          subhead={
            <>
              Run a project&rsquo;s local CLIs with <Mono>nubx</Mono>: it resolves{' '}
              <Mono>node_modules/.bin</Mono>{' '}in Rust and execs the binary
              directly — no Node process in the wrapper. A drop-in for{' '}
              <Mono>npx</Mono>{' '}and <Mono>pnpm exec</Mono>.
            </>
          }
          accent="sky"
        />

        <div className="mt-10 divide-y divide-fd-border/60">
          <Feature
            accent="sky"
            eyebrow="Performance"
            title="No Node bootstrap in the wrapper"
            body={
              <>
                For a native CLI like <Mono>esbuild</Mono>, the entire delta is wrapper
                overhead. Whereas <Mono>npx</Mono>{' '}and <Mono>pnpm exec</Mono>{' '}boot a
                full Node first, Nub walks <Mono>node_modules/.bin</Mono>{' '}and execs the
                binary directly. The binary that runs is identical; the ~150 ms tax is
                gone.
              </>
            }
            visual={
              <div className="rounded-xl border border-fd-border bg-[#0b0a08] p-6">
                <p className="mb-5 font-mono text-[0.7rem] uppercase tracking-[0.14em] text-fd-muted-foreground">
                  esbuild --version · hyperfine, 20 runs
                </p>
                <BenchBars
                  accent="sky"
                  max={226}
                  rows={[
                    { cmd: 'nubx esbuild --version', ms: 11, us: true },
                    { cmd: 'pnpm exec esbuild --version', ms: 191, ratio: 17 },
                    { cmd: 'npx esbuild --version', ms: 226, ratio: 20 },
                  ]}
                />
              </div>
            }
          />

          <Feature
            accent="sky"
            reverse
            eyebrow="Resolution"
            title="Works with any package manager"
            body={
              <>
                Nub resolves the CLI the way <Mono>pnpm</Mono>,{' '}<Mono>yarn</Mono>, and{' '}
                <Mono>npm</Mono>{' '}do, so it runs the exact binary your install put there —
                even in a monorepo. Add <Mono>--node</Mono>{' '}to run one under plain Node.
              </>
            }
            visual={
              <Terminal
                lines={[
                  { cmd: 'nubx eslint .', comment: "member's .bin first" },
                  { cmd: 'nubx prisma generate', comment: 'then workspace root' },
                  { cmd: 'nubx tsc --noEmit', comment: 'then ancestors' },
                  { cmd: 'nubx --node some-cli', comment: 'run under plain Node' },
                ]}
              />
            }
          />
        </div>
      </Container>
    </section>
  );
}

/* Watch mode, rendered as a subsection of the runtime band (RunFileBand). */
function WatchBlock() {
  return (
    <div className="mt-16 border-t border-fd-border/60 pt-14">
      <div className="grid items-center gap-12 xl:grid-cols-2">
        <div className="min-w-0">
          <p className="eyebrow text-ember">Watch mode</p>
          <h3 className="mt-3 text-balance font-display text-2xl font-medium leading-snug md:text-3xl">
            A dependency-aware watch mode
          </h3>
          <p className="mt-4 text-pretty text-lg leading-relaxed text-fd-muted-foreground">
            Powered by Node&rsquo;s built-in <Mono>--watch</Mono>, <Mono>nub watch</Mono>{' '}
            restarts on changes to the resolved module graph — every source file your entry
            imports, including the <Mono>.ts</Mono>{' '}Nub transpiles on the fly, which a bare{' '}
            <Mono>node --watch</Mono>{' '}would miss. It also tracks the files that shape a run
            but aren&rsquo;t imported — your <Mono>.env*</Mono>{' '}files, the{' '}
            <Mono>tsconfig.json</Mono>{' '}extends chain, and <Mono>package.json</Mono>. No glob
            list to maintain.
          </p>
        </div>
        <div className="min-w-0">
          <Terminal
            lines={[
              { cmd: 'nub watch src/server.ts' },
              { out: 'Listening on http://localhost:3000' },
              { out: ' ' },
              { out: '↺ src/db.ts changed — restarting' },
              { out: 'Listening on http://localhost:3000' },
            ]}
          />
        </div>
      </div>
    </div>
  );
}

/* ------------------------------------------------------------ Compatibility */

const COMPAT = [
  { name: 'Node 25.8', rate: 100, tests: '4,366 / 4,366', us: false, dim: false },
  { name: 'Nub', rate: 98.7, tests: '4,309 / 4,366', us: true, dim: false },
  { name: 'Deno 2.8', rate: 76.7, tests: '3,347 / 4,366', us: false, dim: true },
  { name: 'Bun 1.3.14', rate: 40.2, tests: '1,754 / 4,366', us: false, dim: true },
];

function Compatibility() {
  return (
    <section className="border-b border-fd-border">
      <Container className="py-20">
        <div className="mx-auto max-w-2xl text-center">
          <p className="eyebrow text-ember">Compatibility</p>
          <h2 className="mt-3 text-balance font-display text-4xl font-medium leading-[1.05] md:text-5xl">
            It passes Node&rsquo;s test suite because it <span className="italic">is</span> Node
          </h2>
          <p className="mt-5 text-balance text-lg leading-relaxed text-fd-muted-foreground">
            Your code runs on the actual <Mono>node</Mono>{' '}binary, not a reimplementation, so there
            is no separate compatibility surface to chase. The other runtimes are still
            playing catch-up.
          </p>
        </div>

        <div className="mx-auto mt-12 max-w-3xl space-y-5">
          {COMPAT.map((r) => {
            // Short bars can't fit the label inside the fill (it gets clipped),
            // so for anything under ~22% the label sits just outside the fill.
            const labelInside = r.rate >= 22;
            return (
              <div key={r.name} className="grid grid-cols-[5.5rem_1fr_auto] items-center gap-3 sm:grid-cols-[7.5rem_1fr_auto] sm:gap-4">
                <span className={`font-mono text-sm ${r.us ? 'font-semibold text-ember' : 'text-fd-foreground'}`}>
                  {r.name}
                </span>
                <div className="flex h-8 items-center overflow-hidden rounded-md bg-fd-card/50">
                  <div
                    className={`flex h-full shrink-0 items-center justify-end pr-3 ${r.us ? 'bg-ember/85' : r.dim ? 'bg-fd-foreground/15' : 'bg-fd-foreground/25'}`}
                    style={{ width: `${r.rate}%` }}
                  >
                    {labelInside ? (
                      <span className={`font-mono text-xs font-medium ${r.us ? 'text-[#160c08]' : 'text-fd-foreground'}`}>
                        {r.rate}%
                      </span>
                    ) : null}
                  </div>
                  {labelInside ? null : (
                    <span className="ml-2 font-mono text-xs font-medium text-fd-foreground">
                      {r.rate}%
                    </span>
                  )}
                </div>
                <span className="font-mono text-xs tabular-nums text-fd-muted-foreground">{r.tests}</span>
              </div>
            );
          })}
        </div>
        <p className="mx-auto mt-6 max-w-lg text-center text-sm leading-relaxed text-fd-muted-foreground">
          Running against Deno&rsquo;s Node-compat suite, node-relative. The 1% that differs — a
          handful of tests pinning Node&rsquo;s exact internals (error wording, the built-in module
          list), which shift when TypeScript and source maps are on.{' '}
          <span className="italic">Deliberate</span>, documented — nothing missing.{' '}
          <a
            href="https://github.com/nub-js/nub/tree/main/tests/cross-runtime"
            target="_blank"
            rel="noopener noreferrer"
            className="text-sky underline underline-offset-4"
          >
            View benchmark repo
          </a>
        </p>
      </Container>
    </section>
  );
}

/* ------------------------------------------------------------------ Lock-in */

const RULES = [
  'No Nub global',
  'No nub:* module namespace',
  'No @nub/* npm scope',
  'No NUB_* environment variables',
  'No "nub" field in package.json',
];

/* Rendered as a subsection inside the runtime band (RunFileBand), not a
   standalone section — the "it's just Node, no lock-in" message closes the
   runtime story. */
function LockinBlock() {
  return (
    <div className="mt-16 border-t border-fd-border/60 pt-14">
      <div className="grid gap-12 lg:grid-cols-2 lg:items-center">
        <div>
          <p className="eyebrow text-ember">The brand stops at the binary</p>
          <h3 className="mt-3 text-balance font-display text-2xl font-medium leading-snug md:text-3xl">
            Zero lock-in.
          </h3>
          <p className="mt-4 text-pretty text-lg leading-relaxed text-fd-muted-foreground">
            Nub is <span className="text-fd-foreground">not a runtime</span>. Your code
            runs on the real <Mono>node</Mono>{' '}binary — no Nub
            engine, no reimplementation, no proprietary API surface. Everything Nub
            ships is a web standard, a TC39 proposal, an unflagged Node feature, or a
            pragmatic TypeScript affordance. Remove Nub tomorrow and your code keeps
            working, unchanged.
          </p>
        </div>
        <ul className="space-y-3">
          {RULES.map((rule) => (
            <li
              key={rule}
              className="flex items-center gap-3 border-b border-fd-border/60 pb-3 font-mono text-sm text-fd-foreground"
            >
              <span className="text-ember" aria-hidden>✗</span>
              {rule}
            </li>
          ))}
        </ul>
      </div>
    </div>
  );
}

/* -------------------------------------------------------------- Final CTA */

function FinalCta() {
  return (
    <section className="relative border-b border-fd-border">
      <div
        aria-hidden
        className="pointer-events-none absolute inset-0 opacity-60"
        style={{
          background:
            'radial-gradient(50% 60% at 50% 120%, rgba(255,93,59,0.14), transparent 70%)',
        }}
      />
      <Container className="relative py-28 text-center">
        <h2 className="text-balance font-display text-4xl font-medium leading-[1.05] md:text-6xl">
          The toolkit that <span className="italic text-ember">augments</span> Node.js.
        </h2>
        <div className="mt-10 flex flex-col items-center">
          <InstallTabs className="mx-auto" />
        </div>
      </Container>
    </section>
  );
}

function Footer() {
  return (
    <footer className="border-fd-border">
      <Container className="flex flex-col items-center justify-between gap-4 py-10 text-sm text-fd-muted-foreground sm:flex-row">
        <span className="font-display text-base text-fd-foreground">
          nub<span className="text-ember">.</span>
        </span>
        <div className="flex items-center gap-6">
          <Link href="/docs" className="hover:text-fd-foreground">Docs</Link>
          <Link href="/blog" className="hover:text-fd-foreground">Blog</Link>
          <a href="https://github.com/nub-js/nub" className="hover:text-fd-foreground">GitHub</a>
          <a href="https://github.com/nub-js/nub/blob/main/LICENSE" className="hover:text-fd-foreground">License</a>
        </div>
      </Container>
    </footer>
  );
}
