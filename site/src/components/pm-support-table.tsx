import type { ReactNode } from 'react';

// At-a-glance support summary: one row per INFERRED package manager, with a
// flat set of code chips for the config files / package.json fields / env-var
// prefixes it reads. A green check = honored, an amber check = partial, a red
// X = a deliberate gap. A chip with a `note` reveals the partial breakdown on
// hover/focus (CSS-only tooltip + sr-only text). Chips are grounded in each
// PM's detailed CompatTable on its own install page.

interface PmItem {
  /** A config file, package.json field, or env-var glob — rendered as a code chip. */
  code: string;
  ok: boolean;
  /** Amber glyph — partial support (e.g. read-only). Overrides `ok` for the icon. */
  partial?: boolean;
  /** Plain-text breakdown surfaced on hover/focus of the chip. */
  note?: string;
}

interface PmRow {
  pm: ReactNode;
  /** Link the PM name (left column) to its full per-PM write-up. */
  href?: string;
  /** Small qualifier after the PM name, e.g. "read-only". */
  qualifier?: string;
  /** Accent the row — used for nub's own identity row at the bottom. */
  highlight?: boolean;
  items: PmItem[];
}

const STATUS_ICON_STYLE = { color: 'var(--status-ok)' } as const;
const WARN_ICON_STYLE = { color: 'var(--status-warn)' } as const;
const BAD_ICON_STYLE = { color: 'var(--status-bad)' } as const;

function CheckIcon() {
  return (
    <svg
      aria-hidden
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="3.5"
      strokeLinecap="round"
      strokeLinejoin="round"
      className="size-3 shrink-0"
      style={STATUS_ICON_STYLE}
    >
      <path d="M20 6 9 17l-5-5" />
    </svg>
  );
}

function PartialIcon() {
  return (
    <svg
      aria-hidden
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="3.5"
      strokeLinecap="round"
      strokeLinejoin="round"
      className="size-3 shrink-0"
      style={WARN_ICON_STYLE}
    >
      <path d="M20 6 9 17l-5-5" />
    </svg>
  );
}

function CrossIcon() {
  return (
    <svg
      aria-hidden
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="3.5"
      strokeLinecap="round"
      strokeLinejoin="round"
      className="size-3 shrink-0"
      style={BAD_ICON_STYLE}
    >
      <path d="M18 6 6 18" />
      <path d="m6 6 12 12" />
    </svg>
  );
}

// Sort rank so chips render supported → partial → unsupported (greens first).
const chipRank = (it: PmItem) => (it.partial ? 1 : it.ok ? 0 : 2);

function Chip({ code, ok, partial, note }: PmItem) {
  const glyph = partial ? <PartialIcon /> : ok ? <CheckIcon /> : <CrossIcon />;
  const label = partial ? 'Partially supported' : ok ? 'Supported' : 'Not supported';
  const chip = (
    <span
      className={`inline-flex items-center gap-1.5 rounded-md border border-fd-border bg-fd-muted/40 px-2 py-1 font-mono text-xs ${
        note ? 'cursor-help' : ''
      }`}
    >
      {glyph}
      <span className={partial || ok ? 'text-fd-foreground' : 'text-fd-muted-foreground'}>
        {code}
      </span>
      <span className="sr-only">
        {`. ${label}`}
        {note ? `. ${note}` : ''}
      </span>
    </span>
  );
  if (!note) return chip;
  // Reveal the breakdown on hover/focus via a CSS-only `group` + `group-hover`/
  // `group-focus-within` tooltip, mirroring the per-PM CompatTable pattern. The
  // wrapper is focusable, and the sr-only text inside the chip carries the note
  // for screen readers. No native `title` — it would double up with the custom
  // popover on hover.
  return (
    <span className="group relative inline-flex" tabIndex={0}>
      {chip}
      <span
        role="tooltip"
        className="pointer-events-none invisible absolute left-1/2 top-full z-20 mt-1.5 w-64 -translate-x-1/2 whitespace-normal rounded-md border border-fd-border bg-fd-popover px-3 py-2 text-left text-xs font-normal leading-relaxed text-fd-popover-foreground opacity-0 shadow-md transition-opacity duration-100 group-hover:visible group-hover:opacity-100 group-focus-within:visible group-focus-within:opacity-100"
      >
        {note}
      </span>
    </span>
  );
}

export function PmSupport({ rows }: { rows: PmRow[] }) {
  return (
    // No `overflow-hidden` on the wrapper, and `overflow-visible` forced onto the
    // table: a clip on either one swallows the chip tooltips of the LAST row,
    // which drop below the table's own bottom edge. The rounded corners the clip
    // used to buy are recovered by rounding the header and final-row cells
    // directly, so nothing paints outside the border radius.
    <div className="my-6 rounded-lg border border-fd-border [&_table]:my-0 [&_table]:overflow-visible">
      <table className="w-full border-collapse text-left text-sm">
        <thead>
          <tr className="border-b border-fd-border bg-fd-muted/40">
            <th className="w-px whitespace-nowrap rounded-tl-lg px-4 py-2.5 font-medium text-fd-muted-foreground">
              Package manager
            </th>
            <th className="rounded-tr-lg px-4 py-2.5 font-medium text-fd-muted-foreground">
              Config it reads
            </th>
          </tr>
        </thead>
        <tbody>
          {rows.map((r, i) => (
            <tr
              key={i}
              className={`border-b border-fd-border/60 align-top last:border-0 [&:last-child>td:first-child]:rounded-bl-lg [&:last-child>td:last-child]:rounded-br-lg ${
                r.highlight ? 'bg-pink/[0.04]' : ''
              }`}
            >
              <td
                className={`whitespace-nowrap px-4 py-3 font-mono font-medium ${
                  r.highlight ? 'text-pink' : 'text-fd-foreground'
                }`}
              >
                {r.href ? (
                  <a href={r.href} className="underline decoration-fd-border underline-offset-4 hover:decoration-current">
                    {r.pm}
                  </a>
                ) : (
                  r.pm
                )}
                {r.qualifier ? (
                  <span className="ml-1.5 font-sans text-xs font-normal text-fd-muted-foreground">
                    {r.qualifier}
                  </span>
                ) : null}
              </td>
              <td className="px-4 py-3">
                <span className="flex flex-wrap gap-1.5">
                  {[...r.items]
                    .sort((a, b) => chipRank(a) - chipRank(b))
                    .map((it, j) => (
                      <Chip key={j} {...it} />
                    ))}
                </span>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
