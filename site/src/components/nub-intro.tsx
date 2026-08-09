import type { ReactNode } from 'react';

/* The standing one-paragraph "what is Nub" aside for blog posts. A post assumes a
   reader who already knows Nub; this catches up someone arriving cold, in the same
   words every time. Rendered as a plain <blockquote> so it inherits the editorial-aside
   treatment `.blog-prose blockquote` already defines — no second styling to keep in sync.

   The copy is maintainer-authored and shared across posts: edit it HERE, not in an
   individual .mdx. `children` carries per-post extras (a command block, say) inside the
   same aside. */
export function NubIntro({ children }: { children?: ReactNode }) {
  return (
    <blockquote>
      <p>
        Nub is an all-in-one toolkit for Node.js written in Rust. The <code>nub</code> command is flag-for-flag compatible with <code>node</code>, while adding full support for TypeScript, JSX, <code>tsconfig.json</code>, <code>.env</code> loading, and modern Web and ECMAScript APIs. It also includes a fast script runner (<code>nub run</code>), package runner (<code>nubx</code>), Node version manager (<code>nub node</code>), and <code>pnpm</code>-compatible package manager.
      </p>
      {children}
    </blockquote>
  );
}
