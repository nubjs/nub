'use client';

import { useCallback, useEffect, useRef, useState } from 'react';
import { CodeBlock, Pre } from 'fumadocs-ui/components/codeblock';
import {
  AnsiScreen,
  type AnsiRecording,
  type AnsiRow,
} from '@/lib/ansi-screen';

/* Plays a recorded terminal session — a real `script -r` capture of the nub
   CLI, escapes and timings intact — inside the same code-block chrome a static
   ```ansi fence gets. Spinners, in-place redraws and the boxed first-run
   message are behaviors a screenshot cannot show and a GIF shows badly: a GIF
   of a 100-column terminal is either unreadably small or a megabyte, and it
   loops whether or not anyone is looking.

   NEVER AUTOPLAYS, NEVER LOOPS. The poster is the session's FINAL frame, so the
   block reads exactly like the equivalent static fence before anyone touches
   it, and a reader who does not press play loses nothing. Playback starts from
   a blank screen only on an explicit click and stops on the last event, back at
   the poster. That also settles `prefers-reduced-motion` by construction —
   there is no motion until the reader asks for it, and the resting state is a
   still frame either way.

   The screen model, and the reasoning behind hand-rolling one, is in
   `src/lib/ansi-screen.ts`. Recordings are built by `src/lib/ansi-record.ts`. */

function PlayGlyph() {
  return (
    <svg
      aria-hidden
      viewBox="0 0 24 24"
      className="size-5 translate-x-[1px] fill-current"
    >
      <path d="M8 5.5v13l11-6.5z" />
    </svg>
  );
}

