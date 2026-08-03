/* Build-time substitution of current Node and Nub versions into docs code samples.
   Replaces the tokens `{{NODE_VERSION}}` (full, e.g. 26.3.0) and `{{NODE_MAJOR}}`
   (e.g. 26), plus `{{NUB_VERSION}}`, wherever they appear in code blocks and
   inline code. Node comes from the same source as the homepage (`getLatestNode`);
   Nub comes from the canonical npm package manifest updated by `make version`.

   Tokens MUST live inside fenced code or inline code only — MDX evaluates `{…}` in
   prose as a JS expression, so a bare token in prose would not compile. Code and
   inline-code node values are literal text, where the substitution is safe. */

import nubPackage from '../../../npm/nub/package.json';
import { getLatestNode, type NodeVersion } from './node-version';

let cached: Promise<NodeVersion> | undefined;
function latestNode(): Promise<NodeVersion> {
  return (cached ??= getLatestNode());
}

export function remarkNodeVersion() {
  return async (tree: unknown): Promise<void> => {
    const { full, major } = await latestNode();
    const substitute = (value: string): string =>
      value
        .split('{{NODE_VERSION}}').join(full)
        .split('{{NODE_MAJOR}}').join(major)
        .split('{{NUB_VERSION}}').join(nubPackage.version);

    // mdast `code` and `inlineCode` nodes carry literal text on `.value`; walk the
    // tree and rewrite any that contain a token. (No unist-util-visit dependency.)
    const walk = (node: unknown): void => {
      if (!node || typeof node !== 'object') return;
      const n = node as { type?: string; value?: unknown; children?: unknown[] };
      if ((n.type === 'code' || n.type === 'inlineCode') && typeof n.value === 'string') {
        n.value = substitute(n.value);
      }
      if (Array.isArray(n.children)) for (const child of n.children) walk(child);
    };
    walk(tree);
  };
}
