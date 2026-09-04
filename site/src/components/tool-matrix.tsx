import React from 'react';
import type { ReactNode } from 'react';

// Multi-column capability matrix: one row per capability, one column per tool.
// `CompatTable` next door covers the one- and two-column PM-parity shape; this is
// the N-tool comparison, and the two deliberately share a status vocabulary so a
// green check means the same thing on both.
//
// The rule that governs what may be written here: a `partial` cell MUST carry a
// note. An amber glyph with no explanation reads as a hedge, and a reader cannot
// tell a narrow documented caveat from a guess. `assertNotes` enforces it in
// development rather than leaving it to review.

export type Status = 'yes' | 'no' | 'partial';

/** A cell is a bare status, or a status plus the note its glyph explains. */
export type Cell = Status | { s: Status; note: string };

export interface MatrixRow {
  /** The capability. A ReactNode so backticked code renders from MDX. */
  feature: ReactNode;
  /** One entry per column, in `tools` order. */
  cells: Cell[];
}

const SVG_PROPS = {
  'aria-hidden': true,
  width: 14,
  height: 14,
  viewBox: '0 0 24 24',
  fill: 'none',
  stroke: 'currentColor',
  strokeWidth: 3,
  strokeLinecap: 'round',
  strokeLinejoin: 'round',
  className: 'size-3.5 shrink-0',
} as const;

const CHECK = (
  <svg {...SVG_PROPS}>
    <path d="M20 6 9 17l-5-5" />
  </svg>
);

const STATUS_META: Record<
  Status,
  { style: React.CSSProperties; label: string; icon: ReactNode }
> = {
  yes: { style: { color: 'var(--status-ok)' }, label: 'Supported', icon: CHECK },
  partial: {
    style: { color: 'var(--status-warn)' },
    label: 'Partially supported',
    icon: CHECK,
  },
  no: {
    style: { color: 'var(--status-bad)' },
    label: 'Not supported',
    icon: (
      <svg {...SVG_PROPS}>
        <path d="M18 6 6 18" />
        <path d="m6 6 12 12" />
      </svg>
    ),
  },
};

function split(cell: Cell): { s: Status; note?: string } {
  return typeof cell === 'string' ? { s: cell } : cell;
}

/** Every amber cell explains itself, or the build says which one does not. */
function assertNotes(rows: MatrixRow[], tools: string[]) {
  if (process.env.NODE_ENV === 'production') return;
  for (const row of rows) {
    row.cells.forEach((cell, i) => {
      const { s, note } = split(cell);
      if (s === 'partial' && !note) {
        const feature = typeof row.feature === 'string' ? row.feature : '(non-string row)';
        console.warn(
          `ToolMatrix: "${feature}" × ${tools[i] ?? `column ${i}`} is amber with no note. ` +
            `Write one, or make the cell green or red.`,
        );
      }
    });
  }
}

function StatusGlyph({ s, note }: { s: Status; note?: string }) {
  const { style, label, icon } = STATUS_META[s];
  // CSS-only reveal, matching CompatTable: `group` plus `group-hover` /
  // `group-focus-within`, with the note also in sr-only text so a screen reader
  // gets it without hovering. No native `title` — it would double up on hover.
  const glyph = (
    <span
      className={`inline-flex items-center font-medium ${
        note ? 'cursor-help underline decoration-dotted decoration-from-font underline-offset-4' : ''
      }`}
      style={style}
    >
      {icon}
      <span className="sr-only">
        {label}
        {note ? `. ${note}` : ''}
      </span>
    </span>
  );
  if (!note) return glyph;
  return (
    <span className="group relative inline-flex" tabIndex={0}>
      {glyph}
      <span
        role="tooltip"
        className="pointer-events-none invisible absolute left-1/2 top-full z-20 mt-1.5 w-64 -translate-x-1/2 whitespace-normal rounded-md border border-fd-border bg-fd-popover px-3 py-2 text-left text-xs font-normal leading-relaxed text-fd-popover-foreground opacity-0 shadow-md transition-opacity duration-100 group-hover:visible group-hover:opacity-100 group-focus-within:visible group-focus-within:opacity-100"
      >
        {note}
      </span>
    </span>
  );
}

export function ToolMatrix({
  tools,
  rows,
  us,
  caption,
}: {
  /** Column headers, left to right. */
  tools: string[];
  rows: MatrixRow[];
  /** The column to tint as ours. Matched against `tools` by exact string. */
  us?: string;
  caption?: ReactNode;
}) {
  assertNotes(rows, tools);
  const usIndex = us ? tools.indexOf(us) : -1;
  // Fixed layout with one <col> per tool: every tool column is the same width and
  // the capability column takes the rest. The blog's prose-table rule (first
  // columns shrink, last column expands) is scoped away from `.tool-matrix` in
  // global.css — under it the LAST tool column grew and the rest collapsed.
  //
  // `border-separate` with the row rule on the cells, not the <tr>: a collapsed
  // border is painted between the cells' backgrounds, so a tinted column showed
  // a hairline seam at every row.
  const usCell = (i: number) => (i === usIndex ? 'tm-us' : '');
  const rule = (last: boolean) => (last ? '' : 'border-b border-fd-border/60');
  return (
    <figure className="my-6">
      <div className="overflow-x-auto rounded-lg border border-fd-border [&_table]:my-0">
        <table className="tool-matrix w-full table-fixed border-separate border-spacing-0 text-left text-sm">
          <colgroup>
            <col />
            {tools.map((tool) => (
              <col key={tool} style={{ width: '5.5rem' }} />
            ))}
          </colgroup>
          <thead>
            <tr>
              <th className="border-b border-fd-border bg-fd-muted/40 px-4 py-2.5 font-medium text-fd-muted-foreground">
                Capability
              </th>
              {tools.map((tool, i) => (
                <th
                  key={tool}
                  scope="col"
                  className={`border-b border-fd-border bg-fd-muted/40 px-3 py-2.5 text-center font-mono text-[13px] font-medium ${usCell(i)} ${
                    i === usIndex ? 'text-fd-foreground' : 'text-fd-muted-foreground'
                  }`}
                >
                  {tool}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {rows.map((row, r) => {
              const last = r === rows.length - 1;
              return (
                <tr key={r}>
                  <th
                    scope="row"
                    className={`px-4 py-2.5 text-left align-middle font-normal text-fd-foreground ${rule(last)}`}
                  >
                    {row.feature}
                  </th>
                  {row.cells.map((cell, c) => {
                    const { s, note } = split(cell);
                    return (
                      <td
                        key={c}
                        className={`px-3 py-2.5 text-center align-middle ${usCell(c)} ${rule(last)}`}
                      >
                        <span className="inline-flex justify-center">
                          <StatusGlyph s={s} note={note} />
                        </span>
                      </td>
                    );
                  })}
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
      {caption ? (
        <figcaption className="mt-2 text-xs leading-relaxed text-fd-muted-foreground">
          {caption}
        </figcaption>
      ) : null}
    </figure>
  );
}
