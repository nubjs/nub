'use client';

import type { ComponentPropsWithoutRef, ReactNode } from 'react';
import { useCopyButton } from 'fumadocs-ui/utils/use-copy-button';
import { buttonVariants } from 'fumadocs-ui/components/ui/button';

/* Drop-in replacement for fumadocs-ui's MDX `Heading`. Identical markup and
   affordance — a `data-card` anchor around the heading text plus a hover-reveal
   copy button — with one change: the copy button yields a SECTION-shareable URL
   carrying a `?section=<slug>` query param (kept alongside the `#<slug>`
   fragment) instead of the bare `#<slug>` fragment fumadocs copies.

   The query param is what makes a shared heading link render its own OG card:
   a `#fragment` never reaches the server, so `generateMetadata` can't see it,
   but `?section=` does — `next.config.mjs` rewrites such a request to the
   on-demand `docs/section/[[...slug]]` route, which reads it; the plain docs
   route stays fully static. The visible text anchor stays a plain `#<slug>`
   for pure in-page scroll; only the copy action emits the section URL. Icons are inline SVG per the site's no-icon-dependency
   convention (matching GitHubIcon/InfoGlyph elsewhere). */

function cn(...parts: (string | undefined | false)[]): string {
  return parts.filter(Boolean).join(' ');
}

function LinkGlyph() {
  return (
    <svg
      aria-hidden
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="2"
      strokeLinecap="round"
      strokeLinejoin="round"
      className="size-3.5"
    >
      <path d="M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71" />
      <path d="M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71" />
    </svg>
  );
}

function CheckGlyph() {
  return (
    <svg
      aria-hidden
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="2"
      strokeLinecap="round"
      strokeLinejoin="round"
      className="size-3.5"
    >
      <path d="M20 6 9 17l-5-5" />
    </svg>
  );
}

type HeadingProps = ComponentPropsWithoutRef<'h1'> & {
  as?: 'h1' | 'h2' | 'h3' | 'h4' | 'h5' | 'h6';
  children?: ReactNode;
};

export function SectionHeading({ as, children, className, ...props }: HeadingProps) {
  const As = as ?? 'h1';

  const [checked, onCopy] = useCopyButton(() => {
    if (!props.id) return;
    const { origin, pathname } = window.location;
    const slug = encodeURIComponent(props.id);
    return navigator.clipboard.writeText(
      `${origin}${pathname}?section=${slug}#${props.id}`,
    );
  });

  // Headings without an id (rare) get no anchor affordance — mirror fumadocs.
  if (!props.id) {
    return (
      <As className={className} {...props}>
        {children}
      </As>
    );
  }

  return (
    <As
      className={cn(
        'group/heading flex scroll-m-28 flex-row items-center gap-1',
        className,
      )}
      {...props}
    >
      <a data-card="" href={`#${props.id}`}>
        {children}
      </a>
      <button
        type="button"
        aria-label="Copy link to section"
        onClick={onCopy}
        className={cn(
          buttonVariants({ variant: 'ghost', size: 'icon-xs' }),
          'not-prose shrink-0 text-fd-muted-foreground opacity-0 transition-opacity group-hover/heading:opacity-100',
        )}
      >
        {checked ? <CheckGlyph /> : <LinkGlyph />}
      </button>
    </As>
  );
}
