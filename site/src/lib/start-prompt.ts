/* The exact text the homepage "Copy agent prompt" button copies. A short,
   paste-into-your-agent prompt modeled on Vite+'s: it installs Nub's agent
   skill, points the agent at the full adoption guide (https://nubjs.com/start.md)
   and command reference (https://nubjs.com/llms-full.txt) to read, and lists
   the day-to-day commands — the detailed 7-step playbook lives in start.md,
   which the agent fetches and follows rather than having it pasted inline. Keep
   this in sync with the install commands in InstallTabs and the surface in
   start.md. */
export const START_PROMPT = `I want to use nub in my project. nub is a single Rust CLI that augments your installed Node.js — one tool that runs TypeScript and JSX files directly, runs your scripts and local CLIs, manages packages, and provisions Node versions, with no new runtime and no lock-in. First, install Nub's agent skill in this project with \`npx skills add nubjs/nub --skill nub\`. Then read https://nubjs.com/start.md — the guide to adopting nub — and follow it; the full command reference is https://nubjs.com/llms-full.txt. Install the \`nub\` CLI:
- macOS / Linux: curl -fsSL https://nubjs.com/install.sh | bash
- Windows (PowerShell): irm https://nubjs.com/install.ps1 | iex
- Homebrew: brew install nubjs/tap/nub
- npm: npm install -g @nubjs/nub
Then open a new terminal and run \`nub --help\`. Day-to-day commands: \`nub <file>\` (run a TS or JS file), \`nub run <script>\` (package scripts), \`nubx <tool>\` (run a local or remote CLI), \`nub install\` (dependencies), and \`nub add <pkg>\` / \`nub remove <pkg>\`. nub reads and writes your project's existing lockfile, so there's nothing to migrate and no package manager to switch. Help me get set up, explain anything I should know, and make no other changes to my project without asking first.`;
