import defaultMdxComponents from 'fumadocs-ui/mdx';
import { Callout as FumaCallout } from 'fumadocs-ui/components/callout';
import type { MDXComponents } from 'mdx/types';
import type { ComponentProps, ReactNode } from 'react';
import { Bench } from '@/components/code';
import { ShimDemo } from '@/components/shim-demo';
import { CompatTable } from '@/components/compat-table';
import { PmSupport } from '@/components/pm-support-table';
import { InstallTabs } from '@/components/install-tabs';
import { TypesSetup } from '@/components/types-setup';
import { SectionHeading } from '@/components/section-heading';

// Neutral info glyph (lucide "info" path) drawn with currentColor so it inherits a
// muted tone — no loud accent. Inline SVG keeps us off a runtime icon dependency,
// matching the rest of the site's icon convention.
function InfoGlyph() {
  return (
    <svg
      aria-hidden
      width="16"
      height="16"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="2"
      strokeLinecap="round"
      strokeLinejoin="round"
      className="size-4 shrink-0 translate-y-[3px] text-fd-muted-foreground"
    >
      <circle cx="12" cy="12" r="10" />
      <path d="M12 16v-4" />
      <path d="M12 8h.01" />
    </svg>
  );
}

// Docs callouts: suppress fumadocs' default (loud, --callout-color-tinted) leading
// icon and render a neutral muted glyph inline with the title instead — vertically
// centered with the title text via a small optical nudge. No colored bar (hidden in
// global.css), no loud accent. Uniform across every docs callout.
function Callout({
  title,
  ...props
}: { title?: ReactNode } & Omit<ComponentProps<typeof FumaCallout>, 'title'>) {
  return (
    <FumaCallout
      icon={false}
      title={
        title != null ? (
          <span className="inline-flex items-baseline gap-2">
            <InfoGlyph />
            <span>{title}</span>
          </span>
        ) : (
          title
        )
      }
      {...props}
    />
  );
}

export function getMDXComponents(components?: MDXComponents): MDXComponents {
  return {
    ...defaultMdxComponents,
    // Override the heading anchors so a copied section link carries a
    // server-visible `?section=<slug>` param (drives per-heading OG cards).
    h1: (props: ComponentProps<'h1'>) => <SectionHeading as="h1" {...props} />,
    h2: (props: ComponentProps<'h2'>) => <SectionHeading as="h2" {...props} />,
    h3: (props: ComponentProps<'h3'>) => <SectionHeading as="h3" {...props} />,
    h4: (props: ComponentProps<'h4'>) => <SectionHeading as="h4" {...props} />,
    h5: (props: ComponentProps<'h5'>) => <SectionHeading as="h5" {...props} />,
    h6: (props: ComponentProps<'h6'>) => <SectionHeading as="h6" {...props} />,
    Callout,
    Bench,
    ShimDemo,
    CompatTable,
    PmSupport,
    InstallTabs,
    TypesSetup,
    ...components,
  };
}
