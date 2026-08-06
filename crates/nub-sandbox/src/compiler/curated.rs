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
    /// The semver range this entry is scoped to, or `None` for every version.
    ///
    /// A package that stops needing a carve-out — the ecosystem's direction, as builds give
    /// way to prebuilt `optionalDependencies` — can have the grant withheld from the versions
    /// that do not use it rather than kept name-wide. `None` is the default and what every
    /// entry measured before this field existed still means. See [`super::version_scope`].
    versions: Option<&'static str>,
    /// THE TERMINAL RUNG: the whole filesystem, read and write, for this package's
    /// lifecycle spawns.
    ///
    /// It is the one field here that does not add a RULE. Every other field appends a
    /// narrow positive grant to an otherwise default-deny fs axis; this one says the fs
    /// axis does not confine at all, which the IR already has a spelling for (no entries,
    /// `default_effect: Allow`) and which `preset::compile_build_jail` applies once, after
    /// every other grant has compiled. A same-entry `sibling_dirs`/`project_reads`/… is
    /// therefore subsumed rather than contradicted, and `home_paths` still does its OTHER
    /// half — setting the package's cache variable, which stays load-bearing because the
    /// ENV axis still redirects `HOME` even when the fs axis does not confine.
    ///
    /// WHY A TERMINAL RUNG AT ALL, since it looks like giving up: without one, a package
    /// that fails under every targeted grant becomes an investigation, and a catalog whose
    /// tail must be root-caused never reaches 100%. With one, that package is a catalog
    /// line — so full compatibility is reachable by construction and narrowing the scope
    /// back down becomes a later optimisation against a green baseline.
    ///
    /// AND IT IS STILL A REDUCTION FROM THE STATUS QUO. A lifecycle script outside nub runs
    /// with the user's complete authority; this withholds the credential-scrubbed
    /// environment and leaves egress to `packageNetwork`, which this field does not touch.
    /// The gate is unchanged and is the whole security model: ONE NAMED package, keyed on
    /// aube's resolver-assigned identity. An uncatalogued package still gets nothing.
    full_disk: bool,
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
    /// `CYPRESS_CACHE_FOLDER` gets nub's default under the jail, and turns confinement off
    /// (`install.buildJail: false` in `nub.jsonc`) if they need theirs honored. That switch is
    /// global — the per-package opt-out this used to name was removed in c5651408f4.
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
        versions: None,
        full_disk: false,
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
/// The lefthook family's config-file read set, shared by its three entries below because the
/// three names are one tool. Named rather than repeated so a later extension cannot be added
/// to two of the three and quietly diverge them from each other.
#[cfg(test)]
static LEFTHOOK_READS: &[&str] = &[
    ".git/HEAD",
    ".git/config",
    "lefthook.yml",
    "lefthook.yaml",
    "lefthook.json",
    "lefthook.toml",
    ".lefthook.yml",
    ".lefthook.yaml",
    ".lefthook.json",
    ".lefthook.toml",
    "lefthook-local.yml",
    "lefthook-local.yaml",
    "lefthook-local.json",
    "lefthook-local.toml",
    ".lefthook-local.yml",
    ".lefthook-local.yaml",
    ".lefthook-local.json",
    ".lefthook-local.toml",
];

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
    // nx's postinstall starts the daemon client, which reads the consumer's `nx.json`
    // eagerly and — from 18 on — creates `<project>/.nx/cache`. Both are the consumer's own
    // project files, which nx was installed to operate on.
    //
    // A WINDOW, not a cutoff, and both edges sit on adjacent majors that were measured:
    // 13.10.6 needs nothing, 14 through 18 need this, 19 onward needs nothing again. It is
    // the only grant in this table that a measurement taken at LATEST could not have
    // produced — every version from 19 up passes confined and ungranted, so the requirement
    // is invisible unless old pins are measured on purpose.
    (
        "nx",
        CuratedGrant {
            versions: Some(">=14.0.0, <19.0.0"),
            project_reads: &["nx.json"],
            project_writes: ProjectWrites::Literal(&["nx.json", ".nx"]),
            project_cwd: true,
            ..CuratedGrant::NONE
        },
    ),
    // THE HOOK-INSTALLER COHORT — nine packages, and the reason it is NINE ENTRIES rather
    // than one class rule. A `.git/hooks` write is persistent arbitrary code execution: the
    // file runs, UNCONFINED, on the developer's next `git commit`, long after the install
    // that planted it. A class grant keyed on "looks like a hook installer" would hand that
    // to any dependency able to make itself look like one, which is every dependency — the
    // jail exists precisely because a lifecycle script is attacker-authored. Per-package
    // review is what ties the grant to a package whose ENTIRE STATED FUNCTION is writing
    // that file, which is also the thing the consumer installed it to do.
    //
    // The grant is the hooks DIRECTORY, not a hook file, because the nine write at least five
    // different names between them (`pre-commit`, `pre-push`, `commit-msg`,
    // `prepare-commit-msg`, and two of them git's full seventeen) plus `.old`/`.backup`/`.bkp`
    // copies of whatever was there.
    //
    // THE FIRST FIVE NEED ONLY THE WRITE. Each locates `<proj>/.git` itself and fails only
    // on the open, so `.git` is already readable enough under the baseline, and none needs a
    // project-root cwd grant because a lifecycle script's cwd is its own store cell — both
    // measured per package, not inferred from the class. `shared-git-hooks` and the three
    // lefthook names are the exceptions and carry their own notes below. One residual applies
    // to all nine: they fall back to `mkdirSync(<git>/hooks)` when that directory is absent,
    // and that mkdir stays denied.
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
    // THE LEFTHOOK FAMILY — one tool under three published names, and the widest grant in the
    // cohort, because a hook write is only the LAST of its three needs. Its postinstall spawns
    // the bundled Go binary as `lefthook install -f`, which (1) shells `git rev-parse` to find
    // the repository, needing the same staged `.git/HEAD` then `.git/config` reads as
    // `shared-git-hooks`; (2) SEARCHES the project root for its own configuration and, finding
    // none, CREATES `lefthook.yml` there rather than giving up; (3) writes the hooks and, on
    // 2.x, a `lefthook.checksum` beside them under `.git/info`. Each masks the next, so the
    // first two fixes both looked complete and were not.
    //
    // THE CONFIG READ IS NOT ABOUT GIT AT ALL, and it is why `@arkweid` stood as an unexplained
    // failure through a rung that granted the whole `.git` SUBTREE: no `.git` grant, however
    // wide, can express a read of `<proj>/lefthook.yml`.
    //
    // Sixteen config names rather than one because the tool honours four extensions across
    // four spellings, all measured. `lefthook-local` and `.lefthook-local` are secondary
    // configs MERGED over the main one, not alternatives to it, so leaving them out would
    // silently drop a user's local hooks instead of failing visibly — the quiet-wrong-answer
    // direction.
    //
    // The `lefthook.yml` WRITE is the create-if-absent fallback, and it is the ordinary
    // first-install shape: the documented order is to install the package and then write the
    // config, so a project that has not written one yet is the common case, not the corner.
    (
        "lefthook",
        CuratedGrant {
            project_reads: LEFTHOOK_READS,
            project_writes: ProjectWrites::Literal(&[".git/hooks", ".git/info", "lefthook.yml"]),
            ..CuratedGrant::NONE
        },
    ),
    // The same tool under its former scoped name, at the same version. A separate entry rather
    // than a shared one because the table is keyed on the installer-resolved package NAME.
    (
        "@evilmartians/lefthook",
        CuratedGrant {
            project_reads: LEFTHOOK_READS,
            project_writes: ProjectWrites::Literal(&[".git/hooks", ".git/info", "lefthook.yml"]),
            ..CuratedGrant::NONE
        },
    ),
    // 0.7.7, under the author's original scope. Same detection and same config search; it
    // writes no checksum, so `.git/info` is withheld — an ablation, not an inference: with the
    // narrower write set it still reaches the jail-off arm's hooks name-for-name.
    (
        "@arkweid/lefthook",
        CuratedGrant {
            project_reads: LEFTHOOK_READS,
            project_writes: ProjectWrites::Literal(&[".git/hooks", "lefthook.yml"]),
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
    // THE TERMINAL TIER, and the row that motivated it. `wordpos`'s postinstall builds its
    // index by writing into `wordnet-db`'s OWN store entry — a directory belonging to
    // another package, reached not by resolution from `wordpos` but by an absolute store
    // path — so no `sibling_dirs`, `dependency_dirs` or project grant has a spelling for it.
    // Measured through the full ladder: fails ungranted, fails with unscoped project
    // read/write/cwd AND egress together, passes with the whole filesystem.
    (
        "wordpos",
        CuratedGrant {
            full_disk: true,
            ..CuratedGrant::NONE
        },
    ),
];

/// What matching a curated entry left for the caller to finish.
///
/// TWO HALVES THE RULE LIST CANNOT CARRY, and both are silent-if-dropped, which is why they
/// are RETURNED rather than written into the policy here. `env` is argued below. `full_disk`
/// is the [`CuratedGrant::full_disk`] verdict: it is a statement about the whole fs axis, not
/// a rule to append, and the axis is not final until every other grant has compiled.
#[derive(Debug, Default)]
pub struct CuratedOutcome {
    /// Variables the caller must set for the [`CuratedGrant::home_paths`] grants to be
    /// reachable, resolved during the same pass that emitted their rules.
    pub env: Vec<(String, String)>,
    /// The matched entry asked for the whole filesystem. The caller relaxes the fs axis.
    pub full_disk: bool,
}

/// Grant `package_name`'s curated exception, if it has one, and return the parts the caller
/// has to apply itself — see [`CuratedOutcome`].
///
/// THE ENV HALF IS RETURNED RATHER THAN APPLIED because the caller replaces `policy.env`
/// wholesale AFTER every grant is compiled (`preset::compile_build_jail` assigns
/// `defaults::lifecycle_scrubbed_env`), so anything written here would be discarded. Returning
/// the pairs resolved during the same pass that emitted the rules is what keeps the variable
/// and the granted path from ever naming different directories.
///
/// `None` — aube's root is a fetched checkout, so there is no consumer-anchored identity —
/// grants nothing, which is the conservative direction and the reading §0e requires.
/// `package_version` is that same identity's resolved version, consulted only by an entry
/// that carries a range.
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
    package_version: Option<&str>,
) -> CuratedOutcome {
    grant_from_table(
        curated_table(),
        policy,
        homes,
        package_dir,
        package_name,
        package_version,
    )
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
                            versions: g.versions.as_deref(),
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
                            full_disk: g.full_disk,
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
    package_version: Option<&str>,
) -> CuratedOutcome {
    let project_root = homes.project.as_path();
    let Some(grant) = package_name.and_then(|n| lookup(table, n, package_version)) else {
        return CuratedOutcome::default();
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
    CuratedOutcome {
        env,
        full_disk: grant.full_disk,
    }
}

/// Lower a v2 grant's capabilities onto the policy's fs axis.
///
/// EVERY CAPABILITY MUST DO SOMETHING, and that is not rhetoric — the likeliest defect in
/// this file is a grant that compiles to no rule at all, which is invisible without a
/// differential test. `projectReads: ["."]` compiled to NOTHING for an entire measurement
/// campaign because [`contained`] requires `joined != root`, and every result taken through
/// it was worthless. The per-capability tests below exist for exactly that reason.
///
/// `Disk` is not lowered here. It is the absence of confinement rather than a rule, so it
/// rides out on [`CuratedOutcome`] and the caller relaxes the axis once, after every other
/// grant has compiled — the same shape v1's `full_disk` uses.
///
/// Takes the grant ALREADY RESOLVED to this OS ([`crate::catalog_v2::Grant::on`]), never the
/// grant itself: a per-OS overlay can narrow any field, and a lowering that read the outer
/// fields would compile the wrong answer on precisely the packages the overlays exist for.
#[cfg(feature = "build-jail-catalog-override")]
pub fn apply_v2_grant(
    policy: &mut SandboxPolicy,
    homes: &Homes,
    package_dir: &Path,
    grant: &crate::catalog_v2::Caps,
) -> V2Outcome {
    use crate::catalog_v2::{Reach, Scope};

    let mut rules = Vec::new();
    let project_root = homes.project.as_path();

    // WRITE first: it implies read at its own scope, so a later read pass must not
    // downgrade what a write already granted.
    if let Reach::Scopes(scopes) = &grant.write {
        for scope in scopes {
            match scope {
                // DECLARED DEPENDENCIES ONLY — deliberately NOT the enclosing `node_modules`.
                //
                // The spec's earlier wording folded "the node_modules I am installed into"
                // into this scope, to carry `@prisma/client` writing `.prisma` beside itself.
                // Granting that directory wholesale would hand over every sibling package's
                // code AND `.bin`, which is executables; the invariant is asserted by
                // `a_sibling_grant_names_one_entry_of_the_enclosing_node_modules`.
                //
                // It is also unnecessary. Measured: under the isolated linker `.prisma` lands
                // inside what `preset::store_entry_write_root` already grants, and under the
                // hoisted linker it is a `project` write. So the clause bought nothing and
                // cost a deliberate bound.
                Scope::Deps => {
                    for dep in declared_dependencies(package_dir) {
                        // NOT `resolve_dependency_dir`: that clamps the result to INSIDE THE
                        // PROJECT, which is right for v1's `dependencyDirs` (authored against
                        // project-local layouts) and fatal here. Under the default global
                        // virtual store a declared dependency resolves to
                        // `~/.cache/nub/pm/store/<dep>@<hash>/…`, outside the project, so the
                        // clamp dropped every path and `deps` compiled to NOTHING — measured
                        // end-to-end: a real jailed script writing to a declared dependency
                        // got EPERM with `write: {deps: true}` in force.
                        if let Some(path) = resolve_declared_dep(homes, package_dir, &dep) {
                            push_rw(&mut rules, &path);
                        }
                    }
                }
                Scope::Project => push_rw(&mut rules, project_root),
                // ⛔ NOT `push_rw(&homes.home)` — THAT SPELLING HANDED OVER `~/.ssh`. `homes.home`
                // is the REAL profile (the throwaway jail home is granted separately and
                // unconditionally by the base profile), so a bare subtree grant covers every
                // credential under it. `enforce_pure_allowlist` then drops the secret DENY floor
                // on all three platforms — Landlock has no deny primitive at any ABI — so the
                // exclusion has to live inside the allow set or it does not exist at all.
                // MEASURED on macOS against a passing negative control: `{"network":true}` gave
                // `REAL_SSH_EPERM`, `write:{userHome}` gave `REAL_SSH_READABLE`.
                Scope::UserHome => rules.extend(defaults::home_minus_secrets_allows(
                    homes,
                    FsAccess::ReadWrite,
                )),
            }
        }
    }

    if let Reach::Scopes(scopes) = &grant.read {
        for scope in scopes {
            match scope {
                Scope::Project => {
                    for glob in defaults::subtree_globs(&project_root.to_string_lossy()) {
                        rules.push(rule(&glob, FsAccess::Read));
                    }
                }
                // Same exclusion as the write arm above, for the same reason.
                Scope::UserHome => {
                    rules.extend(defaults::home_minus_secrets_allows(homes, FsAccess::Read));
                }
                // Unreachable: the parser rejects `read.deps`, because reading declared
                // dependencies is the base profile.
                Scope::Deps => continue,
            }
        }
    }

    policy.fs.rules.entries.extend(rules);
    V2Outcome {
        read_disk: matches!(grant.read, Reach::Disk),
        write_disk: matches!(grant.write, Reach::Disk),
        network: grant.network,
    }
}

/// What a v2 grant asks of the caller that is not a filesystem RULE.
#[cfg(feature = "build-jail-catalog-override")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct V2Outcome {
    /// Read the whole filesystem. Distinct from `write_disk` because it is a far weaker
    /// grant and, on Windows, a far cheaper one.
    pub read_disk: bool,
    /// The fs axis does not confine at all.
    pub write_disk: bool,
    pub network: bool,
}

