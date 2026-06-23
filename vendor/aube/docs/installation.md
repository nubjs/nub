# Installation

## Recommended: mise

Install aube globally with mise:

```sh
mise use -g aube
```

This installs `aube` on your PATH and lets mise manage future upgrades.

::: tip
We recommend mise because it can manage `aube` and your
[Node.js runtime](https://mise.jdx.dev/lang/node.html) from the same
toolchain. If your projects already pin Node through `package.json`
(`devEngines.runtime`) or files such as `.nvmrc` and
`.node-version`, opt mise into reading those idiomatic version files:

```sh
mise settings add idiomatic_version_file_enable_tools node
```
:::

## From crates.io

If you already have a Rust toolchain installed, you can install the
latest released `aube` from crates.io:

```sh
cargo install aube --locked
```

::: info
`--locked` makes cargo honor the committed `Cargo.lock` so you get the
same dependency versions CI built against. The compiled binary lands in
`~/.cargo/bin/aube`.
:::

## From Homebrew

aube is published from the jdx tap until it lands in homebrew-core:

```sh
brew install jdx/tap/aube
```

The tap formula builds from source and installs shell completions.

## From npm

aube is also published on npm as `@endevco/aube`:

```sh
npm install -g --ignore-scripts=false @endevco/aube
npx --ignore-scripts=false @endevco/aube --version
```

::: warning
The npm package relies on its `preinstall` script to fetch the
platform-specific native binary and wire up the `aube`, `aubr`, and `aubx`
commands. That native binary is what gives aube its startup and install
performance; without the script, npm can leave the package installed
without working commands. The npm commands above pass
`--ignore-scripts=false` so it still works for users with
`ignore-scripts=true` in their npm config.

We recommend installing with mise if you want the native binary without npm
lifecycle-script behavior.
:::

## Ubuntu (PPA)

**Supported:** Ubuntu 26.04 (resolute).

Aube publishes signed `.deb` packages to the Launchpad PPA
[`ppa:jdxcode/aube`](https://launchpad.net/~jdxcode/+archive/ubuntu/aube):

```sh
sudo apt install -y software-properties-common   # if add-apt-repository isn't already available
sudo add-apt-repository -y ppa:jdxcode/aube
sudo apt install aube
```

Future upgrades go through `apt`:

```sh
sudo apt update && sudo apt install --only-upgrade aube
```

## Fedora / RHEL (COPR)

**Supported:** Fedora 42, Fedora 43, Fedora Rawhide, EPEL 9, EPEL 10
(RHEL / Rocky / Alma 9 and 10), both `x86_64` and `aarch64`.

Aube publishes RPMs to the COPR project
[`jdxcode/aube`](https://copr.fedorainfracloud.org/coprs/jdxcode/aube/):

```sh
sudo dnf copr enable jdxcode/aube
sudo dnf install aube
```

The `dnf copr` subcommand ships with `dnf-plugins-core` — install that
first on EPEL and anywhere else the plugin isn't already pulled in.
Future upgrades go through the package manager:

```sh
sudo dnf upgrade aube
```

## From source

If you want to build the current checkout yourself, use the standard source
build flow:

```sh
git clone https://github.com/jdx/aube
cd aube
cargo install --path crates/aube
```

This installs the `aube` binary into `~/.cargo/bin`.

## GitHub Actions

For CI workflows, use the
[`jdx/aube-action`](https://github.com/jdx/aube-action) Action.
It downloads the prebuilt aube binary that matches the runner's OS and
architecture, adds it to `PATH`, and (optionally) installs Node.js
inline via [mise](https://mise.jdx.dev) so a single step covers both
the package manager and the runtime:

```yaml
- uses: jdx/aube-action@v1
- run: aube install
```

Pin a specific aube version, install Node, and run `aube install` in
one go:

```yaml
- uses: jdx/aube-action@v1
  with:
    version: 1.5.1         # or "latest"
    node-version: "22"     # or "auto" to read mise.toml / .tool-versions / .nvmrc
    run-install: true
```

::: tip
With `node-version: auto`, the action runs `mise ls --current node`
against the workspace, so any of `mise.toml`, `.tool-versions`,
`.nvmrc`, `.node-version`, or `package.json` `devEngines.runtime` is
honored — no separate `actions/setup-node` step required.
:::

See the [`aube-action` README](https://github.com/jdx/aube-action#readme)
for the full input/output reference.

## Verify

```sh
aube --version
```

## Shell completions

Completions are powered by [`usage`](https://usage.jdx.dev), so install
that first:

```sh
mise use -g usage
```

Then render the completion script for your shell:

```sh
aube completion bash   > /etc/bash_completion.d/aube
aube completion zsh    > "${fpath[1]}/_aube"
aube completion fish   > ~/.config/fish/completions/aube.fish
```
