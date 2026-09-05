import { SITE_ANSI_PALETTE } from './shiki-ansi';

/* A terminal screen model for replaying a recorded session — the runtime half
   of `<AnsiPlayer>` (`src/components/ansi-player.tsx`).

   WHY NOT AN OFF-THE-SHELF EMULATOR. `@xterm/headless` plus its serialize addon
   is a correct, complete VT emulator, and it is ~250 KB of JavaScript shipped to
   a blog reader to replay two recordings that between them use eleven escape
   sequences. It also renders to its own cell model, so matching the site's ANSI
   palette and `.line`/`.console-prompt` markup would mean walking its buffer
   and rebuilding these spans anyway. The subset below is what the nub CLI
   actually emits — SGR, CR/LF/BS/TAB, CUU/CUD/CUF/CUB/CHA/CUP, ED/EL, the
   alternate screen, cursor visibility — and the escapes it deliberately ignores
   (OSC, synchronized-update `?2026`) are ignorable precisely because a frame is
   rendered only after a whole chunk is applied. A recording that grows past
   this subset should reach for xterm rather than grow this file.

   FIDELITY TO THE STATIC FENCES. A ```ansi fence and a player of the same
   output must be indistinguishable, so the color rules here mirror shiki's ANSI
   tokenizer (`@shikijs/core`, `code-to-tokens-ansi.ts`) exactly: the 16 named
   colors come from the shared `SITE_ANSI_PALETTE`, an unset foreground is the
   `vesper` theme foreground, and `\e[2m` dim is 50% alpha on the resolved
   color rather than a font weight. */

// vesper defines no `editor.background`/`foreground` pair beyond `#FFF`, which
// is what shiki resolves as the theme foreground for an `ansi` fence — verified
// against the rendered markup, where an uncolored token carries `--shiki-dark:#FFF`.
const THEME_FG = '#FFF';
// Only reached through reverse video, which swaps in the theme background.
const THEME_BG = '#101010';

const NAMED = [
  'black',
  'red',
  'green',
  'yellow',
  'blue',
  'magenta',
  'cyan',
  'white',
] as const;
const BRIGHT = [
  'brightBlack',
  'brightRed',
  'brightGreen',
  'brightYellow',
  'brightBlue',
  'brightMagenta',
  'brightCyan',
  'brightWhite',
] as const;

/* Shiki's `dimColor`: half the alpha of an existing 8-digit hex, else append
   `80`. Reproduced rather than imported because it is not exported. */
function dim(color: string): string {
  const m = /^#([0-9a-f]{3,8})$/i.exec(color);
  if (!m) return color;
  const hex = m[1];
  if (hex.length === 3) return `#${hex[0]}${hex[0]}${hex[1]}${hex[1]}${hex[2]}${hex[2]}80`;
  if (hex.length === 6) return `#${hex}80`;
  if (hex.length === 8) {
    const a = Math.round(Number.parseInt(hex.slice(6, 8), 16) / 2)
      .toString(16)
      .padStart(2, '0');
    return `#${hex.slice(0, 6)}${a}`;
  }
  return color;
}

// xterm's 256-color cube and gray ramp. Indices 0-15 route to the site palette
// so `\e[38;5;9m` and `\e[91m` agree, which is also what shiki does.
function color256(i: number): string {
  if (i < 8) return SITE_ANSI_PALETTE[NAMED[i]];
  if (i < 16) return SITE_ANSI_PALETTE[BRIGHT[i - 8]];
  const hx = (n: number) => n.toString(16).padStart(2, '0');
  if (i < 232) {
    const n = i - 16;
    const level = (v: number) => (v === 0 ? 0 : 55 + v * 40);
    return `#${hx(level(Math.floor(n / 36)))}${hx(level(Math.floor(n / 6) % 6))}${hx(level(n % 6))}`;
  }
  const v = 8 + (i - 232) * 10;
  return `#${hx(v)}${hx(v)}${hx(v)}`;
}

interface Attr {
  fg: string | null;
  bg: string | null;
  bold: boolean;
  dim: boolean;
  underline: boolean;
  strike: boolean;
  reverse: boolean;
}

const DEFAULT_ATTR: Attr = {
  fg: null,
  bg: null,
  bold: false,
  dim: false,
  underline: false,
  strike: false,
  reverse: false,
};

/** One contiguous run of same-styled characters within a row. */
export interface AnsiRun {
  text: string;
  color: string;
  background: string | null;
  bold: boolean;
  underline: boolean;
  strike: boolean;
}

/** A rendered row: its runs, plus whether it reads as a `$ ` prompt line. */
export interface AnsiRow {
  runs: AnsiRun[];
  prompt: boolean;
}

interface Cell {
  ch: string;
  color: string;
  background: string | null;
  bold: boolean;
  underline: boolean;
  strike: boolean;
}

