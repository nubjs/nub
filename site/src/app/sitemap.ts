import type { MetadataRoute } from 'next';
import { source, blog, isPublicDoc } from '@/lib/source';

const SITE = 'https://nubjs.com';

// Fully static — the URL set only changes on rebuild.
export const dynamic = 'force-static';
export const revalidate = false;

/**
 * XML sitemap covering every indexable HTML route: the marketing home, the blog
 * index, every blog post, and every docs page. The AI-readable `.mdx` / `.txt`
 * routes are intentionally omitted — they are agent endpoints, not pages we want
 * search engines to index as content.
 */
export default function sitemap(): MetadataRoute.Sitemap {
  const now = new Date();

  const staticRoutes: MetadataRoute.Sitemap = [
    { url: `${SITE}/`, lastModified: now, changeFrequency: 'weekly', priority: 1 },
    { url: `${SITE}/blog`, lastModified: now, changeFrequency: 'weekly', priority: 0.6 },
  ];

  const docsRoutes: MetadataRoute.Sitemap = source
    .getPages()
    .filter(isPublicDoc)
    .map((page) => ({
      url: `${SITE}${page.url}`,
      lastModified: now,
      changeFrequency: 'weekly',
      priority: page.url === '/docs' ? 0.9 : 0.8,
    }));

  const blogRoutes: MetadataRoute.Sitemap = blog.getPages().map((page) => ({
    url: `${SITE}${page.url}`,
    lastModified: page.data.date ? new Date(page.data.date) : now,
    changeFrequency: 'monthly',
    priority: 0.7,
  }));

  return [...staticRoutes, ...docsRoutes, ...blogRoutes];
}
