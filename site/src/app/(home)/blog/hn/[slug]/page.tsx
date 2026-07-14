import type { Metadata } from 'next';
import { notFound } from 'next/navigation';
import { blog } from '@/lib/source';
import { PostArticle, postMetadata } from '../../[slug]/post-article';

/**
 * The `?hn` variant of a blog post: middleware rewrites /blog/<slug>?hn here,
 * and this route renders the post with its `hnTitle` frontmatter as the
 * headline + document title — server-side, so there is no flash and title
 * scrapers see the alternate title. Posts without an hnTitle render
 * unchanged. Canonical always points at the real post URL.
 */
export default async function HnBlogPost(props: {
  params: Promise<{ slug: string }>;
}) {
  const { slug } = await props.params;
  const page = blog.getPage([slug]);
  if (!page) notFound();

  return <PostArticle page={page} titleOverride={page.data.hnTitle} />;
}

// Prerender only the posts that carry an hnTitle; anything else is generated
// on demand (and renders identically to the plain route).
export function generateStaticParams() {
  return blog
    .getPages()
    .filter((page) => page.data.hnTitle)
    .map((page) => ({ slug: page.slugs[0] }));
}

export async function generateMetadata(props: {
  params: Promise<{ slug: string }>;
}): Promise<Metadata> {
  const { slug } = await props.params;
  const page = blog.getPage([slug]);
  if (!page) notFound();

  return postMetadata(page, page.data.hnTitle);
}
