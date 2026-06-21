import type { Metadata } from 'next';
import { source } from '@/lib/source';
import {
  DocsPage,
  DocsBody,
  DocsDescription,
  DocsTitle,
} from 'fumadocs-ui/page';
import { notFound } from 'next/navigation';
import { AIActions } from '@/components/ai-actions';
import { GitHubStarButton } from '@/components/github-star-button';
import { getMDXComponents } from '../../../../mdx-components';

/* GitHub mark SVG (official GitHub Invertocat, simplified mono path). */
function GitHubIcon({ className }: { className?: string }) {
  return (
    <svg
      viewBox="0 0 24 24"
      fill="currentColor"
      className={className}
      aria-hidden="true"
    >
      <path d="M12 0C5.37 0 0 5.37 0 12c0 5.31 3.435 9.795 8.205 11.385.6.105.825-.255.825-.57 0-.285-.015-1.23-.015-2.235-3.015.555-3.795-.735-4.035-1.41-.135-.345-.72-1.41-1.23-1.695-.42-.225-1.02-.78-.015-.795.945-.015 1.62.87 1.845 1.23 1.08 1.815 2.805 1.305 3.495.99.105-.78.42-1.305.765-1.605-2.67-.3-5.46-1.335-5.46-5.925 0-1.305.465-2.385 1.23-3.225-.12-.3-.54-1.53.12-3.18 0 0 1.005-.315 3.3 1.23.96-.27 1.98-.405 3-.405s2.04.135 3 .405c2.295-1.56 3.3-1.23 3.3-1.23.66 1.65.24 2.88.12 3.18.765.84 1.23 1.905 1.23 3.225 0 4.605-2.805 5.625-5.475 5.925.435.375.81 1.095.81 2.22 0 1.605-.015 2.895-.015 3.3 0 .315.225.69.825.57A12.02 12.02 0 0 0 24 12c0-6.63-5.37-12-12-12z" />
    </svg>
  );
}

/* Footer node injected into the docs TOC panel — the "Edit on GitHub" link,
   styled to read as the last entry in the table of contents (matching the TOC
   items' `text-sm text-fd-muted-foreground` and their `ps-3` left inset) rather
   than as a foreign button. Replaces the former repo link that lived here. */
function TocEditLink({ href }: { href: string }) {
  return (
    <>
      <hr className="mt-2 mb-1 border-fd-foreground/10" />
      <a
        href={href}
        target="_blank"
        rel="noopener noreferrer"
        className="flex items-center gap-1.5 py-1.5 ps-3 text-sm text-fd-muted-foreground transition-colors hover:text-fd-accent-foreground"
      >
        <GitHubIcon className="size-3.5 shrink-0" />
        <span>Edit on GitHub</span>
      </a>
    </>
  );
}

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

  /* Construct the GitHub edit URL from the virtual file path fumadocs exposes on
     the page object. The path is relative to the docs content root (e.g.
     "runtime/index.mdx"), so we prepend the repo-relative prefix to build the
     full edit URL. */
  const editHref = `https://github.com/nubjs/nub/edit/main/site/content/docs/${(page as { path?: string }).path ?? ''}`;

  return (
    <DocsPage
      toc={page.data.toc}
      full={page.data.full}
      tableOfContent={{ footer: <TocEditLink href={editHref} /> }}
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
  '/docs/run': 'nub run',
  '/docs/nubx': 'nubx',
  '/docs/install': 'nub install',
  '/docs/node': 'nub node',
  '/docs/pm': 'nub pm',
  '/docs/watch': 'nub watch',
};

/* Build the per-page social-card URL handled by `app/og/route.tsx`. The card
   shows the eyebrow and title only — no description (it rarely fit). */
function ogImageUrl({ url, title }: { url: string; title: string }): string {
  const params = new URLSearchParams({
    title,
    eyebrow: EYEBROW_BY_URL[url] ?? 'Documentation',
  });
  return `/og?${params.toString()}`;
}

export async function generateMetadata(props: {
  params: Promise<{ slug?: string[] }>;
}): Promise<Metadata> {
  const params = await props.params;
  const page = source.getPage(params.slug);
  if (!page) notFound();

  const { title, description } = page.data;
  const ogImage = ogImageUrl({ url: page.url, title });

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
  };
}
