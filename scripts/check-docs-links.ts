#!/usr/bin/env node
// check-docs-links — validate every internal docs link + #anchor in
// site/content/docs/** resolves to a real page and a real heading.
//
// Runs under BOTH plain Node (type-stripping) and nub:
//   node scripts/check-docs-links.ts
//   nub  scripts/check-docs-links.ts
//
// This file uses ERASABLE TypeScript only (type annotations Node's
// --experimental-strip-types removes at load): no enums, no namespaces, no
// parameter properties. Keep it that way so plain modern `node` runs it.
//
// What it checks, for every `[text](/docs/...)` link in the docs MDX:
//   1. The target PAGE exists — /docs/X maps to content/docs/X.mdx OR
//      content/docs/X/index.mdx; /docs maps to content/docs/index.mdx.
//   2. If the link carries a #anchor, that anchor matches a real heading on
//      the target page. Headings are slugified with github-slugger (the same
//      version fumadocs uses) so anchor matching is byte-exact with what the
//      rendered site produces.
//
// This catches the class that broke once: a link to /docs/install#security
// after that heading was removed by a merge. Reports every broken link with
// file:line and exits non-zero if any link is broken.
//
// Scope: only INTERNAL /docs links are validated. External (http/https),
// mailto, and non-/docs absolute/relative links are intentionally ignored —
// this tool owns the docs cross-reference graph, not the whole web.

