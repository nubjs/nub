import { loader } from 'fumadocs-core/source';
import { toFumadocsSource } from 'fumadocs-mdx/runtime/server';
import { docs, blog as blogPosts } from '@/.source/server';
import { renderTitle } from './title';

export const source = loader({
  baseUrl: '/docs',
  source: docs.toFumadocsSource(),
  pageTree: {
    // Backtick spans in frontmatter titles render as <code> in the sidebar.
    attachFile(node) {
      if (typeof node.name === 'string') node.name = renderTitle(node.name);
      return node;
    },
  },
});

export const blog = loader({
  baseUrl: '/blog',
  source: toFumadocsSource(blogPosts, []),
});
