import type { ShikiTransformer } from '@shikijs/types';

/* ANSI terminal output as a first-class code fence.

   Shiki treats `ansi` as a special language: instead of a TextMate grammar it
   runs an SGR parser over the source and emits colored tokens, so a captured
   terminal transcript renders with the colors the terminal actually painted.
   That machinery is built in and needs no grammar registration — what this
   module adds is the two things that make it usable here.

   1. AUTHORING (`transformerAnsi`). The parser wants real U+001B bytes, and a
      raw control character in an `.mdx` file is invisible in every editor,
      survives no round trip through a shell heredoc or a diff review, and is
      trivially destroyed by a formatter. So an `ansi` fence also accepts the
      textual spellings of ESC — `\x1b`, `\u001b`, `\033`, `\e`, and the caret
      notation `^[` that `cat -v`, `less` and a tmux capture emit — and the
      preprocess hook rewrites them to the real byte before tokenization. Write
      the readable form, or paste the raw form; both render identically.

   2. PALETTE (`ANSI_COLOR_REPLACEMENTS`). Shiki resolves the 16 named ANSI
      colors from the theme's `terminal.ansi*` entries and falls back to VS
      Code's defaults when a theme defines none — which `vesper` does not.
      Those defaults are tuned for VS Code's #1e1e1e, not this site's warm
      near-black code panel: `black` lands at 1.06:1 on #0b0a08 (invisible), and
      `red` and `brightBlack` both miss AA. The replacements below map each
      default onto the site's own dark-panel accents, so an ANSI block reads as
      part of the site rather than as a screenshot of someone else's editor. The
      three rules that build that table are stated above it. */

/* THREE RULES BUILD THIS TABLE, and every value is either a site token or a
   stated derivation of one — nothing is picked by eye.

   1. NORMALS ARE SITE TOKENS. Each of the six chromatic slots takes the
      dark-panel accent from `global.css` that occupies it, and `white` /
      `brightBlack` take the panel's own foreground and muted. `cyan` is the one
      slot the site has never needed a color for, so it is CONSTRUCTED as a
      sibling of `--color-acid`: acid is hsl(135, 71%, 60%), and this is the same
      saturation and lightness at hue 175. Mixing an existing token toward the
      background instead was tried and rejected — at 38-48% saturation it read as
      a dull sage next to accents that all sit at 71-100%.
   2. BRIGHTS ARE THE NORMAL LIFTED 35% TOWARD WHITE, so the bright row keeps
      its hue and gains only weight.
   3. EXCEPT WHERE THE SITE ALREADY OWNS A LIGHTER SIBLING, which beats a
      derived lift. `brightMagenta` is `--color-orchid` and `brightCyan` is the
      #99ffe4 mint — both counted on the rendered homepage (orchid once, mint
      fourteen times), so both are colors a reader has already met. Lifted pink
      (#ffa1cf) sat too close to pink to read as a separate color at all, and
      this is also why `magenta` is pink rather than orchid: orchid as the NORMAL
      magenta collided with `--color-sky` on the line above it in a real
      transcript, which is the exact confusion ANSI color exists to prevent.

   Contrast is measured against the panel's #0b0a08. Every entry clears WCAG AA
   (4.5:1) except `black`, which is deliberately the dimmest thing on the panel
   at 3.25:1 — ANSI black's job is to recede, and it still beats the VS Code
   default's 1.06:1, where it was invisible. */
// Exported because the animated player (`src/lib/ansi-screen.ts`) resolves the
// same 16 colors itself: a recorded session and a static fence of the same
// output have to be indistinguishable, and a second copy of a palette drifts.
export const SITE_ANSI_PALETTE = {
  black: '#65625b', //          3.25:1  foreground mixed 60% toward the panel
  red: '#ff5d3b', //            6.48:1  --color-ember
  green: '#4fe173', //         11.64:1  --color-acid
  yellow: '#fbbf24', //        11.85:1  --status-warn
  blue: '#7bb0ff', //           8.95:1  --color-sky
  magenta: '#ff6fb5', //        7.72:1  --color-pink
  cyan: '#51e1d5', //          12.33:1  acid's saturation and lightness at hue 175
  white: '#ece6d8', //         15.90:1  --nub-code-foreground
  brightBlack: '#9f988c', //    6.92:1  --nub-code-muted, matching dimmed console output
  brightRed: '#ff9680', //      9.35:1
  brightGreen: '#8deca4', //   13.82:1
  brightYellow: '#fcd571', //  14.04:1
  brightBlue: '#a9ccff', //    12.03:1
  brightMagenta: '#c9a3ff', //  9.58:1  --color-orchid, per rule 3
  brightCyan: '#99ffe4', //    16.75:1  the site's mint, per rule 3
  brightWhite: '#ffffff', //   19.79:1
} as const;

