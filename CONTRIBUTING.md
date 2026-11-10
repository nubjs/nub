# Contributing to Nub

`wiki/agents.md` is written mainly for maintainers and coding agents. Before you contribute, read its [core design positions](wiki/agents.md#core-design-positions), [the brand boundary](wiki/agents.md#the-brand-boundary--public-surfaces-only-internals-are-exempt) and [the rule for commit messages](wiki/agents.md#the-repo-is-public--never-commit-internal-discussion).

## Reporting bugs and requesting features

Open an issue at [github.com/nubjs/nub/issues](https://github.com/nubjs/nub/issues). For a bug, include the output of `nub --version`, your OS, and the smallest project or command that shows the problem. For a security problem, follow [SECURITY.md](SECURITY.md) instead of opening an issue.

Before you ask for a new flag or API, read the [core design positions](wiki/agents.md#core-design-positions). In particular, the package manager CLI only takes flags and aliases that pnpm accepts, apart from a few commands of Nub's own such as `nub <file>`, `nub upgrade` and `nub pm <verb>`. Nub also adds no Nub-branded public API: no `globalThis.nub`, no `nub:*` imports, no `@nub/*` scope, no `"nub"` config field and no new `NUB_*` environment variable documented for users.

## Setup

You need:

- Rust 1.95 or newer
- Node.js 22.15 or newer
- Nub itself, to install the root JavaScript dependencies (see [Install](README.md#install))
- pnpm, if you work on the docs site in `site/`

```sh
git clone https://github.com/nubjs/nub.git
cd nub
git config core.hooksPath .githooks
nub install --frozen-lockfile --node-linker hoisted
```

The hooks in `.githooks/` strip agent co-author trailers from commit messages. Before a push they also check the `wiki/` graph and, when `site/` changed, build the site.

## Building

```sh
make addon-fast                         # native addon, copied to runtime/addons/nub-native.node
cargo build -p nub-cli --profile fast   # the CLI, at target/fast/nub
```

Use the `fast` profile while you iterate. `make build` makes a release build, which takes much longer to link.

## Running checks

Run only the tests for what you changed:

```sh
cargo test -p nub-cli --test pm_verbs   # one file from crates/nub-cli/tests/
cargo test -p nub-cli <name>            # tests whose name matches
```

Three directories are separate Cargo workspaces. Run their commands from inside the directory:

| Directory | Command |
|---|---|
| `crates/nub-native` | `cargo clippy --all-features --profile fast -- -D warnings && cargo build --profile fast` |
| `crates/nub-launcher` | `cargo clippy --all-targets -- -D warnings && cargo build && cargo test` |
| `vendor/aube` | `cargo test -p <crate>` |

`crates/nub-native` has no tests of its own. To test a change to it, run `make addon-fast` from the repo root, which copies the addon into `runtime/addons/`, then run the `nub-cli` tests that load it.

Before you push, run `make verify`. It checks formatting and the lockfiles, runs Clippy on the whole workspace with all features and warnings as errors, runs the brand lint, and then runs the full `cargo test`, so expect it to take a while. `make fix` applies Clippy's automatic fixes; review its diff before you commit. `make verify` doesn't cover `crates/nub-launcher`, the platform matrix or the Docker jobs.

## Docs

- A change that users can see also updates its page in `site/content/docs/`. A new page for a feature that hasn't been released yet needs `unpublished: true` in its frontmatter. For a quick check, run `pnpm run typecheck` in `site/`. The pre-push hook runs the full `next build`.
- `wiki/` is a [lat.md](https://github.com/1st1/lat.md) graph of design notes and research, and it links into the source code. If you rename something it links to, fix the link in the same commit. `nub run lat:check` lists broken links, and the pre-push hook and CI run it too.
- Write "Nub" in prose. Lowercase `nub` is only for the command and for file and package names, in backticks.

## Commits

- Write the subject as `area: what changed`, for example `pm: add --save-types to nub add`. `git log` shows the area names in use. State what changed in plain words, with no superlatives and no claims that Nub beats another tool.
- Don't add a `Co-authored-by` trailer that names an AI agent. The commit-msg hook removes it, and the `commit-hygiene` workflow fails a pull request that has one. Human co-authors are fine.

## Pull requests

- Open the pull request against `main`. If it fixes an issue, put `Fixes #N` in the description so the issue closes on merge. Use `Refs #N` if it's only related.
- Leave "Allow edits by maintainers" on when you open it from a fork. If your branch needs changes, a maintainer pushes them to it directly instead of asking you to amend it.
- CI doesn't start when you open a pull request or push to it. Once review is done and the branch is ready to merge, a maintainer starts a run by adding the `ci` label, or `ci-full` to add the slower workflows. Adding a label needs write access, so a pull request from a fork waits for a maintainer. Get `make verify` passing first so that one run is enough.
- Vercel and the review bots run on every pull request and can go green when CI hasn't run at all. The `CI gate` check is the one that shows CI ran, so a pull request isn't verified until it appears and passes.
- A run covers [`ci.yml`](.github/workflows/ci.yml) (formatting, Clippy, the test suite on Linux and Windows, some jobs on macOS, the site build and docs links), [`lat-check.yml`](.github/workflows/lat-check.yml) and [`commit-hygiene.yml`](.github/workflows/commit-hygiene.yml). A change under `vendor/aube` also runs aube's own tests in [`aube-parity.yml`](.github/workflows/aube-parity.yml).

## The vendored package manager

Nub's package manager is the [aube](https://github.com/jdx/aube) engine, kept as plain files in `vendor/aube`. Edit it like any other file and send the change in a normal Nub pull request. Standalone aube can't be built from this tree, so `vendor/aube/CONTRIBUTING.md`, which is written for the aube project, doesn't apply here.

## License

Nub is [MIT licensed](LICENSE). Contributions are accepted under the same license.
