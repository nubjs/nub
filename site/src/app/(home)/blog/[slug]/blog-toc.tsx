'use client';

import type { TableOfContents } from 'fumadocs-core/toc';
import {
  TOCProvider,
  TOCScrollArea,
} from 'fumadocs-ui/components/toc';
import { TOCItems, TOCItem } from 'fumadocs-ui/components/toc/default';
import { TocStarNudge } from '@/components/toc-star-nudge';

/**
 * Persistent right-gutter table of contents for the blog post, mirroring the
 * docs pages' sticky TOC (TOCProvider + TOCScrollArea + TOCItems/TOCItem from
 * fumadocs). The TOCProvider wires the scroll-spy that highlights the active
 * heading; TOCItems draws the moving accent thumb along the left border.
 *
 * The owning <aside> in the server component controls placement (sticky, gutter
 * width, hidden below lg). This component only renders the list + scroll-spy.
 */
export function BlogTOC({ toc }: { toc: TableOfContents }) {
  return (
    <TOCProvider toc={toc}>
      <h3 className="inline-flex items-center gap-1.5 font-mono text-xs uppercase tracking-[0.14em] text-fd-muted-foreground">
        <svg
          aria-hidden
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          strokeWidth={2}
          strokeLinecap="round"
          strokeLinejoin="round"
          className="size-3.5"
        >
          <path d="M17 6.1H3M21 12.1H3M15.1 18H3" />
        </svg>
        On this page
      </h3>
      <TOCScrollArea className="mt-3">
        <TOCItems>
          {toc.map((item) => (
            <TOCItem key={item.url} item={item} />
          ))}
        </TOCItems>
      </TOCScrollArea>
      <TocStarNudge href="https://github.com/nubjs/nub" />
    </TOCProvider>
  );
}
