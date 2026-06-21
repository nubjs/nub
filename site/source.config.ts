import {
  defineConfig,
  defineDocs,
  defineCollections,
  frontmatterSchema,
} from 'fumadocs-mdx/config';
import { z } from 'zod';
import { rehypeCodeDefaultOptions } from 'fumadocs-core/mdx-plugins';
import { transformerConsole } from './src/lib/shiki-console';

export const docs = defineDocs({
  dir: 'content/docs',
  docs: {
    // Export stringified Markdown via `_markdown` so `page.data.getText('processed')`
    // works (used by /llms.txt, /llms-full.txt, and /llms/*.mdx).
    postprocess: {
      includeProcessedMarkdown: true,
    },
  },
});

export const guides = defineDocs({
  dir: 'content/guides',
  docs: {
    postprocess: {
      includeProcessedMarkdown: true,
    },
  },
});

export const blog = defineCollections({
  type: 'doc',
  dir: 'content/blog',
  schema: frontmatterSchema.extend({
    author: z.string(),
    date: z.string().date().or(z.date()),
  }),
  postprocess: {
    includeProcessedMarkdown: true,
  },
});

export default defineConfig({
  mdxOptions: {
    // Warm `vesper` theme (matches the homepage `<Source>` cards), plus a
    // transformer that gives ```console fences a terminal look — ember `$`
    // prompt, bright commands, dimmed output. See `src/lib/shiki-console.ts`.
    rehypeCodeOptions: {
      themes: { light: 'vesper', dark: 'vesper' },
      // Keep fumadocs' default notation transformers (highlight/diff/focus/word)
      // and append the console terminal-look transformer.
      transformers: [
        ...(rehypeCodeDefaultOptions.transformers ?? []),
        transformerConsole(),
      ],
    },
  },
});
