'use client';

import { useState } from 'react';

export function CopyInstall({
  command = 'npm i -g @nubjs/nub',
  className = '',
}: {
  command?: string;
  className?: string;
}) {
  const [copied, setCopied] = useState(false);

  async function copy() {
    try {
      await navigator.clipboard.writeText(command);
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
      className={`group inline-flex items-center gap-3 rounded-full border border-fd-border bg-fd-card/60 px-5 py-2.5 font-mono text-sm text-fd-foreground backdrop-blur transition hover:border-ember/60 ${className}`}
      aria-label={`Copy: ${command}`}
    >
      <span className="select-none text-ember">$</span>
      <span className="tabular-nums">{command}</span>
      <span className="ml-1 inline-flex w-4 justify-center text-fd-muted-foreground transition group-hover:text-ember">
        {copied ? (
          <svg width="15" height="15" viewBox="0 0 24 24" fill="none" aria-hidden>
            <path d="M20 6 9 17l-5-5" stroke="currentColor" strokeWidth="2.2" strokeLinecap="round" strokeLinejoin="round" />
          </svg>
        ) : (
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" aria-hidden>
            <rect x="9" y="9" width="11" height="11" rx="2" stroke="currentColor" strokeWidth="2" />
            <path d="M5 15V5a2 2 0 0 1 2-2h10" stroke="currentColor" strokeWidth="2" strokeLinecap="round" />
          </svg>
        )}
      </span>
    </button>
  );
}
