import { loader } from 'fumadocs-core/source';
import { toFumadocsSource } from 'fumadocs-mdx/runtime/server';
import { docs, blog as blogPosts } from '@/.source/server';

export const source = loader({
  baseUrl: '/docs',
  source: docs.toFumadocsSource(),
});

export const blog = loader({
  baseUrl: '/blog',
  source: toFumadocsSource(blogPosts, []),
});
