import type { Metadata } from 'next';
import { notFound } from 'next/navigation';
import { source } from '@/lib/source';
import { DocsPageView, docsPageMetadata } from '../../[[...slug]]/docs-page';

/* The `?section=<slug>` variant of a docs page: `next.config.mjs` rewrites
   /docs/<path>?section=<slug> here (the address bar keeps the original URL),
   and this route renders the same page with the shared heading as the OG
   card's title. Reading the query makes this route dynamic, which is the point
   of splitting it out — only share links pay for a function invocation, while
   the plain `docs/[[...slug]]` route stays static. Canonical always points at
   the real docs URL. Same pattern as the blog's `?hn` -> `/blog/hn/<slug>`. */

export default async function SectionPage(props: {
  params: Promise<{ slug?: string[] }>;
}) {
  const params = await props.params;
  const page = source.getPage(params.slug);
  if (!page) notFound();

  return <DocsPageView page={page} />;
}

export async function generateMetadata(props: {
  params: Promise<{ slug?: string[] }>;
  searchParams: Promise<{ section?: string | string[] }>;
}): Promise<Metadata> {
  const params = await props.params;
  const page = source.getPage(params.slug);
  if (!page) notFound();

  const rawSection = (await props.searchParams).section;
  const sectionSlug = typeof rawSection === 'string' ? rawSection : undefined;

  return docsPageMetadata(page, sectionSlug);
}
