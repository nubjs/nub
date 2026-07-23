import {
  defineConfig,
  defineDocs,
  defineCollections,
  frontmatterSchema,
} from 'fumadocs-mdx/config';
import { z } from 'zod';
import { rehypeCodeDefaultOptions } from 'fumadocs-core/mdx-plugins';
import { transformerConsole } from './src/lib/shiki-console';
import { transformerDiff } from './src/lib/shiki-diff';
import { remarkNodeVersion } from './src/lib/remark-node-version';
import { remarkGithubAlerts } from './src/lib/remark-github-alerts';

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
    // Alternate headline for `?hn` visits (Hacker News submissions):
    // middleware rewrites /blog/<slug>?hn to the statically prerendered
    // /blog/hn/<slug> variant, which renders this as the title server-side.
    hnTitle: z.string().optional(),
  }),
  postprocess: {
    includeProcessedMarkdown: true,
  },
});

export default defineConfig({
  mdxOptions: {
    // Substitute the live latest-Node version into `{{NODE_VERSION}}` /
    // `{{NODE_MAJOR}}` tokens in code samples on each rebuild. Callback form so
    // fumadocs' default remark plugins are preserved, not replaced.
    remarkPlugins: (v) => [...v, remarkNodeVersion, remarkGithubAlerts],
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
        transformerDiff(),
      ],
    },
  },
});
