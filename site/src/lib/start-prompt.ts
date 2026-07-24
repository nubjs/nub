/* The exact text the homepage "Copy agent prompt" button copies. A short,
   paste-into-your-agent prompt modeled on Vite+'s: it installs Nub's agent
   skill globally, points the agent at the full adoption guide
   (https://nubjs.com/start.md) and command reference
   (https://nubjs.com/llms-full.txt) to read, and lists the day-to-day
   commands — the detailed playbook (investigate → propose an opt-in plan →
   implement only approved steps) lives in start.md, which the agent fetches
   and follows rather than having it pasted inline. Keep this in sync with the
   install commands in InstallTabs and the surface in start.md. */
export const START_PROMPT = `I want to use Nub in my project. Nub is a single Rust CLI that augments your installed Node.js — one tool that runs TypeScript and JSX files directly, runs your scripts and local CLIs, manages packages, and provisions Node versions, with no new runtime and no lock-in. First, install Nub's agent skill globally with \`npx skills add nubjs/nub --skill nub -g\`. Then read https://nubjs.com/start.md — the guide to adopting Nub — and follow it; the full command reference is https://nubjs.com/llms-full.txt. Install the \`nub\` CLI:
- macOS / Linux: curl -fsSL https://nubjs.com/install.sh | bash
- Windows (PowerShell): irm https://nubjs.com/install.ps1 | iex
- Homebrew: brew install nubjs/tap/nub
- npm: npm install -g @nubjs/nub
Then open a new terminal and run \`nub --help\`. Day-to-day commands: \`nub <file>\` (run a TS or JS file), \`nub run <script>\` (package scripts), \`nubx <tool>\` (run a local or remote CLI), \`nub install\` (dependencies), and \`nub add <pkg>\` / \`nub remove <pkg>\`. Nub reads and writes my project's existing lockfile, so there's no package manager to switch. Investigate my project, explain how Nub would simplify it, and propose an integration plan as a set of opt-in steps I can pick from — make no changes without my approval.`;