/// The names in the package's own `dependencies`. Reading the manifest is what makes `deps`
/// mean "what I declared" rather than "what happens to sit beside me" — the latter is the
/// `siblingDirs` shape that v2 retires, and it could name a neighbour the package cannot
/// even `require`.
///
/// `optionalDependencies` are included: a prebuilt-binary package routinely declares its
/// platform builds there, and a grant that missed them would break exactly the native
/// packages this capability exists for. `devDependencies` are NOT — they are absent from a
/// consumer's install by definition.
/// Resolve one DECLARED dependency name to the directory `deps` may write.
///
/// The name is LOOKED UP the way Node would ([`resolve_package_from`]), never joined onto a
/// directory, so the reachable set is exactly what the package can already `require` — that
/// bound is the security argument for `deps` being narrower than "the store", and a separator
/// in a name cannot escape because no name is ever joined.
///
/// WHERE THE RESULT MAY LAND, and why this is not [`resolve_dependency_dir`]'s clamp. That one
/// requires the resolved path to be inside the PROJECT, which under the default global virtual
/// store is never true of a dependency — it lives at `~/.cache/nub/pm/store/<dep>@<hash>/…` —
/// so the v1 clamp silently dropped every path. Here the path is accepted when it is inside the
/// project OR inside a virtual store root, which is the same pair
/// [`super::preset::store_entry_write_root`] recognises for the package's own entry.
///
/// A `node_modules` container itself is still refused: it holds `.bin` (run UNCONFINED by later
/// tooling) and every sibling's source. A symlink could otherwise land the resolution there.
///
/// THE DEPENDENCY'S ENTRY, NOT ITS PACKAGE DIRECTORY, when the two differ. `unrs-resolver`
/// mkdirs `<napi-postinstall entry>/node_modules/unrs-resolver` — a SIBLING of the dependency's
/// package dir, inside that dependency's store entry. Granting the package dir alone left that
/// one level short, which the search caught: `unrs-resolver` settled on `userHome` (cost 7)
/// when `deps` (cost 3) was meant to carry it.
///
/// Widening to the entry stays bounded. It is ONE declared dependency's entry, not the store;
/// the entry's own `node_modules` holds symlinks to ITS dependencies, and a write THROUGH one
/// resolves outside the granted root, where the backends match on the canonical path and refuse
/// it. So the reachable set is still what the package could already `require`, one hop.
#[cfg(feature = "build-jail-catalog-override")]
fn resolve_declared_dep(homes: &Homes, package_dir: &Path, name: &str) -> Option<PathBuf> {
    let resolved = resolve_package_from(package_dir, name)?;
    if resolved.file_name().is_some_and(|n| n == "node_modules") {
        return None;
    }
    // `<store>/<dep>@<hash>/node_modules/<dep>` -> `<store>/<dep>@<hash>`. Only when the result
    // really is a store entry: `enclosing_node_modules(..).parent()` is the entry root, and it
    // is accepted below only if it sits directly under a virtual store.
    let resolved = enclosing_node_modules(&resolved)
        .and_then(|nm| nm.parent().map(Path::to_path_buf))
        .filter(|entry| {
            let parent = entry
                .parent()
                .map(crate::matcher::path::canonicalize_including_nonexistent);
            let is_store_root = |root: PathBuf| {
                parent.as_deref()
                    == Some(
                        crate::matcher::path::canonicalize_including_nonexistent(&root).as_path(),
                    )
            };
            is_store_root(homes.cache.join("nub").join("pm").join("store"))
                || is_store_root(
                    homes
                        .project
                        .join("node_modules")
                        .join(super::preset::PROJECT_VIRTUAL_STORE_LEAF),
                )
        })
        .unwrap_or(resolved);
    let under = |root: PathBuf| {
        let root = crate::matcher::path::canonicalize_including_nonexistent(&root);
        resolved != root && resolved.starts_with(&root)
    };
    let global_store = homes.cache.join("nub").join("pm").join("store");
    let project_store = homes
        .project
        .join("node_modules")
        .join(super::preset::PROJECT_VIRTUAL_STORE_LEAF);
    (inside_project(&homes.project, &resolved) || under(global_store) || under(project_store))
        .then_some(resolved)
}

