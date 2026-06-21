/* A GitHub-style "Star" button with a live stargazer count, modeled on
   GitHub's own native button: an OUTLINE star icon, the word "Star", and a
   count pill. It renders in the UNSTARRED state (outline, never filled) so it
   invites a click. No third-party script (no `buttons.github.io`) — the count
   is fetched server-side with ISR, so there is no FOUC and we keep full styling
   control + a brand-clean markup. */

/* Outline star (GitHub's `star` octicon, 16px viewBox). */
function StarIcon({ className }: { className?: string }) {
  return (
    <svg
      viewBox="0 0 16 16"
      fill="currentColor"
      className={className}
      aria-hidden="true"
    >
      <path d="M8 .25a.75.75 0 0 1 .673.418l1.882 3.815 4.21.612a.75.75 0 0 1 .416 1.279l-3.046 2.97.719 4.192a.751.751 0 0 1-1.088.791L8 12.347l-3.766 1.98a.75.75 0 0 1-1.088-.79l.72-4.194L.818 6.374a.75.75 0 0 1 .416-1.28l4.21-.611L7.327.668A.75.75 0 0 1 8 .25Zm0 2.445L6.615 5.5a.75.75 0 0 1-.564.41l-3.097.45 2.24 2.184a.75.75 0 0 1 .216.664l-.528 3.084 2.769-1.456a.75.75 0 0 1 .698 0l2.77 1.456-.53-3.084a.75.75 0 0 1 .216-.664l2.24-2.183-3.096-.45a.75.75 0 0 1-.564-.41L8 2.694Z" />
    </svg>
  );
}

/* Compact thousands formatting matching GitHub's "1.1k" presentation. */
function formatStars(count: number): string {
  if (count < 1000) return String(count);
  const k = count / 1000;
  // One decimal below 10k (1.1k), whole thousands above (12k).
  return k < 10 ? `${k.toFixed(1)}k` : `${Math.round(k)}k`;
}

/* Fetch the repo's stargazer count. ISR-cached hourly. On any failure we return
   null and the button renders without a count rather than breaking the build. */
async function getStarCount(repo: string): Promise<number | null> {
  try {
    const res = await fetch(`https://api.github.com/repos/${repo}`, {
      headers: { Accept: 'application/vnd.github+json' },
      next: { revalidate: 3600 },
    });
    if (!res.ok) return null;
    const data = (await res.json()) as { stargazers_count?: number };
    return typeof data.stargazers_count === 'number'
      ? data.stargazers_count
      : null;
  } catch {
    return null;
  }
}

export async function GitHubStarButton({ repo }: { repo: string }) {
  const stars = await getStarCount(repo);

  return (
    <a
      href={`https://github.com/${repo}`}
      target="_blank"
      rel="noopener noreferrer"
      aria-label={`Star ${repo} on GitHub`}
      className="inline-flex items-stretch overflow-hidden rounded-md border border-fd-border text-sm font-medium shadow-sm"
    >
      <span className="flex items-center gap-1.5 bg-fd-muted px-3 py-1.5 text-fd-foreground transition-colors hover:bg-fd-accent">
        <StarIcon className="size-4 shrink-0" />
        <span>Star</span>
      </span>
      {stars !== null && (
        <span className="flex items-center border-s border-fd-border bg-fd-card px-3 py-1.5 tabular-nums text-fd-muted-foreground">
          {formatStars(stars)}
        </span>
      )}
    </a>
  );
}