export function AnsiPlayer({
  src,
  cols,
  rows,
  caption,
}: {
  /** A recording under `public/ansi/`, built by `src/lib/ansi-record.ts`. */
  src: string;
  /* The recorded terminal size. Both are in the recording too; passing them
     reserves the block's exact box before the fetch lands, and `cols` is
     load-bearing beyond that — see the width note below. */
  cols: number;
  rows: number;
  caption: string;
}) {
  const [screen, setScreen] = useState<AnsiRow[] | null>(null);
  const [playing, setPlaying] = useState(false);
  const [ready, setReady] = useState(false);
  const recording = useRef<AnsiRecording | null>(null);
  const frame = useRef<number | null>(null);

  useEffect(() => {
    let cancelled = false;
    fetch(src)
      .then((r) => r.json() as Promise<AnsiRecording>)
      .then((rec) => {
        if (cancelled) return;
        recording.current = rec;
        // The poster: every event applied, i.e. the frame playback ends on.
        const end = new AnsiScreen(rec.cols, rec.rows);
        for (const [, chunk] of rec.events) end.write(chunk);
        setScreen(end.render());
        setReady(true);
      })
      .catch(() => {
        // A failed fetch leaves the reserved-height skeleton and no button —
        // an empty terminal beats a play control that does nothing.
      });
    return () => {
      cancelled = true;
    };
  }, [src]);

  useEffect(
    () => () => {
      if (frame.current !== null) cancelAnimationFrame(frame.current);
    },
    [],
  );

  const play = useCallback(() => {
    const rec = recording.current;
    if (!rec) return;
    const live = new AnsiScreen(rec.cols, rec.rows);
    let next = 0;
    const start = performance.now();
    setScreen(live.render());
    setPlaying(true);

    /* Absolute timestamps, re-derived from the clock every frame: a frame the
       browser skipped (a background tab, a slow paint) is caught up in the next
       one instead of stretching the session. Re-rendering only when an event
       actually landed keeps React at the recording's own rate — roughly 15
       renders a second for a braille spinner — rather than at 60. */
    const step = () => {
      const elapsed = performance.now() - start;
      let advanced = false;
      while (next < rec.events.length && rec.events[next][0] <= elapsed) {
        live.write(rec.events[next][1]);
        next++;
        advanced = true;
      }
      if (advanced) setScreen(live.render());
      if (next < rec.events.length) {
        frame.current = requestAnimationFrame(step);
      } else {
        frame.current = null;
        setPlaying(false);
      }
    };
    frame.current = requestAnimationFrame(step);
  }, []);

  const lines = screen ?? Array.from({ length: rows }, () => null);

  /* WIDTH. The card is sized to the recording's own column count rather than to
     the prose column, because the CLI centers its boxed messages against the
     terminal width it saw. Trailing blanks are trimmed out of every row, so
     shrink-to-fit would land on the longest LINE and push a centered box off
     centre; pinning the grid to `cols` and letting the card shrink around it is
     what makes "centered in the terminal" read as centered on the page. `ch` is
     resolved on the `code` element, which is the one in the monospace font, and
     the two padding variables are fumadocs' own `.line` insets — they sit
     INSIDE the grid, so leaving them out costs four columns.

     LEADING. Box-drawing verticals have to tile. `│` in Geist Mono inks 17.03px
     at the 13px code size, against the site's default 18.57px line box — the
     1.5px shortfall is what turns a box border into a dashed one. 1.31 makes the
     line box exactly the glyph, so borders close and the rounded corners meet
     the verticals. It is also roughly a terminal's own leading. */
  const grid = {
    width: `calc(${cols}ch + var(--padding-left) + var(--padding-right))`,
  } as const;

  return (
    <figure className="not-prose my-8">
      <div className="relative mx-auto w-max max-w-full">
        <CodeBlock allowCopy={false} className="my-0 w-full">
          <Pre className="leading-[1.31]">
            <code style={grid}>
              {lines.map((row, i) => (
                <span
                  key={i}
                  className="line"
                  {...(row?.prompt ? { 'data-cmd': '' } : {})}
                >
                  {row?.runs.map((run, j) => {
                    // `$ ` on a command line becomes the unselectable ember
                    // prompt the shell fences use — same markup, same rules in
                    // global.css, so a copy skips it here too.
                    if (row.prompt && j === 0) {
                      const at = run.text.indexOf('$ ');
                      return (
                        <span key={j}>
                          {at > 0 ? run.text.slice(0, at) : null}
                          <span className="console-prompt">$ </span>
                          <span
                            style={{
                              color: run.color,
                              backgroundColor: run.background ?? undefined,
                              fontWeight: run.bold ? 'bold' : undefined,
                            }}
                          >
                            {run.text.slice(at + 2)}
                          </span>
                        </span>
                      );
                    }
                    return (
                      <span
                        key={j}
                        style={{
                          color: run.color,
                          backgroundColor: run.background ?? undefined,
                          fontWeight: run.bold ? 'bold' : undefined,
                          textDecoration: run.underline
                            ? 'underline'
                            : run.strike
                              ? 'line-through'
                              : undefined,
                        }}
                      >
                        {run.text}
                      </span>
                    );
                  })}
                </span>
              ))}
            </code>
          </Pre>
        </CodeBlock>

        {ready && !playing ? (
          <button
            type="button"
            onClick={play}
            aria-label={`Play the recorded terminal session: ${caption}`}
            /* The scrim is keyed to the panel's own ground, not to `fd-card`:
               a code panel is near-black in BOTH themes, so a theme-following
               scrim turns the poster grey in light mode. */
            style={{
              backgroundColor:
                'color-mix(in srgb, var(--nub-code-background) 55%, transparent)',
            }}
            className="ansi-player-overlay group absolute inset-0 flex cursor-pointer items-center justify-center rounded-xl focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-fd-ring"
          >
            <span className="flex size-11 items-center justify-center rounded-full bg-ember text-white shadow-lg transition-transform group-hover:scale-105">
              <PlayGlyph />
            </span>
          </button>
        ) : null}
      </div>
      <figcaption className="mx-auto mt-3 max-w-full sm:max-w-[60%] text-center text-sm leading-relaxed text-fd-muted-foreground">
        {caption}
      </figcaption>
    </figure>
  );
}
