'use client';

import { useState } from 'react';

/* Minimal fallback used only if the start.md read fails at build time, so the
   button still copies something actionable rather than an empty string. The
   real payload is start.md's full text, sourced from the file and passed in via
   the `prompt` prop (see the home page) — start.md is the single source of
   truth and the button reproduces it verbatim. */
const FALLBACK_PROMPT =
  'Adopt Nub (https://nubjs.com) in this Node.js project: read https://nubjs.com/start.md and follow the steps there. The full docs are at https://nubjs.com/llms.txt.';

export function MigrationPrompt({
  prompt,
  className = '',
}: {
  /* The exact text the button copies — start.md's content, read at build time.
     Omitted/empty falls back to the pointer above. */
  prompt?: string;
  className?: string;
}) {
  const [copied, setCopied] = useState(false);
  const payload = prompt && prompt.length > 0 ? prompt : FALLBACK_PROMPT;

  async function copy() {
    try {
      await navigator.clipboard.writeText(payload);
      setCopied(true);
      setTimeout(() => setCopied(false), 1600);
    } catch {
      /* clipboard unavailable */
    }
  }

  return (
    <button
      type="button"
      onClick={copy}
      aria-label="Copy agent prompt"
      className={`group inline-flex items-center gap-1.5 text-sm leading-none text-fd-muted-foreground hover:text-ember ${className}`}
    >
      <span className="inline-flex h-4 w-4 shrink-0 -translate-y-px items-center justify-center">
        {copied ? (
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" aria-hidden>
            <path d="M20 6 9 17l-5-5" stroke="currentColor" strokeWidth="2.2" strokeLinecap="round" strokeLinejoin="round" />
          </svg>
        ) : (
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" aria-hidden>
            <rect x="9" y="9" width="11" height="11" rx="2" stroke="currentColor" strokeWidth="2" />
            <path d="M5 15V5a2 2 0 0 1 2-2h10" stroke="currentColor" strokeWidth="2" strokeLinecap="round" />
          </svg>
        )}
      </span>
      <span className="underline-offset-4 group-hover:underline">Copy agent prompt</span>
    </button>
  );
}

/* A link to the project's GitHub repo, styled to match MigrationPrompt: a small
   inline glyph + muted label that brightens to ember on hover. The octocat path
   is the same one used in ai-actions.tsx's "Open on GitHub" menu item. */
export function ViewRepoLink({ className = '' }: { className?: string }) {
  return (
    <a
      href="https://github.com/nubjs/nub"
      target="_blank"
      rel="noopener noreferrer"
      aria-label="View repo on GitHub"
      className={`group inline-flex items-center gap-1.5 text-sm leading-none text-fd-muted-foreground hover:text-ember ${className}`}
    >
      <span className="inline-flex h-4 w-4 shrink-0 -translate-y-px items-center justify-center">
        <svg viewBox="0 0 24 24" fill="currentColor" width="16" height="16" aria-hidden>
          <path d="M12 .297c-6.63 0-12 5.373-12 12 0 5.303 3.438 9.8 8.205 11.385.6.113.82-.258.82-.577 0-.285-.01-1.04-.015-2.04-3.338.724-4.042-1.61-4.042-1.61C4.422 18.07 3.633 17.7 3.633 17.7c-1.087-.744.084-.729.084-.729 1.205.084 1.838 1.236 1.838 1.236 1.07 1.835 2.809 1.305 3.495.998.108-.776.417-1.305.76-1.605-2.665-.3-5.466-1.332-5.466-5.93 0-1.31.465-2.38 1.235-3.22-.135-.303-.54-1.523.105-3.176 0 0 1.005-.322 3.3 1.23.96-.267 1.98-.399 3-.405 1.02.006 2.04.138 3 .405 2.28-1.552 3.285-1.23 3.285-1.23.645 1.653.24 2.873.12 3.176.765.84 1.23 1.91 1.23 3.22 0 4.61-2.805 5.625-5.475 5.92.42.36.81 1.096.81 2.22 0 1.606-.015 2.896-.015 3.286 0 .315.21.69.825.57C20.565 22.092 24 17.592 24 12.297c0-6.627-5.373-12-12-12" />
        </svg>
      </span>
      <span className="underline-offset-4 group-hover:underline">View repo</span>
    </a>
  );
}
