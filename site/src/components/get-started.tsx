import { InstallTabs } from '@/components/install-tabs';

/* The standing "Get started" block for blog posts: the install tabs plus a pointer
   at the agent adoption prompt (public/start.md, served at /start.md). Every release
   post ends with one, so a reader who arrives from a link can install without hunting
   for the docs.

   The copy is maintainer-authored and shared across posts: edit it HERE, not in an
   individual .mdx. The link must stay root-relative — a bare `start.md` would resolve
   against /blog/ and 404. */
export function GetStarted() {
  return (
    <div className="my-6">
      <InstallTabs />
      {/* The space after the link is written as an explicit {' '} expression: the
          literal space that follows a closing tag on the same line does not survive
          this build, which renders as "prompt`into`". */}
      <p className="mt-4">
        Or paste this <a href="/start.md">&quot;Get Started&quot; prompt</a>{' '}
        into an agent. It will install Nub and explain how it can be used in your project. (It won&apos;t make any changes without permission.)
      </p>
    </div>
  );
}
