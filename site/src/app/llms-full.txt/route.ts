import { source } from '@/lib/source';
import { getLLMText } from '@/lib/get-llm-text';

// Static at build time — the content set only changes on rebuild.
export const dynamic = 'force-static';
export const revalidate = false;

/**
 * `/llms-full.txt` — every docs page concatenated as a single Markdown
 * document, for pasting whole into an LLM context window.
 *
 * Docs only (per spec). To include the blog as well, concatenate
 * `blog.getPages()` here too.
 */
export async function GET() {
  const scanned = await Promise.all(source.getPages().map(getLLMText));

  return new Response(scanned.join('\n\n'), {
    headers: { 'Content-Type': 'text/plain; charset=utf-8' },
  });
}