import { readFileSync, readdirSync, statSync, existsSync } from "node:fs";
import { join, dirname, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const __dirname = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(__dirname, "..");
const DOCS_ROOT = join(REPO_ROOT, "site", "content", "docs");

// github-slugger slugifies headings identically to the rendered site (it is
// fumadocs-core's heading-slug dependency, pinned to the SAME version in
// site/package.json so anchor matching stays byte-exact). It is an EXPLICIT
// devDependency of the site workspace, so it links into site/node_modules and
// resolves with a plain require — no pnpm virtual-store scanning. github-slugger
// is ESM-only ("type":"module"); Node 22+/24 require(esm) returns its default
// export. Resolving from site/ keeps it independent of the repo-root install.
function resolveGithubSlugger(): any {
  try {
    return require(
      require.resolve("github-slugger", { paths: [join(REPO_ROOT, "site")] }),
    ).default;
  } catch (err) {
    console.error(
      "check-docs-links: could not resolve github-slugger from site/node_modules.",
    );
    console.error("Run `pnpm install` in site/ first.");
    console.error(String((err as Error)?.message ?? err));
    process.exit(2);
  }
}

const GithubSlugger = resolveGithubSlugger();

// ---- file discovery ---------------------------------------------------------

function walkMdx(dir: string): string[] {
  const out: string[] = [];
  for (const entry of readdirSync(dir)) {
    const full = join(dir, entry);
    const st = statSync(full);
    if (st.isDirectory()) {
      out.push(...walkMdx(full));
    } else if (entry.endsWith(".mdx") || entry.endsWith(".md")) {
      out.push(full);
    }
  }
  return out;
}

// ---- markdown parsing -------------------------------------------------------

// Strip fenced code blocks (``` … ``` and ~~~ … ~~~) so `# comment` lines
// inside code are not mistaken for headings, and so links inside code samples
// are not validated as real links.
function stripFences(src: string): string {
  const lines = src.split("\n");
  const out: string[] = [];
  let fence: string | null = null;
  for (const line of lines) {
    const m = line.match(/^\s*(`{3,}|~{3,})/);
    if (m) {
      if (fence === null) {
        fence = m[1][0];
        out.push(""); // keep line numbering stable
        continue;
      } else if (line.includes(fence)) {
        fence = null;
        out.push("");
        continue;
      }
    }
    out.push(fence === null ? line : "");
  }
  return out.join("\n");
}

// Strip inline code spans (`…`) within a single line, used when extracting
// heading TEXT for slugifying — github-slugger slugs the rendered text content,
// and fumadocs renders `--node` to the text "--node". Backticks are removed,
// content kept. (Block fences are already gone before this is called.)
function stripInlineCode(text: string): string {
  return text.replace(/`([^`]*)`/g, "$1");
}

// Extract heading slugs from a page (fences already stripped). Returns the set
// of github-slugger slugs, mirroring fumadocs' remark-heading: a fresh slugger
// per document (so duplicate headings get -1, -2 … suffixes deterministically).
function headingSlugs(srcNoFences: string): Set<string> {
  const slugger = new GithubSlugger();
  const slugs = new Set<string>();
  for (const line of srcNoFences.split("\n")) {
    const m = line.match(/^(#{1,6})\s+(.+?)\s*#*\s*$/);
    if (!m) continue;
    let text = m[2];
    // fumadocs custom heading id: `## Title [#custom-id]` — remark-heading
    // strips the trailing `[#slug]` and uses it verbatim as the id (NOT
    // slugified). Mirror that exactly (regex from fumadocs' remark-heading)
    // so a `#custom-id` link validates and the shadowed auto-slug isn't used.
    const custom = text.match(/\s*\[#(?<slug>[^\]]+?)\]\s*$/);
    if (custom && custom.groups) {
      slugs.add(custom.groups.slug);
      continue;
    }
    slugs.add(slugger.slug(stripInlineCode(text).trim()));
  }
  return slugs;
}

// Find every markdown link `[text](href)` and inline `<a href="…">`. Returns
// the href plus the 1-based line it sits on. Fences are stripped first so
// sample links in code blocks are not flagged.
function findLinks(srcNoFences: string): Array<{ href: string; line: number }> {
  const out: Array<{ href: string; line: number }> = [];
  const lines = srcNoFences.split("\n");
  // Markdown link target: ](...) — the simplest robust form; docs use plain
  // `[text](/docs/x)` with no titles, so a non-greedy paren capture suffices.
  const mdLink = /\]\(([^)\s]+)\)/g;
  const htmlHref = /href=["']([^"']+)["']/g;
  for (let i = 0; i < lines.length; i++) {
    let m: RegExpExecArray | null;
    mdLink.lastIndex = 0;
    while ((m = mdLink.exec(lines[i])) !== null) {
      out.push({ href: m[1], line: i + 1 });
    }
    htmlHref.lastIndex = 0;
    while ((m = htmlHref.exec(lines[i])) !== null) {
      out.push({ href: m[1], line: i + 1 });
    }
  }
  return out;
}

// ---- url → file resolution --------------------------------------------------

// Map a /docs URL path (no anchor, no query) to the file that backs it, or
// null if no such page exists. /docs -> index.mdx; /docs/x -> x.mdx or
// x/index.mdx; nested likewise.
function pageFileForUrl(urlPath: string): string | null {
  // Normalize: drop the leading /docs, trim slashes.
  let rel = urlPath.replace(/^\/docs/, "").replace(/^\/+|\/+$/g, "");
  const baseDir = DOCS_ROOT;
  if (rel === "") {
    const idx = join(baseDir, "index.mdx");
    return existsSync(idx) ? idx : existsSync(join(baseDir, "index.md")) ? join(baseDir, "index.md") : null;
  }
  const candidates = [
    join(baseDir, rel + ".mdx"),
    join(baseDir, rel + ".md"),
    join(baseDir, rel, "index.mdx"),
    join(baseDir, rel, "index.md"),
  ];
  for (const c of candidates) if (existsSync(c)) return c;
  return null;
}

// ---- main -------------------------------------------------------------------

interface Broken {
  file: string;
  line: number;
  href: string;
  reason: string;
}

function main(): void {
  if (!existsSync(DOCS_ROOT)) {
    console.error(`check-docs-links: docs root not found: ${DOCS_ROOT}`);
    process.exit(2);
  }

  const files = walkMdx(DOCS_ROOT);

  // Pre-compute the heading-slug set for every page so anchor checks are O(1).
  const slugCache = new Map<string, Set<string>>();
  function slugsFor(file: string): Set<string> {
    let s = slugCache.get(file);
    if (!s) {
      s = headingSlugs(stripFences(readFileSync(file, "utf8")));
      slugCache.set(file, s);
    }
    return s;
  }

  const broken: Broken[] = [];
  let linkCount = 0;

  for (const file of files) {
    const srcNoFences = stripFences(readFileSync(file, "utf8"));
    for (const { href, line } of findLinks(srcNoFences)) {
      // Only validate internal /docs links.
      if (!href.startsWith("/docs")) continue;
      // Split off the anchor.
      const hashIdx = href.indexOf("#");
      const urlPath = hashIdx === -1 ? href : href.slice(0, hashIdx);
      const anchor = hashIdx === -1 ? "" : href.slice(hashIdx + 1);
      linkCount++;

      const targetFile = pageFileForUrl(urlPath);
      if (!targetFile) {
        broken.push({
          file,
          line,
          href,
          reason: `no page backs ${urlPath} (expected ${urlPath.replace(/^\/docs\/?/, "content/docs/") || "content/docs"}.mdx or …/index.mdx)`,
        });
        continue;
      }
      if (anchor) {
        const slugs = slugsFor(targetFile);
        if (!slugs.has(anchor)) {
          broken.push({
            file,
            line,
            href,
            reason: `anchor #${anchor} not found in ${relative(REPO_ROOT, targetFile)} (no heading slugifies to "${anchor}")`,
          });
        }
      }
    }
  }

  if (broken.length === 0) {
    console.log(
      `check-docs-links: OK — ${linkCount} internal docs link(s) across ${files.length} page(s), all resolve.`,
    );
    process.exit(0);
  }

  console.error(
    `check-docs-links: ${broken.length} broken internal docs link(s):\n`,
  );
  for (const b of broken) {
    console.error(`  ${relative(REPO_ROOT, b.file)}:${b.line}  ${b.href}`);
    console.error(`      ${b.reason}`);
  }
  console.error(
    `\n${broken.length} broken link(s). Fix the link target or restore the heading.`,
  );
  process.exit(1);
}

main();
