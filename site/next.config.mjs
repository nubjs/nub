import { fileURLToPath } from 'node:url';
import { createMDX } from 'fumadocs-mdx/next';

const withMDX = createMDX();

// The repo root carries its own package-lock.json (nub's Node-side runtime deps)
// alongside this project's pnpm-lock.yaml, so Next infers an ambiguous workspace
// root by default and warns about it. Pin it to this directory explicitly —
// `turbopack.root` for the (default) Turbopack dev/build path, `outputFileTracingRoot`
// for the webpack path — rather than removing the tracked root lockfile.
const siteRoot = fileURLToPath(new URL('.', import.meta.url));

/** @type {import('next').NextConfig} */
const config = {
  reactStrictMode: true,
  outputFileTracingRoot: siteRoot,
  turbopack: {
    root: siteRoot,
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
