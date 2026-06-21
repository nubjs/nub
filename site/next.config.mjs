import { createMDX } from 'fumadocs-mdx/next';

const withMDX = createMDX();

/** @type {import('next').NextConfig} */
const config = {
  reactStrictMode: true,
  // Docs slugs were aligned to their commands (2026-06-10); keep the old
  // descriptive URLs working.
  async redirects() {
    return [
      { source: '/docs/running-files', destination: '/docs/runtime', permanent: true },
      { source: '/docs/files', destination: '/docs/runtime', permanent: true },
      { source: '/docs/running-scripts', destination: '/docs/run', permanent: true },
      { source: '/docs/managing-node', destination: '/docs/node', permanent: true },
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
