//! The curated per-package grant table for the build jail.
//!
//! §0c asks one question — "what is the smallest positive grant set that works, and which
//! packages have earned an exception?" — and this module is the second half of it. A
//! handful of long-known codegen packages write outside the one subtree the jail's
//! baseline grants them (their own `package_dir`), and without an exception each fails an
//! install that works everywhere else. §0g settles the direction: carve them
//! automatically, curated by nub, because a jail that breaks installs gets turned off and
//! then protects nothing.
//!
//! # The authorship invariant, held structurally (§0e)
//!
//! The table is a nub-side `static`, exactly like [`DOWNLOAD_HOSTS`]. Nothing here is read
//! from a dependency's manifest, so there is no spelling by which a package names itself
//! into it. The two channels that could have leaked authorship are closed by construction:
//!
//! - **The KEY is aube's installer-resolved `registry_name()`**, the same identity
//!   `dependenciesMeta.<name>` matches on and the same one the per-package opt-out uses.
//!   It is the name the RESOLVER assigned, never the `name` a package wrote into its own
//!   manifest, so a `file:` dep, a workspace member, or a symlinked manifest declaring
//!   `"name": "prisma"` resolves under its own real identity and matches nothing. aube
//!   withholds the name entirely (`None`) whenever its root is a checkout it FETCHED, so
//!   attacker-authored content at the root cannot reach this table at all.
//! - **The VALUE is a fixed path list**, and the one entry that is not literal
//!   ([`ProjectPaths::ManifestField`]) reads a nub-NAMED field out of the CONSUMER's own
//!   root manifest — root-authored config, which §0e approves — and clamps every resolved
//!   path back inside the project root. A dependency authors neither the field name nor
//!   the anchor.
//!
//! # Why these shapes and no broader one
//!
//! Every grant is a PURE POSITIVE rw nested inside a subtree the jail already read-grants,
//! so it introduces no deny and `backend::windows::deny_shadows_grant` stays satisfied —
//! the pure-allowlist invariant is untouched (§0b.1).
//!
//! [`SiblingDirs`](CuratedGrant::sibling_dirs) is an ENUMERATED namespace under the
//! package's OWN enclosing `node_modules`, never a pattern over dot-entries. That
//! distinction is load-bearing and is argued at the `package_dir` entry in
//! [`super::preset`]: the dot-entries at a `node_modules` root are not scratch space, they
//! are the install itself — `.store`/`.pnpm` hold every materialized dependency's source
//! before it is executed, and `.bin` is the shim directory later tooling runs UNCONFINED.
//! Naming `.prisma` grants one directory; a dot-entry pattern would grant those.

use crate::compiler::defaults;
use crate::matcher::path::{Homes, canonicalize_glob_prefix};
use crate::policy::{CanonGlob, Effect, FsAccess, FsOrigin, FsRule, SandboxPolicy};
use std::path::{Path, PathBuf};

/// Where a package's project WRITE targets come from, when it has any.
///
/// TWO SHAPES, split on WHO AUTHORED THE PATH, which is the only distinction that matters
/// for what a reviewer has to check:
///
/// - [`ManifestField`](ProjectWrites::ManifestField) — a dotted field path into the
///   CONSUMER's root `package.json` whose value is a string or array of project-relative
///   directories. nub owns the field NAME; the consumer owns the value. For a package that
///   imposes no directory convention of its own, the consumer's manifest is the only place
///   the answer exists, and reading it is the narrow alternative to granting the project
///   tree.
/// - [`Literal`](ProjectWrites::Literal) — a path nub names outright, for a package that
///   writes where IT decides. `.git/hooks` is the case: git owns that path, the consumer
///   configures nothing, and a hook installer's entire function is to write there.
///
/// Both are clamped back inside the project root by [`contained`], so the shapes differ in
/// provenance and not in reach.
#[derive(Debug, Clone, Copy, PartialEq)]
enum ProjectWrites {
    None,
    ManifestField(&'static [&'static str]),
    Literal(&'static [&'static str]),
}

/// One `$HOME`-anchored artifact cache, and the package's own variable that redirects it.
///
/// PER-OS because the default is per-OS: `cachedir('Cypress')` resolves to
/// `~/Library/Caches/Cypress` on macOS and `$XDG_CACHE_HOME/Cypress` on Linux, and Playwright
/// splits the same way. A `None` platform means nub measured nothing there and grants nothing.
#[derive(Debug, Clone, Copy, PartialEq)]
struct HomePath {
    env: &'static str,
    macos: Option<&'static str>,
    linux: Option<&'static str>,
    windows: Option<&'static str>,
}

impl HomePath {
    /// Everything that is not macOS or Windows takes the `linux` spelling, matching how these
    /// packages themselves branch (`cachedir` and Playwright's registry both treat every
    /// remaining platform as the XDG one).
    fn pattern(&self) -> Option<&'static str> {
        if cfg!(target_os = "macos") {
            self.macos
        } else if cfg!(windows) {
            self.windows
        } else {
            self.linux
        }
    }
}

