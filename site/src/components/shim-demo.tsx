/* The pm-shim opt-in demo block. A real captured terminal transcript with
   per-line treatment a plain ```console fence can't express: shiki's console
   grammar colors every output line the same, so it can't separate nub's own
   dim stderr chatter from the actual command results. This mirrors the site's
   `Terminal` component convention exactly — the `$ ` prompt in `text-ember`
   (select-none, so copy skips it, matching `code.tsx`), nub's log lines dimmed
   in `text-fd-muted-foreground`, and the real command results at full
   `text-fd-foreground` brightness — while keeping fumadocs' code-figure chrome
   (`bg-fd-card` + border + rounded-xl, IBM Plex Mono at the same size/leading)
   so it sits among the page's other console fences indistinguishably. Plain
   text directly in the block — fully selectable, no inline-code pill. */

// One transcript line. `prompt` is a `$ ` command. For output lines, `dim` is
// nub's own log chatter (muted); the default is a real command result (bright).
type Line = { text: string; prompt?: boolean; dim?: boolean };

const lines: Line[] = [
  { text: 'nub pm shim', prompt: true },
  { text: 'nub pm shim: 6 entries in ~/.local/share/nub/shims (6 created)', dim: true },
  { text: '  PATH: added ~/.local/share/nub/shims to PATH (~/.zshrc) — restart your shell', dim: true },
  { text: '' },
  { text: 'which pnpm', prompt: true },
  { text: '~/.local/share/nub/shims/pnpm' },
  { text: '' },
  { text: 'pnpm --version', prompt: true },
  { text: 'pnpm@9.5.0 (via nub shim)', dim: true },
  { text: 'Installing... (4 MB)', dim: true },
  { text: 'Installed in 0.8s', dim: true },
  { text: '9.5.0' },
];

export function ShimDemo() {
  return (
    <figure className="nub-code-panel not-prose my-4 overflow-hidden rounded-xl border">
      <pre className="nub-code-fg overflow-x-auto px-5 py-4 font-mono text-[0.8rem] leading-[1.43]">
        {lines.map((line, i) => (
          <div key={i} className="whitespace-pre">
            {line.prompt ? (
              <>
                {/* `$ ` glyph in the ember accent, select-none so copy skips it
                    — the same prompt treatment as the site's Terminal card. */}
                <span className="select-none text-ember">$ </span>
                <span className="nub-code-fg">{line.text}</span>
              </>
            ) : (
              <span className={line.dim ? 'nub-code-muted' : 'nub-code-fg'}>
                {/* Zero-width space keeps blank lines from collapsing. */}
                {line.text || '​'}
              </span>
            )}
          </div>
        ))}
      </pre>
    </figure>
  );
}
