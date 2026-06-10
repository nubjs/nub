import { createMDX } from 'fumadocs-mdx/next';

const withMDX = createMDX();

/** @type {import('next').NextConfig} */
const config = {
  reactStrictMode: true,
  // Docs slugs were aligned to their commands (2026-06-10); keep the old
  // descriptive URLs working.
  async redirects() {
    return [
      { source: '/docs/running-files', destination: '/docs/files', permanent: true },
      { source: '/docs/running-scripts', destination: '/docs/run', permanent: true },
      { source: '/docs/managing-node', destination: '/docs/node', permanent: true },
    ];
  },
};

export default withMDX(config);
