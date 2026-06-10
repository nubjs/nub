import { DocsLayout } from 'fumadocs-ui/layouts/docs';
import type { ReactNode } from 'react';
import type { Node as TreeNode, Root as TreeRoot } from 'fumadocs-core/page-tree';
import { baseOptions } from '@/lib/layout.shared';
import { source } from '@/lib/source';

/* Pages that map to a concrete command get a subtle, right-aligned mono chip
   in the sidebar — descriptive label on the left, the command on the right. */
const COMMAND_BY_URL: Record<string, string> = {
  '/docs/files': 'nub <file>',
  '/docs/node': 'nub node',
  '/docs/pm': 'nub pm',
  '/docs/run': 'nub run',
  '/docs/watch': 'nub watch',
  '/docs/nubx': 'nubx',
};

function LabelWithChip({ label, command }: { label: ReactNode; command: string }) {
  return (
    <span className="flex w-full items-center justify-between gap-2">
      <span>{label}</span>
      <code className="rounded border border-fd-border/50 bg-fd-muted px-1 py-px font-mono text-[0.58rem] leading-tight font-normal text-fd-muted-foreground in-data-[active=true]:border-fd-primary/30 in-data-[active=true]:bg-fd-primary/10 in-data-[active=true]:text-fd-primary">
        {command}
      </code>
    </span>
  );
}

function styleNode(node: TreeNode): TreeNode {
  if (node.type === 'folder') {
    return { ...node, children: node.children.map(styleNode) };
  }
  if (node.type === 'page') {
    const command = COMMAND_BY_URL[node.url];
    if (command) {
      return { ...node, name: <LabelWithChip label={node.name} command={command} /> };
    }
  }
  return node;
}

export default function Layout({ children }: { children: ReactNode }) {
  // Keep the nav title + GitHub link, but drop the "Docs"/"Blog" nav links
  // from the docs sidebar — they only belong in the top home nav.
  const { links, ...base } = baseOptions();

  const tree: TreeRoot = {
    ...source.pageTree,
    children: source.pageTree.children.map(styleNode),
  };

  return (
    <DocsLayout tree={tree} {...base} links={[]}>
      {children}
    </DocsLayout>
  );
}
