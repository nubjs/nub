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
import {
  transformerAnsi,
  ANSI_COLOR_REPLACEMENTS,
} from './src/lib/shiki-ansi';
import { envSpecLang } from './src/lib/shiki-env-spec';
import { remarkNodeVersion } from './src/lib/remark-node-version';
import { remarkGithubAlerts } from './src/lib/remark-github-alerts';

// `unpublished: true` in any page's frontmatter keeps it out of a production
// build — no route, no nav entry, no sitemap, search or llms.txt line — while
// `next dev` still serves it for review. The filter is `published()` in
// `src/lib/source.ts`; the key is declared here so every collection accepts it.
const unpublished = { unpublished: z.boolean().optional() };

export const docs = defineDocs({
  dir: 'content/docs',
  docs: {
    schema: frontmatterSchema.extend(unpublished),
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
    schema: frontmatterSchema.extend(unpublished),
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
    ...unpublished,
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
    //
    // A ```ansi fence renders real terminal color instead: paste output that
    // still carries its escape sequences, in any spelling (`\x1b[32m`, `\e[32m`,
    // `\033[32m`, `^[[32m`, or the raw byte). See `src/lib/shiki-ansi.ts`.
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
        transformerAnsi(),
      ],
      // Retarget shiki's 16 named ANSI colors onto the site's dark code panel —
      // ```ansi fences only, in effect: the keys are the VS Code defaults shiki
      // falls back to when a theme (here `vesper`) defines no `terminal.ansi*`
      // colors, and no vesper token color collides with one. See
      // `src/lib/shiki-ansi.ts` for the palette and the coupling note.
      colorReplacements: ANSI_COLOR_REPLACEMENTS,
      // Promote a bare `full` in the fence meta to a real attribute, so a block can
      // opt out of fumadocs' 600px viewport cap — see `pre` in mdx-components.tsx.
      // fumadocs only promotes `title` and `tab`; everything else lands in `__raw`,
      // which React drops before it reaches the component. Strip our token first,
      // then delegate so `title` and `lineNumbers` parse exactly as they did.
      parseMetaString(meta, node, tree) {
        const full = /(^|\s)full(\s|$)/.test(meta);
        const rest = full ? meta.replace(/(^|\s)full(?=\s|$)/, ' ').trim() : meta;
        const data =
          rehypeCodeDefaultOptions.parseMetaString?.(rest, node, tree) ?? {};
        if (full) data['data-full'] = true;
        return data;
      },
    },
  },
});