function blank(): Cell {
  return {
    ch: ' ',
    color: THEME_FG,
    background: null,
    bold: false,
    underline: false,
    strike: false,
  };
}

export class AnsiScreen {
  readonly cols: number;
  readonly rows: number;
  private grid: Cell[][];
  private saved: { grid: Cell[][]; x: number; y: number } | null = null;
  private x = 0;
  private y = 0;
  private attr: Attr = { ...DEFAULT_ATTR };
  // A chunk can be cut mid-escape by the pty, so the tail carries over.
  private pending = '';

  constructor(cols: number, rows: number) {
    this.cols = cols;
    this.rows = rows;
    this.grid = Array.from({ length: rows }, () =>
      Array.from({ length: cols }, blank),
    );
  }

  private clearRow(row: Cell[], from = 0, to = this.cols): void {
    for (let i = from; i < to; i++) row[i] = blank();
  }

  private scroll(): void {
    this.grid.shift();
    this.grid.push(Array.from({ length: this.cols }, blank));
    this.y = this.rows - 1;
  }

  private newline(): void {
    this.y++;
    if (this.y >= this.rows) this.scroll();
  }

  private put(ch: string): void {
    if (this.x >= this.cols) {
      this.x = 0;
      this.newline();
    }
    const { fg, bg } = this.resolve();
    this.grid[this.y][this.x] = {
      ch,
      color: fg,
      background: bg,
      bold: this.attr.bold,
      underline: this.attr.underline,
      strike: this.attr.strike,
    };
    this.x++;
  }

  private resolve(): { fg: string; bg: string | null } {
    const a = this.attr;
    let fg = a.reverse ? (a.bg ?? THEME_BG) : (a.fg ?? THEME_FG);
    const bg = a.reverse ? (a.fg ?? THEME_FG) : a.bg;
    if (a.dim) fg = dim(fg);
    return { fg, bg };
  }

  private sgr(params: number[]): void {
    const a = this.attr;
    for (let i = 0; i < params.length; i++) {
      const p = params[i];
      if (p === 0) Object.assign(a, DEFAULT_ATTR);
      else if (p === 1) a.bold = true;
      else if (p === 2) a.dim = true;
      else if (p === 4) a.underline = true;
      else if (p === 7) a.reverse = true;
      else if (p === 9) a.strike = true;
      // ECMA-48 22 clears bold AND faint; the CLI leans on it heavily.
      else if (p === 22) {
        a.bold = false;
        a.dim = false;
      } else if (p === 24) a.underline = false;
      else if (p === 27) a.reverse = false;
      else if (p === 29) a.strike = false;
      else if (p >= 30 && p <= 37) a.fg = SITE_ANSI_PALETTE[NAMED[p - 30]];
      else if (p === 39) a.fg = null;
      else if (p >= 40 && p <= 47) a.bg = SITE_ANSI_PALETTE[NAMED[p - 40]];
      else if (p === 49) a.bg = null;
      else if (p >= 90 && p <= 97) a.fg = SITE_ANSI_PALETTE[BRIGHT[p - 90]];
      else if (p >= 100 && p <= 107) a.bg = SITE_ANSI_PALETTE[BRIGHT[p - 100]];
      else if (p === 38 || p === 48) {
        const target = p === 38 ? 'fg' : 'bg';
        if (params[i + 1] === 5) {
          a[target] = color256(params[i + 2] ?? 0);
          i += 2;
        } else if (params[i + 1] === 2) {
          const [r, g, b] = [params[i + 2] ?? 0, params[i + 3] ?? 0, params[i + 4] ?? 0];
          const hx = (n: number) => (n & 0xff).toString(16).padStart(2, '0');
          a[target] = `#${hx(r)}${hx(g)}${hx(b)}`;
          i += 4;
        }
      }
    }
  }

