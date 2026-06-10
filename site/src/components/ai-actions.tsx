'use client';

/**
 * On-page AI actions: a "Copy Markdown" button plus an "Open" dropdown with
 * "View Markdown", "Open in ChatGPT", "Open in Claude", and "Open in Cursor".
 *
 * Hand-written for this project. Fumadocs does NOT export these as components
 * from `fumadocs-ui` (the official path is `npx @fumadocs/cli add ai/page-actions`,
 * which scaffolds source that imports `lucide-react`, `@/utils/cn`,
 * `@/components/ui/popover`, and the i18n context — none of which are wired
 * here, and `lucide-react` is not a dependency of this site). So this is a
 * dependency-light reimplementation:
 *
 *   - copy logic + the `ClipboardItem` async-write pattern are taken verbatim
 *     from the official scaffold;
 *   - the `useCopyButton` hook is the real one, imported from
 *     `fumadocs-ui/utils/use-copy-button` (confirmed in the package exports map);
 *   - icons are inline SVGs (no lucide);
 *   - the ChatGPT / Claude / Cursor URL formats are copied verbatim from the
 *     official `page-actions.tsx`;
 *   - styling matches the site's dark-editorial pills (`ember`, `fd-*` tokens).
 *
 * `markdownUrl` should be the page's raw-markdown route, e.g.
 * `/llms/docs/nubx.mdx` (served by `app/llms/[...slug]/route.ts`). `pageUrl`
 * is the human page path, used to build an absolute URL for the LLM prompt.
 */

import { useEffect, useRef, useState, type ReactNode } from 'react';
import { useCopyButton } from 'fumadocs-ui/utils/use-copy-button';

const PROMPT = (url: string) =>
  `Read ${url}, I want to ask questions about it.`;

const cache = new Map<string, Promise<string>>();

function CheckIcon() {
  return (
    <svg width="15" height="15" viewBox="0 0 24 24" fill="none" aria-hidden>
      <path
        d="M20 6 9 17l-5-5"
        stroke="currentColor"
        strokeWidth="2.2"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  );
}

function CopyIcon() {
  return (
    <svg width="14" height="14" viewBox="0 0 24 24" fill="none" aria-hidden>
      <rect x="9" y="9" width="11" height="11" rx="2" stroke="currentColor" strokeWidth="2" />
      <path d="M5 15V5a2 2 0 0 1 2-2h10" stroke="currentColor" strokeWidth="2" strokeLinecap="round" />
    </svg>
  );
}

function ChevronDownIcon() {
  return (
    <svg width="13" height="13" viewBox="0 0 24 24" fill="none" aria-hidden>
      <path d="m6 9 6 6 6-6" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" />
    </svg>
  );
}

function TextIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" aria-hidden>
      <path d="M4 6h16M4 12h16M4 18h10" stroke="currentColor" strokeWidth="2" strokeLinecap="round" />
    </svg>
  );
}

function ExternalIcon() {
  return (
    <svg width="13" height="13" viewBox="0 0 24 24" fill="none" aria-hidden>
      <path d="M14 4h6v6M20 4l-9 9" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" />
      <path d="M18 14v4a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h4" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" />
    </svg>
  );
}

/* Brand glyphs lifted verbatim from Fumadocs' page-actions scaffold (simple-icons paths). */
function OpenAIIcon() {
  return (
    <svg viewBox="0 0 24 24" fill="currentColor" width="16" height="16" aria-hidden>
      <path d="M22.2819 9.8211a5.9847 5.9847 0 0 0-.5157-4.9108 6.0462 6.0462 0 0 0-6.5098-2.9A6.0651 6.0651 0 0 0 4.9807 4.1818a5.9847 5.9847 0 0 0-3.9977 2.9 6.0462 6.0462 0 0 0 .7427 7.0966 5.98 5.98 0 0 0 .511 4.9107 6.051 6.051 0 0 0 6.5146 2.9001A5.9847 5.9847 0 0 0 13.2599 24a6.0557 6.0557 0 0 0 5.7718-4.2058 5.9894 5.9894 0 0 0 3.9977-2.9001 6.0557 6.0557 0 0 0-.7475-7.0729zm-9.022 12.6081a4.4755 4.4755 0 0 1-2.8764-1.0408l.1419-.0804 4.7783-2.7582a.7948.7948 0 0 0 .3927-.6813v-6.7369l2.02 1.1686a.071.071 0 0 1 .038.052v5.5826a4.504 4.504 0 0 1-4.4945 4.4944zm-9.6607-4.1254a4.4708 4.4708 0 0 1-.5346-3.0137l.142.0852 4.783 2.7582a.7712.7712 0 0 0 .7806 0l5.8428-3.3685v2.3324a.0804.0804 0 0 1-.0332.0615L9.74 19.9502a4.4992 4.4992 0 0 1-6.1408-1.6464zM2.3408 7.8956a4.485 4.485 0 0 1 2.3655-1.9728V11.6a.7664.7664 0 0 0 .3879.6765l5.8144 3.3543-2.0201 1.1685a.0757.0757 0 0 1-.071 0l-4.8303-2.7865A4.504 4.504 0 0 1 2.3408 7.872zm16.5963 3.8558L13.1038 8.364 15.1192 7.2a.0757.0757 0 0 1 .071 0l4.8303 2.7913a4.4944 4.4944 0 0 1-.6765 8.1042v-5.6772a.79.79 0 0 0-.407-.667zm2.0107-3.0231l-.142-.0852-4.7735-2.7818a.7759.7759 0 0 0-.7854 0L9.409 9.2297V6.8974a.0662.0662 0 0 1 .0284-.0615l4.8303-2.7866a4.4992 4.4992 0 0 1 6.6802 4.66zM8.3065 12.863l-2.02-1.1638a.0804.0804 0 0 1-.038-.0567V6.0742a4.4992 4.4992 0 0 1 7.3757-3.4537l-.142.0805L8.704 5.459a.7948.7948 0 0 0-.3927.6813zm1.0976-2.3654l2.602-1.4998 2.6069 1.4998v2.9994l-2.5974 1.4997-2.6067-1.4997Z" />
    </svg>
  );
}

