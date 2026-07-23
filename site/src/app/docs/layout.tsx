import { DocsLayout } from 'fumadocs-ui/layouts/docs';
import type { ReactNode } from 'react';
import type { Node as TreeNode, Root as TreeRoot } from 'fumadocs-core/page-tree';
import { baseOptions } from '@/lib/layout.shared';
import { source } from '@/lib/source';

/* Pages that map to a concrete command get a subtle, right-aligned mono chip
   in the sidebar — descriptive label on the left, the command on the right. */
const COMMAND_BY_URL: Record<string, string> = {
  '/docs/runtime': 'nub <file>',
  '/docs/runner': 'nubx',
  '/docs/runner/run': 'nub run',
  '/docs/runner/exec': 'nub exec',
  '/docs/runner/dlx': 'nub dlx',
  '/docs/install': 'nub install',
  '/docs/node': 'nub node',
  '/docs/pm': 'nub pm',
  '/docs/watch': 'nub watch',
};

function LabelWithChip({ label, command }: { label: ReactNode; command: string }) {
  return (
    <span className="flex w-full items-center justify-between gap-2">
      <span>{label}</span>
      <code className="shrink-0 whitespace-nowrap rounded border border-fd-border/50 bg-fd-muted px-1 py-px font-mono text-[0.58rem] leading-tight font-normal text-fd-muted-foreground in-data-[active=true]:border-fd-primary/30 in-data-[active=true]:bg-fd-primary/10 in-data-[active=true]:text-fd-primary">
        {command}
      </code>
    </span>
  );
}

function styleNode(node: TreeNode): TreeNode {
  if (node.type === 'folder') {
    // The folder header renders a clickable link to its index page. When the
    // folder has no `index` (a meta whose `pages` lists "index" explicitly
    // rather than "..."), the index instead appears as a regular child page —
    // find it by its source file, promote it to the header, and drop that
    // duplicate child so the folder name isn't repeated beneath itself. A
    // command chip is added below when the promoted page maps to a command.
    const indexChild = node.children.find(
      (c): c is Extract<TreeNode, { type: 'page' }> =>
        c.type === 'page' && /(^|\/)index\.mdx?$/.test(c.$ref ?? ''),
    );
    const folderUrl = node.index?.url ?? indexChild?.url;
    const isDuplicateIndex = (c: TreeNode) =>
      !node.index && c.type === 'page' && c.url === folderUrl;

    // Force every folder non-collapsible: Fumadocs then draws no chevron toggle
    // and keeps the children always visible (collapsible:false makes the folder
    // default-open and disables the Collapsible). This is the sidebar-wide
    // "no chevron, always expanded" behavior, applied in the component so it
    // covers every folder uniformly without touching per-folder meta.json.
    // Promote the discovered index child to `index` so the header renders as a
    // clickable link (SidebarFolderLink) rather than a non-link trigger — the
    // folders whose meta lists "index" explicitly (not "...") otherwise have no
    // folder-index and render an unclickable header.
    const styled: TreeNode = {
      ...node,
      collapsible: false,
      index: node.index ?? indexChild,
      children: node.children.filter((c) => !isDuplicateIndex(c)).map(styleNode),
    };
    // With no chevron competing for the right edge, the chip aligns into the
    // shared column like every other row.
    const command = folderUrl ? COMMAND_BY_URL[folderUrl] : undefined;
    if (command) {
      styled.name = <LabelWithChip label={node.name} command={command} />;
    }
    return styled;
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
  // Drop the "Docs"/"Blog" nav links AND the GitHub pill from the docs sidebar —
  // the pill lives in the home nav, and the docs surface its own "Leave a star"
  // nudge in the TOC footer, so a sidebar pill here would be redundant.
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
