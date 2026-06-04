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
      className={`overflow-hidden rounded-xl border border-fd-border bg-[#0b0a08] shadow-[0_30px_80px_-40px_rgba(0,0,0,0.9)] ${className}`}
    >
      <div className={`flex items-center border-b border-fd-border/70 ${head}`}>
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
  lines: { cmd?: string; comment?: string; out?: string }[];
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
              <span className="text-fd-muted-foreground">{line.out}</span>
            ) : (
              <>
                <span className="select-none text-ember">$ </span>
                <span className="text-fd-foreground">
                  {line.comment && hasComments ? line.cmd!.padEnd(width) : line.cmd}
                </span>
                {line.comment ? (
                  <span className="text-fd-muted-foreground">{`   # ${line.comment}`}</span>
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
          className="overflow-x-auto bg-transparent px-5 py-4 font-mono text-[0.8rem] leading-7"
        />
      ),
    },
  });

  return <Window className={className}>{rendered}</Window>;
}

/* Horizontal benchmark bars. The fastest row is tinted with `accent`. */
export function BenchBars({
  rows,
  max,
  accent = 'ember',
  unit = 'ms',
}: {
  rows: { cmd: string; ms: number; ratio?: number | null; us?: boolean }[];
  max: number;
  accent?: 'ember' | 'acid' | 'sky';
  unit?: string;
}) {
  const barAccent =
    accent === 'acid' ? 'bg-acid' : accent === 'sky' ? 'bg-sky' : 'bg-ember';
  const textAccent =
    accent === 'acid' ? 'text-acid' : accent === 'sky' ? 'text-sky' : 'text-ember';

  return (
    <div className="space-y-4">
      {rows.map((r) => (
        <div key={r.cmd}>
          <div className="mb-1.5 flex items-baseline justify-between gap-4">
            <span
              className={`font-mono text-sm ${r.us ? `font-semibold ${textAccent}` : 'text-fd-foreground'}`}
            >
              {r.cmd}
            </span>
            <span className="shrink-0 font-mono text-xs tabular-nums text-fd-muted-foreground">
              {r.ms} {unit}
              {r.ratio ? `  ·  ${r.ratio}× slower` : ''}
            </span>
          </div>
          <div className="h-2.5 overflow-hidden rounded-full bg-fd-card/60">
            <div
              className={`h-full rounded-full ${r.us ? barAccent : 'bg-fd-foreground/25'}`}
              style={{ width: `${Math.max((r.ms / max) * 100, 3)}%` }}
            />
          </div>
        </div>
      ))}
    </div>
  );
}
