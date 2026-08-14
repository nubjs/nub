#!/usr/bin/env node
/**
 * Dump every reviewer's perspective on a PR — top-level reviews, inline review
 * threads (with resolved/outdated state + file:line anchors), and PR-level
 * conversation comments — as JSON in a single GraphQL round trip. Designed to
 * be piped into `jq` for ad-hoc filtering.
 *
 * `gh pr view --json reviews,comments` silently omits inline `pulls/{n}/comments`,
 * so agents that rely on it miss actionable feedback. This closes that gap and
 * gives you everything in one call (not N+1).
 *
 * Each thread carries its GraphQL node `id`, which is what the `resolveReviewThread`
 * mutation takes — pulling and resolving are the two halves of the same loop, so
 * the id has to come back with the thread or the second half needs another query.
 *
 * Runs under BOTH plain Node (type-stripping) and nub:
 *   node scripts/pr-reviews.ts <pr-number>
 *   nub  scripts/pr-reviews.ts <pr-number>
 *   node scripts/pr-reviews.ts 717 | jq '.reviewThreads[] | select(.isResolved | not)'
 *   node scripts/pr-reviews.ts 717 | jq '.reviews[] | {author, state, body}'
 *
 * Erasable TypeScript only (no enums/namespaces/parameter-properties) so plain
 * modern `node` runs it with no build step.
 *
 * Auth: borrows the `gh auth token` (no env vars required).
 * Repo: inferred from the current git remote via `gh repo view`.
 */

import { execSync } from "node:child_process";

const prNumber = Number(process.argv[2]);
if (!Number.isInteger(prNumber) || prNumber <= 0) {
  console.error("usage: node scripts/pr-reviews.ts <pr-number>");
  process.exit(1);
}

function sh(cmd: string): string {
  return execSync(cmd, { encoding: "utf8" }).trim();
}

const token = sh("gh auth token");
if (!token) {
  console.error("no `gh auth token`; run `gh auth login` first");
  process.exit(1);
}

const repo: { owner: { login: string }; name: string } = JSON.parse(
  sh("gh repo view --json owner,name")
);

const query = `
query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      number
      title
      url
      state
      author { login }
      reviews(first: 100) {
        nodes { author { login } state submittedAt body url }
      }
      reviewThreads(first: 100) {
        nodes {
          id
          isResolved
          isOutdated
          path
          line
          originalLine
          comments(first: 50) {
            nodes { author { login } body createdAt outdated url }
          }
        }
      }
      comments(first: 100) {
        nodes { author { login } body createdAt url }
      }
    }
  }
}`;

const res = await fetch("https://api.github.com/graphql", {
  method: "POST",
  headers: { Authorization: `bearer ${token}`, "Content-Type": "application/json" },
  body: JSON.stringify({
    query,
    variables: { owner: repo.owner.login, name: repo.name, number: prNumber },
  }),
});

if (!res.ok) {
  console.error(`GraphQL HTTP ${res.status}: ${await res.text()}`);
  process.exit(1);
}

interface RawConnection<T> {
  nodes: T[];
}
interface RawThread {
  id: string;
  isResolved: boolean;
  isOutdated: boolean;
  path: string;
  line: number | null;
  originalLine: number | null;
  comments: RawConnection<unknown>;
}
interface RawPullRequest {
  number: number;
  title: string;
  url: string;
  state: string;
  author: { login: string } | null;
  reviews: RawConnection<unknown>;
  reviewThreads: RawConnection<RawThread>;
  comments: RawConnection<unknown>;
}

const json: {
  data?: { repository?: { pullRequest?: RawPullRequest | null } | null };
  errors?: unknown;
} = await res.json();

if (json.errors) {
  console.error("GraphQL errors:", JSON.stringify(json.errors, null, 2));
  process.exit(1);
}

const pr = json.data?.repository?.pullRequest;
if (!pr) {
  console.error(`PR #${prNumber} not found in ${repo.owner.login}/${repo.name}`);
  process.exit(1);
}

// flatten GraphQL `{ nodes: [...] }` connection wrappers for jq ergonomics:
// callers want `.reviews[0].body`, not `.reviews.nodes[0].body`.
const out = {
  number: pr.number,
  title: pr.title,
  url: pr.url,
  state: pr.state,
  author: pr.author?.login ?? null,
  reviews: pr.reviews.nodes,
  reviewThreads: pr.reviewThreads.nodes.map((thread) => ({
    id: thread.id,
    isResolved: thread.isResolved,
    isOutdated: thread.isOutdated,
    path: thread.path,
    line: thread.line ?? thread.originalLine,
    comments: thread.comments.nodes,
  })),
  comments: pr.comments.nodes,
};

console.log(JSON.stringify(out, null, 2));
