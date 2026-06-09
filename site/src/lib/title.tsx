import { Fragment, type ReactNode } from 'react';

/**
 * Render backtick spans in a frontmatter title as real <code> elements, so
 * `title: Package meta-manager (\`nub pm\`)` gets code styling in the h1 and
 * sidebar instead of literal backticks.
 */
export function renderTitle(title: string): ReactNode {
  if (!title.includes('`')) return title;
  const parts = title.split('`');
  return (
    <>
      {parts.map((part, i) =>
        i % 2 === 1 ? <code key={i}>{part}</code> : <Fragment key={i}>{part}</Fragment>,
      )}
    </>
  );
}

/** The same title with backticks stripped — for places that need plain text (<title>, og tags). */
export function plainTitle(title: string): string {
  return title.replaceAll('`', '');
}
