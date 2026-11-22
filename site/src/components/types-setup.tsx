'use client';

import { useState, type ReactNode } from 'react';

/* lucide `Clipboard` / `Check` glyphs (24×24 viewBox, stroke-width 2, rendered at
   16px = Tailwind size-4) — a 1:1 match for the icon Fumadocs paints on every
   code-block copy button, so this control reads identically to the copy buttons
   the user already sees on the `<pre>` blocks just below it. */
function CopyIcon({ copied }: { copied: boolean }) {
  return copied ? (
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
      <path d="M20 6 9 17l-5-5" />
    </svg>
  ) : (
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
      <rect width="8" height="4" x="8" y="2" rx="1" ry="1" />
      <path d="M16 4h2a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2h2" />
    </svg>
  );
}

const DEFAULT_INSTALL = 'npm i -D @nubjs/types @types/node@26';
const DEFAULT_TSCONFIG = `{ "compilerOptions": { "types": ["node", "@nubjs/types"] } }`;

export function TypesSetup({
  install = DEFAULT_INSTALL,
  tsconfig = DEFAULT_TSCONFIG,
  children,
}: {
  /** The install command shown + copied. */
  install?: string;
  /** The tsconfig.json snippet shown + copied. */
  tsconfig?: string;
  /** Page-specific one-liner: which types come from where. */
  children?: ReactNode;
}) {
  const [copied, setCopied] = useState(false);

  /* Agent-ready directive: a short, pasteable instruction the user drops into
     their AI coding agent. Plain imperative prose + the two snippets. */
  const prompt = [
    'Set up TypeScript types for nub in this project:',
    '',
    `1. Install the type packages: \`${install}\``,
    '',
    `2. In tsconfig.json, set compilerOptions.types: \`${tsconfig}\``,
  ].join('\n');

  async function copy() {
    try {
      await navigator.clipboard.writeText(prompt);
      setCopied(true);
      setTimeout(() => setCopied(false), 1600);
    } catch {
      /* clipboard unavailable */
    }
  }

  return (
    /* The whole setup lives in ONE bordered callout card. Inside: the title + note +
       a single merged code block (install line + tsconfig snippet) — one inner box, so
       it no longer reads as a box-in-a-box. The subtle copy control sits in the CARD's
       top-right and copies the agent-ready setup prompt for the whole block. */
    <div className="relative my-6 rounded-lg border border-fd-border bg-fd-card/40 px-5 py-4">
      {/* Subtle copy control — bordered `bg-fd-secondary` chip + clipboard glyph + hover,
          matching the Fumadocs code-block copy button so it's visible (the old bare glyph
          was invisible). Covers the WHOLE card; copies the agent-ready prompt, not raw text. */}
      <button
        type="button"
        onClick={copy}
        aria-label="Copy TypeScript setup instructions for your AI agent"
        title="Copy setup instructions for your AI agent"
        data-checked={copied || undefined}
        className="absolute right-3 top-3 z-10 inline-flex items-center justify-center rounded-md border bg-fd-secondary p-1 text-fd-muted-foreground transition-colors duration-100 hover:bg-fd-accent hover:text-fd-accent-foreground data-checked:text-fd-accent-foreground"
      >
        <CopyIcon copied={copied} />
      </button>

      <div className="mb-1 pr-8 font-medium text-fd-foreground">TypeScript setup</div>

      {children ? <div className="mb-3 text-[0.9375rem] text-fd-muted-foreground [&>p]:m-0">{children}</div> : null}

      <pre className="overflow-x-auto rounded-lg border border-fd-border bg-fd-card/60 px-4 py-3.5 font-mono text-[0.9rem] leading-relaxed text-fd-foreground">{`# install the package\n${install}\n\n# tsconfig.json\n${tsconfig}`}</pre>
    </div>
  );
}