/// One package's exception. Absent fields mean the jail's baseline already suffices.
#[derive(Debug, Clone, Copy, PartialEq)]
struct CuratedGrant {
    /// Named entries of the package's OWN enclosing `node_modules` it may write —
    /// ENUMERATED, never a pattern. Correct under every linker: `enclosing_node_modules`
    /// resolves to the store cell's `node_modules` under the isolated layout and to the
    /// project's under a hoisted one, which is the same directory the package's own
    /// `path.resolve(cwd, '../..')` arithmetic lands on either way.
    sibling_dirs: &'static [&'static str],
    /// Chains of package NAMES whose resolved directories the package may write.
    ///
    /// The shape `sibling_dirs` cannot express. A sibling is a name JOINED to the package's
    /// own enclosing `node_modules`; under the isolated layout a package's DEPENDENCIES do
    /// not live there at all — each resolves through a symlink into its own store cell — so
    /// a package that legitimately writes into another package's directory has no spelling.
    /// `@prisma/client`'s postinstall re-execs the `prisma` CLI, which downloads the query
    /// engine into `@prisma/engines`' package dir; measured, that write is the difference
    /// between a generated client and none (see the catalog entry's `observed`).
    ///
    /// A CHAIN, not a flat list, because resolution is relative: `@prisma/engines` is not
    /// resolvable from `@prisma/client` under an isolated linker — only from `prisma`. So
    /// `[["prisma"], ["prisma", "@prisma/engines"]]` reads "the `prisma` this package
    /// resolves, and the `@prisma/engines` THAT one resolves", and each hop is the ordinary
    /// `node_modules` ancestor walk Node itself would do. That makes the field
    /// linker-agnostic for free: under a hoisted layout both hops land on
    /// `<project>/node_modules/<name>`.
    ///
    /// THE AUTHORSHIP INVARIANT SURVIVES because a name is not a path. nub authors the
    /// names; the tree decides what they resolve to; a dependency can therefore only be
    /// reached if the granted package could already `require` it. Nothing a package writes
    /// into its own manifest enters here.
    dependency_dirs: &'static [&'static [&'static str]],
    /// `$HOME`-anchored artifact caches the package downloads into, each named by the
    /// package's OWN documented override variable. Granted read-write, and the variable is
    /// SET to the granted path.
    ///
    /// THE PROBLEM THIS EXISTS FOR IS RUN TIME, NOT INSTALL TIME. `preset::private_home_dir`
    /// already gives every jailed script a writable private `HOME`, so a package that
    /// downloads a browser into `$HOME/…` installs and exits 0 today. It is the app that
    /// breaks afterwards: the user's own `HOME` is the real one, nub sets no variable outside
    /// the install, so `cypress verify` looks in `~/Library/Caches/Cypress` and finds nothing
    /// — measured, `No version of Cypress is installed in: …`. Pointing the package's install
    /// at the SAME path its run-time lookup computes is what closes that, and it is why the
    /// path has to be the tool's real default rather than a directory nub picks: nub is not in
    /// the loop when the app runs, so a nub-chosen location would be equally unreachable.
    ///
    /// WHY THIS IS A NARROW GRANT AND NOT THE JAIL INVERTED. The alternative shapes both fail
    /// on authorship. Copying the private home out into the real one publishes
    /// DEPENDENCY-CHOSEN paths — `~/.zshrc`, `~/.config/git/config` with `core.hooksPath`,
    /// `~/Library/LaunchAgents/*` — as nub. Dropping the `HOME` redirect returns every package
    /// that uses the free scratch home to EPERM and reopens the `$HOME/.npmrc` cross-package
    /// channel `private_home_dir` was built to close. This grants ONE directory, per package,
    /// by name, read-write; it reads nothing else under `$HOME` and opens no socket. Homebrew
    /// resolves the same tension the same way — real `$HOME`, `deny_read_home`, and a curated
    /// allowlist of specific writable paths.
    ///
    /// SUBSUMPTION: a package that can write its own binary cache already has arbitrary code
    /// execution as itself, which depending on it already grants. The grant adds the ability
    /// to write a file the package would then run — not the ability to run one.
    ///
    /// THE VARIABLE IS SET UNCONDITIONALLY, overwriting an ambient value. Honoring an ambient
    /// one would make a path the environment authored decide where nub grants write, which is
    /// exactly the authorship channel this module's invariant closes; and the grant is
    /// compiled against nub's path either way, so a respected ambient value would install to a
    /// directory the policy does not permit. Accepted residual: a user who has set
    /// `CYPRESS_CACHE_FOLDER` gets nub's default under the jail, and turns confinement off for
    /// that package (`dependenciesMeta.<name>.sandbox: false`) if they need theirs honored.
    home_paths: &'static [HomePath],
    /// Project-relative subtrees it may READ — a codegen INPUT the consumer authored, and
    /// the reason this is a separate field from `project_writes`: a generator needs its
    /// schema readable, not writable.
    ///
    /// NOT a way to narrow another field. These rules are appended AFTER `sibling_dirs`,
    /// so for a while an entry here that ENCLOSED a sibling-dir target revoked its write on
    /// macOS — `emit_fs` compiled a read Allow into a write deny. That is fixed at the
    /// backend (an Allow emits no deny anywhere), so the fields compose: each one adds, and
    /// nothing in this struct subtracts.
    project_reads: &'static [&'static str],
    /// Project-relative subtrees it may WRITE.
    project_writes: ProjectWrites,
    /// Grant READ on the project root DIRECTORY NODE — the node alone, never `/**`.
    ///
    /// For a package whose postinstall makes the consumer's project its working directory:
    /// `@prisma/client` calls `process.chdir(INIT_CWD)` outright, msw spawns its CLI with
    /// `cwd` set there. Node resolves the new cwd through `uv_cwd`, which needs the
    /// directory itself readable, and the baseline grants only `package.json` INSIDE it —
    /// so without this both die in `uv_cwd` before running a line of their own logic
    /// (measured: `EPERM: process.cwd failed … uv_cwd`).
    ///
    /// Node-only is what keeps this from being a project read: the CONTENTS of the
    /// project stay ungranted, and everything the package then reads there still has to be
    /// named in `project_reads`.
    ///
    /// THE NODE/SUBTREE DISTINCTION IS THE BACKENDS' JOB AND BOTH ONCE LOST IT, in the
    /// widening direction, silently — an earlier version of this comment claimed the
    /// residual was "filenames, never contents", and it was not. Seatbelt rendered the bare
    /// path as `(subpath …)`, so the read covered the whole project AND the same term became
    /// a subtree write-DENY that revoked `package_dir` and `sibling_dirs` under
    /// last-match-wins. Landlock's mount plan collapsed it into a subtree grant whose rights
    /// every file below inherits. Both are fixed at their backend — `(literal …)` on macOS,
    /// `MountAccess::ListOnly` (`READ_DIR` alone) on Linux — so a change here should re-read
    /// those before assuming a term named a node just because this field says so.
    ///
    /// The residual now IS filenames: Landlock's `READ_DIR` lets the package list the
    /// project root's top-level entries. macOS grants the node's own reads. Contents stay
    /// out on both, and only for the handful of packages in the table.
    ///
    /// LINUX GETS NOTHING FROM THIS and keeps it anyway: `chdir` is not a Landlock-handled
    /// access, so the operation the grant exists to permit was never denied there (measured,
    /// 6.17 / ABI 7). It stays because Seatbelt DOES gate it — `uv_cwd` was the measured
    /// failure — and the field is one catalog entry, not one per platform.
    project_cwd: bool,
}

// THE TABLE IS DATA, NOT CODE. `CURATED_GRANTS` is generated by `build.rs` from the
// `packageGrants` array of `data/build-jail-catalog.json`. Each entry's mechanism — the
// thing that BOUNDS the grant, since a future version writing somewhere else needs a new
// measurement rather than a wider path — is recorded there beside the denial that was
// measured and the platform it was measured on. `data/README.md` states what evidence a
// new entry needs.
//
// Moving to a data file did NOT move the authorship invariant: this is still a nub-side
// `static` baked in at compile time, still keyed on aube's installer-resolved name (see
// the module docs). What the move DID add is a text surface a contributor edits directly,
// so the generator rejects at BUILD time a `sibling_dirs` entry carrying a separator or
// `..`, and a `project_reads` entry that traverses out of the project — shapes that were
// conspicuous as a Rust literal and are easy to slip into JSON.
include!(concat!(env!("OUT_DIR"), "/curated_grants.rs"));

#[cfg(test)]
impl CuratedGrant {
    const NONE: Self = Self {
        sibling_dirs: &[],
        dependency_dirs: &[],
        home_paths: &[],
        project_reads: &[],
        project_writes: ProjectWrites::None,
        project_cwd: false,
    };
}

