'use client';

import { useEffect, useState } from 'react';

type TabId = 'unix' | 'windows' | 'npm';

const TABS: { id: TabId; label: string; command: string }[] = [
  { id: 'unix', label: 'macOS / Linux', command: 'curl -fsSL https://nubjs.com/install.sh | bash' },
  { id: 'windows', label: 'Windows', command: 'powershell -c "irm https://nubjs.com/install.ps1 | iex"' },
  { id: 'npm', label: 'npm', command: 'npm install -g @nubjs/nub' },
];

export function InstallTabs({ className = '' }: { className?: string }) {
  const [active, setActive] = useState<TabId>('unix');
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    if (/Windows/i.test(navigator.userAgent)) setActive('windows');
  }, []);

  const command = TABS.find((t) => t.id === active)!.command;

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
    <div className={`w-full max-w-xl text-left ${className}`}>
      {/* Tab strip + command box form one connected panel (shared border, no
          seam). Symmetric padding keeps the labels vertically centered. */}
      <div
        role="tablist"
        aria-label="Install method"
        className="flex items-center gap-1 rounded-t-lg border-x border-t border-fd-border bg-fd-card/60 p-1.5"
      >
        {TABS.map((t) => (
          <button
            key={t.id}
            type="button"
            role="tab"
            aria-selected={active === t.id}
            onClick={() => setActive(t.id)}
            className={`rounded-md px-3.5 py-1.5 font-mono text-xs leading-none ${
              active === t.id
                ? 'bg-fd-muted text-fd-foreground'
                : 'text-fd-muted-foreground hover:text-fd-foreground'
            }`}
          >
            {t.label}
          </button>
        ))}
      </div>

      <button
        type="button"
        onClick={copy}
        aria-label={`Copy: ${command}`}
        className="group flex w-full items-center gap-3 rounded-b-lg border border-fd-border bg-fd-card/60 px-5 py-4 text-left font-mono text-[0.95rem] text-fd-foreground backdrop-blur hover:border-ember/50 hover:bg-ember/[0.06]"
      >
        <span className="select-none text-ember">$</span>
        <span className="min-w-0 flex-1 truncate">{command}</span>
        <span className="inline-flex w-4 shrink-0 justify-center text-fd-muted-foreground group-hover:text-ember">
          {copied ? (
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" aria-hidden>
              <path d="M20 6 9 17l-5-5" stroke="currentColor" strokeWidth="2.2" strokeLinecap="round" strokeLinejoin="round" />
            </svg>
          ) : (
            <svg width="15" height="15" viewBox="0 0 24 24" fill="none" aria-hidden>
              <rect x="9" y="9" width="11" height="11" rx="2" stroke="currentColor" strokeWidth="2" />
              <path d="M5 15V5a2 2 0 0 1 2-2h10" stroke="currentColor" strokeWidth="2" strokeLinecap="round" />
            </svg>
          )}
        </span>
      </button>
    </div>
  );
}
