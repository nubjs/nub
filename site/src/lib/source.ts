import { loader } from 'fumadocs-core/source';
import type { StaticSource } from 'fumadocs-core/source';
import { toFumadocsSource } from 'fumadocs-mdx/runtime/server';
import { docs, guides, blog as blogPosts } from '@/.source/server';

/* A page whose frontmatter says `unpublished: true` is dropped from the source
   before any loader sees it, so a production build has no route for it, no nav
   entry, no sitemap line, no search hit and no llms.txt link — every consumer
   reads from these loaders. The point is landing a post or docs page for a
   feature ahead of its release on main, where it can be reviewed and merged
   without going live.

   `next dev` keeps the page, so it can be read in the browser while it is being
   written; `SITE_SHOW_UNPUBLISHED=1` does the same for a production build (a
   preview deploy). The gate is the source rather than each route so a new
   consumer cannot forget it. */
const showUnpublished =
  process.env.NODE_ENV !== 'production' || process.env.SITE_SHOW_UNPUBLISHED === '1';

function published<S extends StaticSource>(source: S): S {
  if (showUnpublished) return source;
  const files = source.files.filter(
    (file) => file.type !== 'page' || !(file.data as { unpublished?: boolean }).unpublished,
  );
  return { ...source, files };
}

export const source = loader({
  baseUrl: '/docs',
  source: published(docs.toFumadocsSource()),
});

export const guidesSource = loader({
  baseUrl: '/guides',
  source: published(guides.toFumadocsSource()),
});

export const blog = loader({
  baseUrl: '/blog',
  source: published(toFumadocsSource(blogPosts, [])),
});
