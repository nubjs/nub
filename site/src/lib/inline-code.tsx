import { Fragment, type ReactNode } from 'react';

/* Frontmatter titles are plain strings, so backtick spans in them never pass
   through MDX. These two helpers close that gap: `renderInlineCode` for HTML
   surfaces (the global `:not(pre) > code` rule sizes the token em-relative, so
   it tracks the heading), `stripInlineCode` for plain-text surfaces — the
   document title, og/twitter cards, and the OG image — where a literal
   backtick would render as unprocessed markup. An unpaired backtick is left
   verbatim in both. */

const CODE_SPAN = /`([^`]+)`/g;

export function renderInlineCode(text: string): ReactNode {
  const parts = text.split(CODE_SPAN);
  if (parts.length === 1) return text;
  return parts.map((part, i) =>
    i % 2 === 1 ? <code key={i}>{part}</code> : <Fragment key={i}>{part}</Fragment>,
  );
}

export function stripInlineCode(text: string): string {
  return text.replace(CODE_SPAN, '$1');
}
