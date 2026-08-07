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
import { envSpecLang } from './src/lib/shiki-env-spec';
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
    // Substitute current Node and Nub versions into code samples on each rebuild.
    // Callback form preserves fumadocs' default remark plugins.
    remarkPlugins: (v) => [...v, remarkNodeVersion, remarkGithubAlerts],
    // Warm `vesper` theme (matches the homepage `<Source>` cards), plus a
    // transformer that gives ```console fences a terminal look — ember `$`
    // prompt, bright commands, dimmed output. See `src/lib/shiki-console.ts`.
    rehypeCodeOptions: {
      themes: { light: 'vesper', dark: 'vesper' },
      // `langs` PRELOADS grammars; it does not restrict the bundled set, which
      // stays lazily available. Only non-bundled languages need to be listed.
      langs: [envSpecLang],
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
