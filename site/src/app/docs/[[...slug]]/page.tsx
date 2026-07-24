import type { Metadata } from 'next';
import { source, isPublicDoc } from '@/lib/source';
import {
  DocsPage,
  DocsBody,
  DocsDescription,
  DocsTitle,
} from 'fumadocs-ui/page';
import { notFound } from 'next/navigation';
import { AIActions } from '@/components/ai-actions';
import { GitHubStarButton } from '@/components/github-star-button';
import { TocStarNudge } from '@/components/toc-star-nudge';
import { getMDXComponents } from '../../../../mdx-components';

/* Footer rendered below the prev/next pager: a GitHub-style Star button with a
   live stargazer count, with generous vertical breathing room above + below. */
function PageStarFooter() {
  return (
    <div className="my-10 flex items-center justify-center">
      <GitHubStarButton repo="nubjs/nub" />
    </div>
  );
}

export default async function Page(props: {
  params: Promise<{ slug?: string[] }>;
}) {
  const params = await props.params;
  const page = source.getPage(params.slug);
  if (!page) notFound();

  const MDXContent = page.data.body;

  return (
    <DocsPage
      toc={page.data.toc}
      full={page.data.full}
      tableOfContent={{ footer: <TocStarNudge href="https://github.com/nubjs/nub" /> }}
      footer={{ children: <PageStarFooter /> }}
    >
      <DocsTitle>{page.data.title}</DocsTitle>
      <DocsDescription>{page.data.description}</DocsDescription>
      <AIActions
        markdownUrl={`/llms${page.url}.mdx`}
        pageUrl={page.url}
        githubUrl="https://github.com/nubjs/nub"
      />
      {/* fumadocs' DocsPage emits no <main>/landmark; mark the prose body as the
          page's main landmark so the doc has exactly one (WCAG / Lighthouse). */}
      <DocsBody role="main">
        <MDXContent components={getMDXComponents()} />
      </DocsBody>
    </DocsPage>
  );
}

export function generateStaticParams() {
  return source.generateParams();
}

/* Pages mapping to a concrete command surface get the command spelling as the
   social-card eyebrow (matching the sidebar chips in docs/layout.tsx); others
   fall back to a plain "Documentation" label. */
const EYEBROW_BY_URL: Record<string, string> = {
  '/docs/runtime': 'nub <file>',
  '/docs/runner': 'nubx',
  '/docs/runner/run': 'nub run',
  '/docs/runner/exec': 'nub exec',
  '/docs/runner/dlx': 'nub dlx',
  '/docs/install': 'nub install',
  '/docs/node': 'nub node',
  '/docs/pm': 'nub pm',
  '/docs/watch': 'nub watch',
};

/* Build the social-card URL handled by `app/og/route.tsx`. The card shows the
   eyebrow and title only — no description (it rarely fit). */
function ogImageUrl({ title, eyebrow }: { title: string; eyebrow: string }): string {
  const params = new URLSearchParams({ title, eyebrow });
  return `/og?${params.toString()}`;
}

/* Flatten a TOC entry's `title` (a ReactNode — plain text, or an element tree
   when the heading contains inline code/formatting) to its visible text, so a
   heading like `nub pm which` resolves to that string for the OG card. */
function nodeToText(node: React.ReactNode): string {
  if (node == null || typeof node === 'boolean') return '';
  if (typeof node === 'string' || typeof node === 'number') return String(node);
  if (Array.isArray(node)) return node.map(nodeToText).join('');
  if (typeof node === 'object' && 'props' in node) {
    return nodeToText((node as { props?: { children?: React.ReactNode } }).props?.children);
  }
  return '';
}

/* Resolve a section slug to its heading text via the page TOC. Heading slugs are
   unique per page, so a direct `#<slug>` match returns the right heading at any
   depth (h2/h3/…) — the deepest/most-specific heading the reader shared. */
function headingTextForSlug(
  toc: { url: string; title: React.ReactNode }[],
  slug: string,
): string | undefined {
  const item = toc.find((entry) => entry.url === `#${slug}`);
  if (!item) return undefined;
  const text = nodeToText(item.title).trim();
  return text.length > 0 ? text : undefined;
}

export async function generateMetadata(props: {
  params: Promise<{ slug?: string[] }>;
  searchParams: Promise<{ section?: string | string[] }>;
}): Promise<Metadata> {
  const params = await props.params;
  const page = source.getPage(params.slug);
  if (!page) notFound();

  const { title, description } = page.data;

  // A shared section link (`?section=<slug>#<slug>`) recasts the card: the
  // heading becomes the main title and the page title moves to the eyebrow.
  // Absent (or unresolvable) `section` keeps the page-level card. Only the OG
  // image varies on `section`; the page body/metadata are otherwise identical.
  const searchParams = await props.searchParams;
  const rawSection = searchParams.section;
  const sectionSlug = typeof rawSection === 'string' ? rawSection : undefined;
  const sectionTitle = sectionSlug
    ? headingTextForSlug(page.data.toc, sectionSlug)
    : undefined;

  const ogImage = sectionTitle
    ? ogImageUrl({ title: sectionTitle, eyebrow: title })
    : ogImageUrl({ title, eyebrow: EYEBROW_BY_URL[page.url] ?? 'Documentation' });

  return {
    title,
    description,
    // Self-canonical: each docs page points at its own URL rather than
    // inheriting the root layout's `/` canonical.
    alternates: { canonical: page.url },
    openGraph: {
      type: 'article',
      url: page.url,
      title,
      description,
      images: [{ url: ogImage, width: 1200, height: 630, alt: title }],
    },
    twitter: {
      card: 'summary_large_image',
      title,
      description,
      images: [ogImage],
    },
    robots: isPublicDoc(page) ? undefined : { index: false, follow: false },
  };
}
