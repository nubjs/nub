import { DocsLayout } from 'fumadocs-ui/layouts/docs';
import type { ReactNode } from 'react';
import { baseOptions } from '@/lib/layout.shared';
import { guidesSource } from '@/lib/source';

export default function Layout({ children }: { children: ReactNode }) {
  // Keep the nav title + GitHub link, but drop the "Docs"/"Blog" nav links
  // from the guides sidebar — they only belong in the top home nav. Guides are
  // a separate top-level section, distinct from the docs sidebar.
  const { links, ...base } = baseOptions();

  return (
    <DocsLayout tree={guidesSource.pageTree} {...base} links={[]}>
      {children}
    </DocsLayout>
  );
}