/// The grant table written out BY HAND, as the independent mirror of the catalog for
/// `the_catalog_matches_the_hand_written_mirror`. It is deliberately a hand-authored literal
/// rather than a re-read of the JSON: its whole job is to fail if the parse-and-codegen path
/// changed a single grant, so it must not be derived from the same source it checks. That is
/// what keeps the check non-circular even though both sides are now maintained together — one
/// side arrives through `catalog::parse` plus `build.rs`, the other through `rustc`.
///
/// It began life as the frozen PRE-CATALOG table, proving the migration to JSON was lossless.
/// That proof is done and in the past; the mirror kept its value and gained a maintenance
/// cost, which is the trade taken deliberately. A catalog edit updates both.
#[cfg(test)]
static GOLDEN_PRE_CATALOG_GRANTS: &[(&str, CuratedGrant)] = &[
    // FOUR needs, and they are staged — each one masks the next, which is why the first fix
    // looked complete and was not. `scripts/postinstall.js`:
    //   1. `process.chdir(INIT_CWD)` — the consumer's project. Needs `project_cwd`.
    //   2. `createDefaultGeneratedThrowFiles` mkdirs
    //      `path.join(__dirname, '../../../.prisma')`, a SIBLING of `@prisma` in the
    //      package's own enclosing `node_modules`, one level ABOVE the granted
    //      `package_dir`. UNCONDITIONAL — it runs before any schema is looked at.
    //   3. it looks for the `prisma` CLI and re-execs it as `prisma generate`, which READS
    //      the consumer's `prisma/` schema directory.
    //   4. that CLI writes the query engine into `@prisma/engines`' package directory and
    //      copies it into its own — two directories belonging to OTHER packages, which is
    //      what `dependency_dirs` exists for.
    // Granting only (2) makes the script exit 0 having written nothing but the throw-stubs
    // it writes unconditionally — a green run with no generated client. The nonce test
    // (a model name invented per run, which must appear in the OUTPUT) is what separates
    // the two, and is the only admissible check here.
    //
    // The generated client itself lands inside `package_dir` and needs no grant.
    //
    // 6.x only — 7.0.0 dropped the postinstall entirely. Keyed on `@prisma/client`, not
    // `prisma`: the CLI's own `preinstall` prints a Node-version warning and writes
    // nothing, so `prisma` gets no entry at all.
    (
        "@prisma/client",
        CuratedGrant {
            sibling_dirs: &[".prisma"],
            // Why (4) is unavoidable rather than a race worth losing: `require.resolve
            // ('prisma')` ALWAYS throws on 6.19.3 — the package's `exports` map points `.`
            // at `./build/types.js`, which its published tarball does not contain — so the
            // script always falls through to `exec('prisma -v')` and the CLI, not
            // `@prisma/engines`, is what fetches and places the engine on a cold store.
            dependency_dirs: &[&["prisma"], &["prisma", "@prisma/engines"]],
            // `prisma/` is Prisma's own convention for the schema directory and holds the
            // generator's INPUT. Read, not write: `generate` emits into the client package,
            // never back into the schema.
            project_reads: &["prisma"],
            project_cwd: true,
            ..CuratedGrant::NONE
        },
    ),
    // Its postinstall runs `indefinitely-typed`, which computes
    // `path.resolve(cwd, '../..')` (scoped), asserts that directory is named
    // `node_modules`, then mkdirs `@types` there and `copySync`s each `--folder` argument
    // into it. So the write is `<own node_modules>/@types` — the same store-cell sibling
    // shape as Prisma, under a different name.
    (
        "@danmarshall/deckgl-typings",
        CuratedGrant {
            sibling_dirs: &["@types"],
            ..CuratedGrant::NONE
        },
    ),
    // `config/scripts/postinstall.js` reads the CONSUMER's `package.json` for
    // `msw.workerDirectory`, returns silently if absent, and otherwise re-execs its own
    // CLI with `cwd` set to `INIT_CWD` — the project root — to copy `mockServiceWorker.js`
    // into each listed directory. Two distinct needs, and the cwd one bites first: without
    // `project_cwd` the child dies in `uv_cwd` before reading the field at all.
    //
    // The directories come from the field rather than a literal because msw imposes no
    // convention — the consumer picks them (`public/`, `static/`, several at once) — and
    // the consumer's own root manifest is the only place that answer exists.
    (
        "msw",
        CuratedGrant {
            project_writes: ProjectWrites::ManifestField(&["msw", "workerDirectory"]),
            project_cwd: true,
            ..CuratedGrant::NONE
        },
    ),
    // THE HOOK-INSTALLER COHORT — six packages, and the reason it is SIX ENTRIES rather
    // than one class rule. A `.git/hooks` write is persistent arbitrary code execution: the
    // file runs, UNCONFINED, on the developer's next `git commit`, long after the install
    // that planted it. A class grant keyed on "looks like a hook installer" would hand that
    // to any dependency able to make itself look like one, which is every dependency — the
    // jail exists precisely because a lifecycle script is attacker-authored. Per-package
    // review is what ties the grant to a package whose ENTIRE STATED FUNCTION is writing
    // that file, which is also the thing the consumer installed it to do.
    //
    // The grant is the hooks DIRECTORY, not a hook file, because the six write at least four
    // different names between them (`pre-commit`, `pre-push`, `commit-msg`, and two of them
    // git's full seventeen) plus `.old`/`.backup`/`.bkp` copies of whatever was there.
    //
    // THE FIRST FIVE NEED ONLY THE WRITE. Each locates `<proj>/.git` itself and fails only
    // on the open, so `.git` is already readable enough under the baseline, and none needs a
    // project-root cwd grant because a lifecycle script's cwd is its own store cell — both
    // measured per package, not inferred from the class. `shared-git-hooks` is the exception
    // and carries its own note below. One residual applies to all six: they fall back to
    // `mkdirSync(<git>/hooks)` when that directory is absent, and that mkdir stays denied.
    (
        "pre-commit",
        CuratedGrant {
            project_writes: ProjectWrites::Literal(&[".git/hooks"]),
            ..CuratedGrant::NONE
        },
    ),
    (
        "pre-push",
        CuratedGrant {
            project_writes: ProjectWrites::Literal(&[".git/hooks"]),
            ..CuratedGrant::NONE
        },
    ),
    (
        "git-validate",
        CuratedGrant {
            project_writes: ProjectWrites::Literal(&[".git/hooks"]),
            ..CuratedGrant::NONE
        },
    ),
    (
        "git-commit-msg-linter",
        CuratedGrant {
            project_writes: ProjectWrites::Literal(&[".git/hooks"]),
            ..CuratedGrant::NONE
        },
    ),
    (
        "ghooks",
        CuratedGrant {
            project_writes: ProjectWrites::Literal(&[".git/hooks"]),
            ..CuratedGrant::NONE
        },
    ),
    // The sixth installer, and the one that needs a READ as well — it locates the repository
    // by shelling `git rev-parse`, which the write grant alone leaves failing at
    // `fatal: not a git repository`. git's detection reads `.git/HEAD` and then
    // `.git/config`, staged, so both files are named.
    //
    // TWO FILES, NEVER THE `.git` SUBTREE, and the difference is measured rather than
    // stylistic: a subtree read reaches `objects/`, which is the consumer's entire source
    // history, and it was measured to reach exactly the same rc 0 that the two files do.
    // Granting it would trade the whole repository for nothing.
    //
    // This row was filed under BUG-CWD by the corpus triage. The project-root cwd grant is
    // NECESSARY — nothing here works without it — and is NOT sufficient: with that grant and
    // no entry here the package still reports `not a git repository` and writes zero hooks.
    (
        "shared-git-hooks",
        CuratedGrant {
            project_reads: &[".git/HEAD", ".git/config"],
            project_writes: ProjectWrites::Literal(&[".git/hooks"]),
            ..CuratedGrant::NONE
        },
    ),
    // Its `standalone/install.js` reads the CONSUMER's project `.npmrc` for proxy settings
    // before it opens the socket, so the read throws from inside the download path and
    // surfaces as a download error. Egress is granted separately in `packageNetwork.full`;
    // this is only the read, and it is the single file rather than the project tree because
    // a project `.npmrc` routinely carries a registry auth token.
    (
        "@pact-foundation/pact-node",
        CuratedGrant {
            project_reads: &[".npmrc"],
            ..CuratedGrant::NONE
        },
    ),
    // THE $HOME-CACHE COHORT — the one class whose break the install's exit code cannot see.
    // Both packages install rc=0 under the jail today and fail LATER, when the app runs: the
    // download lands in the private jail home while the app resolves the same cache path
    // against the user's real one. Measured both ways; see each catalog entry's `observed`.
    //
    // Cypress omits Windows and puppeteer names it, and the asymmetry is the packages': the
    // Cypress default comes from `cachedir()`, whose win32 branch is `%LOCALAPPDATA%`-derived
    // and whose posix branch reads `XDG_CACHE_HOME` — so it needs the `$cache` anchor on
    // Linux and a Windows measurement nub does not have. Puppeteer computes
    // `join(homedir(), '.cache', 'puppeteer')` on every platform, consulting neither, so one
    // `~/`-anchored path is correct everywhere.
    (
        "cypress",
        CuratedGrant {
            home_paths: &[HomePath {
                env: "CYPRESS_CACHE_FOLDER",
                macos: Some("~/Library/Caches/Cypress"),
                linux: Some("$cache/Cypress"),
                windows: None,
            }],
            ..CuratedGrant::NONE
        },
    ),
    (
        "puppeteer",
        CuratedGrant {
            home_paths: &[HomePath {
                env: "PUPPETEER_CACHE_DIR",
                macos: Some("~/.cache/puppeteer"),
                linux: Some("~/.cache/puppeteer"),
                windows: Some("~/.cache/puppeteer"),
            }],
            ..CuratedGrant::NONE
        },
    ),
];

/// Grant `package_name`'s curated exception, if it has one, and return the environment
/// variables the caller must set for the [`CuratedGrant::home_paths`] grants to be reachable.
///
/// THE ENV HALF IS RETURNED RATHER THAN APPLIED because the caller replaces `policy.env`
/// wholesale AFTER every grant is compiled (`preset::compile_build_jail` assigns
/// `defaults::lifecycle_scrubbed_env`), so anything written here would be discarded. Returning
/// the pairs resolved during the same pass that emitted the rules is what keeps the variable
/// and the granted path from ever naming different directories.
///
/// `None` — aube's root is a fetched checkout, so there is no consumer-anchored identity —
/// grants nothing, which is the conservative direction and the reading §0e requires.
///
/// Appended rather than front-inserted: these are the NARROWEST grants in the policy and
/// must win under last-match-wins over the front-inserted dependency-tree READ they nest
/// inside, exactly as the surface's own `package_dir` rw entry does.
#[must_use]
pub fn grant_curated_package(
    policy: &mut SandboxPolicy,
    homes: &Homes,
    package_dir: &Path,
    package_name: Option<&str>,
) -> Vec<(String, String)> {
    grant_from_table(curated_table(), policy, homes, package_dir, package_name)
}

