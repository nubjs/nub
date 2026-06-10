import Link from 'next/link';
import type { Metadata } from 'next';
import type { ReactNode } from 'react';
import { InstallTabs } from '@/components/install-tabs';
import { Terminal, Source, BenchBars } from '@/components/code';
import { ToolkitTabs } from '@/components/toolkit-tabs';

export const metadata: Metadata = {
  title: 'Nub — a fast runner CLI for Node.js',
};

export default function HomePage() {
  return (
    <div className="relative w-full overflow-x-hidden">
      <Hero />
      <Toolkit />
      <RunFileBand />
      <RunScriptBand />
      <NubxBand />
      <NodeVersionBand />
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
      className="underline decoration-fd-border underline-offset-4 hover:decoration-fd-muted-foreground"
    >
      {children}
    </a>
  );
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
      The unified JavaScript toolkit that{' '}
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
      <div className="relative mx-auto flex min-h-[calc(100svh-3.5rem)] w-full max-w-[88rem] items-center px-6 py-16 sm:px-8">
        <div className="grid w-full items-center gap-12 xl:grid-cols-[minmax(0,1fr)_minmax(0,1fr)] xl:gap-20">
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
      </div>
    </section>
  );
}

/* ------------------------------------------------------------------- Toolkit */

/* Replaces the old "pile" section: a color-coded, auto-advancing overview of the
   four commands, introducing the accent system each band below reuses. The
   interactive tabs live in the ToolkitTabs client component. */
function Toolkit() {
  return (
    <section className="border-b border-fd-border">
      <Container className="py-28 md:py-[180px]">
        <div className="mx-auto max-w-2xl text-center">
          <p className="eyebrow text-fd-muted-foreground">The toolchain</p>
          <h2 className="mt-3 text-balance font-display text-3xl font-medium leading-tight md:text-4xl">
            A unified toolchain for Node.js
          </h2>
          <p className="mt-4 text-balance text-lg leading-relaxed text-fd-muted-foreground">
            One Rust binary that runs your files, scripts, and local CLIs — and manages
            Node itself.
          </p>
        </div>
        <div className="mt-10">
          <ToolkitTabs />
        </div>
      </Container>
    </section>
  );
}

/* ----------------------------------------------------------- Band: nub <file> */

