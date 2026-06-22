import type { ReactNode } from 'react';
import { highlight } from 'fumadocs-core/highlight';

/* A window chrome with the three traffic-light dots and no title label. */
function Window({ children, className = '', size = 'sm' }: { children: ReactNode; className?: string; size?: 'sm' | 'lg' }) {
  // Scale the chrome (dots + header) with the body so a larger terminal stays
  // proportional, instead of bumping only the font and leaving tiny dots.
  const dot = size === 'lg' ? 'h-3 w-3' : 'h-2.5 w-2.5';
  const head = size === 'lg' ? 'gap-2.5 px-5 py-3.5' : 'gap-2 px-4 py-2.5';
  return (
    <div
      className={`nub-code-panel overflow-hidden rounded-xl border ${className}`}
    >
      <div className={`nub-code-separator flex items-center border-b ${head}`}>
        <span className={`${dot} rounded-full bg-ember/80`} />
        <span className={`${dot} rounded-full bg-acid/70`} />
        <span className={`${dot} rounded-full bg-sky/70`} />
      </div>
      {children}
    </div>
  );
}

/* A terminal card. Comments are padded to a common column so every `# ...`
   lines up regardless of command length. */
export function Terminal({
  lines,
  className = '',
  size = 'sm',
}: {
  lines: {
    cmd?: string;
    comment?: string;
    out?: string;
    bright?: boolean;
    /* tone for an `out` line: an error/refusal line reads ember-red */
    tone?: 'error';
  }[];
  className?: string;
  size?: 'sm' | 'lg';
}) {
  const hasComments = lines.some((l) => l.comment);
  const width = Math.max(...lines.map((l) => l.cmd?.length ?? 0));
  const body =
    size === 'lg' ? 'px-6 py-5 text-[0.98rem] leading-[2.1]' : 'px-5 py-4 text-[0.8rem] leading-7';

  return (
    <Window className={className} size={size}>
      <pre className={`overflow-x-auto font-mono ${body}`}>
        {lines.map((line, i) => (
          <div key={i} className="whitespace-pre">
            {line.out !== undefined ? (
              <span
                className={
                  line.tone === 'error'
                    ? 'text-ember'
                    : line.bright
                      ? 'nub-code-fg'
                      : 'nub-code-muted'
                }
              >
                {line.out}
              </span>
            ) : (
              <>
                <span className="select-none text-ember">$ </span>
                <span className="nub-code-fg">
                  {line.comment && hasComments ? line.cmd!.padEnd(width) : line.cmd}
                </span>
                {line.comment ? (
                  <span className="nub-code-muted">{`   # ${line.comment}`}</span>
                ) : null}
              </>
            )}
          </div>
        ))}
      </pre>
    </Window>
  );
}

/* A syntax-highlighted source card. Uses Fumadocs' shiki highlighter with the
   warm `vesper` theme; the window background shows through (shiki's own bg is
   dropped). Async server component — resolved at build time. */
export async function Source({
  code,
  lang = 'tsx',
  className = '',
}: {
  code: string;
  lang?: string;
  className?: string;
}) {
  const rendered = await highlight(code.trim(), {
    lang,
    theme: 'vesper',
    components: {
      pre: ({ style: _style, ...props }) => (
        <pre
          {...props}
          className="nub-code-fg overflow-x-auto bg-transparent px-5 py-4 font-mono text-[0.8rem] leading-7"
        />
      ),
    },
  });

  return <Window className={className}>{rendered}</Window>;
}

/* A self-contained benchmark panel for MDX content (blog posts): the homepage's
   dark card + label, wrapping BenchBars. `not-prose` keeps article typography
   from restyling the chart. */
export function Bench({
  label,
  rows,
  max,
  accent = 'ember',
  source,
  caption,
}: {
  label: string;
  rows: { cmd: string; ms: number; ratio?: number | null; label?: string; us?: boolean }[];
  max: number;
  accent?: 'ember' | 'acid' | 'sky';
  /* Optional link to the benchmark source — rendered as a small centered
     caption underneath the card. */
  source?: string;
  /* Optional descriptive caption text shown before the link. */
  caption?: string;
}) {
  return (
    <div className="not-prose my-6">
      <div className="nub-code-panel rounded-xl border p-6">
        <p className="nub-code-muted mb-5 font-mono text-[0.7rem] uppercase tracking-[0.14em]">
          {label}
        </p>
        <BenchBars accent={accent} max={max} rows={rows} />
      </div>
      {(caption || source) && (
        <p className="mt-2.5 text-center text-xs text-fd-muted-foreground">
          {caption ? <span>{caption} </span> : null}
          {source ? (
            <a
              href={source}
              target="_blank"
              rel="noopener noreferrer"
              className="nub-code-link nub-code-muted underline decoration-dotted underline-offset-4"
            >
              View benchmark →
            </a>
          ) : null}
        </p>
      )}
    </div>
  );
}

/* Horizontal benchmark bars. The fastest row is tinted with `accent`. */
export function BenchBars({
  rows,
  max,
  accent = 'ember',
  unit = 'ms',
}: {
  rows: { cmd: string; ms: number; ratio?: number | null; label?: string; us?: boolean }[];
  max: number;
  accent?: 'ember' | 'acid' | 'sky' | 'pink';
  unit?: string;
}) {
  const barAccent =
    accent === 'acid' ? 'bg-acid' : accent === 'sky' ? 'bg-sky' : accent === 'pink' ? 'bg-pink' : 'bg-ember';
  const textAccent =
    accent === 'acid' ? 'text-acid' : accent === 'sky' ? 'text-sky' : accent === 'pink' ? 'text-pink' : 'text-ember';

  return (
    <div className="space-y-4">
      {rows.map((r) => (
        <div key={r.cmd}>
          <div className="mb-1.5 flex items-baseline justify-between gap-4">
            <span
              className={`font-mono text-sm ${r.us ? `font-semibold ${textAccent}` : 'nub-code-fg'}`}
            >
              {r.cmd}
            </span>
            <span className="nub-code-muted shrink-0 font-mono text-xs tabular-nums">
              {r.ms} {unit}
              {r.label ? `  ·  ${r.label}` : r.ratio ? `  ·  ${r.ratio}× slower` : ''}
            </span>
          </div>
          <div className="nub-code-track h-2.5 overflow-hidden rounded-full">
            <div
              className={`h-full rounded-full ${r.us ? barAccent : 'nub-code-bar-muted'}`}
              style={{ width: `${Math.max((r.ms / max) * 100, 3)}%` }}
            />
          </div>
        </div>
      ))}
    </div>
  );
}
