import type { ReactNode } from 'react';

/* A captioned image for blog and docs prose. Every image on the site carries a
   caption, so the caption is required rather than optional; `href` wraps the
   image in a link (a screenshot of a comment links to the comment). A plain
   `<img>` rather than next/image: the sources are static files under
   `public/` with no dimension metadata, and the layout is a full-width figure
   inside a prose column, so there is nothing for the optimizer to size against. */
export function Figure({
  src,
  alt,
  caption,
  href,
}: {
  src: string;
  alt: string;
  caption: ReactNode;
  href?: string;
}) {
  // eslint-disable-next-line @next/next/no-img-element
  const img = <img src={src} alt={alt} className="w-full rounded-lg border border-fd-border" />;
  return (
    <figure className="not-prose my-8">
      {href ? (
        <a href={href} target="_blank" rel="noreferrer" className="block">
          {img}
        </a>
      ) : (
        img
      )}
      {/* Capped at 60% of the column and centered. A caption set to the full
          measure reads as body copy and competes with the figure above it. */}
      <figcaption className="mx-auto mt-3 max-w-full sm:max-w-[60%] text-center text-sm leading-relaxed text-fd-muted-foreground">
        {caption}
      </figcaption>
    </figure>
  );
}