function RunFileBand() {
  return (
    <section className="border-b border-fd-border">
      <Container className="py-32 md:py-[180px]">
        <BandHeader
          command={'nub <file>'}
          title="A TypeScript-first Node.js"
          subhead={
            <>
              Nub adds support for TypeScript, JSX, decorators, <Mono>.env</Mono>{' '}files,
              YAML/TOML imports, and modern APIs syntax on top of stock Node. Flag-for-flag
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
                <Mono>node</Mono>{' '}binary — there&rsquo;s no Nub runtime, just real Node.
                Your code is run by the version of Node your project expects. If unavailable,
                it&rsquo;s installed on the fly. Runs on Node.js 18 LTS and newer.
              </>
            }
            visual={
              <Terminal
                lines={[
                  { cmd: 'nub app.ts' },
                  { out: '# oxc transpiles in memory, then stock node runs it' },
                  { out: 'running on node v26.2.0' },
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
                <Mono>.env.[NODE_ENV]</Mono>{' '}and injects them before Node starts — no{' '}
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
                <Mono>emitDecoratorMetadata</Mono>{' '}and explicit resource management, no
                build step required.
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

import { host, port } from "./config.yaml" // named exports`}
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

          <Compatibility />

          <Feature
            accent="ember"
            eyebrow="Drop-in"
            title={<>Flag-for-flag compatible with <HeadingCode>node</HeadingCode></>}
            body={
              <>
                Nub is a true drop-in replacement for <Mono>node</Mono>. Same flags, same
                argv, same runtime behavior.
              </>
            }
            visual={
              <Terminal
                lines={[
                  {
                    cmd: `nub \\
  --max-old-space-size=4096 \\
  --inspect \\
  --import ./instrument.js \\
  app.ts --port 3000`,
                  },
                ]}
              />
            }
          />

          <Feature
            accent="ember"
            reverse
            eyebrow="The brand stops at the binary"
            title="Zero lock-in"
            body={
              <>
                Nub is <span className="text-fd-foreground">not a runtime</span>. Your code
                runs on the real <Mono>node</Mono>{' '}binary — no Nub engine, no
                reimplementation, no proprietary API surface. Everything Nub ships is a web
                standard, a TC39 proposal, an unflagged Node feature, or a pragmatic
                TypeScript affordance. Remove Nub tomorrow and your code keeps working,
                unchanged.
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

/* Modern web-platform + TC39 globals — "browser APIs on the server". */
const APIS: { name: string; label: string }[] = [
  { name: 'Web Workers', label: 'Auto-polyfilled' },
  { name: 'Temporal', label: 'Polyfilled < 26' },
  { name: 'URLPattern', label: 'Polyfilled < 24' },
  { name: 'WebSocket', label: 'Polyfilled < 22.5' },
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
    <div className="py-14">
      <div className="mx-auto max-w-2xl text-center">
        <p className="eyebrow text-ember">Forward compatibility</p>
        <h3 className="mt-3 text-balance font-display text-2xl font-medium md:text-3xl">
          Modern APIs and syntax, fully supported
        </h3>
        <p className="mt-4 text-balance text-lg leading-relaxed text-fd-muted-foreground">
          Nub polyfills APIs like <Mono>Temporal</Mono>{' '}and <Mono>Worker</Mono>, adds
          support for new ECMAScript syntax like <Mono>using</Mono>, and unflags all
          experimental Node.js features.
        </p>
      </div>
      <div className="mt-10 grid grid-cols-2 gap-3 sm:grid-cols-3 lg:grid-cols-4">
        {APIS.map((api) => (
          <div
            key={api.name}
            className="rounded-lg border border-fd-border bg-fd-card/40 px-4 py-3.5 hover:border-ember/50"
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
      <Container className="py-32 md:py-[180px]">
        <BandHeader
          command="nub node"
          title="A built-in Node version manager"
          subhead={
            <>
              Nub reads your <Mono>.node-version</Mono>{' '}or <Mono>.nvmrc</Mono>{' '}and, if
              that Node isn&rsquo;t installed, downloads it from nodejs.org —
              checksum-verified and installed. Replaces <Mono>nvm</Mono>{' '}and <Mono>fnm</Mono>.
            </>
          }
          accent="orchid"
        />
        <div className="mt-10 divide-y divide-fd-border/60">
          <Feature
            accent="orchid"
            eyebrow="Per-project"
            title="Resolves your project's Node version"
            body={
              <>
                Nub resolves the right Node for each project from <Mono>.node-version</Mono>,{' '}
                <Mono>.nvmrc</Mono>, or <Mono>package.json#engines</Mono> — automatically,
                before your code runs.
              </>
            }
            visual={
              <Terminal
                lines={[
                  { cmd: 'nub node which' },
                  { out: '~/.nub/node/26.2.0/bin/node' },
                  { out: '» resolved from package.json#engines (>=26)' },
                ]}
              />
            }
          />

          <Feature
            accent="orchid"
            reverse
            eyebrow="On demand"
            title="Auto-installs Node versions"
            body={
              <>
                If the resolved version isn&rsquo;t on your machine, Nub downloads it from
                nodejs.org — checksum-verified and installed — then runs your code on it.
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

          <Feature
            accent="orchid"
            eyebrow="Direct control"
            title="Or manage versions by hand"
            body={
              <>
                Install, list, pin, and remove Node versions directly with{' '}
                <Mono>nub node</Mono>. No shell hooks, no <Mono>PATH</Mono>{' '}munging.
              </>
            }
            visual={
              <Terminal
                lines={[
                  { cmd: 'nub node install 26', comment: 'install a version' },
                  { cmd: 'nub node ls', comment: "what's installed" },
                  { cmd: 'nub node pin 26', comment: 'write .node-version' },
                  { cmd: 'nub node uninstall 22', comment: 'remove a version' },
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
      <Container className="py-32 md:py-[180px]">
        <BandHeader
          command="nub run"
          title={<>An 18× faster <HeadingCode>pnpm run</HeadingCode></>}
          subhead={
            <>
              A drop-in for <Mono>npm run</Mono>{' '}and <Mono>pnpm run</Mono>{' '}with lifecycle
              hooks, <Mono>npm_*</Mono>{' '}env vars, and arg forwarding all working, minus the
              per-call Node bootstrap.
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
                Scripts run with <Mono>npm run</Mono>{' '}or <Mono>pnpm run</Mono>{' '}feel
                perceptibly laggy due to the 100+ms of overhead introduced by these tools.
                They&rsquo;re written in Node.js themselves, so they pay the Node.js
                bootstrap tax.
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
                <a
                  href="https://github.com/nubjs/nub/tree/main/benchmarks"
                  target="_blank"
                  rel="noopener noreferrer"
                  className="mt-5 inline-block font-mono text-[0.7rem] uppercase tracking-[0.14em] text-sky underline underline-offset-4"
                >
                  Reproduce →
                </a>
              </div>
            }
          />

          <Feature
            accent="acid"
            reverse
            eyebrow="Workspaces"
            title="Monorepo-friendly"
            body={
              <>
                Nub implements pnpm&rsquo;s <Mono>--filter</Mono>{' '}grammar and{' '}
                <Mono>-r</Mono>, reading workspaces from <Mono>package.json#workspaces</Mono>{' '}
                or <Mono>pnpm-workspace.yaml</Mono>. Packages run in dependency order —
                without the per-package Node bootstrap.
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
          title={<>A 20× faster <HeadingCode>npx</HeadingCode></>}
          subhead={
            <>
              <Mono>nubx</Mono>{' '}resolves <Mono>node_modules/.bin</Mono>{' '}in Rust and
              execs the binary directly — no Node process in the wrapper. A drop-in for{' '}
              <Mono>npx</Mono>{' '}and <Mono>pnpm exec</Mono>.
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
                itself (written in JS) adds a noticeable 200ms of cold-start latency — even
                when running a CLI command that&rsquo;s instantaneous. Nub walks{' '}
                <Mono>node_modules/.bin</Mono>{' '}and execs the binary directly.
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
                <a
                  href="https://github.com/nubjs/nub/tree/main/benchmarks"
                  target="_blank"
                  rel="noopener noreferrer"
                  className="mt-5 inline-block font-mono text-[0.7rem] uppercase tracking-[0.14em] text-sky underline underline-offset-4"
                >
                  Reproduce →
                </a>
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

/* ------------------------------------------------------------ Compatibility */

const COMPAT = [
  { name: 'Node 25.8', rate: 100, tests: '4,366 / 4,366', us: false, dim: false },
  { name: 'Nub', rate: 98.7, tests: '4,309 / 4,366', us: true, dim: false },
  { name: 'Deno 2.8', rate: 76.7, tests: '3,347 / 4,366', us: false, dim: true },
  { name: 'Bun 1.3.14', rate: 40.2, tests: '1,754 / 4,366', us: false, dim: true },
];

function Compatibility() {
  return (
    <div className="py-14">
        <div className="mx-auto max-w-2xl text-center">
          <p className="eyebrow text-ember">Compatibility</p>
          <h3 className="mt-3 text-balance font-display text-2xl font-medium leading-snug md:text-3xl">
            100% runtime compatibility with Node
          </h3>
          <p className="mt-5 text-balance text-lg leading-relaxed text-fd-muted-foreground">
            Nub passes Node&rsquo;s test suite because it <span className="italic">is</span>{' '}
            Node. Your code is transpiled and executed with the stock <Mono>node</Mono>{' '}
            binary. It&rsquo;s not a reimplementation; other Node alternatives continue to
            play catch-up.
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
          Deno&rsquo;s Node-compat suite, node-relative. The 1% gap is unavoidable divergence due
          to Nub&rsquo;s module-hook preload, unflagging of experimental features, and use of native addons.{' '}
          <a
            href="https://github.com/nubjs/nub/tree/main/tests/cross-runtime"
            target="_blank"
            rel="noopener noreferrer"
            className="text-sky underline underline-offset-4"
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
  'No NUB_* environment variables',
  'No "nub" field in package.json',
];

/* -------------------------------------------------------------- Final CTA */

/* ----------------------------------------------------------- Package hypermanager */

function HypermanagerBand() {
  return (
    <section className="border-b border-fd-border">
      <Container className="py-32 md:py-[180px]">
        <BandHeader
          command="nub pm"
          title="A package manager manager"
          subhead={
            <>
              Not a new package manager — Nub provisions and runs the one your project pins.
              Corepack&rsquo;s job, in native Rust: the exact npm, pnpm, or yarn is fetched, verified,
              cached, and run on the project&rsquo;s Node.
            </>
          }
          accent="pink"
        />

        <div className="mt-10 divide-y divide-fd-border/60">
          <Feature
            accent="pink"
            eyebrow="Pin + provision"
            title="Pin a version, get it everywhere"
            body={
              <>
                <Mono>nub pm pin</Mono>{' '}takes an exact version, a range, or a tag — resolves it,
                fetches and integrity-verifies the tarball, and writes the pin to{' '}
                <Mono>package.json</Mono>. Every machine and CI runner provisions the same manager on
                demand; a cached pin runs fully offline.
              </>
            }
            visual={
              <Terminal
                lines={[
                  { cmd: 'nub pm pin pnpm@^9' },
                  { out: 'Fetching pnpm 9.15.4 (4 MB)...' },
                  { out: '✓ Installed pnpm 9.15.4 in 0.7s' },
                  { out: 'pinned pnpm@9.15.4 → package.json' },
                  { cmd: 'nub pm which' },
                  { out: '~/.cache/nub/pm/pnpm/9.15.4/package/bin/pnpm.cjs' },
                  { out: '» resolved from packageManager (pnpm@9.15.4)' },
                ]}
              />
            }
          />

          <Feature
            accent="pink"
            reverse
            eyebrow="nub pm shim"
            title="Bare commands, pinned versions"
            body={
              <>
                Opt-in shims make <em>bare</em>{' '}<Mono>npm</Mono>, <Mono>pnpm</Mono>, and{' '}
                <Mono>yarn</Mono>{' '}— typed from muscle memory, or spawned by tools you don&rsquo;t
                control — run the pinned version. The wrong manager is refused before it can write a
                competing lockfile.
              </>
            }
            visual={
              <Terminal
                lines={[
                  { cmd: 'nub pm shim', comment: 'opt-in, one-time' },
                  { cmd: 'pnpm --version' },
                  { out: '9.15.4' },
                  { cmd: 'npm install' },
                  { out: 'nub: this project pins pnpm — refusing to run npm.', bright: true },
                ]}
              />
            }
          />

          <Feature
            accent="pink"
            eyebrow="vs Corepack"
            title="Corepack, without the Node tax"
            body={
              <>
                Every Corepack shim boots a whole Node process just to decide which pnpm to run. A Nub
                shim <em>is</em>{' '}the native binary — resolution costs zero Node boots, then it
                execs the real manager. Unpinned projects fall through to the manager already on your{' '}
                <Mono>PATH</Mono>{' '}(Corepack never does), and there&rsquo;s no baked version table to
                go stale.
              </>
            }
            visual={
              <Terminal
                lines={[
                  { cmd: 'corepack enable', comment: 'every call boots Node to resolve' },
                  { cmd: 'nub pm shim', comment: 'native resolution, then exec' },
                ]}
              />
            }
          />

          <Feature
            accent="pink"
            reverse
            eyebrow="nub pm"
            title="Take control when you want it"
            body={
              <>
                The <Mono>nub pm</Mono>{' '}commands surface it all explicitly: see what resolves and
                why, pin or switch managers, bump within your range, or inspect the binary cache.
              </>
            }
            visual={
              <Terminal
                lines={[
                  { cmd: 'nub pm update' },
                  { out: 'pinned pnpm@9.15.9 → package.json' },
                  { cmd: 'nub pm cache' },
                  { out: 'pnpm@9.15.9' },
                  { out: 'yarn@1.22.22' },
                ]}
              />
            }
          />
        </div>
      </Container>
    </section>
  );
}

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
      <Container className="relative py-32 text-center md:py-[180px]">
        <h2 className="text-balance font-display text-4xl font-medium leading-[1.05] md:text-6xl">
          The toolkit that <span className="italic text-ember">augments</span> Node.js
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
          <a href="https://github.com/nubjs/nub" className="hover:text-fd-foreground">GitHub</a>
          <a href="https://github.com/nubjs/nub/blob/main/LICENSE" className="hover:text-fd-foreground">License</a>
        </div>
      </Container>
    </footer>
  );
}
