import type { InferPageType } from 'fumadocs-core/source';
import { source, blog } from '@/lib/source';

type DocsPage = InferPageType<typeof source>;
type BlogPage = InferPageType<typeof blog>;

/**
 * Render a single page (docs or blog) to LLM-friendly Markdown.
 *
 * Uses `page.data.getText('processed')`, which returns the stringified MDAST
 * for the page. That method only works when `includeProcessedMarkdown` is
 * enabled on the collection in `source.config.ts` (see config_changes); without
 * it, `getText('processed')` throws.
 */
export async function getLLMText(page: DocsPage | BlogPage): Promise<string> {
  const processed = await page.data.getText('processed');
  const description = page.data.description
    ? `\n> ${page.data.description}\n`
    : '\n';

  return `# ${page.data.title} (${page.url})
${description}
${processed}`;
}
