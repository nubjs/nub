import { defineConfig } from "vitepress";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import spec from "../cli/commands.json";

interface Cmd {
  name: string;
  full_cmd: string[];
  subcommands: Record<string, Cmd>;
  hide?: boolean;
}

interface ReleaseMetadata {
  version?: string;
  releasedAt?: string;
}

function getCommands(cmd: Cmd): string[][] {
  const commands: string[][] = [];
  for (const [name, sub] of Object.entries(cmd.subcommands)) {
    if (sub.hide) continue;
    commands.push(sub.full_cmd);
    commands.push(...getCommands(sub));
  }
  return commands;
}

const commands = getCommands(spec.cmd as unknown as Cmd);
const configDir = dirname(fileURLToPath(import.meta.url));
const cargoToml = readFileSync(resolve(configDir, "../../Cargo.toml"), "utf8");
const versionMatch = cargoToml.match(/\[workspace\.package\][\s\S]*?\nversion\s*=\s*"([^"]+)"/);
const aubeVersion = versionMatch?.[1] ?? "0.0.0";
const releaseMetadata = JSON.parse(
  readFileSync(resolve(configDir, "../../release.json"), "utf8"),
) as ReleaseMetadata;
const aubeReleasedAt =
  releaseMetadata.version === aubeVersion ? (releaseMetadata.releasedAt ?? "") : "";

export default defineConfig({
  title: "aube",
  description: "A fast Node.js package manager",
  appearance: "force-dark",
  head: [
    ["link", { rel: "icon", href: "/favicon.svg", type: "image/svg+xml" }],
    ["link", { rel: "icon", href: "/favicon.ico", sizes: "any" }],
    [
      "link",
      {
        rel: "icon",
        href: "/favicon-16x16.png",
        type: "image/png",
        sizes: "16x16",
      },
    ],
    [
      "link",
      {
        rel: "icon",
        href: "/favicon-32x32.png",
        type: "image/png",
        sizes: "32x32",
      },
    ],
    [
      "link",
      {
        rel: "apple-touch-icon",
        href: "/apple-touch-icon.png",
        sizes: "180x180",
      },
    ],
    ["link", { rel: "manifest", href: "/site.webmanifest" }],
    ["meta", { name: "theme-color", content: "#FFB13B" }],
  ],
  themeConfig: {
    logo: "/logo.svg",
    nav: [
      { text: "Home", link: "/" },
      { text: "Benchmarks", link: "/benchmarks" },
      { text: "CLI Reference", link: "/cli/" },
      { text: "Settings", link: "/settings/" },
      { text: "Releases", link: "https://github.com/jdx/aube/releases" },
    ],

    sidebar: [
      {
        text: "Guide",
        items: [
          { text: "Overview", link: "/guide" },
          { text: "Getting Started", link: "/getting-started" },
          { text: "Installation", link: "/installation" },
          { text: "Contributing", link: "/contributing" },
          { text: "For pnpm users", link: "/pnpm-users" },
          { text: "For npm users", link: "/npm-users" },
          { text: "For yarn users", link: "/yarn-users" },
          { text: "For bun users", link: "/bun-users" },
          { text: "Troubleshooting", link: "/troubleshooting" },
          { text: "Error codes", link: "/error-codes" },
        ],
      },
      {
        text: "Package Manager",
        items: [
          { text: "Install dependencies", link: "/package-manager/install" },
          { text: "Manage dependencies", link: "/package-manager/dependencies" },
          { text: "Run scripts and binaries", link: "/package-manager/scripts" },
          { text: "Node runtime switching", link: "/package-manager/node-runtime" },
          { text: "Workspaces", link: "/package-manager/workspaces" },
          { text: "Lockfiles", link: "/package-manager/lockfiles" },
          { text: "node_modules layout", link: "/package-manager/node-modules" },
          { text: "Global virtual store", link: "/package-manager/global-virtual-store" },
          { text: "Lifecycle scripts", link: "/package-manager/lifecycle-scripts" },
          { text: "Configuration", link: "/package-manager/configuration" },
          { text: "Registry and auth", link: "/package-manager/registry-auth" },
          { text: "Publishing", link: "/package-manager/publishing" },
        ],
      },
      {
        text: "Security",
        items: [
          { text: "Overview", link: "/security" },
          { text: "Jailed builds", link: "/package-manager/jailed-builds" },
          { text: "Security scanner", link: "/package-manager/security-scanner" },
        ],
      },
      {
        text: "Performance",
        items: [
          { text: "Benchmarks", link: "/benchmarks" },
        ],
      },
      {
        text: "CLI Reference",
        link: "/cli/",
        collapsed: true,
        items: commands.map((cmd) => ({
          text: cmd.join(" "),
          link: `/cli/${cmd.join("/")}`,
        })),
      },
      {
        text: "Settings Reference",
        link: "/settings/",
      },
    ],

    outline: { level: [2, 3] },

    footer: false,

    editLink: {
      pattern: "https://github.com/jdx/aube/edit/main/docs/:path",
      text: "Edit this page on GitHub",
    },

    search: { provider: "local" },
  },
  vite: {
    define: {
      __AUBE_VERSION__: JSON.stringify(aubeVersion),
      __AUBE_RELEASED_AT__: JSON.stringify(aubeReleasedAt),
    },
    plugins: [
      {
        name: "aube-version-file",
        closeBundle() {
          const distDir = resolve(configDir, "dist");
          mkdirSync(distDir, { recursive: true });
          writeFileSync(resolve(distDir, "VERSION"), `${aubeVersion}\n`);
        },
      },
    ],
  },
});
