/* Build tool — converts a recorded terminal session into the JSON that
   `<AnsiPlayer>` replays. Not imported by the site; run it by hand with `nub`
   when a recording needs to be recaptured.

   RECORDING. `script -r` writes every pty write with a timestamp, which is the
   whole input this tool needs. The pty it allocates has no size when stdout is
   not a terminal, and a zero-column terminal makes the nub CLI collapse its
   boxed output to a single line — so the size is set from inside:

     script -q -r out.bin sh -c '
       stty cols 100 rows 40
       cd <fixture>
       printf "$ nub compile cli.ts\r\n"
       sleep 0.35
       nub compile cli.ts
     '

   The two recordings under `public/ansi/` were captured that way. `cli.ts` is a
   commander/chalk/ora/zod CLI; the install-message one is a `--smol` build run
   with a cold `XDG_CACHE_HOME`, which is what makes it download a Node.

   CONVERSION. Playback columns must equal the recorded columns, because the
   spinner repositions with a column-relative `\e[100D` and the boxed message is
   centered by the CLI against the size it saw. Rows are free for the compile
   recording (it only moves relatively) and fixed for the boxed one.

     nub src/lib/ansi-record.ts out.bin public/ansi/name.json --cols 100 --rows 10

   Long silences are capped rather than replayed: a reader watching a play-button
   animation reads a two-second pause as a stall. Nothing else is rescaled, so
   every phase runs at the duration it really took. */

import { readFileSync, writeFileSync } from 'node:fs';
import { AnsiScreen, type AnsiRecording } from './ansi-screen';

// `script -r` record header, packed little-endian: u64 length, u64 seconds,
// u32 microseconds, u32 direction ('s' start, 'o' output, 'i' input, 'e' end).
const HEADER = 24;

interface RawRecord {
  t: number;
  direction: string;
  data: string;
}

function readScriptRecording(file: string): RawRecord[] {
  const buf = readFileSync(file);
  const out: RawRecord[] = [];
  let off = 0;
  while (off + HEADER <= buf.length) {
    const len = Number(buf.readBigUInt64LE(off));
    const sec = Number(buf.readBigUInt64LE(off + 8));
    const usec = buf.readUInt32LE(off + 16);
    const direction = String.fromCharCode(buf.readUInt32LE(off + 20) & 0xff);
    off += HEADER;
    if (off + len > buf.length) break;
    out.push({
      t: sec + usec / 1e6,
      direction,
      data: buf.subarray(off, off + len).toString('utf8'),
    });
    off += len;
  }
  return out;
}

function arg(name: string, fallback?: string): string {
  const i = process.argv.indexOf(`--${name}`);
  if (i !== -1 && process.argv[i + 1]) return process.argv[i + 1];
  if (fallback !== undefined) return fallback;
  throw new Error(`missing --${name}`);
}

const [input, output] = process.argv.slice(2).filter((a) => !a.startsWith('--'));
if (!input || !output) {
  console.error('usage: ansi-record.ts <script -r file> <out.json> --cols N --rows N');
  process.exit(1);
}
const cols = Number(arg('cols'));
const rows = Number(arg('rows'));
const maxGap = Number(arg('max-gap', '600'));
// Writes closer together than a frame are indistinguishable on playback, and
// merging them roughly halves the file.
const coalesce = Number(arg('coalesce', '16'));

const records = readScriptRecording(input).filter((r) => r.direction === 'o');
// The shell echoes an EOF marker onto the pty before the command runs. It is an
// artifact of driving `script` non-interactively, not part of the session.
if (records.length && /^\^D\x08*$/.test(records[0].data)) records.shift();
if (!records.length) throw new Error('no output records');

const base = records[0].t;
const events: [number, string][] = [];
let clock = 0;
let previous = base;
for (const r of records) {
  clock += Math.min(r.t - previous, maxGap / 1000);
  previous = r.t;
  const ms = Math.round(clock * 1000);
  const last = events[events.length - 1];
  if (last && ms - last[0] <= coalesce) last[1] += r.data;
  else events.push([ms, r.data]);
}

const recording: AnsiRecording = {
  cols,
  rows,
  durationMs: events[events.length - 1][0],
  events,
  timing: `captured with script -r; real timing, silences over ${maxGap}ms capped`,
};
writeFileSync(output, `${JSON.stringify(recording)}\n`);

// Replaying into the same screen model the player uses is the cheap check that
// the capture survived conversion: a wrong column count or a dropped escape
// shows up here as a mangled final frame, before anything reaches a page.
const screen = new AnsiScreen(cols, rows);
for (const [, chunk] of events) screen.write(chunk);
console.error(
  `${output}: ${events.length} events, ${recording.durationMs}ms, ${cols}x${rows}\n`,
);
for (const row of screen.render()) {
  console.error(`|${row.runs.map((r) => r.text).join('').padEnd(cols)}|`);
}
