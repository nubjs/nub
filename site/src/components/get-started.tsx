import { InstallTabs } from '@/components/install-tabs';
import { MigrationPrompt, ViewRepoLink } from '@/components/migration-prompt';
import { START_PROMPT } from '@/lib/start-prompt';

/* The standing "Get started" block for blog posts: the install tabs at the full
   prose width, with the same "Copy agent prompt" / "View repo" row the homepage
   puts under its hero tabs. Every release post ends with one, so a reader who
   arrives from a link can install without hunting for the docs.

   Shared across posts: edit it HERE, not in an individual .mdx. */
export function GetStarted() {
  return (
    <div className="not-prose my-6">
      <InstallTabs wide />
      <div className="mt-4 flex flex-wrap items-center gap-x-5 gap-y-2">
        <MigrationPrompt prompt={START_PROMPT} />
        <ViewRepoLink />
      </div>
    </div>
  );
}
