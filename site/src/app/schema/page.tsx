import fs from 'node:fs';
import path from 'node:path';
import type { Metadata } from 'next';
import Link from 'next/link';

export const metadata: Metadata = {
  // The root layout appends "— Nub" through its title template.
  title: 'Schema index',
  description:
    'Published JSON Schemas for the Nub project configuration file, one per release plus a rolling latest.',
};

const SCHEMA_DIR = path.join(process.cwd(), 'public', 'schema');

type Entry = {
  name: string;
  href: string;
  bytes: number;
  /** Sorts newest-version-first; `latest.json` always leads. */
  rank: [number, number, number];
};

/**
 * Version files are `v<major>.<minor>[.<patch>].json`. Anything that does not
 * match sorts last rather than being hidden — a file present on disk and absent
 * from this listing would be the one failure mode a directory index must not
 * have.
 */
function rankOf(name: string): Entry['rank'] {
  const match = /^v(\d+)\.(\d+)(?:\.(\d+))?\.json$/.exec(name);
  if (!match) return [-1, -1, -1];
  return [Number(match[1]), Number(match[2]), Number(match[3] ?? 0)];
}

function readEntries(): Entry[] {
  let names: string[];
  try {
    names = fs.readdirSync(SCHEMA_DIR).filter((n) => n.endsWith('.json'));
  } catch {
    return [];
  }

  const entries = names.map((name) => ({
    name,
    href: `/schema/${name}`,
    bytes: fs.statSync(path.join(SCHEMA_DIR, name)).size,
    rank: rankOf(name),
  }));

  return entries.sort((a, b) => {
    if (a.name === 'latest.json') return -1;
    if (b.name === 'latest.json') return 1;
    for (let i = 0; i < 3; i++) {
      if (a.rank[i] !== b.rank[i]) return b.rank[i] - a.rank[i];
    }
    return a.name.localeCompare(b.name);
  });
}

export default function SchemaIndexPage() {
  const entries = readEntries();

  return (
    <main className="mx-auto w-full max-w-3xl px-6 py-16 font-mono text-sm">
      <h1 className="text-2xl font-semibold tracking-tight">Index of /schema</h1>
      <p className="mt-4 font-sans text-fd-muted-foreground">
        JSON Schemas for <Link href="/docs/config" className="underline underline-offset-4">nub.jsonc</Link>, project and global alike. Point
        the file&apos;s <span className="rounded bg-fd-muted px-1 py-0.5">$schema</span> key at one of
        these for editor completion and validation.
      </p>

      <pre className="mt-8 overflow-x-auto rounded-lg border border-fd-border bg-fd-card p-4 font-sans text-xs text-fd-muted-foreground">
        {'{\n  "$schema": "https://nubjs.com/schema/latest.json"\n}'}
      </pre>

      <hr className="my-8 border-fd-border" />

      {entries.length === 0 ? (
        <p className="text-fd-muted-foreground">No schemas published.</p>
      ) : (
        <table className="w-full text-left">
          <thead>
            <tr className="border-b border-fd-border text-fd-muted-foreground">
              <th className="pb-2 font-normal">Name</th>
              <th className="pb-2 font-normal">Size</th>
            </tr>
          </thead>
          <tbody>
            {entries.map((entry) => (
              <tr key={entry.name} className="border-b border-fd-border/50 last:border-0">
                <td className="py-2">
                  <a href={entry.href} className="underline underline-offset-4">
                    {entry.name}
                  </a>
                  {entry.name === 'latest.json' ? (
                    <span className="ml-2 font-sans text-xs text-fd-muted-foreground">
                      rolling — tracks the newest release
                    </span>
                  ) : null}
                </td>
                <td className="py-2 tabular-nums text-fd-muted-foreground">
                  {(entry.bytes / 1024).toFixed(1)} KB
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}

      <p className="mt-8 font-sans text-xs text-fd-muted-foreground">
        A versioned file names a minor series, not one release: every release in the series rewrites
        it from the rolling file, so it changes only when the field surface does. Pin one to track a
        single series; use the rolling file to pick up new fields as they ship.
      </p>
    </main>
  );
}