function AnthropicIcon() {
  return (
    <svg viewBox="0 0 24 24" fill="currentColor" width="16" height="16" aria-hidden>
      <path d="M17.3041 3.541h-3.6718l6.696 16.918H24Zm-10.6082 0L0 20.459h3.7442l1.3693-3.5527h7.0052l1.3693 3.5528h3.7442L10.5363 3.5409Zm-.3712 10.2232 2.2914-5.9456 2.2914 5.9456Z" />
    </svg>
  );
}

function CursorIcon() {
  return (
    <svg viewBox="0 0 24 24" fill="currentColor" width="16" height="16" aria-hidden>
      <path d="M11.503.131 1.891 5.678a.84.84 0 0 0-.42.726v11.188c0 .3.162.575.42.724l9.609 5.55a1 1 0 0 0 .998 0l9.61-5.55a.84.84 0 0 0 .42-.724V6.404a.84.84 0 0 0-.42-.726L12.497.131a1.01 1.01 0 0 0-.996 0M2.657 6.338h18.55c.263 0 .43.287.297.515L12.23 22.918c-.062.107-.229.064-.229-.06V12.335a.59.59 0 0 0-.295-.51l-9.11-5.257c-.109-.063-.064-.23.061-.23" />
    </svg>
  );
}

const pill =
  'inline-flex items-center gap-2 rounded-full border border-fd-border bg-fd-card/60 px-3.5 py-1.5 text-[0.8rem] text-fd-foreground backdrop-blur transition hover:border-ember/60 disabled:opacity-60';

/* Newsreader's ink mass rides high in its line box (the eye discounts descender
   space), so serif labels need a ~1px optical down-nudge inside the pills. */
const label = 'translate-y-[1px]';

function CopyMarkdownButton({ markdownUrl }: { markdownUrl: string }) {
  const [loading, setLoading] = useState(false);
  const [checked, onClick] = useCopyButton(async () => {
    const cached = cache.get(markdownUrl);
    if (cached) {
      await navigator.clipboard.writeText(await cached);
      return;
    }
    setLoading(true);
    try {
      const promise = fetch(markdownUrl).then((res) => res.text());
      cache.set(markdownUrl, promise);
      await navigator.clipboard.write([
        new ClipboardItem({ 'text/plain': promise }),
      ]);
    } finally {
      setLoading(false);
    }
  });

  return (
    <button
      type="button"
      disabled={loading}
      onClick={onClick}
      className={`group ${pill}`}
      aria-label="Copy this page as Markdown"
    >
      <span className="inline-flex w-4 justify-center text-fd-muted-foreground transition group-hover:text-ember">
        {checked ? <CheckIcon /> : <CopyIcon />}
      </span>
      <span className={label}>{checked ? 'Copied' : 'Copy Markdown'}</span>
    </button>
  );
}