#[cfg(feature = "build-jail-catalog-override")]
fn declared_dependencies(package_dir: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(package_dir.join("package.json")) else {
        return Vec::new();
    };
    let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for field in ["dependencies", "optionalDependencies"] {
        if let Some(obj) = manifest.get(field).and_then(|v| v.as_object()) {
            names.extend(obj.keys().cloned());
        }
    }
    names.sort();
    names.dedup();
    names
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

/// The exact-name match, then the entry's own version scope. Both halves must hold: the name
/// is the authorship key, and a range is a narrowing its author measured a boundary for.
fn lookup(
    table: &[(&str, CuratedGrant)],
    name: &str,
    version: Option<&str>,
) -> Option<CuratedGrant> {
    table
        .iter()
        .find(|(key, grant)| *key == name && super::version_scope::applies(grant.versions, version))
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
    use std::collections::BTreeMap;

    fn project() -> PathBuf {
        PathBuf::from(if cfg!(windows) { "C:/proj" } else { "/proj" })
    }

    fn policy_for(package_dir: &Path, name: Option<&str>) -> SandboxPolicy {
        policy_for_project(&project(), package_dir, name)
    }

    /// Every shipped entry is unscoped, so the version these helpers pass is arbitrary and
    /// only has to be a version at all — the scope itself is asserted separately, against a
    /// synthetic table, in `a_version_scoped_entry_applies_only_within_its_range`.
    fn policy_for_project(
        project_root: &Path,
        package_dir: &Path,
        name: Option<&str>,
    ) -> SandboxPolicy {
        let mut policy = SandboxPolicy::default();
        let _ = grant_curated_package(
            &mut policy,
            &homes_for(project_root),
            package_dir,
            name,
            Some("1.0.0"),
        );
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
    /// BY NAME, NOT BY POSITION, and the two orders are now genuinely independent: the
    /// catalog's is a function of `apply-matrix.mjs`, which sorts on every ingest so a run's
    /// finishing order cannot show up as a diff, while the mirror's is the order its comments
    /// argue in. Neither order is semantic — `grant_from_table` reaches the table only through
    /// `lookup(table, name)` — so comparing positionally would fail on a difference that
    /// cannot reach a policy. It failed exactly that way once the ingester's first sort
    /// landed. Name UNIQUENESS is asserted first, because a set comparison over duplicate
    /// keys would let a doubled entry hide one that is missing.
    /// CONTAINMENT, NOT EQUALITY — and the constant's own name is the reason.
    /// `GOLDEN_PRE_CATALOG_GRANTS` is the set as it stood BEFORE the JSON migration, and the
    /// property being defended is that moving them did not alter or drop any. The catalog is
    /// now MACHINE-INGESTED by `apply-matrix.mjs`, so it legitimately GROWS — a measured sweep
    /// adds entries this mirror was never meant to predict. Equality therefore turned every
    /// ingest into a failure while proving nothing extra: the migration guarantee is about the
    /// golden entries, not the table's size. A NEW grant is reviewed at its own commit and by
    /// the catalog validator, which is where that review belongs.
    #[test]
    fn the_catalog_preserves_every_pre_catalog_grant() {
        fn by_name<'a>(table: &'a [(&'a str, CuratedGrant)]) -> BTreeMap<&'a str, CuratedGrant> {
            let map: BTreeMap<&'a str, CuratedGrant> = table.iter().copied().collect();
            assert_eq!(map.len(), table.len(), "a package is listed twice");
            map
        }
        let generated = by_name(CURATED_GRANTS);
        for (name, golden) in by_name(GOLDEN_PRE_CATALOG_GRANTS) {
            match generated.get(name) {
                None => {
                    panic!("`{name}` was in the pre-catalog set and the generated table lost it")
                }
                Some(got) => assert_eq!(
                    got, &golden,
                    "`{name}` survived the migration carrying a DIFFERENT grant than it had"
                ),
            }
        }
    }

    /// A version the grant's range admits, so a scoped row is probed inside its own window
    /// instead of at a fixed version that may fall outside it. Scanning rather than parsing
    /// the range: the scan can only ever return a version the SHIPPED predicate accepts,
    /// whereas a second range parser could disagree with the first and hide the divergence
    /// it was written to find. An empty result is itself a finding — a range no ordinary
    /// version satisfies is a grant that silently applies to nobody.
    fn probe_version(range: Option<&str>) -> String {
        let Some(range) = range else {
            return "1.0.0".to_string();
        };
        (0..=200)
            .flat_map(|n| [format!("{n}.0.0"), format!("0.{n}.0")])
            .find(|v| crate::compiler::version_scope::applies(Some(range), Some(v.as_str())))
            .unwrap_or_else(|| panic!("no version satisfies the range `{range}`"))
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
            // A version the grant's OWN range admits, not a fixed one. `nx` is scoped to
            // `>=14.0.0, <19.0.0`, so probing every row at "1.0.0" compiled it to nothing and
            // tripped the vacuity guard below — the guard working, but on a correct table.
            let probe = probe_version(grant.versions);
            let compile = |table| {
                let mut p = SandboxPolicy::default();
                // The env pairs ride into the comparison too: a `home_paths` entry the
                // generator dropped would leave the fs rules identical and only differ here.
                let env = grant_from_table(
                    table,
                    &mut p,
                    &homes_for(project),
                    &dir,
                    Some(name),
                    Some(&probe),
                );
                p.fs.rules
                    .entries
                    .iter()
                    .map(|r| format!("{} {:?} {:?}", r.matcher.as_str(), r.effect, r.access))
                    .chain(env.env.iter().map(|(k, v)| format!("env {k}={v}")))
                    // The full-disk verdict rides in too. It emits no RULE by design, so a
                    // generator that dropped the field would leave both sides identical
                    // here and the comparison would pass on a tier that had gone inert.
                    .chain(env.full_disk.then(|| "full-disk".to_string()))
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
                || grant.full_disk
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
            Some("6.19.3"),
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

    // ── v2: every capability must DO something ────────────────────────────────
    //
    // These are the proof the model is workable, not a formality. The likeliest defect in
    // this file is a capability that compiles to no rule — `projectReads: ["."]` did exactly
    // that for a whole measurement campaign, and nothing failed, so every number taken
    // through it was worthless. Each test below asserts a rule APPEARS with the capability
    // and is ABSENT without it, varying exactly that one capability.

    #[cfg(feature = "build-jail-catalog-override")]
    mod v2 {
        use super::*;

        /// The (glob, access) pairs a grant compiles to. Access matters: a `read` capability
        /// that silently emitted `ReadWrite` would pass any glob-only assertion.
        fn compiled(json: &str, package_dir: &Path) -> (Vec<(String, FsAccess)>, V2Outcome) {
            let catalog = crate::catalog_v2::parse(&format!(
                r#"{{"packages":{{"p":{{"default":{{{json},"notes":"per-capability differential test"}}}}}}}}"#
            ))
            .expect("test grant must parse");
            let mut policy = SandboxPolicy::default();
            let outcome = apply_v2_grant(
                &mut policy,
                &homes_for(&project()),
                package_dir,
                &catalog.packages["p"]
                    .default
                    .on(crate::catalog_v2::Platform::current()),
            );
            let rules = policy
                .fs
                .rules
                .entries
                .iter()
                .map(|r| (r.matcher.as_str().to_string(), r.access))
                .collect();
            (rules, outcome)
        }

        fn touches(rules: &[(String, FsAccess)], root: &Path, access: FsAccess) -> bool {
            let root = root.to_string_lossy().to_string();
            rules
                .iter()
                .any(|(g, a)| *a == access && g.starts_with(&root))
        }

        #[test]
        fn read_project_grants_read_and_only_read() {
            let dir = cell("p");
            let (with, _) = compiled(r#""read":{"project":true}"#, &dir);
            assert!(
                touches(&with, &project(), FsAccess::Read),
                "read.project compiled to no readable rule under {}: {with:?}",
                project().display()
            );
            assert!(
                !touches(&with, &project(), FsAccess::ReadWrite),
                "read.project must not grant write: {with:?}"
            );
            // The control: the same package with no capability gets nothing at all, so the
            // assertion above is this capability firing rather than something ambient.
            let (without, _) = compiled(r#""network":true"#, &dir);
            assert!(
                !touches(&without, &project(), FsAccess::Read),
                "a grant with no fs capability must compile to no fs rule: {without:?}"
            );
        }

        #[test]
        fn write_project_grants_write_where_read_project_did_not() {
            let dir = cell("p");
            let (w, _) = compiled(r#""write":{"project":true}"#, &dir);
            let (r, _) = compiled(r#""read":{"project":true}"#, &dir);
            assert!(
                touches(&w, &project(), FsAccess::ReadWrite),
                "write.project compiled to no writable rule: {w:?}"
            );
            assert!(
                !touches(&r, &project(), FsAccess::ReadWrite),
                "the read-only arm must differ from the write arm, or neither is proven"
            );
        }

        #[test]
        fn user_home_is_the_real_home_not_the_jails_private_one() {
            let dir = cell("p");
            let home = homes_for(&project()).home;
            let (rd, _) = compiled(r#""read":{"userHome":true}"#, &dir);
            let (wr, _) = compiled(r#""write":{"userHome":true}"#, &dir);
            assert!(
                touches(&rd, &home, FsAccess::Read),
                "read.userHome compiled to no rule under {}: {rd:?}",
                home.display()
            );
            assert!(
                touches(&wr, &home, FsAccess::ReadWrite),
                "write.userHome compiled to no writable rule: {wr:?}"
            );
            assert!(
                !touches(&rd, &home, FsAccess::ReadWrite),
                "read.userHome must not grant write: {rd:?}"
            );
        }

        /// The `userHome` scope must not hand over `~/.ssh`, on either axis.
        ///
        /// ⛔ `touches` CANNOT EXPRESS THIS AND USING IT HERE WOULD BE A TEST THAT CANNOT FAIL. It
        /// asks whether a rule's glob STARTS WITH the path, and the defective spelling emitted
        /// `<home>/**`, which does not start with `<home>/.ssh` — so a `!touches(.., ~/.ssh, ..)`
        /// assertion was already green against the bug it is meant to catch. Coverage is the
        /// question, so `covers` is the predicate.
        ///
        /// The positive control is the half that gives the negative one meaning: a real non-secret
        /// child of `$HOME` must still be granted. Without it an emitter that produced NOTHING
        /// would pass, and "grants nothing" is the other way to get this wrong.
        /// ⛔ IT NEEDS A HOME THAT EXISTS, WHICH `homes_for` DELIBERATELY DOES NOT GIVE. The walk
        /// enumerates the real directory, so against the synthetic `/testhome` it emits only the
        /// self-grant and every assertion below goes vacuous. This builds its own tree instead —
        /// which also makes the test hermetic rather than a function of whoever's `$HOME` runs it.
        #[test]
        fn user_home_excludes_the_secret_subtrees_on_both_axes() {
            let raw = std::env::temp_dir().join(format!("nub-v2-home-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&raw);
            std::fs::create_dir_all(&raw).expect("fixture");
            // The emitter canonicalizes, and on macOS the temp dir is a symlink (`/var` ->
            // `/private/var`), so comparing against the raw path fails on a path difference that
            // has nothing to do with the property under test.
            let root = std::fs::canonicalize(&raw).expect("fixture");
            let ssh = root.join(".ssh");
            let ordinary = root.join("work");
            std::fs::create_dir_all(&ssh).expect("fixture");
            std::fs::create_dir_all(&ordinary).expect("fixture");
            std::fs::write(ssh.join("id_ed25519"), b"secret").expect("fixture");
            std::fs::write(ordinary.join("build.log"), b"ordinary").expect("fixture");
            let homes = Homes {
                cache: root.join(".cache"),
                tmp: root.join("tmp"),
                home: root.clone(),
                project: project(),
            };

            let covers = |rules: &[(String, FsAccess)], p: &Path| {
                let p = p.to_string_lossy().to_string();
                rules.iter().any(|(g, _)| {
                    g == &p || g.strip_suffix("/**").is_some_and(|pre| p.starts_with(pre))
                })
            };
            let rules_for = |json: &str| {
                let catalog = crate::catalog_v2::parse(&format!(
                    r#"{{"packages":{{"p":{{"default":{{{json},"notes":"userHome secret exclusion"}}}}}}}}"#
                ))
                .expect("test grant must parse");
                let mut policy = SandboxPolicy::default();
                apply_v2_grant(
                    &mut policy,
                    &homes,
                    &cell("p"),
                    &catalog.packages["p"]
                        .default
                        .on(crate::catalog_v2::Platform::current()),
                );
                policy
                    .fs
                    .rules
                    .entries
                    .iter()
                    .map(|r| (r.matcher.as_str().to_string(), r.access))
                    .collect::<Vec<_>>()
            };

            for (axis, json) in [
                ("read", r#""read":{"userHome":true}"#),
                ("write", r#""write":{"userHome":true}"#),
            ] {
                let rules = rules_for(json);
                // The POSITIVE CONTROL, and it is what stops this from being a test that cannot
                // fail: an emitter producing NOTHING would satisfy the exclusion trivially, and
                // "grants nothing" is an UNDER-grant — the one direction §0 forbids outright.
                assert!(
                    covers(&rules, &ordinary.join("build.log")),
                    "{axis}.userHome granted nothing for an ordinary home file, so it is an \
                     under-grant and the exclusion below proves nothing: {rules:?}"
                );
                assert!(
                    !covers(&rules, &ssh.join("id_ed25519")),
                    "{axis}.userHome covers the ssh key; the secret DENY floor cannot save it \
                     because `enforce_pure_allowlist` drops every deny on all three platforms"
                );
            }
            let _ = std::fs::remove_dir_all(&raw);
        }

        /// `deps` needs a REAL tree: it reads the package's own manifest and resolves each
        /// declared name the way Node would, so a synthetic path grants nothing by design.
        #[test]
        fn write_deps_reaches_a_declared_dependency_and_nothing_else() {
            let tmp = std::env::temp_dir().join(format!("nub-v2-deps-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&tmp);
            let nm = tmp.join("node_modules");
            let me = nm.join("me");
            let declared = nm.join("declared");
            let stranger = nm.join("stranger");
            for d in [&me, &declared, &stranger] {
                std::fs::create_dir_all(d).expect("fixture");
            }
            std::fs::write(
                me.join("package.json"),
                br#"{"name":"me","dependencies":{"declared":"1.0.0"}}"#,
            )
            .expect("fixture");

            let catalog = crate::catalog_v2::parse(
                r#"{"packages":{"p":{"default":{"write":{"deps":true},"notes":"deps differential test"}}}}"#,
            )
            .expect("parses");
            let mut policy = SandboxPolicy::default();
            let homes = Homes {
                project: tmp.clone(),
                ..homes_for(&tmp)
            };
            apply_v2_grant(
                &mut policy,
                &homes,
                &me,
                &catalog.packages["p"]
                    .default
                    .on(crate::catalog_v2::Platform::current()),
            );
            let rules: Vec<String> = policy
                .fs
                .rules
                .entries
                .iter()
                .map(|r| r.matcher.as_str().to_string())
                .collect();

            let reaches = |p: &Path| {
                let s = crate::matcher::path::canonicalize_including_nonexistent(p)
                    .to_string_lossy()
                    .to_string();
                rules.iter().any(|g| g.starts_with(&s))
            };
            assert!(
                reaches(&declared),
                "a DECLARED dependency must be reachable: {rules:?}"
            );
            assert!(
                !reaches(&stranger),
                "an undeclared neighbour must NOT be reachable — that bound is the whole \
                 security argument for `deps`: {rules:?}"
            );
            assert!(
                !rules.iter().any(|g| {
                    let n = crate::matcher::path::canonicalize_including_nonexistent(&nm);
                    g == &n.to_string_lossy()
                }),
                "the enclosing node_modules itself must never be granted (it is `.bin` and \
                 the virtual store): {rules:?}"
            );
            let _ = std::fs::remove_dir_all(&tmp);
        }

        /// THE SHAPE THAT ACTUALLY SHIPS: the package and its dependency both live in the
        /// GLOBAL VIRTUAL STORE, outside the project. The project-local fixture above cannot
        /// catch a containment clamp, and one shipped: `resolve_dependency_dir` requires the
        /// resolved path to be inside the project, so `deps` compiled to NOTHING in every real
        /// install while the project-local test stayed green. Emitting a rule and enforcing one
        /// are different claims, and only this fixture can tell them apart here.
        #[test]
        fn write_deps_reaches_a_dependency_in_the_global_store() {
            let tmp = std::env::temp_dir().join(format!("nub-v2-gvs-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&tmp);
            let store = tmp.join("home/.cache/nub/pm/store");
            let me = store.join("me@1.0.0-aaaa/node_modules/me");
            let dep = store.join("declared@1.0.0-bbbb/node_modules/declared");
            let project = tmp.join("proj");
            for d in [&me, &dep, &project] {
                std::fs::create_dir_all(d).expect("fixture");
            }
            std::fs::write(
                me.join("package.json"),
                br#"{"name":"me","dependencies":{"declared":"1.0.0"}}"#,
            )
            .expect("fixture");
            // The edge Node actually walks: a sibling link inside the package's own entry.
            let link = store.join("me@1.0.0-aaaa/node_modules/declared");
            #[cfg(unix)]
            symlink_dir(&dep, &link);
            #[cfg(not(unix))]
            let _ = &link;

            let catalog = crate::catalog_v2::parse(
                r#"{"packages":{"p":{"default":{"write":{"deps":true},"notes":"global-store deps reach"}}}}"#,
            )
            .expect("parses");
            let mut policy = SandboxPolicy::default();
            let homes = Homes {
                project: project.clone(),
                cache: tmp.join("home/.cache"),
                home: tmp.join("home"),
                tmp: tmp.join("tmp"),
            };
            apply_v2_grant(
                &mut policy,
                &homes,
                &me,
                &catalog.packages["p"]
                    .default
                    .on(crate::catalog_v2::Platform::current()),
            );

            let want = crate::matcher::path::canonicalize_including_nonexistent(&dep)
                .to_string_lossy()
                .to_string();
            let got: Vec<String> = policy
                .fs
                .rules
                .entries
                .iter()
                .map(|r| r.matcher.as_str().to_string())
                .collect();
            // REACHABILITY, and the direction of containment is the point: the grant is the
            // dependency's ENTRY, which is a PREFIX of its package dir. Ask whether some rule's
            // root CONTAINS the dependency, not whether a rule sits exactly at it.
            let covers = |p: &str| {
                got.iter()
                    .any(|g| p.starts_with(g.trim_end_matches("/**").trim_end_matches('/')))
            };
            assert!(
                covers(&want),
                "a declared dependency in the GLOBAL store must be reachable — this is the case \
                 the project clamp silently dropped. wanted a rule covering {want}, got {got:?}"
            );
            // The widening stops AT that entry: an undeclared package's entry sitting beside it
            // in the same store must stay out of reach, or `deps` has become "the store".
            let stranger = crate::matcher::path::canonicalize_including_nonexistent(
                &store.join("stranger@1.0.0-cccc/node_modules/stranger"),
            )
            .to_string_lossy()
            .to_string();
            assert!(
                !covers(&stranger),
                "an UNDECLARED package's store entry must stay unreachable: {got:?}"
            );
            let _ = std::fs::remove_dir_all(&tmp);
        }

        /// `disk` is the ABSENCE of confinement, so it is correct for it to emit no rule —
        /// but then the only thing that can prove it fired is the outcome, which is exactly
        /// why the outcome is asserted rather than the rule set.
        #[test]
        fn disk_and_network_ride_on_the_outcome_not_the_rules() {
            let dir = cell("p");
            let (rules, out) = compiled(r#""write":"disk""#, &dir);
            assert!(out.write_disk, "write:disk must reach the caller");
            assert!(rules.is_empty(), "disk must not also emit rules: {rules:?}");

            let (_, read_only) = compiled(r#""read":"disk""#, &dir);
            assert!(read_only.read_disk, "read:disk must reach the caller");
            assert!(
                !read_only.write_disk,
                "read:disk must NOT relax the write axis — that is the whole point of \
                 splitting them, and on Windows it is the difference between free and costly"
            );

            let (_, net) = compiled(r#""network":true"#, &dir);
            assert!(net.network);
            let (_, no_net) = compiled(r#""read":{"project":true}"#, &dir);
            assert!(
                !no_net.network,
                "egress must not come along with an fs grant"
            );
        }
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

    /// A range on an entry BINDS: inside it the grant compiles, outside it the package is
    /// treated exactly as if it had no entry at all.
    ///
    /// Driven through a synthetic table because every shipped `packageGrants` entry is
    /// deliberately unscoped — asserting against the catalog would test whatever it happens
    /// to contain, and would go vacuous the moment that is nothing. The unscoped control in
    /// the same table is what proves the withholding is the RANGE firing rather than the
    /// version argument disarming the lookup wholesale.
    #[test]
    fn a_version_scoped_entry_applies_only_within_its_range() {
        static TABLE: &[(&str, CuratedGrant)] = &[
            (
                "scoped",
                CuratedGrant {
                    versions: Some("<2.0.0"),
                    sibling_dirs: &["@types"],
                    ..CuratedGrant::NONE
                },
            ),
            (
                "unscoped",
                CuratedGrant {
                    sibling_dirs: &["@types"],
                    ..CuratedGrant::NONE
                },
            ),
        ];
        let granted = |name: &str, version: Option<&str>| {
            let mut policy = SandboxPolicy::default();
            let _ = grant_from_table(
                TABLE,
                &mut policy,
                &homes_for(&project()),
                &cell(name),
                Some(name),
                version,
            );
            !globs(&policy).is_empty()
        };

        assert!(granted("scoped", Some("1.9.9")));
        assert!(!granted("scoped", Some("2.0.0")));
        assert!(!granted("scoped", None));
        assert!(granted("unscoped", Some("2.0.0")));
        assert!(granted("unscoped", None));
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
            Some("6.19.3"),
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
            Some("1.0.0"),
        );
        let real = |p: PathBuf| {
            crate::matcher::path::canonicalize_including_nonexistent(&p)
                .to_string_lossy()
                .into_owned()
        };
        let want_home = real(home.join("Library/Caches/Tool"));
        let want_cache = real(cache.join("Tool"));

        assert_eq!(
            env.env,
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
                Some("evil"),
                Some("1.0.0"),
            )
            .env
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
            Some("1.0.0"),
        );
        assert!(
            env.env.is_empty(),
            "no variable may point at a dropped grant"
        );
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
            Some("2.11.5"),
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
