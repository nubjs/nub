import type { Metadata } from 'next';
import { notFound } from 'next/navigation';
import { source } from '@/lib/source';
import { DocsPageView, docsPageMetadata } from './docs-page';

/* Fully static: prerendered from `generateStaticParams` and never reads the
   request. The `?section=` share-link variant, which must read the query to
   pick its OG card, is a separate on-demand route (`docs/section/[[...slug]]`)
   that `next.config.mjs` rewrites to — keep every `searchParams` read out of
   this file, or the whole route (and each sidebar prefetch) goes dynamic. */

export default async function Page(props: {
  params: Promise<{ slug?: string[] }>;
}) {
  const params = await props.params;
  const page = source.getPage(params.slug);
  if (!page) notFound();

  return <DocsPageView page={page} />;
}

export function generateStaticParams() {
  return source.generateParams();
}

export async function generateMetadata(props: {
  params: Promise<{ slug?: string[] }>;
}): Promise<Metadata> {
  const params = await props.params;
  const page = source.getPage(params.slug);
  if (!page) notFound();

  return docsPageMetadata(page);
}
