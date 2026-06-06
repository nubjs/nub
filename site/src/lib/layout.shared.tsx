import type { BaseLayoutProps } from 'fumadocs-ui/layouts/shared';

/* The wordmark — stylized with a trailing period as a logo. */
export function Wordmark() {
  return (
    <span className="relative -top-0.5 font-display text-lg font-medium tracking-tight text-fd-foreground">
      nub<span className="text-ember">.</span>
    </span>
  );
}

export function baseOptions(): BaseLayoutProps {
  return {
    nav: {
      title: <Wordmark />,
    },
    links: [
      { text: 'Docs', url: '/docs', active: 'nested-url' },
      { text: 'Blog', url: '/blog', active: 'nested-url' },
    ],
    githubUrl: 'https://github.com/nubjs/nub',
  };
}
