import type { ShikiTransformer } from '@shikijs/types';
import type { Element, Text } from 'hast';

/* A shiki transformer that gives shell-language fences a terminal look: a `$ `
   prompt renders in the ember accent (and is unselectable, so a copy skips it),
   command text reads bright, and every non-command (output) line is dimmed.
   Mirrors the hand-built `Terminal`/`ShimDemo` components, but applied
   automatically — no per-block authoring.

   SCOPE — two gates, both required:
   1. Language gate: only `console`/`bash`/`sh`/`shell`/`shellscript`/
      `shellsession` fences are eligible. ts/json/yaml/text/etc. are untouched.
   2. Prompt gate: within an eligible block, the styling activates ONLY when at
      least one line's first non-whitespace character is `$`. A shell block with
      NO prompt line (a plain command list, a config snippet, an all-output
      transcript) is left completely unstyled — rendered as a normal code block,
      nothing dimmed. This is what keeps the many plain ```bash command-lists in
      the docs from being incorrectly dimmed: with no `$` line, there is nothing
      to differentiate, so we do nothing.

   When both gates pass, each line is classified: a `$ ` prompt line is tagged
   `data-cmd` (its leading `$ ` glyph replaced by a `select-none` ember span) and
   every other line is tagged `data-output`. The actual colors live in
   `global.css`, so the transformer stays presentation-free.

   Mechanism for the prompt gate: shiki's `preprocess` hook sees the raw source
   before tokenization, so we scan it there and stash the decision on `this.meta`
   (per-code-block state) for the `line()` hook to read. */

const SHELL_LANGS = new Set([
  'console',
  'bash',
  'sh',
  'shell',
  'shellscript',
  'shellsession',
  'ansi',
]);

// An `ansi` fence is a transcript whose output lines carry their own terminal
// colors (`src/lib/shiki-ansi.ts`), so it gets the prompt treatment only: the
// `$ ` lines are tagged and brightened, and the output lines are left alone
// rather than dimmed, which would paint over the captured colors.
const PROMPT_ONLY_LANGS = new Set(['ansi']);

// First non-whitespace char of the line is `$` followed by a space or EOL — a
// prompt line. `$VAR=…` / `$(cmd)` (no following space) are not prompts.
function isPromptLine(line: string): boolean {
  const trimmed = line.replace(/^\s+/, '');
  return trimmed === '$' || trimmed.startsWith('$ ');
}

function lineText(node: Element): string {
  let out = '';
  for (const child of node.children) {
    if (child.type === 'text') out += child.value;
    else if (child.type === 'element') out += lineText(child);
  }
  return out;
}

// Strip the leading `$ ` from the line's hast so we can re-add it as a styled,
// unselectable prompt span. Walks the token spans, dropping the first two
// characters (`$` then a space) wherever they fall. Leading whitespace before
// the prompt is preserved (we only strip starting at the `$`).
function stripPrompt(node: Element): void {
  let seenDollar = false;
  let remaining = 2; // "$ "
  for (const child of node.children) {
    if (remaining === 0) break;
    if (child.type !== 'element') continue;
    for (const grandchild of child.children) {
      if (remaining === 0) break;
      if (grandchild.type !== 'text') continue;
      const t = grandchild as Text;
      if (!seenDollar) {
        const idx = t.value.indexOf('$');
        if (idx === -1) continue; // leading-whitespace-only token; keep it
        // Drop from the `$` onward, up to `remaining` chars.
        const take = Math.min(remaining, t.value.length - idx);
        t.value = t.value.slice(0, idx) + t.value.slice(idx + take);
        remaining -= take;
        seenDollar = true;
      } else {
        const take = Math.min(remaining, t.value.length);
        t.value = t.value.slice(take);
        remaining -= take;
      }
    }
  }
}

export function transformerConsole(): ShikiTransformer {
  return {
    name: 'nub:console',
    preprocess(code) {
      // Prompt gate: decide once per block whether any line is a `$ ` prompt.
      // Stashed on `this.meta` (per-code-block) for `line()` to read.
      if (!SHELL_LANGS.has(this.options.lang)) return;
      const active = code.split('\n').some(isPromptLine);
      (this.meta as Record<string, unknown>).nubConsoleActive = active;
    },
    line(node) {
      if (!SHELL_LANGS.has(this.options.lang)) return;
      if (!(this.meta as Record<string, unknown>).nubConsoleActive) return;

      if (isPromptLine(lineText(node))) {
        node.properties['data-cmd'] = '';
        stripPrompt(node);
        node.children.unshift({
          type: 'element',
          tagName: 'span',
          properties: { class: 'console-prompt' },
          children: [{ type: 'text', value: '$ ' }],
        });
      } else if (!PROMPT_ONLY_LANGS.has(this.options.lang)) {
        node.properties['data-output'] = '';
      }
    },
  };
}
