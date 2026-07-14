import { NextRequest, NextResponse } from 'next/server';

/**
 * Content negotiation: when an AI agent or CLI requests a canonical docs URL
 * with `Accept: text/markdown`, serve the pre-built raw-markdown variant from
 * the `/llms/` prefix instead of the rendered HTML page.
 *
 * Example:
 *   curl -H 'Accept: text/markdown' https://nubjs.com/docs/run
 *   -> 200 text/markdown (same body as /llms/docs/run.mdx)
 *
 * Normal browser requests (Accept: text/html or no Accept) pass through
 * unchanged. The trigger is the Accept header only — not the User-Agent.
 *
 * Implementation note: this is a Next.js edge middleware rewrite. The target
 * `/llms/docs/<slug>.mdx` routes are pre-rendered as static files at build
 * time (force-static), so the rewrite serves a static asset with zero
 * server-side overhead.
 */

function acceptsMarkdown(accept: string): boolean {
  // Accept can be a comma-separated list of media-types with optional q-values,
  // e.g. "text/html,application/xhtml+xml,text/markdown;q=0.9,*/*;q=0.8".
  // We just need to detect the presence of text/markdown anywhere in the list.
  return accept.split(',').some((part) => part.trim().split(';')[0].trim() === 'text/markdown');
}

export function middleware(req: NextRequest) {
  const accept = req.headers.get('accept') ?? '';
  const { pathname } = req.nextUrl;

  if (acceptsMarkdown(accept)) {
    if (pathname.startsWith('/docs/')) {
      // /docs/run  ->  /llms/docs/run.mdx
      const slug = pathname.slice('/docs'.length); // e.g. "/run" or "/some/nested"
      return NextResponse.rewrite(new URL(`/llms/docs${slug}.mdx`, req.url));
    }

    if (pathname.startsWith('/blog/')) {
      // /blog/some-post  ->  /llms/blog/some-post.mdx
      const slug = pathname.slice('/blog'.length);
      return NextResponse.rewrite(new URL(`/llms/blog${slug}.mdx`, req.url));
    }
  }

  // `?hn` (Hacker News submissions) serves the statically prerendered
  // /blog/hn/<slug> variant, which renders the post's `hnTitle` frontmatter
  // as the headline + document title. Same URL in the address bar, no
  // client-side swap, no flash.
  if (
    req.nextUrl.searchParams.has('hn') &&
    pathname.startsWith('/blog/') &&
    !pathname.startsWith('/blog/hn/')
  ) {
    const url = req.nextUrl.clone();
    url.pathname = `/blog/hn${pathname.slice('/blog'.length)}`;
    return NextResponse.rewrite(url);
  }

  return NextResponse.next();
}

export const config = {
  // Run on /docs/* and /blog/* paths only. Excludes static assets, API routes,
  // and the /llms/* routes themselves.
  matcher: ['/docs/:path*', '/blog/:path*'],
};
