import assert from "node:assert/strict";
import test from "node:test";
import { ogImagePath, renderOgSvg, titleFromMarkdown } from "./generate-og-images.mjs";

test("uses the homepage tagline for the root card", () => {
  assert.equal(titleFromMarkdown("---\nlayout: home\n---\n", "index.md"), "A fast Node.js package manager");
});

test("reads titles from frontmatter and markdown headings", () => {
  assert.equal(
    titleFromMarkdown('---\ntitle: "Workspace YAML Settings"\n---\n# ignored', "settings/workspace-yaml.md"),
    "Workspace YAML Settings",
  );
  assert.equal(titleFromMarkdown("# `aube install`\n", "cli/install.md"), "aube install");
});

test("maps documentation paths to stable image paths", () => {
  assert.equal(ogImagePath("cli/install.md"), "og/cli/install.png");
});

test("renders only the white logo on the dark background", () => {
  const svg = renderOgSvg();
  assert.equal((svg.match(/<path /g) ?? []).length, 2);
  assert.equal((svg.match(/stroke="#fff"/g) ?? []).length, 2);
  assert.doesNotMatch(svg, /<text|<defs|gradient/);
});
