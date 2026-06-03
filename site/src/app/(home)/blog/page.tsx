import Link from 'next/link';
import type { Metadata } from 'next';
import { blog } from '@/lib/source';

export const metadata: Metadata = {
  title: 'Blog',
  description: 'Writing on Nub — the toolkit for Node.js.',
};

export default function BlogIndex() {
  const posts = [...blog.getPages()].sort(
    (a, b) =>
      new Date(b.data.date ?? 0).getTime() -
      new Date(a.data.date ?? 0).getTime(),
  );

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
              {post.data.title}
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
