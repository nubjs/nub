import Link from 'next/link';
import type { Metadata } from 'next';
import { InlineTOC } from 'fumadocs-ui/components/inline-toc';
import { blog } from '@/lib/source';
import { getMDXComponents } from '../../../../../mdx-components';
import { BlogTOC } from './blog-toc';

type BlogPage = NonNullable<ReturnType<typeof blog.getPage>>;

/**
 * Shared renderer for a blog post, used by both /blog/[slug] and the
 * /blog/hn/[slug] variant that middleware serves for `?hn` visits.
 * `titleOverride` swaps the headline server-side; everything else is
 * identical.
 */
export function PostArticle({
  page,
  titleOverride,
}: {
  page: BlogPage;
  titleOverride?: string;
}) {
  const MDXContent = page.data.body;

  const hasToc = page.data.toc.length > 0;

  return (
    /* Centered shell. At xl+ it widens to make room for a right gutter that
       holds the sticky TOC; below xl it stays a single readable column. The
       article keeps its max-w-3xl measure in both cases. */
    <div className="mx-auto w-full min-w-0 max-w-3xl px-6 py-20 xl:grid xl:max-w-6xl xl:grid-cols-[minmax(0,1fr)_16rem] xl:gap-12">
      <div className="min-w-0 xl:max-w-3xl">
        <Link
          href="/blog"
          className="font-mono text-xs uppercase tracking-[0.14em] text-fd-muted-foreground transition hover:text-ember"
        >
          ← Blog
        </Link>

        <header className="mt-8 border-b border-fd-border pb-10">
          <div className="flex items-center gap-3 font-mono text-xs uppercase tracking-[0.14em] text-ember">
            <time>{formatDate(page.data.date)}</time>
            <span aria-hidden className="text-fd-muted-foreground">
              ·
            </span>
            <span className="text-fd-muted-foreground">{page.data.author}</span>
          </div>
          <h1 className="mt-5 font-display text-4xl font-medium leading-[1.15] tracking-tight md:text-5xl">
            {titleOverride ?? page.data.title}
          </h1>
          {page.data.description ? (
            <p className="mt-5 text-xl leading-relaxed text-fd-muted-foreground">
              {page.data.description}
            </p>
          ) : null}
        </header>

        {/* Below xl there's no gutter, so keep the collapsible in-body TOC as
            the fallback; hide it once the sticky gutter TOC takes over. */}
        {hasToc ? (
          <InlineTOC items={page.data.toc} className="mt-8 xl:hidden" />
        ) : null}

        <article className="prose blog-prose mt-10">
          <MDXContent components={getMDXComponents()} />
        </article>
      </div>

      {/* Sticky right-gutter TOC — xl+ only (no room for a gutter below that). */}
      {hasToc ? (
        <aside className="sticky top-24 hidden h-[calc(100vh-8rem)] flex-col overflow-hidden xl:flex">
          <BlogTOC toc={page.data.toc} />
        </aside>
      ) : null}
    </div>
  );
}

export function postMetadata(page: BlogPage, titleOverride?: string): Metadata {
  const { description, date, author } = page.data;
  const title = titleOverride ?? page.data.title;
  const ogImage = `/og?${new URLSearchParams({ title, eyebrow: 'Blog' }).toString()}`;

  return {
    title,
    description,
    // The hn variant canonicalizes to the real post URL too.
    alternates: { canonical: page.url },
    openGraph: {
      type: 'article',
      url: page.url,
      title,
      description,
      publishedTime: date ? new Date(date).toISOString() : undefined,
      authors: author ? [author] : undefined,
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

function formatDate(date: string | Date | undefined): string {
  if (!date) return '';
  return new Date(date).toLocaleDateString('en-US', {
    year: 'numeric',
    month: 'long',
    day: 'numeric',
  });
}
