import { mkdir, readFile, readdir } from "node:fs/promises";
import { dirname, join, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";
import sharp from "sharp";

const DOCS_DIR = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const DEFAULT_OUTPUT_DIR = resolve(DOCS_DIR, ".vitepress/dist/og");

const WIDTH = 1200;
const HEIGHT = 630;

export function ogImagePath(relativePath) {
  return `og/${relativePath.replace(/\.md$/, ".png")}`;
}

function stripMarkdown(value) {
  return value
    .replace(/`([^`]+)`/g, "$1")
    .replace(/\[([^\]]+)\]\([^)]*\)/g, "$1")
    .replace(/[*_~]/g, "")
    .trim();
}

function unquote(value) {
  if (
    value.length >= 2 &&
    ((value.startsWith('"') && value.endsWith('"')) ||
      (value.startsWith("'") && value.endsWith("'")))
  ) {
    return value.slice(1, -1);
  }
  return value;
}

export function titleFromMarkdown(markdown, relativePath) {
  if (relativePath === "index.md") return "A fast Node.js package manager";

  const frontmatter = markdown.match(/^---\r?\n([\s\S]*?)\r?\n---(?:\r?\n|$)/)?.[1];
  const frontmatterTitle = frontmatter
    ?.split(/\r?\n/)
    .find((line) => /^title\s*:/.test(line))
    ?.replace(/^title\s*:\s*/, "");
  if (frontmatterTitle) return stripMarkdown(unquote(frontmatterTitle));

  let inFence = false;
  for (const line of markdown.split(/\r?\n/)) {
    if (/^\s*(```|~~~)/.test(line)) {
      inFence = !inFence;
      continue;
    }
    if (!inFence && /^#\s+/.test(line)) {
      return stripMarkdown(line.replace(/^#\s+/, ""));
    }
  }

  return relativePath
    .replace(/(?:^|\/)index\.md$/, "")
    .replace(/\.md$/, "")
    .split("/")
    .filter(Boolean)
    .at(-1)
    ?.replaceAll("-", " ") ?? "Documentation";
}

export function renderOgSvg() {
  return `<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" width="${WIDTH}" height="${HEIGHT}" viewBox="0 0 ${WIDTH} ${HEIGHT}">
  <rect width="1200" height="630" fill="#100f0d"/>
  <g transform="translate(390 145.5) scale(1.5)">
    <path d="M0 154H280" stroke="#fff" stroke-width="32" stroke-linecap="round"/>
    <path d="M58 154A82 82 0 0 1 222 154" stroke="#fff" stroke-width="32" stroke-linecap="round"/>
  </g>
</svg>`;
}

async function markdownFiles(directory) {
  const files = [];
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    if (entry.name.startsWith(".") || entry.name === "node_modules") continue;
    const path = join(directory, entry.name);
    if (entry.isDirectory()) files.push(...(await markdownFiles(path)));
    else if (entry.isFile() && entry.name.endsWith(".md")) files.push(path);
  }
  return files;
}

export async function generateOgImages(outputDir = DEFAULT_OUTPUT_DIR) {
  const files = await markdownFiles(DOCS_DIR);
  await Promise.all(
    files.map(async (file) => {
      const relativePath = relative(DOCS_DIR, file).split(sep).join("/");
      const markdown = await readFile(file, "utf8");
      const title = titleFromMarkdown(markdown, relativePath);
      const output = resolve(outputDir, ogImagePath(relativePath).slice(3));
      await mkdir(dirname(output), { recursive: true });
      await sharp(Buffer.from(renderOgSvg(title))).png().toFile(output);
    }),
  );
  return files.length;
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const outputDir = process.argv[2] ? resolve(process.argv[2]) : DEFAULT_OUTPUT_DIR;
  const count = await generateOgImages(outputDir);
  console.log(`generated ${count} Open Graph images in ${outputDir}`);
}