function ViewOptions({
  markdownUrl,
  pageUrl,
  githubUrl,
}: {
  markdownUrl: string;
  pageUrl: string;
  githubUrl?: string;
}) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const onDown = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setOpen(false);
    };
    document.addEventListener('mousedown', onDown);
    document.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('mousedown', onDown);
      document.removeEventListener('keydown', onKey);
    };
  }, [open]);

  // Build an absolute URL for the LLM prompt so the assistant can fetch the
  // raw markdown directly. Falls back to the relative markdown path during SSR.
  const absMarkdown =
    typeof window === 'undefined'
      ? markdownUrl
      : new URL(markdownUrl, window.location.origin).toString();
  const q = PROMPT(absMarkdown);

  type Item = { key: string; title: string; href: string; icon: ReactNode };

  const items: Item[] = [];
  if (githubUrl) {
    items.push({
      key: 'github',
      title: 'Open on GitHub',
      href: githubUrl,
      icon: (
        <svg viewBox="0 0 24 24" fill="currentColor" width="16" height="16" aria-hidden>
          <path d="M12 .297c-6.63 0-12 5.373-12 12 0 5.303 3.438 9.8 8.205 11.385.6.113.82-.258.82-.577 0-.285-.01-1.04-.015-2.04-3.338.724-4.042-1.61-4.042-1.61C4.422 18.07 3.633 17.7 3.633 17.7c-1.087-.744.084-.729.084-.729 1.205.084 1.838 1.236 1.838 1.236 1.07 1.835 2.809 1.305 3.495.998.108-.776.417-1.305.76-1.605-2.665-.3-5.466-1.332-5.466-5.93 0-1.31.465-2.38 1.235-3.22-.135-.303-.54-1.523.105-3.176 0 0 1.005-.322 3.3 1.23.96-.267 1.98-.399 3-.405 1.02.006 2.04.138 3 .405 2.28-1.552 3.285-1.23 3.285-1.23.645 1.653.24 2.873.12 3.176.765.84 1.23 1.91 1.23 3.22 0 4.61-2.805 5.625-5.475 5.92.42.36.81 1.096.81 2.22 0 1.606-.015 2.896-.015 3.286 0 .315.21.69.825.57C20.565 22.092 24 17.592 24 12.297c0-6.627-5.373-12-12-12" />
        </svg>
      ),
    });
  }
  items.push(
    {
      key: 'markdown',
      title: 'View as Markdown',
      href: markdownUrl,
      icon: <TextIcon />,
    },
    {
      key: 'chatgpt',
      title: 'Open in ChatGPT',
      href: `https://chatgpt.com/?${new URLSearchParams({ hints: 'search', q })}`,
      icon: <OpenAIIcon />,
    },
    {
      key: 'claude',
      title: 'Open in Claude',
      href: `https://claude.ai/new?${new URLSearchParams({ q })}`,
      icon: <AnthropicIcon />,
    },
    {
      key: 'cursor',
      title: 'Open in Cursor',
      href: `https://cursor.com/link/prompt?${new URLSearchParams({ text: q })}`,
      icon: <CursorIcon />,
    },
  );

  void pageUrl; // reserved: pageUrl is available if you'd rather prompt with the HTML page

  return (
    <div ref={ref} className="relative">
      <button
        type="button"
        onClick={() => setOpen((o) => !o)}
        aria-expanded={open}
        aria-haspopup="menu"
        className={`${pill} ${open ? 'border-ember/60' : ''}`}
      >
        <span className={label}>Open</span>
        <span className="text-fd-muted-foreground">
          <ChevronDownIcon />
        </span>
      </button>
      {open ? (
        <div
          role="menu"
          className="absolute right-0 z-50 mt-2 flex min-w-52 flex-col gap-0.5 rounded-xl border border-fd-border bg-fd-popover/95 p-1.5 shadow-[0_30px_80px_-40px_rgba(0,0,0,0.9)] backdrop-blur"
        >
          {items.map((item) => (
            <a
              key={item.key}
              href={item.href}
              target="_blank"
              rel="noreferrer noopener"
              role="menuitem"
              className="inline-flex items-center gap-2.5 rounded-lg px-2.5 py-1.5 text-[0.82rem] text-fd-foreground transition hover:bg-fd-accent hover:text-fd-accent-foreground"
            >
              <span className="text-fd-muted-foreground">{item.icon}</span>
              <span className={label}>{item.title}</span>
              <span className="ms-auto text-fd-muted-foreground">
                <ExternalIcon />
              </span>
            </a>
          ))}
        </div>
      ) : null}
    </div>
  );
}

export function AIActions({
  markdownUrl,
  pageUrl,
  githubUrl,
}: {
  /** Raw-markdown route for this page, e.g. `/llms/docs/nubx.mdx`. */
  markdownUrl: string;
  /** Human page path, e.g. `/docs/nubx`. */
  pageUrl: string;
  /** Optional source link, e.g. a GitHub blob URL. */
  githubUrl?: string;
}) {
  return (
    <div className="not-prose mb-6 flex flex-row items-center gap-2 border-b border-fd-border pb-4">
      <CopyMarkdownButton markdownUrl={markdownUrl} />
      <ViewOptions markdownUrl={markdownUrl} pageUrl={pageUrl} githubUrl={githubUrl} />
    </div>
  );
}
