import { source, blog } from '@/lib/source';

// Static at build time — the content set only changes on rebuild.
export const dynamic = 'force-static';
export const revalidate = false;

const SITE = 'https://nub.sh';

/**
 * `/llms.txt` — the llmstxt.org index.
 *
 * An H1 + blockquote summary, then one section per content area. Each entry is
 * a Markdown link to the page's *raw markdown* (served by
 * `app/llms/[...slug]/route.ts`) so an agent can fetch clean source, plus the
 * page description as the list-item text.
 *
 * We build the index by hand (rather than `llms(source).index()`) so we can (a)
 * cover both the `source` and `blog` loaders in one file and (b) point links at
 * absolute `.mdx` URLs instead of the rendered HTML routes.
 */
export function GET() {
  const lines: string[] = [];

  lines.push('# Nub');
  lines.push('');
  lines.push(
    '> Nub is a Rust CLI that augments your installed Node: a fast script runner and a TypeScript-just-works runtime. This file indexes the documentation as Markdown for LLMs.',
  );
  lines.push('');

  const section = (
    title: string,
    pages: { url: string; data: { title?: string; description?: string } }[],
  ) => {
    if (pages.length === 0) return;
    lines.push(`## ${title}`);
    lines.push('');
    for (const page of pages) {
      const name = page.data.title ?? page.url;
      const mdUrl = `${SITE}/llms${page.url}.mdx`;
      const desc = page.data.description?.trim();
      lines.push(desc ? `- [${name}](${mdUrl}): ${desc}` : `- [${name}](${mdUrl})`);
    }
    lines.push('');
  };

  section('Docs', source.getPages());
  section('Blog', blog.getPages());

  return new Response(lines.join('\n'), {
    headers: { 'Content-Type': 'text/plain; charset=utf-8' },
  });
}
