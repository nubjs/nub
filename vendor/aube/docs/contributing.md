# Contributing

## Contribution Expectations

Before opening a PR, unless it is something obvious, consider creating a
discussion or mentioning what you plan to do in
[Discord](https://discord.gg/UBa7pJUN7Z). The important part is to settle the
direction before much review happens. aube has a specific scope and design
taste. I am comfortable saying no to changes that do not clearly fit.

Before I review a PR, CI must be passing and all automated AI review comments
must be addressed. If those are still open, assume I will wait to look at the
PR.

If I am on the fence about a contribution, I will probably reject it for that
reason alone. If I did not do this, aube would suffer from feature bloat. I
may also reject a PR if the quality is poor enough that I do not have confidence
the contributor can get it across the finish line. I do not have time to coach
contributors.

I get hundreds of PRs per week across my projects, so I do not have time to
respond to every PR with detailed context. A rejection may be brief.

## Code Style

All of these repos use [hk](https://hk.jdx.dev) for linting and formatting.
Run the checks before opening a PR:

```sh
hk check --all
hk fix --all
```

Some repos also expose wrapper tasks such as `mise run lint` and
`mise run lint-fix`; prefer those when they exist.

## Commit and PR Titles

Use Conventional Commits for commit messages and PR titles. Examples:

- `fix: handle missing config file`
- `docs: clarify installation steps`
- `feat: add quiet output mode`

## Building and running tests

```bash
cargo build
cargo test                                 # Unit tests
cargo clippy --all-targets -- -D warnings  # Lint
cargo fmt --check                          # Formatting
cargo audit --deny warnings                # RustSec advisories
cargo deny check bans licenses sources     # License/source policy

# BATS integration tests (needs Node.js 22, GNU `parallel`, and
# `verdaccio` on PATH; the first run will `npm i -g verdaccio@6` if it
# isn't installed). The mise task shards files across cores via
# `bats --jobs` — prefer it over the raw runner.
mise run test:bats                            # full suite, in parallel
mise run test:bats test/install.bats          # one or more files
./test/bats/bin/bats -f "<substring>" test/   # filter by test name
```

## The offline Verdaccio test registry

The BATS suite does not talk to `registry.npmjs.org`. Everything it installs
comes from a pre-seeded local Verdaccio instance under `test/registry/`.

### Layout

- `test/registry/config.yaml` — Verdaccio config. **No `uplinks` block**,
  no `proxy:` on the `packages['**']` entry — local-only by design.
- `test/registry/start.bash` — sourced by `test/setup_suite.bash`. Boots
  Verdaccio on `localhost:4873` before the suite, tears it down after.
- `test/registry/storage/` — Verdaccio's on-disk storage. Committed to
  git. Each subdirectory is an npm package: a `package.json` packument
  plus one `.tgz` per version the fixture needs. The `.verdaccio-db.json`
  index that Verdaccio regenerates on startup is `.gitignore`d.

Each BATS test writes `registry=http://localhost:4873` into a per-test
`.npmrc` (see `test/test_helper/common_setup.bash`), so `aube` picks up
the fake registry via its normal `NpmConfig` loader without any special
plumbing in the Rust code.

### Adding a new package to the fixture set

Say a new test needs `cowsay`. Because Verdaccio is local-only, the
fixture set has to contain every package (and every transitive dep) the
test will install. The recipe:

1. **Temporarily restore the `npmjs` uplink** in `test/registry/config.yaml`:

   ```yaml
   storage: ./storage

   uplinks:
     npmjs:
       url: https://registry.npmjs.org/
       cache: true

   packages:
     '**':
       access: $all
       publish: $all
       proxy: npmjs
   ```

2. **Run the test** that exercises the new package. Verdaccio will
   proxy the request upstream on the first hit and save the tarball and
   packument into `test/registry/storage/<pkg>/`. Transitive deps get
   pulled in the same way as the resolver walks the graph.

   ```bash
   cargo build
   ./test/bats/bin/bats test/your_new_test.bats
   ```

3. **Revert the uplink changes** in `test/registry/config.yaml` so it's
   back to local-only.

4. **Re-run the full suite offline** to prove the fixture set is
   complete and nothing still reaches for the network:

   ```bash
   ./test/bats/bin/bats test/
   ```

5. **Commit the new files under `test/registry/storage/`** along with
   your test. Check the diff for surprises — Verdaccio sometimes fetches
   additional versions beyond what the test asked for; drop anything
   unneeded to keep the fixture set small.

### Picking test packages

- Prefer tiny packages with few transitive deps. Every new package
  inflates the committed fixture set.
- For `aube dlx` / `exec` / `run` tests, prefer packages with a stable,
  deterministic stdout (e.g. `semver <version>` echoes its input). Tests
  that rely on help output are brittle because clap flags and trailing
  args can swallow `--help` / `--version`.
- If you need a package whose binary name differs from its package name
  (to exercise `aube dlx -p`), `which` ships `node-which` and is only a
  few KB including `isexe`.

### Why no uplink in CI?

Keeping the fixture set committed means:

- CI runs are hermetic and don't flake on npmjs.org outages or rate
  limits.
- Test failures are reproducible on any checkout of a given commit.
- Offline development works without caveats.

The tradeoff is the ~16 MB of `.tgz` files under `test/registry/storage/`
and the manual seeding step above. For a package manager project that
feels like the right tradeoff — we're exercising real tarball extraction
and linking, so mock registries aren't an option.

## Commit style

Follow the existing commit log — short imperative subject, blank line,
body wrapped at ~72 columns. Don't amend published commits. Don't skip
hooks (`--no-verify`).
