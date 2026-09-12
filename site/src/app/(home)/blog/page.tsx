import Link from 'next/link';
import type { Metadata } from 'next';
import { blog } from '@/lib/source';
import { renderInlineCode } from '@/lib/inline-code';

const blogOgImage = `/og?${new URLSearchParams({ title: 'Blog', eyebrow: 'Blog' }).toString()}`;

export const metadata: Metadata = {
  title: 'Blog',
  description: 'Writing on Nub — the all-in-one toolkit for Node.js. Notes on the thesis, the toolchain, and what ships next.',
  alternates: { canonical: '/blog' },
  openGraph: {
    type: 'website',
    url: '/blog',
    title: 'Blog',
    description: 'Writing on Nub — the all-in-one toolkit for Node.js.',
    images: [{ url: blogOgImage, width: 1200, height: 630, alt: 'Nub Blog' }],
  },
  twitter: {
    card: 'summary_large_image',
    title: 'Blog',
    description: 'Writing on Nub — the all-in-one toolkit for Node.js. Notes on the thesis, the toolchain, and what ships next.',
    images: [blogOgImage],
  },
};

// Newest first: by date, then by release version so same-day releases (two posts
// sharing a `date`) still order by version rather than falling back to glob order.
function versionRank(url: string): number {
  const m = url.match(/nub-(\d+)-(\d+)-(\d+)/);
  if (!m) return 0;
  const [, major, minor, patch] = m;
  return Number(major) * 1_000_000 + Number(minor) * 1_000 + Number(patch);
}

export default function BlogIndex() {
  // `date` accepts an ISO 8601 UTC timestamp (e.g. 2026-07-07T12:00:00Z) to
  // order same-day posts; a date-only value parses as UTC midnight. Same-day
  // release posts tie-break by version; the URL compare is a last-resort guard
  // against nondeterministic file order.
  const posts = [...blog.getPages()].sort((a, b) => {
    const byDate =
      new Date(b.data.date ?? 0).getTime() -
      new Date(a.data.date ?? 0).getTime();
    if (byDate !== 0) return byDate;
    const byVersion = versionRank(b.url) - versionRank(a.url);
    return byVersion !== 0 ? byVersion : b.url.localeCompare(a.url);
  });

  return (
    <div className="mx-auto max-w-3xl px-6 py-24">
      <p className="eyebrow text-ember">Writing</p>
      <h1 className="mt-4 font-display text-5xl font-medium tracking-tight">
        The Nub blog
      </h1>
      <p className="mt-4 text-lg text-fd-muted-foreground">
        Notes on the toolkit, the thesis, and what ships next.
      </p>

      <div className="mt-16 space-y-2">
        {posts.map((post) => (
          <Link
            key={post.url}
            href={post.url}
            className="group block border-t border-fd-border py-8 transition last:border-b"
          >
            <div className="flex items-center gap-3 font-mono text-xs uppercase tracking-[0.14em] text-fd-muted-foreground">
              <time>{formatDate(post.data.date)}</time>
              <span aria-hidden>·</span>
              <span>{post.data.author}</span>
            </div>
            <h2 className="mt-3 font-display text-2xl font-medium leading-snug transition group-hover:text-ember md:text-3xl">
              {renderInlineCode(post.data.title)}
            </h2>
            {post.data.description ? (
              <p className="mt-2 max-w-2xl text-fd-muted-foreground">
                {post.data.description}
              </p>
            ) : null}
            <span className="mt-4 inline-flex items-center gap-1.5 text-sm text-sky">
              Read{' '}
              <span aria-hidden className="transition-transform group-hover:translate-x-0.5">
                →
              </span>
            </span>
          </Link>
        ))}
      </div>
    </div>
  );
}

function formatDate(date: string | Date | undefined): string {
  if (!date) return '';
  return new Date(date).toLocaleDateString('en-US', {
    year: 'numeric',
    month: 'long',
    day: 'numeric',
  });
}
