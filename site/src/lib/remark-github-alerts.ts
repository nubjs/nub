/* Render GitHub-style alert blockquotes as the site's <Callout>.

   Markdown has no native handling for GitHub's alert syntax — `> [!NOTE]`,
   `[!TIP]`, `[!IMPORTANT]`, `[!WARNING]`, `[!CAUTION]` — so without this the
   marker renders as literal `[!NOTE]` text inside a plain blockquote. This walks
   the mdast, finds a blockquote whose first line is an alert marker, strips the
   marker, and rewrites the blockquote into the `<Callout>` registered in
   mdx-components.tsx — so an alert authored in a blog post or doc gets the site's
   uniform callout treatment instead of a raw blockquote.

   Appended to fumadocs' default remark plugins, so it runs AFTER remark-mdx:
   emitting an `mdxJsxFlowElement` is valid at that point and it renders through
   the JSX component map. (No unist-util-visit dependency, matching
   `remark-node-version`.) */

// GitHub alert kind → the site Callout `type` (drives the color rule) + the title
// label GitHub shows. IMPORTANT has no purple analogue in fumadocs; `info` keeps it
// neutral, consistent with the site treating callouts as uniform cards.
const ALERTS: Record<string, { type: string; title: string }> = {
  NOTE: { type: 'info', title: 'Note' },
  TIP: { type: 'success', title: 'Tip' },
  IMPORTANT: { type: 'info', title: 'Important' },
  WARNING: { type: 'warn', title: 'Warning' },
  CAUTION: { type: 'error', title: 'Caution' },
};

// The marker plus the horizontal whitespace and single newline that follow it, so
// the callout body starts at the next line. `$` covers the marker sitting alone in
// its own paragraph (body in a following paragraph).
const MARKER = /^\[!(NOTE|TIP|IMPORTANT|WARNING|CAUTION)\][^\S\n]*(?:\n|$)/i;

interface MdNode {
  type?: string;
  value?: unknown;
  children?: MdNode[];
}

function toCallout(blockquote: MdNode): MdNode | undefined {
  const firstPara = blockquote.children?.[0];
  if (firstPara?.type !== 'paragraph') return undefined;
  const lead = firstPara.children?.[0];
  if (lead?.type !== 'text' || typeof lead.value !== 'string') return undefined;

  const m = lead.value.match(MARKER);
  if (!m) return undefined;
  const meta = ALERTS[m[1].toUpperCase()];

  lead.value = lead.value.slice(m[0].length);
  // Marker was the whole leading paragraph (body follows in later paragraphs) →
  // drop the now-empty paragraph so the callout doesn't open with a blank line.
  if (lead.value === '' && firstPara.children?.length === 1) {
    blockquote.children?.shift();
  }

  const callout = {
    type: 'mdxJsxFlowElement',
    name: 'Callout',
    attributes: [
      { type: 'mdxJsxAttribute', name: 'type', value: meta.type },
      { type: 'mdxJsxAttribute', name: 'title', value: meta.title },
    ],
    children: blockquote.children ?? [],
  };
  return callout as unknown as MdNode;
}

export function remarkGithubAlerts() {
  return (tree: unknown): void => {
    const walk = (node: MdNode): void => {
      const kids = node.children;
      if (!Array.isArray(kids)) return;
      for (let i = 0; i < kids.length; i++) {
        const child = kids[i];
        if (child?.type === 'blockquote') {
          const callout = toCallout(child);
          if (callout) {
            kids[i] = callout;
            continue; // marker stripped; children carried over, no re-descent needed
          }
        }
        walk(child);
      }
    };
    walk(tree as MdNode);
  };
}