// Shiki's `defaultAnsiColors` (@shikijs/core, `code-to-tokens-ansi.ts`) — the
// values it emits for a theme with no `terminal.ansi*` colors. Keying the
// replacement map on these is what lets a plain `colorReplacements` option
// retarget the palette without forking the theme.
//
// COUPLING, deliberate and checked: `colorReplacements` is global to every
// fence, not just `ansi` ones. It is safe here only because none of these 16
// hexes appears anywhere in `vesper`'s own `colors` or `tokenColors` —
// verified, and worth re-verifying if the site ever changes theme. Shiki
// applies the map inside its ANSI tokenizer, which is also what makes
// 256-color indices 0-15 pick up the same palette for free.
const SHIKI_DEFAULT_ANSI = {
  black: '#000000',
  red: '#cd3131',
  green: '#0DBC79',
  yellow: '#E5E510',
  blue: '#2472C8',
  magenta: '#BC3FBC',
  cyan: '#11A8CD',
  white: '#E5E5E5',
  brightBlack: '#666666',
  brightRed: '#F14C4C',
  brightGreen: '#23D18B',
  brightYellow: '#F5F543',
  brightBlue: '#3B8EEA',
  brightMagenta: '#D670D6',
  brightCyan: '#29B8DB',
  brightWhite: '#FFFFFF',
} as const;

// Keys are lowercased because shiki looks a replacement up as
// `replacements[color.toLowerCase()]` — the mixed-case defaults above (#0DBC79,
// #E5E510, …) silently miss otherwise, leaving half the palette on VS Code's
// values and the other half on ours.
export const ANSI_COLOR_REPLACEMENTS: Record<string, string> =
  Object.fromEntries(
    Object.entries(SHIKI_DEFAULT_ANSI).map(([name, hex]) => [
      hex.toLowerCase(),
      SITE_ANSI_PALETTE[name as keyof typeof SITE_ANSI_PALETTE],
    ]),
  );

const ESC = '\u001b';

// The unambiguous textual spellings of ESC. None of these can plausibly occur
// verbatim in terminal output, so they are rewritten wherever they appear.
const NAMED_ESC = /\\(?:x1b|x1B|u001b|u001B|u\{1[bB]\}|033|e|E)/g;

// Caret notation is ambiguous — `^` is an ordinary character — so it converts
// only where it introduces a control sequence (`^[[`) or an OS command (`^[]`).
const CARET_ESC = /\^\[(?=[[\]])/g;

/* Collapse carriage returns the way a terminal would. A captured progress bar,
   spinner, or download meter rewrites one line many times with CR; rendered
   literally that is a single garbled line holding every frame at once. Only the
   final frame was ever visible, so keep the text after the last CR. A CRLF line
   ending is stripped first, otherwise the rule would empty every line of a
   Windows capture. */
function applyCarriageReturns(code: string): string {
  if (!code.includes('\r')) return code;
  return code
    .split('\n')
    .map((line) => {
      const body = line.endsWith('\r') ? line.slice(0, -1) : line;
      const last = body.lastIndexOf('\r');
      return last === -1 ? body : body.slice(last + 1);
    })
    .join('\n');
}

// An SGR sequence with omitted parameters. ECMA-48 says an omitted parameter
// takes its default, which for SGR is 0 — so a bare `ESC[m` is a full reset and
// `ESC[;31m` resets before setting red. Both are common in the wild: git emits
// `ESC[m` for every reset it writes.
const SGR_SEQUENCE = new RegExp(`${ESC}\\[([0-9;]*)m`, 'g');

/* Spell out those defaulted parameters, because shiki's bundled parser drops
   them. `ansi-sequence-parser` splits the parameter string on `;` and skips any
   entry that is falsy (`if (!code) continue`), so an empty parameter emits no
   command at all rather than the reset it stands for — and a real transcript
   then renders as one unbroken run of whatever color it opened with. Rewriting
   each empty parameter to an explicit `0` costs one pass and leaves every
   already-explicit sequence (`ESC[38;5;9m`) byte-identical. */
function expandDefaultSgrParams(code: string): string {
  return code.replace(SGR_SEQUENCE, (_, params: string) =>
    `${ESC}[${params
      .split(';')
      .map((p) => (p === '' ? '0' : p))
      .join(';')}m`,
  );
}

export function normalizeAnsiEscapes(code: string): string {
  return expandDefaultSgrParams(
    applyCarriageReturns(code.replace(NAMED_ESC, ESC).replace(CARET_ESC, ESC)),
  );
}

export function transformerAnsi(): ShikiTransformer {
  return {
    name: 'nub:ansi',
    preprocess(code) {
      if (this.options.lang !== 'ansi') return;
      return normalizeAnsiEscapes(code);
    },
    // Marks the block for the CSS in `global.css` that restores the SGR
    // attributes fumadocs drops. In dual-theme mode shiki writes every token
    // attribute as a CSS variable (`--shiki-dark-bg`, `--shiki-dark-font-weight`,
    // `--shiki-dark-text-decoration`, …) and leaves it to the consumer to map
    // them onto real properties; fumadocs maps only `color` and `font-style`, so
    // background colors, `\e[1m` bold, `\e[4m` underline, `\e[9m` strikethrough
    // and `\e[7m` reverse video all render as nothing. That is invisible in a
    // syntax-highlighted language, which uses none of them, and glaring in ANSI
    // output, where an inverse-video `FAIL` badge is the whole message. Scoped to
    // `ansi` rather than fixed globally so no other fence changes appearance.
    code(node) {
      if (this.options.lang !== 'ansi') return;
      node.properties['data-ansi'] = '';
    },
  };
}