  private csi(prefix: string, params: number[], final: string): void {
    const n = params[0] ?? 0;
    const one = n === 0 ? 1 : n;
    if (prefix === '?') {
      // 1049 = alternate screen. 25 = cursor visibility, 2026 = synchronized
      // update: both are invisible here, since a frame is only rendered once a
      // whole chunk has been applied.
      if (params[0] === 1049) {
        if (final === 'h') {
          this.saved = { grid: this.grid.map((r) => r.slice()), x: this.x, y: this.y };
          this.grid = Array.from({ length: this.rows }, () =>
            Array.from({ length: this.cols }, blank),
          );
          this.x = 0;
          this.y = 0;
        } else if (final === 'l' && this.saved) {
          this.grid = this.saved.grid;
          this.x = this.saved.x;
          this.y = this.saved.y;
          this.saved = null;
        }
      }
      return;
    }
    switch (final) {
      case 'm':
        this.sgr(params.length ? params : [0]);
        break;
      case 'A':
        this.y = Math.max(0, this.y - one);
        break;
      case 'B':
        this.y = Math.min(this.rows - 1, this.y + one);
        break;
      case 'C':
        this.x = Math.min(this.cols - 1, this.x + one);
        break;
      case 'D':
        this.x = Math.max(0, this.x - one);
        break;
      case 'G':
        this.x = Math.min(this.cols - 1, Math.max(0, one - 1));
        break;
      case 'H':
      case 'f':
        this.y = Math.min(this.rows - 1, Math.max(0, (params[0] || 1) - 1));
        this.x = Math.min(this.cols - 1, Math.max(0, (params[1] || 1) - 1));
        break;
      case 'J':
        if (n === 0) {
          this.clearRow(this.grid[this.y], this.x);
          for (let r = this.y + 1; r < this.rows; r++) this.clearRow(this.grid[r]);
        } else if (n === 1) {
          this.clearRow(this.grid[this.y], 0, this.x + 1);
          for (let r = 0; r < this.y; r++) this.clearRow(this.grid[r]);
        } else {
          for (let r = 0; r < this.rows; r++) this.clearRow(this.grid[r]);
        }
        break;
      case 'K':
        if (n === 0) this.clearRow(this.grid[this.y], this.x);
        else if (n === 1) this.clearRow(this.grid[this.y], 0, this.x + 1);
        else this.clearRow(this.grid[this.y]);
        break;
      default:
        break;
    }
  }

  write(chunk: string): void {
    const s = this.pending + chunk;
    this.pending = '';
    let i = 0;
    while (i < s.length) {
      const ch = s[i];
      if (ch === '\u001b') {
        const next = s[i + 1];
        if (next === undefined) {
          this.pending = s.slice(i);
          return;
        }
        if (next === '[') {
          // CSI: optional private prefix, `;`-separated params, final byte.
          const m = /^\u001b\[([?<>=]?)([0-9;]*)([@-~])/.exec(s.slice(i));
          if (!m) {
            this.pending = s.slice(i);
            return;
          }
          this.csi(
            m[1],
            m[2] === '' ? [] : m[2].split(';').map((p) => (p === '' ? 0 : Number(p))),
            m[3],
          );
          i += m[0].length;
          continue;
        }
        if (next === ']') {
          // OSC — the progress reports (`\e]9;4;…`) and title sets. Runs to BEL
          // or ST; an unterminated one means the chunk was cut, so carry it.
          const end = s.slice(i).search(/\u0007|\u001b\\/);
          if (end === -1) {
            this.pending = s.slice(i);
            return;
          }
          i += end + (s[i + end] === '\u0007' ? 1 : 2);
          continue;
        }
        i += 2;
        continue;
      }
      if (ch === '\r') {
        this.x = 0;
        i++;
      } else if (ch === '\n') {
        this.newline();
        i++;
      } else if (ch === '\b') {
        this.x = Math.max(0, this.x - 1);
        i++;
      } else if (ch === '\t') {
        this.x = Math.min(this.cols - 1, (Math.floor(this.x / 8) + 1) * 8);
        i++;
      } else if (ch < ' ') {
        i++;
      } else {
        // Every glyph these recordings use — braille spinners, box drawing,
        // arrows, check marks — is one cell wide. An emoji would not be, and is
        // the tripwire for needing a real width table.
        const cp = s.codePointAt(i)!;
        const glyph = String.fromCodePoint(cp);
        this.put(glyph);
        i += glyph.length;
      }
    }
  }

  /** The screen as rows of styled runs, trailing blank cells dropped. */
  render(): AnsiRow[] {
    return this.grid.map((row) => {
      let end = row.length;
      while (end > 0 && row[end - 1].ch === ' ' && row[end - 1].background === null) end--;

      const runs: AnsiRun[] = [];
      for (let i = 0; i < end; i++) {
        const c = row[i];
        const last = runs[runs.length - 1];
        if (
          last &&
          last.color === c.color &&
          last.background === c.background &&
          last.bold === c.bold &&
          last.underline === c.underline &&
          last.strike === c.strike
        ) {
          last.text += c.ch;
        } else {
          runs.push({
            text: c.ch,
            color: c.color,
            background: c.background,
            bold: c.bold,
            underline: c.underline,
            strike: c.strike,
          });
        }
      }

      // Mirrors the `ansi` branch of `transformerConsole`: a line whose first
      // non-space character is `$ ` is a command line, so the prompt glyph gets
      // the ember, unselectable treatment and the command text reads bright.
      const text = runs.map((r) => r.text).join('');
      const trimmed = text.replace(/^\s+/, '');
      return { runs, prompt: trimmed === '$' || trimmed.startsWith('$ ') };
    });
  }
}

/** A recorded session: absolute-millisecond timestamps against raw pty bytes. */
export interface AnsiRecording {
  cols: number;
  rows: number;
  durationMs: number;
  /** `[millisecondsFromStart, chunk]`, ascending. */
  events: [number, string][];
  /** How the timings were captured and what, if anything, was adjusted. */
  timing?: string;
}
