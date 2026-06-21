import { readFile } from 'node:fs/promises';
import path from 'node:path';
import Link from 'next/link';
import type { ReactNode } from 'react';
import { InstallTabs } from '@/components/install-tabs';
import { MigrationPrompt, ViewRepoLink } from '@/components/migration-prompt';
import { Terminal, Source, BenchBars } from '@/components/code';
import { ToolkitTabs } from '@/components/toolkit-tabs';
import { getLatestNode } from '@/lib/node-version';

/* The "Copy agent prompt" button copies start.md VERBATIM — start.md is the
   single source of truth for the adoption flow, so the button reproduces it
   byte-for-byte rather than paraphrasing or linking. Read the raw file at build
   time; on any read failure return undefined so the component falls back to a
   short pointer instead of breaking the build. */
async function getStartPrompt(): Promise<string | undefined> {
  try {
    return await readFile(
      path.join(process.cwd(), 'public', 'start.md'),
      'utf8',
    );
  } catch {
    return undefined;
  }
}

export default function HomePage() {
  return (
    <div className="relative w-full overflow-x-hidden">
      <Hero />
      <Toolkit />
      <RunFileBand />
      <RunScriptBand />
      <NubxBand />
      <HypermanagerBand />
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

/* An external link to upstream docs (Node, oxc). Neutral underline that brightens
   on hover; opens in a new tab. Wrap a <Mono> inside for a linked code term. */
function DocLink({ href, children }: { href: string; children: ReactNode }) {
  return (
    <a
      href={href}
      target="_blank"
      rel="noopener noreferrer"
      className="underline decoration-dotted decoration-fd-muted-foreground/50 underline-offset-4 hover:decoration-fd-muted-foreground"
    >
      {children}
    </a>
  );
}

/* Inline code sized for a display heading: monospace, a touch smaller than the
   serif around it, with a faint tinted pill so a command reads as a command. */
function HeadingCode({ children }: { children: ReactNode }) {
  return (
    <code className="rounded-md border border-fd-border/70 bg-fd-muted/40 px-2 py-0.5 align-[-0.09em] font-mono text-[0.66em] font-normal tracking-tight text-fd-foreground">
      {children}
    </code>
  );
}

type Accent = 'ember' | 'acid' | 'sky' | 'orchid' | 'pink';
const ACCENT_TEXT: Record<Accent, string> = {
  ember: 'text-ember',
  acid: 'text-acid',
  sky: 'text-sky',
  orchid: 'text-orchid',
  pink: 'text-pink',
};
const ACCENT_PILL: Record<Accent, string> = {
  ember: 'border-ember/40 text-ember',
  acid: 'border-acid/40 text-acid',
  sky: 'border-sky/40 text-sky',
  orchid: 'border-orchid/40 text-orchid',
  pink: 'border-pink/40 text-pink',
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

const heroLines = (major: string) => [
  { cmd: 'nub index.ts', comment: 'TypeScript-first Node.js runtime' },
  { cmd: 'nub run dev', comment: '24× faster pnpm run' },
  { cmd: 'nubx prisma generate', comment: '19× faster npx' },
  { cmd: 'nub install', comment: '2.5× faster pnpm install' },
  { cmd: 'nub watch src/server.ts', comment: 'native watch mode' },
  { cmd: 'nub pm shim', comment: 'built-in Corepack-style shims' },
  { cmd: `nub node install ${major}`, comment: 'Node version manager' },
];

function HeroPill() {
  return (
    <Link
      href="/blog/introducing-nub"
      className="group inline-flex items-center gap-2 rounded-full border border-fd-border bg-fd-card/50 py-1 pl-1 pr-3 text-sm leading-none text-fd-muted-foreground hover:border-ember/50"
    >
      <span className="rounded-full bg-ember px-2.5 py-0.5 font-mono text-[0.7rem] font-medium uppercase tracking-wider text-[#160c08]">
        New
      </span>
      <span className="translate-y-px text-fd-foreground">Introducing Nub</span>
      <span aria-hidden className="translate-y-px text-fd-muted-foreground group-hover:translate-x-0.5">
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
      The all-in-one JavaScript toolkit that{' '}
      <span className="italic text-ember">augments</span> Node.js instead of trying
      to replace it
    </h1>
  );
}

function HeroSub({ className = '' }: { className?: string }) {
  return (
    <p
      className={`text-balance text-lg leading-relaxed text-fd-muted-foreground md:text-xl ${className}`}
    >
      A TypeScript-first toolchain for Node.js. Run TypeScript files,{' '}
      <Mono>package.json</Mono>{' '}scripts, and local CLIs on the{' '}
      <span className="text-fd-foreground">node</span>{' '}and package manager you already
      have. No new runtime, no lock-in.
    </p>
  );
}

async function Hero() {
  const node = await getLatestNode();
  const startPrompt = await getStartPrompt();
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
      <div className="relative mx-auto flex min-h-[calc(100svh-3.5rem)] w-full max-w-[88rem] items-center px-6 py-16 sm:px-8">
        <div className="grid w-full items-center gap-12 xl:grid-cols-[minmax(0,1fr)_minmax(640px,1fr)] xl:gap-12">
          <div className="min-w-0">
            <HeroPill />
            <HeroH1 className="mt-6 text-4xl md:text-5xl" />
            <HeroSub className="mt-6" />
            <div className="mt-9">
              <InstallTabs />
            </div>
            <div className="mt-4 flex flex-wrap items-center gap-x-5 gap-y-2">
              <MigrationPrompt prompt={startPrompt} />
              <ViewRepoLink />
            </div>
          </div>
          <Terminal size="lg" className="w-full min-w-0 max-w-2xl xl:max-w-none" lines={heroLines(node.major)} />
        </div>
      </div>
    </section>
  );
}

/* ------------------------------------------------------------------- Toolkit */

/* Replaces the old "pile" section: a color-coded, auto-advancing overview of the
   four commands, introducing the accent system each band below reuses. The
   interactive tabs live in the ToolkitTabs client component. */
async function Toolkit() {
  const node = await getLatestNode();
  return (
    <section className="border-b border-fd-border">
      <Container className="py-28 md:py-[180px]">
        <div className="mx-auto max-w-2xl text-center">
          <p className="eyebrow text-fd-muted-foreground">The toolchain</p>
          <h2 className="mt-3 text-balance font-display text-3xl font-medium leading-tight md:text-4xl">
            An all-in-one toolkit for Node.js
          </h2>
          <p className="mt-4 text-balance text-lg leading-relaxed text-fd-muted-foreground">
            One Rust binary to run your files and scripts, install dependencies, and
            manage Node itself.
          </p>
        </div>
        <div className="mt-10">
          <ToolkitTabs node={node} />
        </div>
      </Container>
    </section>
  );
}

/* ----------------------------------------------------------- Band: nub <file> */

async function RunFileBand() {
  const node = await getLatestNode();
  return (
    <section className="border-b border-fd-border">
      <Container className="py-32 md:py-[180px]">
        <BandHeader
          command={'nub <file>'}
          title="A TypeScript-first Node.js"
          subhead={
            <>
              Nub adds support for TypeScript, JSX, decorators, <Mono>.env</Mono>{' '}files,
              YAML/TOML imports, and modern syntax and APIs on top of stock Node. Flag-for-flag
              compatible with <Mono>node</Mono>. Powered by Rust and oxc.
            </>
          }
          accent="ember"
        />

        <div className="mt-10 divide-y divide-fd-border/60">
          <Feature
            accent="ember"
            eyebrow="Architecture"
            title="Transpiles in Rust, runs on real Node"
            body={
              <>
                Nub transpiles your code in memory with{' '}
                <DocLink href="https://oxc.rs">oxc</DocLink>{' '}
                (compiled into a{' '}
                <DocLink href="https://nodejs.org/api/n-api.html">native Node addon</DocLink>) and
                runs the output on the stock{' '}
                <Mono>node</Mono>{' '}binary. There&rsquo;s no Nub runtime, just real Node.
                Runs on Node.js 18 LTS and newer.
              </>
            }
            visual={
              <Terminal
                lines={[
                  { cmd: 'nub app.ts' },
                  { out: '# oxc transpiles in memory, then stock node runs it' },
                  { out: `running on node v${node.full}` },
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
                Recent versions of Node support{' '}
                <DocLink href="https://nodejs.org/api/typescript.html">type stripping</DocLink>,
                which erases annotations but rejects non-erasable syntax. Nub&rsquo;s load hook
                transpiles each file through its native addon instead, so enums, parameter
                properties, and extensionless imports that Node doesn&rsquo;t allow all just work.
              </>
            }
            visual={
              <Source
                lang="tsx"
                code={`import { Model } from "./base"   // extensionless → ./base.ts

enum Status { Draft, Sent, Paid }

class Invoice extends Model {
  constructor(public status = Status.Draft) {} // parameter property
}`}
              />
            }
          />

          <Feature
            accent="ember"
            eyebrow="tsconfig"
            title="Respects your tsconfig.json"
            body={
              <>
                Nub resolves your <Mono>tsconfig.json</Mono>{' '}(including{' '}
                <Mono>{'"extends"'}</Mono>) and feeds its <Mono>paths</Mono>{' '}into Node&rsquo;s own
                resolver through a{' '}
                <DocLink href="https://nodejs.org/api/module.html#moduleregisterhooksoptions">
                  <Mono>module.registerHooks()</Mono>
                </DocLink>{' '}resolve hook. No more <Mono>tsconfig-paths</Mono>{' '}or disagreement
                between Node.js and your editor.
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
                <Mono>.env.[NODE_ENV]</Mono>{' '}and injects them before Node starts. No{' '}
                <Mono>dotenv</Mono>{' '}required. Automatic var expansion via{' '}
                <Mono>{'${VAR}'}</Mono>{' '}just like Vite and Next.js.
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
                Nub supports decorators and JSX, transpiling it according to your{' '}
                <Mono>tsconfig.json</Mono>{' '}settings. Full support for{' '}
                <DocLink href="https://www.typescriptlang.org/tsconfig/#emitDecoratorMetadata">
                  <Mono>emitDecoratorMetadata</Mono>
                </DocLink>{' '}and explicit resource management, no build step required.
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
                Import <Mono>.yml</Mono>, <Mono>.yaml</Mono>, <Mono>.toml</Mono>,{' '}
                <Mono>.json5</Mono>, and <Mono>.jsonc</Mono>{' '}files directly. A{' '}
                <DocLink href="https://nodejs.org/api/module.html#moduleregisterhooksoptions">
                  <Mono>module.registerHooks()</Mono>
                </DocLink>{' '}load hook routes them through fast Rust parsers in Nub&rsquo;s native
                addon, resolving each import to a plain JavaScript object. (Oh, <Mono>.txt</Mono>{' '}works too)
              </>
            }
            visual={
              <Source
                lang="ts"
                code={`import config from "./config.yaml"   // parsed object
import flags  from "./feature.jsonc" // comments stripped
import pkg    from "./Cargo.toml"    // parsed object
import prompt from "./prompt.txt"    // string

const { host, port } = config        // destructure fields`}
              />
            }
          />

          <Feature
            accent="ember"
            eyebrow="Auto-restart"
            title="A dependency-aware watch mode"
            body={
              <>
                Powered by{' '}
                <DocLink href="https://nodejs.org/api/cli.html#--watch">
                  <Mono>node --watch</Mono>
                </DocLink>, Nub&rsquo;s <Mono>watch</Mono>{' '}command
                watches for changes to your entrypoint or any file transitively imported.
                It also adds TypeScript/JSX sourcemap support and watches your <Mono>package.json</Mono>, tsconfigs, and{' '}
                <Mono>.env</Mono>{' '}files.
              </>
            }
            visual={
              <Terminal
                lines={[
                  { cmd: 'nub watch src/server.ts' },
                  { out: 'Listening on http://localhost:3000' },
                  { out: ' ' },
                  { out: '↺ src/db.ts changed — restarting' },
                  { out: 'Listening on http://localhost:3000' },
                ]}
              />
            }
          />

          <Feature
            accent="ember"
            reverse
            eyebrow="Node version management"
            title="Auto-installs Node, on demand"
            body={
              <>
                Nub reads your <Mono>.node-version</Mono>, <Mono>.nvmrc</Mono>, or{' '}
                <Mono>engines</Mono>/<Mono>devEngines</Mono>{' '}pin and runs your code on exactly
                that version. If it isn&rsquo;t on your machine, Nub downloads it from nodejs.org,
                verifies the checksum, and installs it on the fly — replacing <Mono>nvm</Mono>{' '}
                and <Mono>fnm</Mono>. You can also{' '}
                <Link
                  href="/docs/node"
                  className="underline decoration-dotted decoration-fd-muted-foreground/50 underline-offset-4 hover:decoration-fd-muted-foreground"
                >
                  manage versions manually
                </Link>.
              </>
            }
            visual={
              <Terminal
                lines={[
                  { cmd: `echo ${node.major} > .node-version` },
                  { cmd: 'nub hello.ts' },
                  { out: `Using Node.js ${node.full} (resolved from .node-version)` },
                  { out: 'Installed in 9.8s' },
                  { out: 'Hello world!' },
                ]}
              />
            }
          />

          <Feature
            accent="ember"
            eyebrow="Performance"
            title="Negligible overhead over plain Node"
            body={
              <>
                Nub transpiles each file in memory through its native Rust addon, then runs it on
                the real <Mono>node</Mono>{' '}binary. Its own startup is a few milliseconds of Rust,
                dwarfed by Node&rsquo;s, so a <Mono>.ts</Mono>{' '}file starts up on par with plain{' '}
                <Mono>node</Mono>{' '}and about 2.9× faster than <Mono>tsx</Mono>, which loads esbuild
                and its loader hooks on every run.
              </>
            }
            visual={
              <div className="nub-code-panel rounded-xl border p-6">
                {/* Source: benchmarks/results.md "Direct TS execution". */}
                <p className="nub-code-muted mb-5 font-mono text-[0.7rem] uppercase tracking-[0.14em]">
                  run a TypeScript file · macOS
                </p>
                <BenchBars
                  accent="ember"
                  max={128}
                  rows={[
                    { cmd: 'node hello.ts', ms: 44 },
                    { cmd: 'nub hello.ts', ms: 44, us: true },
                    { cmd: 'tsx hello.ts', ms: 128, ratio: 2.9 },
                  ]}
                />
                <a
                  href="https://github.com/nubjs/nub/blob/main/benchmarks/results.md#direct-ts-execution-nub-hellots-vs-node-vs-tsx-vs-bun"
                  target="_blank"
                  rel="noopener noreferrer"
                  className="nub-code-link nub-code-muted mt-3 inline-block py-1.5 font-mono text-[0.7rem] uppercase tracking-[0.14em] underline decoration-dotted underline-offset-4"
                >
                  View bench →
                </a>
              </div>
            }
          />

          <Compatibility />

          <Feature
            accent="ember"
            reverse
            eyebrow="truly drop-in"
            title={<>Flag-for-flag compatible with <HeadingCode>node</HeadingCode></>}
            body={
              <>
                Nub is <span className="italic">actually</span> a drop-in replacement for{' '}
                <Mono>node</Mono>. Every V8 and Node flag, <Mono>NODE_OPTIONS</Mono>, argv, exit
                codes, and signals behave identically — Nub forwards them straight to the real{' '}
                <Mono>node</Mono>{' '}it runs. Swap <Mono>node</Mono>{' '}for <Mono>nub</Mono>{' '}in
                any script, Dockerfile, or CI step; nothing else changes.
              </>
            }
            visual={
              <Terminal
                lines={[
                  {
                    cmd: `NODE_OPTIONS='--enable-source-maps' nub \\
  --max-old-space-size=8192 \\
  --import ./instrument.js \\
  app.ts --port 3000`,
                  },
                ]}
              />
            }
          />

          <Feature
            accent="ember"
            eyebrow="No Nub-specific APIs"
            title="Zero lock-in"
            body={
              <>
                Nub is <span className="text-fd-foreground">not a runtime</span>. Your code is
                run using stock <Mono>node</Mono>. Nub simply transpiles your code, polyfills
                missing global APIs, sets some flags, and makes additive modifications to
                Node&rsquo;s module resolution to improve TypeScript support.
              </>
            }
            visual={
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
            }
          />

          <ModernApis />
        </div>
      </Container>
    </section>
  );
}

/* Modern web-platform + TC39 APIs and syntax. */
const APIS: { name: string; label: string }[] = [
  { name: 'Web Workers', label: 'Auto-polyfilled' },
  { name: 'Temporal', label: 'Polyfilled < 26' },
  { name: 'URLPattern', label: 'Polyfilled < 24' },
  { name: 'WebSocket', label: 'Unflagged < 22' },
  { name: 'navigator.locks', label: 'Auto-polyfilled' },
  { name: 'localStorage', label: 'Auto-unflagged' },
  { name: 'using / await using', label: 'Transpiled' },
  { name: 'node:sqlite', label: 'Unflagged < 23' },
  { name: 'vm.Module', label: 'Auto-unflagged' },
  { name: 'RegExp.escape', label: 'Polyfilled < 24' },
  { name: 'Promise.try', label: 'Polyfilled < 24' },
  { name: 'Float16Array', label: 'Polyfilled < 24' },
];

function ModernApis() {
  return (
    <div className="py-14">
      <div className="mx-auto max-w-2xl text-center">
        <p className="eyebrow text-ember">Forward compatibility</p>
        <h3 className="mt-3 text-balance font-display text-2xl font-medium md:text-3xl">
          Modern APIs and syntax, fully supported
        </h3>
        <p className="mt-4 text-balance text-lg leading-relaxed text-fd-muted-foreground">
          Nub polyfills APIs like{' '}
          <DocLink href="https://tc39.es/proposal-temporal/"><Mono>Temporal</Mono></DocLink>{' '}and{' '}
          <DocLink href="https://developer.mozilla.org/en-US/docs/Web/API/Worker"><Mono>Worker</Mono></DocLink>, adds
          support for new ECMAScript syntax like{' '}
          <DocLink href="https://github.com/tc39/proposal-explicit-resource-management"><Mono>using</Mono></DocLink>, and unflags all
          experimental Node.js features.
        </p>
      </div>
      <div className="mt-10 grid grid-cols-2 gap-3 sm:grid-cols-3 lg:grid-cols-4">
        {APIS.map((api) => (
          <div
            key={api.name}
            className="rounded-lg border border-fd-border bg-fd-card/40 px-4 py-3.5"
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

/* ------------------------------------------------------------ Band: nub run */

function RunScriptBand() {
  return (
    <section className="border-b border-fd-border">
      <Container className="py-32 md:py-[180px]">
        <BandHeader
          command="nub run"
          title={<>A 24× faster <HeadingCode>pnpm run</HeadingCode></>}
          subhead={
            <>
              A drop-in for <Mono>npm run</Mono>{' '}and <Mono>pnpm run</Mono>{' '}with lifecycle
              hooks, <Mono>npm_*</Mono>{' '}env vars, and arg forwarding, without the
              JS startup these Node-based tools pay on every call.
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
                Whereas scripts run with <Mono>npm run</Mono>{' '}or <Mono>pnpm run</Mono>{' '}feel
                perceptibly laggy — they&rsquo;re Node.js programs, so each call cold-loads the
                package manager&rsquo;s own JavaScript (config, workspace probe, the works) before
                your script runs — Nub&rsquo;s runner is a Rust binary with no startup of its own.
              </>
            }
            visual={
              <div className="nub-code-panel rounded-xl border p-6">
                {/* Source: tests/bench/script-runner, warm script-dispatch bench, M1 Max, Node v26.2.0, hyperfine 50 runs. */}
                <p className="nub-code-muted mb-5 font-mono text-[0.7rem] uppercase tracking-[0.14em]">
                  script dispatch · warm · 50 runs · macOS
                </p>
                <BenchBars
                  accent="acid"
                  max={442.7}
                  rows={[
                    { cmd: 'nub run', ms: 14.7, us: true },
                    { cmd: 'node --run', ms: 32.2, ratio: 2.2 },
                    { cmd: 'npm run', ms: 329.9, ratio: 22 },
                    { cmd: 'pnpm run', ms: 442.7, ratio: 30 },
                  ]}
                />
                <a
                  href="https://github.com/nubjs/nub/tree/main/tests/bench/script-runner"
                  target="_blank"
                  rel="noopener noreferrer"
                  className="nub-code-link nub-code-muted mt-3 inline-block py-1.5 font-mono text-[0.7rem] uppercase tracking-[0.14em] underline decoration-dotted underline-offset-4"
                >
                  View bench →
                </a>
              </div>
            }
          />

          <Feature
            accent="acid"
            reverse
            eyebrow="Drop-in for pnpm run"
            title={<>Flag-for-flag compatible with <HeadingCode>pnpm run</HeadingCode></>}
            body={
              <>
                Nub accepts <Mono>pnpm run</Mono>&rsquo;s flags with the same spelling and
                semantics, down to the obscure recursive ones. Swap <Mono>pnpm</Mono>{' '}for{' '}
                <Mono>nub</Mono>{' '}and your CI scripts run unchanged, only faster.
              </>
            }
            visual={
              <Terminal
                lines={[
                  { cmd: 'nub -r --resume-from @org/api build', comment: 'resume from a package' },
                  { cmd: 'nub -r --reporter=ndjson build', comment: 'machine-readable CI summary' },
                  { cmd: 'nub -r --stream --reporter-hide-prefix build' },
                  { cmd: 'nub -r --workspace-concurrency 4 build' },
                  { cmd: 'nub -r --if-present test', comment: 'skip packages without it' },
                ]}
              />
            }
          />

          <Feature
            accent="acid"
            eyebrow="Workspaces"
            title="Monorepo-friendly"
            body={
              <>
                Nub implements pnpm&rsquo;s <Mono>--filter</Mono>{' '}grammar and{' '}
                <Mono>-r</Mono>, reading workspaces from <Mono>package.json#workspaces</Mono>{' '}
                or <Mono>pnpm-workspace.yaml</Mono>. Your existing filter commands work unchanged.
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
      <Container className="py-32 md:py-[180px]">
        <BandHeader
          command="nubx"
          title={<>A 19× faster <HeadingCode>npx</HeadingCode></>}
          subhead={
            <>
              The <Mono>nubx</Mono>{' '}command resolves <Mono>node_modules/.bin</Mono>{' '}in Rust
              and execs the binary directly — no Node process in the wrapper. A drop-in for{' '}
              <Mono>npx</Mono>{' '}and <Mono>pnpm dlx</Mono>: it runs a local bin, or fetches an
              uninstalled one from the registry.
            </>
          }
          accent="sky"
        />

        <div className="mt-10 divide-y divide-fd-border/60">
          <Feature
            accent="sky"
            eyebrow="Performance"
            title="Makes commands feel instantaneous"
            body={
              <>
                When invoking native CLIs like <Mono>esbuild</Mono>, <Mono>npx</Mono>{' '}
                itself (written in JS) adds a noticeable 200ms of cold-start latency, even
                when running a CLI command that&rsquo;s instantaneous. Nub walks{' '}
                <Mono>node_modules/.bin</Mono>{' '}and execs the binary directly.
              </>
            }
            visual={
              <div className="nub-code-panel rounded-xl border p-6">
                {/* Source: benchmarks/results.md "Bin runner". */}
                <p className="nub-code-muted mb-5 font-mono text-[0.7rem] uppercase tracking-[0.14em]">
                  esbuild --version · macOS
                </p>
                <BenchBars
                  accent="sky"
                  max={226}
                  rows={[
                    { cmd: 'nubx esbuild --version', ms: 11, us: true },
                    { cmd: 'pnpm exec esbuild --version', ms: 191, ratio: 17 },
                    { cmd: 'npx esbuild --version', ms: 226, ratio: 19 },
                  ]}
                />
                <a
                  href="https://github.com/nubjs/nub/blob/main/benchmarks/results.md#bin-runner-nubx--nub-exec-vs-pnpm-exec-vs-npx"
                  target="_blank"
                  rel="noopener noreferrer"
                  className="nub-code-link nub-code-muted mt-3 inline-block py-1.5 font-mono text-[0.7rem] uppercase tracking-[0.14em] underline decoration-dotted underline-offset-4"
                >
                  View bench →
                </a>
              </div>
            }
          />

          <Feature
            accent="sky"
            reverse
            eyebrow="Drop-in for pnpm exec"
            title={<>Flag-for-flag compatible with <HeadingCode>pnpm exec</HeadingCode></>}
            body={
              <>
                The <Mono>nubx</Mono>{' '}and <Mono>nub exec</Mono>{' '}commands take{' '}
                <Mono>pnpm exec</Mono>&rsquo;s flags, and <Mono>nub dlx</Mono>{' '}matches{' '}
                <Mono>pnpm dlx</Mono>, shell mode included. Swap <Mono>pnpm</Mono>{' '}for{' '}
                <Mono>nub</Mono>{' '}and the command you already know runs.
              </>
            }
            visual={
              <Terminal
                lines={[
                  { cmd: 'nub exec -r tsc --build', comment: 'across the workspace' },
                  { cmd: 'nub exec --parallel vitest', comment: 'every package at once' },
                  { cmd: "nub dlx -p cowsay -c 'cowsay hi | tr a-z A-Z'", comment: 'dlx shell mode' },
                ]}
              />
            }
          />

          <Feature
            accent="sky"
            eyebrow="Resolution"
            title="Works with any package manager"
            body={
              <>
                Nub resolves a locally-installed CLI from{' '}<Mono>node_modules/.bin</Mono>{' '}
                regardless of which package manager put it there — so you get Nub's
                performance without switching package managers.
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

/* ------------------------------------------------------------ Compatibility */

/* Source: tests/cross-runtime/results.json, using Deno's Node-compat corpus (colinhacks/node_test @ node-25.8.1). Rates = runtime_pass / node_pass. */
const COMPAT = [
  { name: 'Node 25.8', rate: 100, tests: '4,368 / 4,368', us: false, dim: false },
  { name: 'Nub', rate: 98.8, tests: '4,315 / 4,368', us: true, dim: false },
  { name: 'Deno 2.8', rate: 77.4, tests: '3,380 / 4,368', us: false, dim: true },
  { name: 'Bun 1.3.14', rate: 40.5, tests: '1,770 / 4,368', us: false, dim: true },
];

function Compatibility() {
  return (
    <div className="py-14">
        <div className="mx-auto max-w-2xl text-center">
          <p className="eyebrow text-ember">Compatibility</p>
          <h3 className="mt-3 text-balance font-display text-2xl font-medium leading-snug md:text-3xl">
            Node-compatible, because it <span className="italic">is</span> Node
          </h3>
          <p className="mt-5 text-balance text-lg leading-relaxed text-fd-muted-foreground">
            Your code is transpiled and executed with the stock <Mono>node</Mono>{' '}binary, so it
            runs on real Node, not a reimplementation. That&rsquo;s where the compatibility comes from.
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
                      <span className={`font-mono text-xs font-medium ${r.us ? 'text-fd-primary-foreground' : 'text-fd-foreground'}`}>
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
          Deno&rsquo;s Node-compat corpus, scored against stock Node.<br/>
          <a
            href="https://github.com/nubjs/nub/tree/main/tests/cross-runtime"
            target="_blank"
            rel="noopener noreferrer"
            className="text-fd-muted-foreground underline decoration-dotted decoration-fd-muted-foreground/60 underline-offset-4 hover:text-fd-foreground"
          >
            View benchmark repo
          </a>
        </p>
    </div>
  );
}

/* ------------------------------------------------------------------ Lock-in */

const RULES = [
  'No Nub global',
  'No nub:* module namespace',
  'No @nub/* npm scope',
  'No "nub" field in package.json',
  'No nub-named lockfile',
];

/* -------------------------------------------------------------- Final CTA */

/* ----------------------------------------------------------- Built-in package manager */

/* Per-config-field support across package managers. Cells derive directly from
   crates/nub-cli/src/pm_engine/config_scope.rs and pm_engine/mod.rs — do NOT
   edit a cell without changing the code it mirrors. Exception: the
   `packageExtensions` row has no pm_engine dialect-scoping (no per-PM conflict);
   the nub=yes cell is grounded in the embedded aube engine, which honors a
   top-level `packageExtensions` natively (vendor/aube/crates/aube-manifest/src/lib.rs
   `package_extensions()` → resolver package_ext.rs). Same for `allowBuilds` — a real
   pnpm field (pnpm-workspace.yaml; pnpm/core/types/src/package.ts) that aube reads via
   its pnpm-compat settings family; bun honors none of it (only `trustedDependencies`).
   Both bun=no cells verified: zero refs in bun source + docs. Legend:
     yes  — honored
     no   — ignored
     —    — n/a
   Notes encode the version gates the code enforces. */
const PM_COLUMNS = ['npm', 'pnpm', 'yarn', 'bun', 'nub'] as const;
type Cell = 'yes' | 'no' | 'na';
const PM_MATRIX: { field: ReactNode; cells: Record<(typeof PM_COLUMNS)[number], Cell> }[] = [
  {
    field: <><Mono>workspaces</Mono></>,
    cells: { npm: 'yes', pnpm: 'yes', yarn: 'yes', bun: 'yes', nub: 'yes' },
  },
  {
    field: <><Mono>overrides</Mono></>,
    cells: { npm: 'yes', pnpm: 'no', yarn: 'no', bun: 'yes', nub: 'yes' },
  },
  {
    field: <><Mono>resolutions</Mono></>,
    cells: { npm: 'no', pnpm: 'yes', yarn: 'yes', bun: 'yes', nub: 'yes' },
  },
  {
    field: <><Mono>catalog:</Mono></>,
    cells: { npm: 'no', pnpm: 'yes', yarn: 'no', bun: 'yes', nub: 'yes' },
  },
  {
    field: <><Mono>packageExtensions</Mono></>,
    cells: { npm: 'no', pnpm: 'yes', yarn: 'yes', bun: 'no', nub: 'yes' },
  },
  {
    field: <><Mono>allowBuilds</Mono></>,
    cells: { npm: 'no', pnpm: 'yes', yarn: 'no', bun: 'no', nub: 'yes' },
  },
  {
    field: <><Mono>trustedDependencies</Mono></>,
    cells: { npm: 'no', pnpm: 'no', yarn: 'no', bun: 'yes', nub: 'yes' },
  },
  {
    field: <><Mono>.npmrc</Mono></>,
    cells: { npm: 'yes', pnpm: 'yes', yarn: 'yes', bun: 'yes', nub: 'yes' },
  },
];

function PMMatrix() {
  const glyph = (c: Cell) =>
    c === 'yes' ? (
      <span className="text-pink">●</span>
    ) : c === 'no' ? (
      <span className="nub-code-muted opacity-45">○</span>
    ) : (
      <span className="nub-code-muted opacity-35">—</span>
    );
  return (
    <div className="nub-code-panel overflow-x-auto rounded-xl border">
      <table className="w-full border-collapse text-left font-mono text-sm">
        <thead>
          <tr className="nub-code-separator border-b">
            <th className="nub-code-muted px-4 py-3 font-normal">config field</th>
            {PM_COLUMNS.map((pm) => (
              <th
                key={pm}
                className={`px-4 py-3 text-center font-normal ${pm === 'nub' ? 'text-pink' : 'nub-code-muted'}`}
              >
                {pm}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {PM_MATRIX.map((row, i) => (
            <tr key={i} className="nub-code-row-border border-b last:border-0">
              <td className="nub-code-fg px-4 py-3">{row.field}</td>
              {PM_COLUMNS.map((pm) => (
                <td key={pm} className="px-4 py-3 text-center">
                  {glyph(row.cells[pm])}
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function HypermanagerBand() {
  return (
    <section className="border-b border-fd-border">
      <Container className="py-32 md:py-[180px]">
        <BandHeader
          command="nub install"
          title={
            <>
              A <span className="text-pink">2.5×</span> faster pnpm
            </>
          }
          subhead={
            <>
              A pnpm-compatible package manager, built in. It reads the lockfile your project
              already has — <Mono>pnpm</Mono>, <Mono>npm</Mono>, or <Mono>bun</Mono> — writes the
              same format back, and configures itself from your <Mono>.npmrc</Mono>{' '}and{' '}
              <Mono>workspaces</Mono>. Powered by the{' '}
              <a
                href="https://github.com/jdx/aube"
                target="_blank"
                rel="noopener noreferrer"
                className="text-fd-muted-foreground underline decoration-dotted decoration-fd-muted-foreground/60 underline-offset-4 hover:text-fd-foreground hover:decoration-fd-foreground"
              >
                aube
              </a>{' '}
              engine.
            </>
          }
          accent="pink"
        />

        <div className="mt-10 divide-y divide-fd-border/60">
          <Feature
            accent="pink"
            eyebrow="Meta package manager"
            title="Change package managers, keep your lockfile."
            body={
              <>
                Nub autodetects your current manager and updates your existing lockfile in
                place. No migration needed. Verified roundtrip compatibility for{' '}
                <Mono>package-lock.json</Mono>, <Mono>pnpm-lock.yaml</Mono>, and <Mono>bun.lock</Mono>.
              </>
            }
            visual={
              <Terminal
                lines={[
                  { cmd: 'nub install', comment: 'npm  package-lock.json → in place' },
                  { cmd: 'nub install', comment: 'pnpm pnpm-lock.yaml    → in place' },
                  { cmd: 'nub install', comment: 'bun  bun.lock          → in place' },
                ]}
              />
            }
          />

          <Feature
            accent="pink"
            reverse
            eyebrow="Drop-in for pnpm"
            title={<>Drop-in <HeadingCode>pnpm</HeadingCode> compatibility</>}
            body={
              <>
                Nub&rsquo;s <Mono>install</Mono>{' '}and <Mono>add</Mono>{' '}accept pnpm&rsquo;s flags
                with the same spelling and semantics, down to advanced features like the workspace
                catalog. Swap <Mono>pnpm</Mono>{' '}for <Mono>nub</Mono>{' '}and your install commands
                run unchanged.
              </>
            }
            visual={
              <Terminal
                lines={[
                  { out: '# exact pin · devDeps · workspace catalog' },
                  { cmd: 'nub add -E -D --save-catalog react' },
                  { cmd: 'nub install --frozen-lockfile --prefer-offline --node-linker hoisted' },
                ]}
              />
            }
          />

          <Feature
            accent="pink"
            eyebrow="Install speed"
            title="Ultrafast installs"
            body={
              <>
                Like pnpm, Nub keeps package files in a global content-addressed store and links
                them into{' '}<Mono>node_modules</Mono>. Nub embeds{' '}
                <DocLink href="https://github.com/jdx/aube">aube</DocLink>, a highly optimized
                Rust-based resolver and linker.
              </>
            }
            visual={
              <div className="nub-code-panel rounded-xl border p-6">
                {/* Source: tests/bench/install/results/warm-t3-20260617-{100017,100453,100743}.json (create-t3-app, Next 16), warm + frozen + offline, node_modules wiped between runs. Bars are arithmetic means across 36 timed runs. */}
                <p className="nub-code-muted mb-5 font-mono text-[0.7rem] uppercase tracking-[0.14em]">
                  warm frozen install · create-t3-app · 222 deps · macOS
                </p>
                <BenchBars
                  accent="pink"
                  max={4163}
                  unit="ms"
                  rows={[
                    { cmd: 'nub', ms: 1122, us: true },
                    { cmd: 'bun', ms: 1444, label: '29% slower' },
                    { cmd: 'pnpm', ms: 2847, ratio: 2.5 },
                    { cmd: 'npm', ms: 4163, ratio: 3.7 },
                  ]}
                />
                <a
                  href="https://github.com/nubjs/nub/blob/main/tests/bench/install/README.md"
                  target="_blank"
                  rel="noopener noreferrer"
                  className="nub-code-link nub-code-muted mt-3 inline-block py-1.5 font-mono text-[0.7rem] uppercase tracking-[0.14em] underline decoration-dotted underline-offset-4"
                >
                  View methodology →
                </a>
              </div>
            }
          />

          <Feature
            accent="pink"
            reverse
            eyebrow="Config compatibility"
            title="Mirrors your package manager's config rules"
            body={
              <>
                Nub supports all the listed configuration mechanisms, but toggles them on and off
                based on the conventions of your project&rsquo;s inferred package manager. There is
                no Nub-specific configuration file.
              </>
            }
            visual={<PMMatrix />}
          />
        </div>
      </Container>
    </section>
  );
}

async function FinalCta() {
  const startPrompt = await getStartPrompt();
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
      <Container className="relative py-32 text-center md:py-[180px]">
        <img
          src="/icon.svg"
          alt=""
          width={40}
          height={40}
          className="mx-auto mb-8 h-10 w-10 rounded-[12px] ring-1 ring-white/10"
        />
        <h2 className="text-balance font-display text-4xl font-medium leading-[1.05] md:text-6xl">
          The all-in-one toolkit for Node.js
        </h2>
        <div className="mt-10 flex flex-col items-center gap-4">
          <InstallTabs className="mx-auto" />
          <div className="flex flex-wrap items-center justify-center gap-x-5 gap-y-2">
            <MigrationPrompt prompt={startPrompt} />
            <ViewRepoLink />
          </div>
        </div>
      </Container>
    </section>
  );
}

/* A footer link column: a small heading + a list of internal/external links. */
function FooterCol({ title, links }: { title: string; links: [label: string, href: string][] }) {
  return (
    <div>
      <p className="text-sm font-medium text-fd-foreground">{title}</p>
      <ul className="mt-4 space-y-2.5">
        {links.map(([label, href]) => (
          <li key={href}>
            {href.startsWith('http') ? (
              <a href={href} className="text-sm text-fd-muted-foreground hover:text-fd-foreground">
                {label}
              </a>
            ) : (
              <Link href={href} className="text-sm text-fd-muted-foreground hover:text-fd-foreground">
                {label}
              </Link>
            )}
          </li>
        ))}
      </ul>
    </div>
  );
}

function Footer() {
  const year = new Date().getFullYear();
  return (
    <footer className="border-t border-fd-border">
      <Container className="py-14">
        <div className="grid gap-10 sm:grid-cols-2 md:grid-cols-[2fr_1fr_1fr]">
          <div className="max-w-xs">
            <span className="font-display text-lg text-fd-foreground">
              nub<span className="text-ember">.</span>
            </span>
            <p className="mt-3 text-sm leading-relaxed text-fd-muted-foreground">
              An all-in-one toolkit for Node.js.
            </p>
          </div>
          <FooterCol
            title="Resources"
            links={[
              ['Docs', '/docs'],
              ['Blog', '/blog'],
              ['FAQ', '/docs/faq'],
              ['GitHub', 'https://github.com/nubjs/nub'],
              ['License', 'https://github.com/nubjs/nub/blob/main/LICENSE'],
            ]}
          />
          <FooterCol
            title="Toolkit"
            links={[
              ['File runner', '/docs/runtime'],
              ['Script runner', '/docs/run'],
              ['Package runner', '/docs/nubx'],
              ['Package manager', '/docs/pm'],
              ['Version manager', '/docs/node'],
              ['Watch mode', '/docs/watch'],
            ]}
          />
        </div>
        <p className="mt-12 flex items-center justify-center gap-1.5 border-t border-fd-border pt-6 text-xs text-fd-muted-foreground">
          <span>© {year} Nub</span>
          <span aria-hidden>·</span>
          <a
            href="https://github.com/nubjs/nub/blob/main/LICENSE"
            className="inline-flex items-center gap-1 hover:text-fd-foreground"
          >
            <svg
              width="12"
              height="12"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="2"
              strokeLinecap="round"
              strokeLinejoin="round"
              aria-hidden
              className="-translate-y-[1.5px]"
            >
              <path d="m16 16 3-8 3 8c-.87.65-1.92 1-3 1s-2.13-.35-3-1Z" />
              <path d="m2 16 3-8 3 8c-.87.65-1.92 1-3 1s-2.13-.35-3-1Z" />
              <path d="M7 21h10" />
              <path d="M12 3v18" />
              <path d="M3 7h2c2 0 5-1 7-2 2 1 5 2 7 2h2" />
            </svg>
            MIT
          </a>
        </p>
      </Container>
    </footer>
  );
}