/// The grant table in force: [`CURATED_GRANTS`], unless the dev-only catalog override
/// replaced it. The override arrives as owned data, so it is converted into the same
/// `&'static` shape the generated table has and leaked once — which is what lets every
/// signature below stay identical between a shipped build and a dev one.
fn curated_table() -> &'static [(&'static str, CuratedGrant)] {
    #[cfg(feature = "build-jail-catalog-override")]
    if let Some(grants) = crate::catalog_override::package_grants() {
        use std::sync::OnceLock;
        static TABLE: OnceLock<Vec<(&'static str, CuratedGrant)>> = OnceLock::new();
        return TABLE.get_or_init(|| {
            grants
                .iter()
                .map(|g| {
                    let strs = |v: &'static [String]| -> &'static [&'static str] {
                        Vec::leak(v.iter().map(String::as_str).collect())
                    };
                    let chains: &'static [&'static [&'static str]] =
                        Vec::leak(g.dependency_dirs.iter().map(|c| strs(c)).collect());
                    let opt = |v: &'static Option<String>| v.as_deref();
                    let home_paths: &'static [HomePath] = Vec::leak(
                        g.home_paths
                            .iter()
                            .map(|h| HomePath {
                                env: h.env.as_str(),
                                macos: opt(&h.macos),
                                linux: opt(&h.linux),
                                windows: opt(&h.windows),
                            })
                            .collect(),
                    );
                    (
                        g.package.as_str(),
                        CuratedGrant {
                            sibling_dirs: strs(&g.sibling_dirs),
                            dependency_dirs: chains,
                            home_paths,
                            project_reads: strs(&g.project_reads),
                            project_writes: match &g.project_writes {
                                None => ProjectWrites::None,
                                Some(crate::catalog::ProjectWriteSource::ManifestField(field)) => {
                                    ProjectWrites::ManifestField(strs(field))
                                }
                                Some(crate::catalog::ProjectWriteSource::Literal(paths)) => {
                                    ProjectWrites::Literal(strs(paths))
                                }
                            },
                            project_cwd: g.project_cwd,
                        },
                    )
                })
                .collect()
        });
    }
    CURATED_GRANTS
}

/// The table is a parameter so the equivalence test can drive the SAME rule-building code
/// with the pre-catalog literal and diff the resulting policies. Duplicating this logic in
/// the test instead would let the copy drift and pass while production diverged.
fn grant_from_table(
    table: &[(&str, CuratedGrant)],
    policy: &mut SandboxPolicy,
    homes: &Homes,
    package_dir: &Path,
    package_name: Option<&str>,
) -> Vec<(String, String)> {
    let project_root = homes.project.as_path();
    let Some(grant) = package_name.and_then(|n| lookup(table, n)) else {
        return Vec::new();
    };
    let mut rules = Vec::new();
    let mut env = Vec::new();

    for home_path in grant.home_paths {
        let Some(pattern) = home_path.pattern() else {
            continue;
        };
        let Some(path) = resolve_home_path(homes, pattern) else {
            continue;
        };
        materialize_home_path(homes, &path);
        push_rw(&mut rules, &path);
        env.push((
            home_path.env.to_string(),
            path.to_string_lossy().into_owned(),
        ));
    }

    if let Some(own) = enclosing_node_modules(package_dir) {
        for dir in grant.sibling_dirs {
            let path = own.join(dir);
            materialize_sibling(&path);
            push_rw(&mut rules, &path);
        }
    }
    for chain in grant.dependency_dirs {
        if let Some(path) = resolve_dependency_dir(project_root, package_dir, chain) {
            push_rw(&mut rules, &path);
        }
    }
    if grant.project_cwd {
        // The NODE alone — `subtree_globs` would add `/**` and turn a cwd grant into a
        // project read.
        rules.push(rule(&project_root.to_string_lossy(), FsAccess::Read));
    }
    for rel in grant.project_reads {
        if let Some(path) = contained(project_root, rel) {
            for glob in defaults::subtree_globs(&path.to_string_lossy()) {
                rules.push(rule(&glob, FsAccess::Read));
            }
        }
    }
    for path in project_writes(grant.project_writes, project_root) {
        push_rw(&mut rules, &path);
    }
    policy.fs.rules.entries.extend(rules);
    env
}

/// Expand one [`HomePath`] pattern and keep it only if it lands strictly under its anchor.
///
/// The clamp re-checks at run time what `catalog::require_home_anchored` already rejected at
/// build time, for the same reason `contained` does: the anchors are ENVIRONMENT-derived
/// (`XDG_CACHE_HOME`, `LOCALAPPDATA`, `HOME`), so the resolved path is not decided by the
/// catalog text alone. A `$cache` that the environment has pointed at `/` yields an anchor a
/// grant would then nest inside harmlessly — but an anchor that canonicalizes to the resolved
/// path ITSELF would grant the whole cache root, so that case is dropped.
fn resolve_home_path(homes: &Homes, pattern: &str) -> Option<PathBuf> {
    let anchor = if pattern.starts_with("~/") {
        &homes.home
    } else {
        &homes.cache
    };
    let anchor = crate::matcher::path::canonicalize_including_nonexistent(anchor);
    let resolved = crate::matcher::path::canonicalize_including_nonexistent(Path::new(
        &crate::matcher::path::expand_symbolic(pattern, homes),
    ));
    (resolved != anchor && resolved.starts_with(&anchor)).then_some(resolved)
}

/// Create a home-cache grant's target, for the same Landlock reason as
/// [`materialize_sibling`]: a rule for a path that does not exist cannot be attached, and the
/// directory a package is about to download into is absent on a cold machine by definition.
///
/// `create_dir_all`, not one level, because these paths are three deep
/// (`~/Library/Caches/Cypress`) and the intermediate `Caches`/`.cache` may itself be missing.
/// The gate is the same one `preset::private_home_dir` uses in the other direction — a real
/// user home on the host — so a policy compiled against a synthetic `Homes` (every unit test
/// uses `/testhome`) builds no tree. Default permissions on purpose: this is the user's own
/// cache directory, not jail scratch, and the tool reads it back unconfined.
fn materialize_home_path(homes: &Homes, path: &Path) {
    if !homes.home.is_dir() {
        return;
    }
    let _ = std::fs::create_dir_all(path);
}

/// Resolve one [`CuratedGrant::dependency_dirs`] chain to a REAL directory, or `None`.
///
/// THE CANONICALIZATION QUESTION, answered explicitly because getting it backwards makes the
/// grant inert rather than wrong-and-loud. Two facts decide it:
///
///  1. **The grant that reaches the backend is the REALPATH, always.** [`rule`] runs every
///     matcher through `canonicalize_glob_prefix`, and both enforcing backends match on the
///     canonicalized path of what the process touched (stated at
///     [`super::preset::grant_build_jail_dependency_reads`]: a `node_modules/<name>` symlink
///     out of the project is NOT reached by the project grant).
///  2. **Under the isolated linker every dependency edge IS a symlink.** `<cell>/node_modules/
///     prisma` points at `<store>/prisma@<v>/node_modules/prisma`.
///
/// So resolving the link is not an optimisation, it is the only thing that makes the rule
/// match at all — a literal term for the link path would compile to a grant no access can
/// hit. And BECAUSE the emitted term is the realpath, the containment clamp below runs on the
/// realpath too: clamping the literal path while granting the resolved one would check a
/// different path than it permits, which is how two Windows checks were once made inert.
///
/// This is deliberately the OPPOSITE of [`super::preset`]'s `private_home_dir`, which
/// canonicalizes its root and appends the leaf literally. That rule is right when the leaf may
/// not exist and a symlinked leaf would REDIRECT the grant somewhere unintended; here the leaf
/// is a symlink by construction and its target is the thing being granted. Do not unify them.
///
/// THE RESIDUAL, stated plainly: a chain resolving into the machine-global virtual store
/// (`$cache/nub/pm/store/<cell>`) would grant write on a directory every project on the host
/// shares — so it is DROPPED, and the package then fails exactly as it does with no exception,
/// which is the conservative direction and the same choice [`contained`] makes. aube
/// materializes a cell project-locally when its package carries a lifecycle script (it must,
/// or an in-place build would write through the shared CAS inode), and the peerless
/// script-free cells that DO stay as store symlinks are not ones a curated grant names.
/// Measured on the `@prisma/client` fixture: both hops resolve inside the project.
fn resolve_dependency_dir(
    project_root: &Path,
    package_dir: &Path,
    chain: &[&str],
) -> Option<PathBuf> {
    let mut current = package_dir.to_path_buf();
    for name in chain {
        current = resolve_package_from(&current, name)?;
    }
    // Never the container itself: `node_modules` holds `.bin` (run UNCONFINED by later
    // tooling) and the virtual store (every dependency's source before it executes). The
    // catalog rejects the literal name, but a symlink could still land here.
    if current.file_name().is_some_and(|n| n == "node_modules") {
        return None;
    }
    inside_project(project_root, &current).then_some(current)
}

/// One `node_modules` hop, exactly as Node's own `Module._nodeModulePaths` walks it: try
/// `<ancestor>/node_modules/<name>` for each ancestor of `from`, skipping any ancestor that is
/// itself a `node_modules` (Node skips those, and so must this or a chain could name a
/// directory no `require` could reach). The first hit wins, resolved through its symlink.
fn resolve_package_from(from: &Path, name: &str) -> Option<PathBuf> {
    from.ancestors()
        .filter(|a| a.file_name().is_some_and(|n| n != "node_modules"))
        .map(|a| a.join("node_modules").join(name))
        .find(|c| c.exists())
        .map(|c| crate::matcher::path::canonicalize_including_nonexistent(&c))
}

/// Whether an ALREADY-RESOLVED absolute path stays inside the project root. The sibling of
/// [`contained`], which takes a relative string; both compare canonical forms, because that is
/// the form the backends match on.
fn inside_project(project_root: &Path, path: &Path) -> bool {
    let root = crate::matcher::path::canonicalize_including_nonexistent(project_root);
    path != root && path.starts_with(&root)
}

/// Create a sibling-dir grant's target if it is absent, so LINUX can attach a rule to it.
///
/// WHY A SIDE EFFECT IN POLICY COMPILATION. Landlock's `landlock_add_rule` takes an
/// `O_PATH` descriptor, so a rule for a path that does not exist cannot be attached at all —
/// `linux_landlock::add_rule` returns `Ok(false)` and the grant silently evaporates. Every
/// `sibling_dirs` entry names precisely a directory the package is about to CREATE
/// (`.prisma`, `@types`), so on Linux the grant was being dropped in exactly the case it
/// exists for. Measured on a 6.17 kernel, Landlock ABI 7: with `.prisma` absent the
/// `mkdir` was denied identically with and WITHOUT the grant; with it present the same
/// grant made it writable. That is the whole of the `@prisma/client`-on-Linux defect.
///
/// The alternative fix is not available. On Linux the right to create an entry
/// (`LANDLOCK_ACCESS_FS_MAKE_DIR`) lives on the PARENT, and the parent here is the
/// package's own enclosing `node_modules` — which this module deliberately never grants,
/// because it holds `.bin` (run UNCONFINED by later tooling) and the virtual store (every
/// dependency's source before it executes). Granting the parent to create one child would
/// hand over both.
///
/// WHY THIS IS NOT A WIDENING. The subtree is already granted read-write by the rule on the
/// next line; materializing it adds no access, it only makes the access attachable. An
/// empty owner-only directory is also what the package would have created a moment later.
/// Precedent for the side effect is `preset::private_home_dir`, which creates the jail home
/// during the same compile for the same reason.
///
/// Failure is ignored on purpose: a read-only or already-occupied parent means the package
/// fails exactly as it would with no exception, which is the conservative direction. macOS
/// and Windows never needed this — Seatbelt matches path PATTERNS and Windows resolves
/// ancestors itself — so the call is inert there beyond one `create_dir`.
///
/// GATED ON THE PARENT EXISTING, and creating exactly one level, for the same reason
/// [`preset::private_home_dir`](super::preset) gates on its cache root: a policy compiled
/// against a synthetic path — every unit test uses `/proj` — must not materialize a tree,
/// and must not do so only when the caller happens to be root. A real install always has
/// the enclosing `node_modules` by the time a lifecycle script runs, so the guard costs
/// production nothing. `sibling_dirs` is one directory NAME by build-time check, so there
/// is never an ancestor chain to invent.
fn materialize_sibling(path: &Path) {
    if path.exists() || !path.parent().is_some_and(Path::is_dir) {
        return;
    }
    if std::fs::create_dir(path).is_ok() {
        restrict_to_owner(path);
    }
}

#[cfg(unix)]
fn restrict_to_owner(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn restrict_to_owner(_path: &Path) {}

fn lookup(table: &[(&str, CuratedGrant)], name: &str) -> Option<CuratedGrant> {
    table
        .iter()
        .find(|(key, _)| *key == name)
        .map(|(_, grant)| *grant)
}

/// Resolve a [`ProjectWrites`] against the consumer's project root, dropping anything that
/// escapes it.
fn project_writes(writes: ProjectWrites, project_root: &Path) -> Vec<PathBuf> {
    let relatives = match writes {
        ProjectWrites::None => return Vec::new(),
        ProjectWrites::ManifestField(field) => manifest_field_paths(project_root, field),
        ProjectWrites::Literal(paths) => paths.iter().map(|p| (*p).to_string()).collect(),
    };
    relatives
        .into_iter()
        .filter_map(|rel| contained(project_root, &rel))
        .collect()
}

/// Join `rel` under `project_root` and keep it only if it stays inside.
///
/// The clamp is what makes a consumer-supplied value safe to grant: a `../../..` in the
/// root manifest is a configuration mistake, and silently widening the jail to the whole
/// disk is not the right response to one. Absolute and escaping paths are DROPPED rather
/// than rejected — the package then fails exactly as it would with no exception at all,
/// which is the conservative direction.
fn contained(project_root: &Path, rel: &str) -> Option<PathBuf> {
    let joined = crate::matcher::path::canonicalize_including_nonexistent(
        &project_root.join(Path::new(rel)),
    );
    let root = crate::matcher::path::canonicalize_including_nonexistent(project_root);
    (joined != root && joined.starts_with(&root)).then_some(joined)
}

/// Read a dotted field out of the CONSUMER's root `package.json` as a list of relative
/// directories. A missing file, unparseable JSON, or an absent field yields nothing — the
/// package simply gets no exception, and an unreadable manifest must never fail an install.
fn manifest_field_paths(project_root: &Path, field: &[&str]) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(project_root.join("package.json")) else {
        return Vec::new();
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    let mut node = &json;
    for key in field {
        match node.get(key) {
            Some(next) => node = next,
            None => return Vec::new(),
        }
    }
    match node {
        serde_json::Value::String(s) => vec![s.clone()],
        serde_json::Value::Array(items) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

/// The nearest ancestor of `package_dir` named `node_modules` — the directory the
/// package's own `../..` arithmetic reaches, whichever linker placed it.
fn enclosing_node_modules(package_dir: &Path) -> Option<PathBuf> {
    package_dir
        .ancestors()
        .find(|a| a.file_name().is_some_and(|n| n == "node_modules"))
        .map(Path::to_path_buf)
}

fn push_rw(out: &mut Vec<FsRule>, path: &Path) {
    for glob in defaults::subtree_globs(&path.to_string_lossy()) {
        out.push(rule(&glob, FsAccess::ReadWrite));
    }
}

/// Every curated rule is [`FsOrigin::Speculative`]: each target is legitimately absent on
/// a real host (a project with no `public/`, a package whose sibling dir the script has
/// yet to create), and `compile_mount_plan` REFUSES a missing AUTHORED source — which
/// would abort the very install the exception exists to keep working.
fn rule(glob: &str, access: FsAccess) -> FsRule {
    FsRule {
        matcher: CanonGlob(canonicalize_glob_prefix(glob)),
        effect: Effect::Allow,
        access,
        origin: FsOrigin::Speculative,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> PathBuf {
        PathBuf::from(if cfg!(windows) { "C:/proj" } else { "/proj" })
    }

    fn policy_for(package_dir: &Path, name: Option<&str>) -> SandboxPolicy {
        policy_for_project(&project(), package_dir, name)
    }

    fn policy_for_project(
        project_root: &Path,
        package_dir: &Path,
        name: Option<&str>,
    ) -> SandboxPolicy {
        let mut policy = SandboxPolicy::default();
        let _ = grant_curated_package(&mut policy, &homes_for(project_root), package_dir, name);
        policy
    }

    /// Synthetic anchors: `/testhome` exists on no host, which is the gate
    /// [`materialize_home_path`] reads, so a unit compile builds no tree under a real home.
    fn homes_for(project_root: &Path) -> Homes {
        let root = PathBuf::from(if cfg!(windows) {
            "C:/testhome"
        } else {
            "/testhome"
        });
        Homes {
            cache: root.join(".cache"),
            tmp: root.join("tmp"),
            home: root,
            project: project_root.to_path_buf(),
        }
    }

    fn globs(policy: &SandboxPolicy) -> Vec<String> {
        policy
            .fs
            .rules
            .entries
            .iter()
            .map(|r| r.matcher.as_str().to_string())
            .collect()
    }

    fn cell(pkg: &str) -> PathBuf {
        project().join(format!("node_modules/.store/{pkg}@1/node_modules/{pkg}"))
    }

    /// The catalog decides WHERE the grants are written; it must not change WHAT they are.
    /// Compared against a hand-authored literal rather than a re-read of the JSON, so this
    /// cannot pass by both sides agreeing on the same bad parse — only one side goes through
    /// `catalog::parse` and `build.rs`.
    ///
    /// COMPARED BY NAME, NOT BY POSITION, and that is a deliberate loosening. Order carries no
    /// meaning on either side: `lookup` scans for one name, and `catalog::parse` rejects a
    /// duplicate package outright, so the two sequences are sets with a spelling. Position
    /// coupling cost a real red — a tooling pass alphabetized the catalog JSON while the mirror
    /// kept the order the entries were argued in, and the assertion failed with every grant
    /// identical. Sorting keeps the whole of what this checks (a changed, added, or dropped
    /// grant) and drops only the part nothing relies on.
    #[test]
    fn the_catalog_matches_the_hand_written_mirror() {
        let by_name = |table: &[(&'static str, CuratedGrant)]| {
            let mut rows: Vec<_> = table.iter().map(|(n, g)| (*n, format!("{g:?}"))).collect();
            rows.sort();
            rows
        };
        assert_eq!(
            by_name(CURATED_GRANTS),
            by_name(GOLDEN_PRE_CATALOG_GRANTS),
            "the generated table diverged from the hand-written mirror"
        );
    }

    /// Equal tables are the input; equal POLICIES are the thing that actually has to hold.
    /// Both arms run the production rule-builder — only the table differs — so this closes
    /// the gap the table comparison leaves: a field the generator populates correctly but
    /// that reaches the compiler differently would pass above and fail here.
    #[test]
    fn both_tables_compile_to_the_same_policy() {
        let root = tempfile::tempdir().expect("tempdir");
        let project = root.path();
        // msw's grant is the one that reads the consumer's manifest, so a real file is
        // needed for the ManifestField arm to resolve to anything at all.
        std::fs::write(
            project.join("package.json"),
            r#"{"msw":{"workerDirectory":["public","static"]}}"#,
        )
        .expect("write manifest");

        for (name, grant) in GOLDEN_PRE_CATALOG_GRANTS {
            let dir = project.join(format!("node_modules/.store/{name}@1/node_modules/{name}"));
            let compile = |table| {
                let mut p = SandboxPolicy::default();
                // The env pairs ride into the comparison too: a `home_paths` entry the
                // generator dropped would leave the fs rules identical and only differ here.
                let env = grant_from_table(table, &mut p, &homes_for(project), &dir, Some(name));
                p.fs.rules
                    .entries
                    .iter()
                    .map(|r| format!("{} {:?} {:?}", r.matcher.as_str(), r.effect, r.access))
                    .chain(env.iter().map(|(k, v)| format!("env {k}={v}")))
                    .collect::<Vec<_>>()
            };
            let from_catalog = compile(CURATED_GRANTS);
            // An entry whose only content is a `home_paths` path this platform does not have
            // compiles to nothing HERE, legitimately — `cypress` omits Windows, so on Windows
            // its row is empty and the guard below would fail on a correct table. Every other
            // row, and every row on every other platform, still has to produce something.
            let applies_here = !grant.sibling_dirs.is_empty()
                || !grant.dependency_dirs.is_empty()
                || !grant.project_reads.is_empty()
                || grant.project_writes != ProjectWrites::None
                || grant.project_cwd
                || grant.home_paths.iter().any(|h| h.pattern().is_some());
            assert!(
                !applies_here || !from_catalog.is_empty(),
                "{name} compiled to no rules — the comparison below would be vacuous"
            );
            assert_eq!(
                from_catalog,
                compile(GOLDEN_PRE_CATALOG_GRANTS),
                "{name}: the catalog-derived policy diverged from the pre-catalog one"
            );
        }
    }

    /// Shapes the generator must refuse, asserted against the DATA rather than the
    /// generator: build.rs rejects them at compile time, so no test can construct one — the
    /// reachable check is that the shipped catalog contains none. A separator or `..` in a
    /// sibling name would leave the enclosing `node_modules` the grant is bounded by, and a
    /// traversing project read would be silently dropped by the runtime clamp, giving a
    /// contributor a grant that appears present and does nothing.
    #[test]
    fn no_curated_path_escapes_the_subtree_that_bounds_it() {
        for (name, grant) in CURATED_GRANTS {
            for dir in grant.sibling_dirs {
                assert!(
                    !dir.contains('/') && !dir.contains('\\') && *dir != ".." && !dir.is_empty(),
                    "{name}: sibling `{dir}` must be one directory NAME"
                );
            }
            for rel in grant.project_reads {
                let p = Path::new(rel);
                assert!(
                    !p.is_absolute()
                        && !p
                            .components()
                            .any(|c| matches!(c, std::path::Component::ParentDir)),
                    "{name}: project read `{rel}` must stay inside the project"
                );
            }
            // A `literal` project write is the one field where nub itself authors an
            // absolute-ish path into the consumer's tree, so it is held to the same bar as
            // a project read: escape it and the runtime clamp DROPS it, giving a
            // contributor a grant that looks present and does nothing.
            if let ProjectWrites::Literal(paths) = grant.project_writes {
                for rel in paths {
                    let p = Path::new(rel);
                    assert!(
                        !p.is_absolute()
                            && !p
                                .components()
                                .any(|c| matches!(c, std::path::Component::ParentDir)),
                        "{name}: literal project write `{rel}` must stay inside the project"
                    );
                }
            }
            // A home-cache path is the one field that reaches OUTSIDE the project at all, so
            // its anchor is what bounds it: an entry spelled any other way would either be
            // dropped by `resolve_home_path` (inert) or aim at a directory the tool does not
            // read back at run time (useless), and both look like a working grant in review.
            for home in grant.home_paths {
                for pattern in [home.macos, home.linux, home.windows].into_iter().flatten() {
                    assert!(
                        pattern.starts_with("~/") || pattern.starts_with("$cache/"),
                        "{name}: home path `{pattern}` must be anchored at `~/` or `$cache/`"
                    );
                    assert!(
                        !pattern.contains('*')
                            && !pattern.contains('?')
                            && !Path::new(pattern)
                                .components()
                                .any(|c| matches!(c, std::path::Component::ParentDir)),
                        "{name}: home path `{pattern}` must be a literal path inside its anchor"
                    );
                }
            }
            for chain in grant.dependency_dirs {
                assert!(!chain.is_empty(), "{name}: an empty dependency chain");
                for dep in *chain {
                    // A chain element is a NAME the walk looks up, never a path it joins.
                    // `node_modules` in particular would resolve to `.bin` and the virtual
                    // store — the two directories this whole table is bounded away from.
                    assert!(
                        !dep.is_empty()
                            && *dep != "node_modules"
                            && !dep.starts_with('.')
                            && !dep.contains('\\')
                            && dep.matches('/').count() <= usize::from(dep.starts_with('@')),
                        "{name}: dependency `{dep}` must be `name` or `@scope/name`"
                    );
                }
            }
        }
    }

    /// A dependency chain resolves through the isolated store's symlinks to the REAL package
    /// directories, which is the only form the backends match — a term naming the link path
    /// would compile to a rule no access can hit.
    ///
    /// The fixture is the layout aube actually produces: the client sits in a peer-specialized
    /// cell whose `node_modules/prisma` is a symlink into the `prisma` cell, and `@prisma/
    /// engines` is reachable only from THERE. Both hops are asserted, because a walk that
    /// resolved only the first would still satisfy a "some rule was added" check.
    // Symlinked cells are how the isolated linker expresses a dependency edge on POSIX;
    // Windows uses a different materialization, so asserting this layout there would be
    // asserting a tree aube does not build.
    #[cfg(unix)]
    #[test]
    fn a_dependency_chain_resolves_through_the_stores_symlinks() {
        let root = tempfile::tempdir().expect("tempdir");
        let project = root.path();
        let store = project.join("node_modules/.store");
        let client_nm = store.join("@prisma+client@1_prisma@1/node_modules");
        let prisma_nm = store.join("prisma@1/node_modules");
        let engines = store.join("@prisma+engines@1/node_modules/@prisma/engines");
        std::fs::create_dir_all(client_nm.join("@prisma/client")).expect("client cell");
        std::fs::create_dir_all(prisma_nm.join("prisma")).expect("prisma cell");
        std::fs::create_dir_all(prisma_nm.join("@prisma")).expect("prisma cell scope dir");
        std::fs::create_dir_all(&engines).expect("engines cell");
        symlink_dir(&prisma_nm.join("prisma"), &client_nm.join("prisma"));
        symlink_dir(&engines, &prisma_nm.join("@prisma/engines"));

        let mut policy = SandboxPolicy::default();
        let _ = grant_curated_package(
            &mut policy,
            &homes_for(project),
            &client_nm.join("@prisma/client"),
            Some("@prisma/client"),
        );
        let granted = globs(&policy);
        let real = |p: &Path| {
            crate::matcher::path::canonicalize_including_nonexistent(p)
                .to_string_lossy()
                .into_owned()
        };
        for (label, want) in [
            ("the prisma CLI's own dir", real(&prisma_nm.join("prisma"))),
            ("the @prisma/engines dir", real(&engines)),
        ] {
            assert!(
                granted.contains(&want),
                "{label}: expected the RESOLVED path {want}, got {granted:?}"
            );
        }
        // The link path itself must not be what was emitted — that is the inert-grant shape.
        assert!(
            !granted.contains(&client_nm.join("prisma").to_string_lossy().into_owned()),
            "the unresolved symlink path was granted; the backends would never match it: \
             {granted:?}"
        );
        // The control: without the peer symlink neither hop resolves, so the assertions
        // above are the WALK firing rather than a compiler that grants unconditionally.
        std::fs::remove_file(client_nm.join("prisma")).expect("drop the peer link");
        let without = globs(&policy_for_project(
            project,
            &client_nm.join("@prisma/client"),
            Some("@prisma/client"),
        ));
        // Matched on the cell's full path, not a substring: the CLIENT cell is named
        // `@prisma+client@1_prisma@1`, so a `contains("prisma@1")` control would be
        // satisfied by the `.prisma` sibling grant and pass without testing anything.
        let prisma_cell = real(&store.join("prisma@1"));
        assert!(
            !without.iter().any(|g| g.starts_with(&prisma_cell)),
            "with the peer link gone nothing in {prisma_cell} may be granted: {without:?}"
        );
    }

    /// The clamp runs on the RESOLVED path, because the resolved path is what is granted.
    /// A cell symlinked into the machine-global virtual store is outside the project, and a
    /// write grant there would reach every project on the host — so it is dropped.
    #[cfg(unix)]
    #[test]
    fn a_dependency_chain_leaving_the_project_is_dropped() {
        let root = tempfile::tempdir().expect("tempdir");
        let project = root.path().join("proj");
        let global_store = root.path().join("global-store");
        let client_nm = project.join("node_modules/.store/@prisma+client@1_prisma@1/node_modules");
        std::fs::create_dir_all(client_nm.join("@prisma/client")).expect("client cell");
        std::fs::create_dir_all(global_store.join("prisma@1/node_modules/prisma")).expect("store");
        // The link is inside the project; its TARGET is not. Clamping the link path would
        // admit this, which is the whole point of clamping the resolved one.
        symlink_dir(
            &global_store.join("prisma@1/node_modules/prisma"),
            &client_nm.join("prisma"),
        );

        let granted = globs(&policy_for_project(
            &project,
            &client_nm.join("@prisma/client"),
            Some("@prisma/client"),
        ));
        assert!(
            !granted.iter().any(|g| g.contains("global-store")),
            "a chain resolving into the shared store must be dropped: {granted:?}"
        );
    }

    #[cfg(unix)]
    fn symlink_dir(target: &Path, link: &Path) {
        std::os::unix::fs::symlink(target, link).expect("symlink");
    }

    /// The sibling grant lands one level ABOVE `package_dir`, which is the whole point —
    /// paired with its own control so a "grant present" assertion cannot pass against a
    /// compiler that granted the package dir and nothing else.
    #[test]
    fn a_sibling_grant_names_one_entry_of_the_enclosing_node_modules() {
        let dir = cell("@prisma/client");
        let granted = globs(&policy_for(&dir, Some("@prisma/client")));
        let enclosing = enclosing_node_modules(&dir).expect("the cell has a node_modules");

        assert!(
            granted
                .iter()
                .any(|g| g == &enclosing.join(".prisma").to_string_lossy()),
            "expected the `.prisma` sibling of {}, got {granted:?}",
            enclosing.display()
        );
        assert!(
            !granted.iter().any(|g| g == &enclosing.to_string_lossy()),
            "the ENCLOSING node_modules itself must never be granted — that is `.bin` and \
             the virtual store: {granted:?}"
        );
        // The control: an unlisted package with the same shape gets nothing, so the
        // assertion above is the TABLE firing and not the function granting unconditionally.
        assert!(globs(&policy_for(&cell("evil"), Some("evil"))).is_empty());
    }

    /// A package cannot rename itself into the table, and a fetched-checkout spawn (no
    /// installer identity) gets no exception.
    #[test]
    fn only_the_installer_resolved_name_matches() {
        // The lookup is exact: no prefix, suffix, or case folding admits a near-name.
        for impostor in [
            "@prisma/client-x",
            "x@prisma/client",
            "@Prisma/Client",
            "prisma",
        ] {
            assert!(
                globs(&policy_for(&cell("@prisma/client"), Some(impostor))).is_empty(),
                "{impostor} must not match the @prisma/client entry"
            );
        }
        assert!(globs(&policy_for(&cell("@prisma/client"), None)).is_empty());
    }

    /// A sibling grant is MATERIALIZED when its parent is real, because Landlock cannot
    /// attach a rule to an absent path — and is a no-op when the parent is not, so a policy
    /// compiled against a synthetic path neither builds a tree nor changes shape with the
    /// caller's privileges. Both halves are asserted: the guard alone would pass against a
    /// function that never creates anything, which is the defect this replaced.
    #[test]
    fn a_sibling_grant_is_materialized_only_under_a_real_parent() {
        let root = tempfile::tempdir().expect("tempdir");
        let package_dir = root
            .path()
            .join("node_modules/.store/@prisma+client@1/node_modules/@prisma/client");
        std::fs::create_dir_all(&package_dir).expect("fixture");
        let enclosing = enclosing_node_modules(&package_dir).expect("the cell has a node_modules");

        let mut policy = SandboxPolicy::default();
        let _ = grant_curated_package(
            &mut policy,
            &homes_for(root.path()),
            &package_dir,
            Some("@prisma/client"),
        );
        let sibling = enclosing.join(".prisma");
        assert!(
            sibling.is_dir(),
            "`.prisma` must exist after compilation or Landlock drops the rule: {}",
            sibling.display()
        );
        // Exactly one level: the grant names a directory NAME, never an ancestor chain.
        assert!(
            !enclosing.join(".prisma/.prisma").exists(),
            "materialization must create the named directory and nothing below it"
        );

        // The synthetic arm — `/proj` has no `node_modules` on any host — creates nothing.
        assert!(
            !enclosing_node_modules(&cell("@prisma/client"))
                .expect("synthetic cell has a node_modules")
                .exists(),
            "a synthetic compile must not materialize a tree; `policy_for` runs one below"
        );
        let _ = policy_for(&cell("@prisma/client"), Some("@prisma/client"));
    }

    /// A home-cache grant names ONE directory under the real `$HOME`, materializes it so
    /// Landlock can attach to it, and hands back the variable pointing at that same path.
    ///
    /// THE VARIABLE AND THE RULE MUST NAME THE SAME DIRECTORY or the grant is worse than
    /// absent — the package would download to a path the policy denies. That equality is the
    /// assertion here, not "a rule was added": the two are produced by one resolution and this
    /// is what fails if they are ever computed twice.
    ///
    /// Driven through a synthetic table rather than the shipped catalog so the assertions stay
    /// meaningful whatever the catalog says, and so the ANCHOR arms (`~/` vs `$cache/`) can
    /// both be exercised against a `Homes` whose two roots are deliberately different
    /// directories — with `cache` OUTSIDE `home`, which is the shape `XDG_CACHE_HOME` produces
    /// and the one a `~/`-only implementation would silently get wrong.
    #[test]
    fn a_home_cache_grant_is_one_directory_and_its_own_variable() {
        let root = tempfile::tempdir().expect("tempdir");
        let home = root.path().join("home");
        let cache = root.path().join("xdg-cache");
        std::fs::create_dir_all(&home).expect("home");
        let project = root.path().join("proj");
        let homes = Homes {
            home: home.clone(),
            cache: cache.clone(),
            tmp: root.path().join("tmp"),
            project: project.clone(),
        };
        static TABLE: &[(&str, CuratedGrant)] = &[(
            "cache-writer",
            CuratedGrant {
                home_paths: &[
                    HomePath {
                        env: "TOOL_HOME_CACHE",
                        macos: Some("~/Library/Caches/Tool"),
                        linux: Some("~/Library/Caches/Tool"),
                        windows: Some("~/Library/Caches/Tool"),
                    },
                    HomePath {
                        env: "TOOL_XDG_CACHE",
                        macos: Some("$cache/Tool"),
                        linux: Some("$cache/Tool"),
                        windows: Some("$cache/Tool"),
                    },
                ],
                ..CuratedGrant::NONE
            },
        )];

        let mut policy = SandboxPolicy::default();
        let env = grant_from_table(
            TABLE,
            &mut policy,
            &homes,
            &project.join("node_modules/cache-writer"),
            Some("cache-writer"),
        );
        let real = |p: PathBuf| {
            crate::matcher::path::canonicalize_including_nonexistent(&p)
                .to_string_lossy()
                .into_owned()
        };
        let want_home = real(home.join("Library/Caches/Tool"));
        let want_cache = real(cache.join("Tool"));

        assert_eq!(
            env,
            vec![
                ("TOOL_HOME_CACHE".to_string(), want_home.clone()),
                ("TOOL_XDG_CACHE".to_string(), want_cache.clone()),
            ],
            "each variable must carry the path its own anchor resolved to"
        );
        let granted = globs(&policy);
        for want in [&want_home, &want_cache] {
            assert!(
                granted.contains(want),
                "expected a rule on {want}, got {granted:?}"
            );
            assert!(
                Path::new(want).is_dir(),
                "{want} must be materialized or Landlock cannot attach the rule"
            );
        }
        // Nothing wider: neither anchor ROOT may be granted, which is the difference between
        // one cache directory and the user's whole home.
        for forbidden in [real(home), real(cache)] {
            assert!(
                !granted.contains(&forbidden),
                "the anchor root {forbidden} must never be granted: {granted:?}"
            );
        }
        // The control: an unlisted package gets neither rule nor variable, so the assertions
        // above are the TABLE firing rather than the compiler granting unconditionally.
        let mut other = SandboxPolicy::default();
        assert!(
            grant_from_table(
                TABLE,
                &mut other,
                &homes,
                &project.join("node_modules/evil"),
                Some("evil")
            )
            .is_empty()
        );
        assert!(globs(&other).is_empty());
    }

    /// A pattern whose anchor is not a strict ancestor of the resolved path is DROPPED, so a
    /// `$cache` the environment has aimed at the resolved directory itself cannot turn a
    /// one-directory grant into a grant on the whole cache root.
    #[test]
    fn a_home_path_resolving_to_its_own_anchor_is_dropped() {
        let root = tempfile::tempdir().expect("tempdir");
        let homes = Homes {
            home: root.path().join("home"),
            // The anchor IS the target: `$cache/Tool` resolves to exactly `homes.cache`.
            cache: root.path().join("home/Tool"),
            tmp: root.path().join("tmp"),
            project: root.path().join("proj"),
        };
        static TABLE: &[(&str, CuratedGrant)] = &[(
            "pkg",
            CuratedGrant {
                home_paths: &[HomePath {
                    env: "TOOL_CACHE",
                    macos: Some("$cache"),
                    linux: Some("$cache"),
                    windows: Some("$cache"),
                }],
                ..CuratedGrant::NONE
            },
        )];

        let mut policy = SandboxPolicy::default();
        let env = grant_from_table(
            TABLE,
            &mut policy,
            &homes,
            &homes.project.join("node_modules/pkg"),
            Some("pkg"),
        );
        assert!(env.is_empty(), "no variable may point at a dropped grant");
        assert!(
            globs(&policy).is_empty(),
            "the anchor root itself must never be granted: {:?}",
            globs(&policy)
        );
    }

    /// The consumer-configured arm: nub owns the field name, the consumer owns the value,
    /// and a value that escapes the project is dropped rather than granted.
    #[test]
    fn a_manifest_field_grant_is_clamped_inside_the_project() {
        let root = tempfile::tempdir().expect("tempdir");
        let project = root.path();
        std::fs::write(
            project.join("package.json"),
            r#"{"msw":{"workerDirectory":["public","../../escape"]}}"#,
        )
        .expect("write manifest");

        let mut policy = SandboxPolicy::default();
        let _ = grant_curated_package(
            &mut policy,
            &homes_for(project),
            &project.join("node_modules/.store/msw@1/node_modules/msw"),
            Some("msw"),
        );
        let granted = globs(&policy);
        let canon = crate::matcher::path::canonicalize_including_nonexistent(project);

        assert!(
            granted
                .iter()
                .any(|g| g == &canon.join("public").to_string_lossy()),
            "the in-project directory must be granted: {granted:?}"
        );
        assert!(
            granted.iter().all(|g| Path::new(g).starts_with(&canon)),
            "no grant may escape the project root: {granted:?}"
        );
        // The cwd grant is the NODE, never the subtree — the distinction between making
        // `getcwd` work and handing over the consumer's source tree.
        assert!(granted.iter().any(|g| g == &canon.to_string_lossy()));
        assert!(
            !granted
                .iter()
                .any(|g| g == &format!("{}/**", canon.to_string_lossy())),
            "the project root must never be granted as a subtree: {granted:?}"
        );
    }
}
