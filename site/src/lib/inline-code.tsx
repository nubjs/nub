import { Fragment, type ReactNode } from 'react';

/* Frontmatter titles are plain strings, so backtick spans in them never pass
   through MDX. These two helpers close that gap: `renderInlineCode` for HTML
   surfaces, `stripInlineCode` for plain-text surfaces — the document title,
   og/twitter cards, and the OG image — where a literal backtick would render as
   unprocessed markup. An unpaired backtick is left verbatim in both.

   The rendered token carries `title-code`: a title sits outside the prose
   scope, so it gets none of the chip styling body inline code inherits, and the
   global `:not(pre) > code` size (0.95em) reads large against display type.
   `global.css` sizes and rounds the chip under that class. */

const CODE_SPAN = /`([^`]+)`/g;

export function renderInlineCode(text: string): ReactNode {
  const parts = text.split(CODE_SPAN);
  if (parts.length === 1) return text;
  return parts.map((part, i) =>
    i % 2 === 1 ? (
      <code key={i} className="title-code">
        {part}
      </code>
    ) : (
      <Fragment key={i}>{part}</Fragment>
    ),
  );
}

export function stripInlineCode(text: string): string {
  return text.replace(CODE_SPAN, '$1');
}
