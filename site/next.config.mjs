import { createMDX } from 'fumadocs-mdx/next';

const withMDX = createMDX();

// NOTE: do not pin `outputFileTracingRoot`/`turbopack.root` to this directory to
// silence Next's ambiguous-workspace-root warning. Commit 0d54774bc7 did that and
// 25 consecutive Vercel production deploys failed at "Deploying outputs" — the build
// itself succeeded each time, so nothing surfaced in CI or in the pre-push hook, and the
// live site went on serving the last bundle that shipped (d3d501231b, 2026-08-19T01:41Z)
// until the revert two and a half days later. The warning is cosmetic; this is not.

/** @type {import('next').NextConfig} */
const config = {
  reactStrictMode: true,
  // A shared docs heading link carries `?section=<slug>` so its OG card can
  // name the heading. A static page has exactly one <head>, so that request has
  // to be served by the on-demand `docs/section/[[...slug]]` route instead —
  // routed here declaratively (a query condition evaluated by the platform
  // router, no middleware code) so the plain `docs/[[...slug]]` route stays
  // fully static. Every docs view and sidebar prefetch used to be a function
  // invocation because the docs route itself read `searchParams`.
  async rewrites() {
    return {
      beforeFiles: [
        {
          source: '/docs/:path*',
          has: [{ type: 'query', key: 'section' }],
          destination: '/docs/section/:path*',
        },
      ],
    };
  },
  // Docs slugs were aligned to their commands (2026-06-10); keep the old
  // descriptive URLs working.
  async redirects() {
    return [
      { source: '/docs/running-files', destination: '/docs/runtime', permanent: true },
      { source: '/docs/files', destination: '/docs/runtime', permanent: true },
      { source: '/docs/running-scripts', destination: '/docs/run', permanent: true },
      { source: '/docs/managing-node', destination: '/docs/node', permanent: true },
      // setup-nub + docker folded into the Deployment section (2026-07-14).
      { source: '/docs/setup-nub', destination: '/docs/deployment/github-action', permanent: true },
      { source: '/docs/docker', destination: '/docs/deployment/docker', permanent: true },
      // pm-shim nested under the pm (package meta-manager) section (2026-07-14).
      { source: '/docs/pm-shim', destination: '/docs/pm/pm-shim', permanent: true },
      // Guides moved from /docs/guides/* to the top-level /guides/* route.
      { source: '/docs/guides/:path*', destination: '/guides/:path*', permanent: true },
    ];
  },
  // Advertise the llms.txt index on every page so crawlers/agents can
  // auto-discover the AI-readable content without prior knowledge of the URL.
  async headers() {
    return [
      {
        source: '/(.*)',
        headers: [
          {
            key: 'Link',
            value: '</llms.txt>; rel="llms-txt", </llms-full.txt>; rel="llms-full-txt"',
          },
          { key: 'X-Llms-Txt', value: '/llms.txt' },
        ],
      },
    ];
  },
};

export default withMDX(config);
