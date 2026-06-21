import { source, guidesSource, blog } from '@/lib/source';
import { getLLMText } from '@/lib/get-llm-text';
import { notFound } from 'next/navigation';

// Pre-render one .mdx file per page at build time; never revalidate.
export const dynamic = 'force-static';
export const revalidate = false;

/**
 * Raw per-page Markdown, e.g. `/llms/docs/faq.mdx` -> the LLM-friendly Markdown
 * for the `/docs/faq` page. These are the targets the `/llms.txt` index links
 * to. Lives under the `/llms` prefix (rather than `/docs/faq.mdx`) because a
 * Next.js route segment cannot host both `page.tsx` and `route.ts`.
 */
export async function GET(
  _req: Request,
  { params }: { params: Promise<{ slug: string[] }> },
) {
  const { slug } = await params;

  // Incoming slug is the page URL split on '/', with '.mdx' on the last part:
  // ['docs', 'faq.mdx'] -> page url '/docs/faq'.
  const parts = [...slug];
  const last = parts.at(-1);
  if (!last?.endsWith('.mdx')) notFound();
  parts[parts.length - 1] = last.slice(0, -'.mdx'.length);
  const url = '/' + parts.join('/');

  const page =
    source.getPages().find((p) => p.url === url) ??
    guidesSource.getPages().find((p) => p.url === url) ??
    blog.getPages().find((p) => p.url === url);
  if (!page) notFound();

  return new Response(await getLLMText(page), {
    headers: { 'Content-Type': 'text/markdown; charset=utf-8' },
  });
}

/**
 * Enumerate the static params so the routes are emitted at build time.
 * Mirrors the `${page.url}.mdx` links produced by `/llms.txt`.
 */
export function generateStaticParams() {
  const toParams = (url: string) => {
    const segs = url.replace(/^\//, '').split('/');
    segs[segs.length - 1] = `${segs[segs.length - 1]}.mdx`;
    return { slug: segs };
  };

  return [
    ...source.getPages().map((p) => toParams(p.url)),
    ...guidesSource.getPages().map((p) => toParams(p.url)),
    ...blog.getPages().map((p) => toParams(p.url)),
  ];
}
