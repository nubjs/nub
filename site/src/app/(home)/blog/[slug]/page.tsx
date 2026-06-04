import Link from 'next/link';
import { notFound } from 'next/navigation';
import { InlineTOC } from 'fumadocs-ui/components/inline-toc';
import { blog } from '@/lib/source';
import { getMDXComponents } from '../../../../../mdx-components';

export default async function BlogPost(props: {
  params: Promise<{ slug: string }>;
}) {
  const { slug } = await props.params;
  const page = blog.getPage([slug]);
  if (!page) notFound();

  const MDXContent = page.data.body;

  return (
    <div className="mx-auto w-full min-w-0 max-w-3xl px-6 py-20">
      <Link
        href="/blog"
        className="font-mono text-xs uppercase tracking-[0.14em] text-fd-muted-foreground transition hover:text-ember"
      >
        ← Blog
      </Link>

      <header className="mt-8 border-b border-fd-border pb-10">
        <div className="flex items-center gap-3 font-mono text-xs uppercase tracking-[0.14em] text-ember">
          <time>{formatDate(page.data.date)}</time>
          <span aria-hidden className="text-fd-muted-foreground">
            ·
          </span>
          <span className="text-fd-muted-foreground">{page.data.author}</span>
        </div>
        <h1 className="mt-5 font-display text-4xl font-medium leading-[1.05] tracking-tight md:text-5xl">
          {page.data.title}
        </h1>
        {page.data.description ? (
          <p className="mt-5 text-xl leading-relaxed text-fd-muted-foreground">
            {page.data.description}
          </p>
        ) : null}
      </header>

      {page.data.toc.length > 0 ? (
        <InlineTOC items={page.data.toc} className="mt-8" />
      ) : null}

      <article className="prose mt-10">
        <MDXContent components={getMDXComponents()} />
      </article>
    </div>
  );
}

export function generateStaticParams() {
  return blog.getPages().map((page) => ({
    slug: page.slugs[0],
  }));
}

export async function generateMetadata(props: {
  params: Promise<{ slug: string }>;
}) {
  const { slug } = await props.params;
  const page = blog.getPage([slug]);
  if (!page) notFound();
  return {
    title: page.data.title,
    description: page.data.description,
  };
}

function formatDate(date: string | Date | undefined): string {
  if (!date) return '';
  return new Date(date).toLocaleDateString('en-US', {
    year: 'numeric',
    month: 'long',
    day: 'numeric',
  });
}
