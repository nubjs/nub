//! The PM-shim library core: everything `nub pm shim` / `nub pm unshim` and the
//! argv0 shim dispatch need, short of argv parsing and the final exec (the CLI
//! owns those). Spec: `wiki/research/package-manager-shims.md` (mechanism +
//! strict-by-default agreement check, both ratified 2026-06-09).
//!
//! Five concerns live here:
//!   1. the shim dir (`~/.nub/shims`) and its hardlink-to-nub entries,
//!   2. the shell-profile PATH block (a Rust port of `install.sh`'s mechanism),
//!   3. the which-style reachability report (Volta's `check_shim_reachable` idea),
//!   4. the PURE decision core — invoked name × pin state × first verb →
//!      run-pinned / refuse / fall-through,
//!   5. the PATH fall-through scan that skips the shim dir (the recursion guard,
//!      same shape as `node::discovery::which_node`'s `nub-node-shim-` skip).
//!
//! The shims live under `~/.nub` (the install surface `install.sh` owns), NOT
//! under `$XDG_CACHE_HOME/nub`: a shim is an installation the user opted into,
//! and wiping a cache must never silently remove entries their PATH points at.

use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::Pm;

// ---------------------------------------------------------------------------
// Shim names and the decision core
// ---------------------------------------------------------------------------

/// The binaries the shim dir intercepts — the PM-shaped names. `nub` itself is
/// NOT shimmed: it is already on PATH via `~/.nub/bin` and a `nub` argv0 is
/// dispatched by the existing `Argv0` machinery, not by this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShimName {
    Npm,
    Npx,
    Pnpm,
    Pnpx,
    Yarn,
    Yarnpkg,
}

impl ShimName {
    /// Parse an argv0 file stem (`npm`, `pnpx`; the CLI already strips `.exe`
    /// via `file_stem`). `None` means "not a PM shim name" — the caller falls
    /// back to the existing nub/nubx/node dispatch.
    pub fn parse(stem: &str) -> Option<Self> {
        Some(match stem {
            "npm" => Self::Npm,
            "npx" => Self::Npx,
            "pnpm" => Self::Pnpm,
            "pnpx" => Self::Pnpx,
            "yarn" => Self::Yarn,
            "yarnpkg" => Self::Yarnpkg,
            _ => return None,
        })
    }

    /// The binary name as invoked — what the PATH fall-through searches for.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Npm => "npm",
            Self::Npx => "npx",
            Self::Pnpm => "pnpm",
            Self::Pnpx => "pnpx",
            Self::Yarn => "yarn",
            Self::Yarnpkg => "yarnpkg",
        }
    }

    /// The package this name belongs to, at the NAME level (yarn never resolves
    /// to [`Pm::YarnBerry`] here — Berry is a property of the *pin*, not of the
    /// invoked binary).
    pub fn pm(self) -> Pm {
        match self {
            Self::Npm | Self::Npx => Pm::Npm,
            Self::Pnpm | Self::Pnpx => Pm::Pnpm,
            Self::Yarn | Self::Yarnpkg => Pm::Yarn,
        }
    }

    /// The entry to pick from the package's `bin` MAP: `npx` lives in the npm
    /// package, `pnpx` in pnpm, `yarnpkg` in yarn — see [`sibling_bin`].
    pub fn bin_entry(self) -> &'static str {
        self.as_str()
    }

    /// `npx` / `pnpx` are runner binaries — transparent ALWAYS, whatever the
    /// first verb (corepack's allowlist shape; ratified 2026-06-09).
    pub fn always_transparent(self) -> bool {
        matches!(self, Self::Npx | Self::Pnpx)
    }
}

/// Verbs that bypass the strict agreement check for npm|pnpm|yarn alike —
/// `npm create vite` in a pnpm repo must work (corepack's allowlist shape).
/// Matched against the FIRST argv token verbatim; a flag before the verb
/// (`npm --yes create x`) is not recognized — strictness errs toward refusing.
pub const TRANSPARENT_VERBS: [&str; 4] = ["init", "create", "dlx", "exec"];

/// Whether this shim invocation was spawned by an already-running package
/// manager (a nested call), versus typed by the user at a shell (a top-level
/// call). Decides whether a NAME-MISMATCH is a hard refusal (top-level — the
/// user can fix their command) or a silent fall-through (nested — a lifecycle
/// script three layers down invoked a different PM, and refusing would break an
/// install the user never directly issued).
///
/// The signal is `npm_config_user_agent` / `npm_execpath` in the environment:
/// every package manager sets `npm_config_user_agent` for the children it
/// spawns (it is THE ecosystem-standard "a PM is running above me" marker — what
/// `ni` / `package-manager-detector` read), so its presence means we were spawned
/// by a running PM, not invoked from a bare shell. This is brand-safe: it is an
/// `npm_*` variable the npm ecosystem owns, never a `NUB_*` sentinel of our own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nesting {
    /// No `npm_config_user_agent`/`npm_execpath` in the environment — the user
    /// typed `pnpm`/`npm`/`yarn` at a shell. Full strict refusal applies.
    TopLevel,
    /// A running PM set `npm_config_user_agent`/`npm_execpath` — we are a child
    /// of an install in progress. A name mismatch falls through instead of
    /// refusing, so a `pnpm` postinstall that shells out to `npm` is not broken.
    Nested,
}

impl Nesting {
    /// Read the nesting context from an env-var lookup. `getenv("npm_config_user_agent")`
    /// or `getenv("npm_execpath")` being present (and non-empty) ⇒ [`Nesting::Nested`].
    /// Pure over the lookup so the decision core stays testable without touching
    /// the process environment.
    pub fn from_env(mut getenv: impl FnMut(&str) -> Option<String>) -> Self {
        let mut present = |k: &str| getenv(k).is_some_and(|v| !v.is_empty());
        if present("npm_config_user_agent") || present("npm_execpath") {
            Self::Nested
        } else {
            Self::TopLevel
        }
    }
}

/// Where a project's PM pin came from — named in the refusal message so the
/// user knows which file to look at. The caller derives this when it resolves
/// the pin (`resolve::committed_yarn_path` → [`PinProvenance::YarnPath`]; else
/// which manifest field carried it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinProvenance {
    /// `package.json#packageManager` (the corepack field).
    PackageManagerField,
    /// `package.json#devEngines.packageManager`.
    DevEngines,
    /// `.yarnrc.yml`'s `yarnPath:` — a committed Berry release.
    YarnPath,
}

impl fmt::Display for PinProvenance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::PackageManagerField => "package.json#packageManager",
            Self::DevEngines => "package.json#devEngines.packageManager",
            Self::YarnPath => ".yarnrc.yml#yarnPath",
        })
    }
}

/// The project's pin state, as the caller resolved it (workspace root —
/// `resolve::resolve_target`). A committed `yarnPath` project is
/// `Pinned { pm: YarnBerry, provenance: YarnPath }`. A pin naming an
/// out-of-scope manager (bun) resolves as unpinned at the resolve layer
/// (warned there), so it is `Unpinned` here and falls through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinState {
    Unpinned,
    Pinned {
        pm: Pm,
        provenance: PinProvenance,
    },
    /// The project pins nub itself (`packageManager: "nub@…"` /
    /// `devEngines.packageManager` naming nub) — nub IS the active manager
    /// here. A foreign-PM shim invocation must refuse and redirect to `nub`,
    /// never provision a competing PM (the resolve layer can't classify `nub`
    /// as a [`Pm`] — nub doesn't provision itself — so the shim carries this
    /// state directly).
    NubPinned {
        provenance: PinProvenance,
    },
}

/// What the shim does with an invocation — decision 1's matrix, encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShimDecision {
    /// Name match: provision the pinned PM (cache-first) and exec `bin_entry`
    /// from its bin map under the PROJECT's Node. Same-PM version drift never
    /// refuses — running the pinned version IS the shim's job. `pm` is the
    /// pinned value, so `YarnBerry` tells the caller to exec the committed
    /// `yarnPath` release instead of provisioning.
    RunPinned { pm: Pm, bin_entry: &'static str },
    /// Strict by default: a non-transparent invocation of the WRONG PM in a
    /// pinned project exits nonzero, naming the pinned PM + its provenance and
    /// the command to paste (message text is the CLI's).
    Refuse {
        pinned_pm: Pm,
        provenance: PinProvenance,
    },
    /// The project pins nub itself: a foreign-PM shim invocation exits nonzero,
    /// telling the user to run `nub` instead (`nub install`, …). Distinct from
    /// [`Self::Refuse`] because the redirect is to nub, not to a sibling PM, and
    /// nub never provisions a competing manager just to have it reject the pin.
    RefuseNubPinned { provenance: PinProvenance },
    /// Transparent or unpinned: search PATH past the shim dir for the invoked
    /// binary ([`find_system_pm`]) and exec it; on a PATH miss provision a
    /// dynamic default of the INVOKED PM (`lockfile_version::infer`, else the
    /// registry's `latest`), announced on stderr. NEVER the pinned PM, never a
    /// baked version, and the shim never writes a pin.
    FallThrough { invoked: ShimName },
}

/// Classify one shim invocation. Pure — the unit-tested heart of the shim.
///
/// The matrix (ratified 2026-06-09, do not re-litigate):
///   - unpinned → fall through, always (transparency is irrelevant);
///   - pinned + name match → run pinned, always (a transparent verb in a
///     MATCHED project still runs the pin — `pnpm dlx` in a pnpm repo uses the
///     pinned pnpm). Nesting is irrelevant here: a same-PM nested call still
///     runs the pin, exactly as a top-level one does;
///   - pinned + name mismatch → refuse, UNLESS transparent (`npx`/`pnpx`
///     binaries, or a first-token verb in [`TRANSPARENT_VERBS`]) → fall
///     through to the system PM on PATH, NOT the pinned PM;
///   - pinned + name mismatch + [`Nesting::Nested`] → fall through, NOT refuse:
///     a running PM spawned this (a lifecycle script invoking a different PM),
///     so refusing would break an install the user never directly typed. Only a
///     TOP-LEVEL mismatch — one the user can actually fix — stays strict.
pub fn decide(
    invoked: ShimName,
    pin: &PinState,
    first_arg: Option<&str>,
    nesting: Nesting,
) -> ShimDecision {
    // A nub-pinned project: nub is the manager. Foreign-PM shims refuse and
    // redirect to nub, with the same transparent/nested escapes a sibling-PM
    // mismatch gets (a `npm create vite` or a nested lifecycle call still flows
    // to the system PM rather than breaking).
    if let PinState::NubPinned { provenance } = pin {
        let transparent = invoked.always_transparent()
            || first_arg.is_some_and(|verb| TRANSPARENT_VERBS.contains(&verb));
        return if transparent || nesting == Nesting::Nested {
            ShimDecision::FallThrough { invoked }
        } else {
            ShimDecision::RefuseNubPinned {
                provenance: *provenance,
            }
        };
    }
    let PinState::Pinned {
        pm: pinned,
        provenance,
    } = pin
    else {
        return ShimDecision::FallThrough { invoked };
    };
    if same_pm_name(invoked.pm(), *pinned) {
        return ShimDecision::RunPinned {
            pm: *pinned,
            bin_entry: invoked.bin_entry(),
        };
    }
    // A name mismatch: transparent verbs always escape; a NESTED invocation
    // (spawned by a running PM, e.g. a pnpm postinstall calling `npm`) escapes
    // too — refusing there breaks an install the user issued at one layer up,
    // never typed `npm` for, and can't fix from the failing command. Only a
    // top-level, non-transparent mismatch the user can correct stays strict.
    let transparent = invoked.always_transparent()
        || first_arg.is_some_and(|verb| TRANSPARENT_VERBS.contains(&verb));
    if transparent || nesting == Nesting::Nested {
        ShimDecision::FallThrough { invoked }
    } else {
        ShimDecision::Refuse {
            pinned_pm: *pinned,
            provenance: *provenance,
        }
    }
}

/// NAME-level PM equality: classic yarn and Berry are both "yarn" — the
/// agreement check keys on the printed name, never the classic/Berry split.
fn same_pm_name(a: Pm, b: Pm) -> bool {
    let yarn = |pm| matches!(pm, Pm::Yarn | Pm::YarnBerry);
    a == b || (yarn(a) && yarn(b))
}

/// Verbs that exist on `npm` but have NO equivalent of the same name on the
/// target PM — suggesting `<pm> <verb>` for these would propose a command that
/// errors. The canonical case is `ci`: npm-only (pnpm uses `install
/// --frozen-lockfile`, yarn `install --immutable`), so `pnpm ci` / `yarn ci`
/// are not real commands. We deliberately keep this list to the verbs we are
/// CERTAIN diverge rather than trying to enumerate every PM's full surface — a
/// false "unsupported" only costs a slightly less specific suggestion, while a
/// false "supported" reintroduces the bug (a redirect to a command that fails).
fn verb_absent_on(pinned: Pm, verb: &str) -> bool {
    match pinned {
        // pnpm and yarn have no `ci`; npm and (defensively) anything else do.
        Pm::Pnpm | Pm::Yarn | Pm::YarnBerry => verb == "ci",
        Pm::Npm => false,
    }
}

/// What the refusal should tell the user to run instead. The redirect must never
/// invent a verb the pinned PM lacks (the bug: a blind `<pm> <args…>` swap that
/// suggested `pnpm ci`). The rule:
///   - empty argv, or a first token that is a flag, not a verb (`npm --version`)
///     → `None`: there is no command to honestly redirect, and echoing argv back
///     produced nonsense advice ("run instead: pnpm --version") for a read-only
///     invocation — the caller drops its "run instead" line entirely;
///   - a first verb the pinned PM also implements → the project PM with the SAME
///     verb and its remaining args (`pnpm install react`);
///   - a first verb the pinned PM does NOT implement → just `use <pinned-pm>`,
///     with no synthesized verb mapping (`pnpm ci` → "use pnpm").
///
/// Returns the redirect TEXT (without the surrounding message framing) so the
/// CLI owns the prose and this stays a pure, unit-testable decision.
pub fn safe_redirect(pinned: Pm, args: &[String]) -> Option<String> {
    let verb = args.first().filter(|a| !a.starts_with('-'))?;
    Some(if verb_absent_on(pinned, verb) {
        // No honest same-verb suggestion exists — don't fabricate one.
        format!("use {pinned}")
    } else {
        format!("{pinned} {}", args.join(" "))
    })
}

// ---------------------------------------------------------------------------
// Shim dir management
// ---------------------------------------------------------------------------

/// The PM names the shim dir intercepts — also the set [`install_shims`] links
/// and the reachability-check set. `nub` itself is deliberately NOT shimmed:
/// `install.sh` already puts `~/.nub/bin/nub` on PATH, and a `nub` argv0 is
/// dispatched by [`Argv0`](../../../nub_cli/cli/enum.Argv0.html) regardless of
/// the shim dir, so a `~/.nub/shims/nub` hardlink intercepts nothing.
pub const PM_SHIM_NAMES: [&str; 6] = ["npm", "npx", "pnpm", "pnpx", "yarn", "yarnpkg"];

/// Everything [`install_shims`] links — the six PM names. Kept as a named alias
/// of [`PM_SHIM_NAMES`] so the install/report path reads as "the full shim set"
/// while the reachability path reads as "the PM set"; they are now identical.
pub const SHIM_NAMES: [&str; 6] = PM_SHIM_NAMES;

/// `~/.nub/shims` — sibling of `install.sh`'s `~/.nub/bin`.
pub fn shim_dir() -> Result<PathBuf> {
    dirs_next::home_dir()
        .map(|h| h.join(".nub").join("shims"))
        .context("cannot locate the home directory for ~/.nub/shims")
}

/// What happened to one shim entry during [`install_shims`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShimAction {
    /// The name did not exist — a fresh link was created.
    Created,
    /// An existing entry pointed at OTHER bytes (a pre-upgrade nub, a stray
    /// file) and was replaced — the post-`nub upgrade` re-link story.
    Relinked,
    /// Already a hardlink of these bytes (same device + inode) — left in place.
    Current,
}

/// One installed shim entry, for the CLI's "created/refreshed" report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledShim {
    pub name: &'static str,
    pub path: PathBuf,
    pub action: ShimAction,
    /// The hardlink failed (shim dir on a different filesystem than the
    /// binary) and a full copy landed instead — costs one binary of disk.
    pub copied: bool,
}

/// How long a shim-dir lockfile may sit before it's deemed stale and stolen —
/// the holder crashed or was killed between create and cleanup. Install/remove
/// is a handful of `link`/`unlink` syscalls (sub-millisecond); 30s is orders of
/// magnitude past the longest honest hold, so stealing it can only happen after
/// a real abandonment, never mid-operation.
const LOCK_STALE: std::time::Duration = std::time::Duration::from_secs(30);

/// A best-effort advisory lock on the shim dir, held for the duration of an
/// install/remove. Created `O_EXCL` so exactly one creator wins the race; a
/// lockfile older than [`LOCK_STALE`] is stolen (the previous holder died). The
/// guard removes the file on drop. Lives at `<dir>.lock` in the parent
/// (`~/.nub`, created first) so it also covers the shim dir's own
/// `create_dir_all` / `remove_dir_all`.
///
/// What it closes: `nub pm shim`'s remove-then-link is NOT atomic per entry
/// ([`install_shims_into`]'s documented ENOENT window). A re-link (e.g. the
/// `nub upgrade` post-swap re-link) racing a parallel `pnpm -r` shim storm —
/// many children exec'ing the same shim names while one process rewrites them —
/// could otherwise catch an entry mid-swap. Serializing every install/remove on
/// this lock means at most one process is rewriting the dir at a time, so a
/// concurrent re-link can't interleave with another's remove-then-link. It does
/// NOT make a single entry's swap atomic against a *reader* exec'ing that exact
/// name — that window (microseconds, documented on [`install_shims_into`])
/// stands; the lock only serializes WRITERS against each other.
struct ShimLock {
    path: PathBuf,
}

impl ShimLock {
    /// Acquire the lock for `dir`. Best-effort: if every attempt fails for a
    /// reason other than "already held" (e.g. a read-only parent), proceed
    /// UNLOCKED rather than blocking a legitimate install — the lock is a
    /// race-narrowing convenience, not a correctness gate. `None` = proceeding
    /// without a held lock; `Some(guard)` = held, released on drop.
    fn acquire(dir: &Path) -> Option<Self> {
        let parent = dir.parent()?;
        // The parent (`~/.nub`) must exist to hold the lockfile; this also
        // pre-creates it for the install path's own create_dir_all.
        std::fs::create_dir_all(parent).ok()?;
        let path = parent.join(format!(
            "{}.lock",
            dir.file_name().unwrap_or_default().to_string_lossy()
        ));
        // Spin for up to LOCK_STALE: a live holder finishes in sub-ms, so a
        // wait this long means the holder is gone — steal and retry once.
        let deadline = std::time::Instant::now() + LOCK_STALE;
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true) // O_EXCL: exactly one creator wins
                .open(&path)
            {
                Ok(_) => return Some(Self { path }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    // Steal a stale lock (holder died mid-operation). Re-read
                    // the mtime each iteration so a freshly-touched lock isn't
                    // stolen out from under a live holder.
                    let stale = std::fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .map(|t| t.elapsed().unwrap_or_default() > LOCK_STALE)
                        .unwrap_or(true);
                    if stale {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    if std::time::Instant::now() > deadline {
                        // Waited a full stale-window for a lock that keeps
                        // looking fresh — proceed unlocked rather than hang.
                        return None;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                // Any other error (read-only parent, etc.): give up on locking
                // and let the caller proceed — the lock is best-effort.
                Err(_) => return None,
            }
        }
    }
}

impl Drop for ShimLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Populate [`shim_dir`] with hardlinks to `nub_binary` under every
/// [`SHIM_NAMES`] entry. Idempotent — re-running re-links, which is also how
/// shims are refreshed after `nub upgrade` (the upgrade writes a new inode, so
/// the old links keep the old bytes until re-linked).
pub fn install_shims(nub_binary: &Path) -> Result<Vec<InstalledShim>> {
    install_shims_into(&shim_dir()?, nub_binary)
}

/// [`install_shims`] with an explicit target dir (the testable body).
///
/// Replacement is remove-then-link, NOT atomic: between the two syscalls a
/// concurrent exec of that shim name fails ENOENT. The window is microseconds,
/// re-linking is a rare explicit action, and a rename-based swap would need a
/// staging link with its own failure modes — documented risk over doubled
/// complexity. The same-inode check skips the window entirely when the entry
/// is already current. The Windows naming/dispatch of this body runs on the
/// windows-latest CI leg (nub-cli's `pm_shim_windows.rs`); what that leg does
/// NOT cover: replacing a RUNNING shim `.exe` fails (the OS locks executing
/// images) — still unverified, no test swaps a live image.
pub fn install_shims_into(dir: &Path, nub_binary: &Path) -> Result<Vec<InstalledShim>> {
    // Serialize concurrent writers (a re-link racing a `pnpm -r` shim storm) on
    // a best-effort advisory lock — see [`ShimLock`]. Held until this returns.
    let _lock = ShimLock::acquire(dir);
    std::fs::create_dir_all(dir).with_context(|| format!("creating shim dir {}", dir.display()))?;
    let mut report = Vec::with_capacity(SHIM_NAMES.len());
    for name in SHIM_NAMES {
        let target = dir.join(shim_file_name(name));
        // Already a hardlink of these exact bytes (same device + inode) — leave
        // it in place and skip the remove-then-link window entirely. This is the
        // common idempotent re-run / `nub upgrade` no-op-entry case.
        if same_file(nub_binary, &target) {
            report.push(InstalledShim {
                name,
                path: target,
                action: ShimAction::Current,
                copied: false,
            });
            continue;
        }
        let existed = target.symlink_metadata().is_ok();
        if existed {
            std::fs::remove_file(&target)
                .with_context(|| format!("removing stale shim {}", target.display()))?;
        }
        // Hardlink first (zero disk, signature travels); copy across
        // filesystems. `fs::copy` carries the source's permission bits on
        // Unix, so a copied shim stays executable.
        let copied = match std::fs::hard_link(nub_binary, &target) {
            Ok(()) => false,
            Err(_) => {
                std::fs::copy(nub_binary, &target)
                    .map(|_| ())
                    .with_context(|| {
                        format!(
                            "linking {} -> {} (tried hard_link, then copy)",
                            target.display(),
                            nub_binary.display()
                        )
                    })?;
                true
            }
        };
        report.push(InstalledShim {
            name,
            path: target,
            action: if existed {
                ShimAction::Relinked
            } else {
                ShimAction::Created
            },
            copied,
        });
    }
    Ok(report)
}

/// Delete the shim dir. Returns whether it existed (false = already clean).
/// Removing the dir that holds the RUNNING nub hardlink is fine on Unix — the
/// inode outlives its last name for as long as the process runs.
pub fn remove_shims() -> Result<bool> {
    remove_shims_from(&shim_dir()?)
}

/// [`remove_shims`] with an explicit dir (the testable body).
pub fn remove_shims_from(dir: &Path) -> Result<bool> {
    // Same writer-serializing lock as [`install_shims_into`]: an `unshim`
    // racing a re-link must not remove the dir mid-link. Held until return.
    let _lock = ShimLock::acquire(dir);
    match std::fs::remove_dir_all(dir) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("removing shim dir {}", dir.display())),
    }
}

/// `pnpm` on Unix, `pnpm.exe` on Windows (same shape as the node shim's A-WIN2
/// hardlink in `node::spawn::setup_path_shim`).
fn shim_file_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

/// Same underlying file? Unix compares device + inode (so an already-linked
/// shim is recognized whatever path it's reached by); Windows falls back to
/// canonical-path equality (good enough for the self-link guard).
fn same_file(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (a.metadata(), b.metadata()) {
            (Ok(ma), Ok(mb)) => ma.dev() == mb.dev() && ma.ino() == mb.ino(),
            _ => false,
        }
    }
    #[cfg(windows)]
    {
        match (a.canonicalize(), b.canonicalize()) {
            (Ok(ca), Ok(cb)) => ca == cb,
            _ => false,
        }
    }
}

/// Whether `dir` sits on a filesystem mounted `noexec` — there the shims
/// install cleanly (linking is allowed) and then EVERY invocation dies with a
/// bare "Permission denied" (the kernel refuses the exec). The CLI probes this
/// once after [`install_shims`] and warns naming the dir and the fix, instead
/// of letting every later `pnpm`/`npm` call fail cryptically.
///
/// Mechanism: the filesystem-stat mount-flag word — cheap (one syscall) and
/// direct, where the plausible alternatives both fail: access(2)/`X_OK` PASSES
/// on a noexec mount (it checks permission bits, not mount flags), and actually
/// exec'ing a probe binary costs a spawn per install. The flag word differs
/// per OS — note the same bit value means different things on each:
///   - **macOS**: statfs(2)'s `f_flags` (u32) carries the BSD `MNT_*` mount
///     flags — `MNT_NOEXEC` is 0x4. (macOS statvfs exposes only
///     RDONLY/NOSUID, so statfs is the only road here.)
///   - **Linux**: statvfs(3)'s `f_flag` (`c_ulong`) carries the `ST_*` flags —
///     `ST_NOEXEC` is 0x8, a DIFFERENT bit than the BSD value. statvfs is the
///     glibc/musl-portable surface (libc's raw `statfs` struct doesn't expose
///     `f_flags` on every Linux target — aarch64-gnu lacks it — and libc
///     doesn't export `ST_NOEXEC` for gnu/musl, so the POSIX constant is
///     named locally). Both libcs populate it from the kernel's mount flags.
///
/// Best-effort by contract: any probe failure (unstattable dir, interior NUL,
/// other unixes, Windows) returns `false` — a missed warning is acceptable,
/// a false alarm or a failed install over the probe is not.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn dir_is_noexec(dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt as _;
    let Ok(c_path) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else {
        return false;
    };
    #[cfg(target_os = "macos")]
    {
        let mut st: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(c_path.as_ptr(), &mut st) } != 0 {
            return false;
        }
        st.f_flags & (libc::MNT_NOEXEC as u32) != 0
    }
    #[cfg(target_os = "linux")]
    {
        // POSIX statvfs: ST_NOEXEC in f_flag. Not exported by libc for gnu/musl.
        const ST_NOEXEC: libc::c_ulong = 0x8;
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c_path.as_ptr(), &mut st) } != 0 {
            return false;
        }
        st.f_flag & ST_NOEXEC != 0
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn dir_is_noexec(_dir: &Path) -> bool {
    false
}

// ---------------------------------------------------------------------------
// Shell-profile PATH block (the install.sh mechanism, ported + unified)
// ---------------------------------------------------------------------------
//
// ONE marked block lists every nub-managed dir in fixed priority order, so the
// layering is deterministic whichever feature wired PATH first. `add_path_block`
// (called by `pm shim` + `node default`) and `remove_path_block` both rewrite it
// from current on-disk state.

/// nub-managed PATH dirs, highest priority first; only the active ones (dir
/// present). `shims` outranks `current` so a shimmed npm still routes through
/// nub's PM engine. `$HOME`-relative for a portable profile line.
fn managed_path_dirs(home: &Path) -> Vec<&'static str> {
    let mut dirs = Vec::new();
    if home.join(".nub").join("shims").is_dir() {
        dirs.push("$HOME/.nub/shims");
    }
    // symlink_metadata: the link's own presence, not its (possibly dangling) target.
    if home
        .join(".nub")
        .join("current")
        .symlink_metadata()
        .is_ok()
    {
        dirs.push("$HOME/.nub/current");
    }
    dirs
}

/// The POSIX `export PATH=…` line wiring `dirs` in priority order, `$HOME`-relative
/// like install.sh's own line. `None` when nothing is active (the block is then
/// removed, never written as an empty `:$PATH` that would put cwd on PATH).
fn posix_path_line(dirs: &[&str]) -> Option<String> {
    (!dirs.is_empty()).then(|| format!(r#"export PATH="{}:$PATH""#, dirs.join(":")))
}

/// The fish equivalent of [`posix_path_line`].
fn fish_path_line(dirs: &[&str]) -> Option<String> {
    (!dirs.is_empty()).then(|| format!("set -gx PATH {} $PATH", dirs.join(" ")))
}

/// The managed block's marker comment. install.sh writes `# nub` above its
/// `~/.nub/bin` line; this is deliberately DISTINCT so a rewrite touches exactly
/// our block and never the installer's.
const MANAGED_MARKER: &str = "# nub path";

/// The pre-unification marker — `nub pm shim` wrote this for the shims-only
/// block. Recognized and stripped on every rewrite so existing installs converge
/// onto the unified [`MANAGED_MARKER`] block.
const LEGACY_MARKER: &str = "# nub shims";

/// Outcome of [`add_path_block`], for the CLI's "what changed" report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileOutcome {
    /// The marked block was appended to this profile (CLI prints a source hint).
    Added(PathBuf),
    /// The profile already carries the PATH line — adding twice is a no-op.
    AlreadyPresent(PathBuf),
    /// No known profile exists / is writable for this shell — the CLI prints
    /// `line` as "add this to your shell config yourself" and exits 0.
    Manual { line: String },
}

/// Rewrite the managed PATH block in ALL of the current shell's profile files —
/// both the INTERACTIVE rc and the NON-INTERACTIVE/LOGIN files a GUI-app-spawned
/// shell, an IDE integrated terminal, an `ssh host command`, or a build tool's
/// bare `pnpm` actually reads. install.sh only wires the interactive rc; the
/// managed block widens coverage (mise's lesson — [`shell_profiles`] documents
/// which files and why), since intercepting tool-spawned bare PMs is the shims'
/// whole reason to exist and those processes never source an interactive rc.
///
/// The block lists every active managed dir ([`managed_path_dirs`]) in priority
/// order, so callers (`nub pm shim`, `nub node default`) need not coordinate —
/// each just creates its dir, then calls this to re-derive the block.
///
/// File selection per shell (`$SHELL` basename):
///   - **zsh** → `~/.zshrc` (interactive) **and** `~/.zshenv` (read by ALL zsh
///     invocations, incl. non-interactive/login) — both created if missing.
///   - **bash** → `~/.bashrc` (interactive, only if it already exists+writable —
///     install.sh parity, never created) **and** a login file: `~/.bash_profile`
///     if it exists+writable, else `~/.profile` (CREATED if neither exists, so a
///     fresh container HOME with no dotfile still gets working shims).
///   - **fish** → `$XDG_CONFIG_HOME|~/.config`/`fish/config.fish` (read by every
///     fish invocation — no interactive/login split), created if missing.
///   - anything else → Manual.
pub fn add_path_block() -> Result<ProfileOutcome> {
    let home = dirs_next::home_dir().context("cannot locate the home directory")?;
    let shell = std::env::var("SHELL").unwrap_or_default();
    let shell = Path::new(&shell)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        // install.sh parity: an unset/empty $SHELL is treated as bash (the
        // CI/container default), not a Manual bailout.
        .unwrap_or_else(|| "bash".to_string());
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    let dirs = managed_path_dirs(&home);
    add_path_block_for(&shell, &home, xdg.as_deref(), &dirs)
}

/// [`add_path_block`] with the environment + active dirs made explicit (the
/// testable body).
///
/// Rewrites the block in every target [`shell_profiles`] yields and folds their
/// per-file outcomes into one (the result cli.rs prints, kept a single
/// [`ProfileOutcome`] so the CLI surface is unchanged): the FIRST file actually
/// written is reported as `Added` (the interactive rc, the one the user sources
/// to pick the dirs up in their current shell); if every target already carried
/// the exact block it's `AlreadyPresent` (naming the first); if nothing could be
/// written it's `Manual`. Non-interactive files are wired silently as extra
/// coverage — the per-file detail isn't surfaced to keep one headline outcome.
pub fn add_path_block_for(
    shell: &str,
    home: &Path,
    xdg_config: Option<&Path>,
    dirs: &[&str],
) -> Result<ProfileOutcome> {
    let posix = posix_path_line(dirs);
    let fish = fish_path_line(dirs);
    let targets = shell_profiles(shell, home, xdg_config, posix.as_deref(), fish.as_deref());
    // Manual hint uses the POSIX line; `dirs.is_empty()` (nothing to add) yields
    // an empty string the caller never reaches via the add path.
    let manual = || ProfileOutcome::Manual {
        line: posix.clone().unwrap_or_default(),
    };
    if targets.is_empty() {
        return Ok(manual());
    }
    let mut first_added: Option<PathBuf> = None;
    let mut first_present: Option<PathBuf> = None;
    for target in &targets {
        match write_block(target)? {
            ProfileOutcome::Added(p) => {
                first_added.get_or_insert(p);
            }
            ProfileOutcome::AlreadyPresent(p) => {
                first_present.get_or_insert(p);
            }
            // A single target that can't be written (e.g. an unwritable, already
            // existing login file) doesn't sink the whole operation — another
            // target may still have landed. A target-level Manual is absorbed.
            ProfileOutcome::Manual { .. } => {}
        }
    }
    Ok(match (first_added, first_present) {
        (Some(p), _) => ProfileOutcome::Added(p),
        (None, Some(p)) => ProfileOutcome::AlreadyPresent(p),
        (None, None) => manual(),
    })
}

/// One profile-file target: which file, the desired managed line in that file's
/// dialect (`None` = no active dirs → remove the block), and whether a missing
/// file may be created.
struct ProfileTarget {
    path: PathBuf,
    line: Option<String>,
    may_create: bool,
}

/// The set of profile files to wire for `shell`, interactive first. See
/// [`add_path_block`] for the per-shell rationale. Empty = unknown shell
/// (Manual). The order matters: the first element is the one cli.rs reports as
/// the "source this" file, so it must be the interactive rc. `posix`/`fish` are
/// the desired line in each dialect (`None` when no dirs are active).
fn shell_profiles(
    shell: &str,
    home: &Path,
    xdg_config: Option<&Path>,
    posix: Option<&str>,
    fish: Option<&str>,
) -> Vec<ProfileTarget> {
    let posix_t = |path: PathBuf, may_create: bool| ProfileTarget {
        path,
        line: posix.map(str::to_string),
        may_create,
    };
    match shell {
        // `.zshrc` is interactive-only; `.zshenv` is sourced by EVERY zsh
        // (interactive, login, and bare non-interactive `zsh -c`), so it is the
        // single file that guarantees a GUI/IDE-spawned shell sees the shims.
        // Both are canonical zsh files — created if missing.
        "zsh" => vec![
            posix_t(home.join(".zshrc"), true),
            posix_t(home.join(".zshenv"), true),
        ],
        "bash" => {
            let mut targets = Vec::with_capacity(2);
            // Interactive: install.sh parity — edit `.bashrc` ONLY if it already
            // exists and is writable; never created here (the login file below
            // is what a fresh HOME gets).
            let bashrc = home.join(".bashrc");
            if bashrc.is_file() && appendable(&bashrc) {
                targets.push(posix_t(bashrc, false));
            }
            // Login / non-interactive: bash reads `.bash_profile` (then
            // `.bash_login`, then `.profile`) for login shells. Prefer an
            // existing+writable `.bash_profile`; otherwise `.profile` — CREATED
            // when neither is present, so a fresh container HOME (the dominant
            // CI/first-run case) actually gets working shims instead of a
            // silent Manual punt.
            let bash_profile = home.join(".bash_profile");
            if bash_profile.is_file() && appendable(&bash_profile) {
                targets.push(posix_t(bash_profile, false));
            } else {
                targets.push(posix_t(home.join(".profile"), true));
            }
            targets
        }
        "fish" => {
            let base = xdg_config
                .map(Path::to_path_buf)
                .unwrap_or_else(|| home.join(".config"));
            vec![ProfileTarget {
                path: base.join("fish").join("config.fish"),
                line: fish.map(str::to_string),
                may_create: true,
            }]
        }
        _ => Vec::new(),
    }
}

/// Writability probe: opening for append is the honest check (touches nothing).
fn appendable(path: &Path) -> bool {
    std::fs::OpenOptions::new().append(true).open(path).is_ok()
}

/// Rewrite `target` so it carries exactly the managed block for `target.line`
/// (`\n# nub path\n<line>\n` — byte-for-byte install.sh's three-`echo` shape).
/// Idempotency keys on the PATH line itself (trimmed equality), so a hand-added
/// identical line also counts as present and is then deliberately NOT rewritten.
/// Otherwise any stale managed/legacy block is stripped and the fresh one
/// appended — handling both a content change (shims-only → shims+current) and
/// migration off the legacy `# nub shims` marker.
fn write_block(target: &ProfileTarget) -> Result<ProfileOutcome> {
    // No active dirs → nothing to add here (full removal is the sweep's job).
    let Some(line) = target.line.as_deref() else {
        return Ok(ProfileOutcome::AlreadyPresent(target.path.clone()));
    };
    let existing = match std::fs::read_to_string(&target.path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if !target.may_create {
                return Ok(ProfileOutcome::Manual {
                    line: line.to_string(),
                });
            }
            String::new()
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return Ok(ProfileOutcome::Manual {
                line: line.to_string(),
            });
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", target.path.display())),
    };
    // Already exactly present — UNLESS a legacy `# nub shims` block is still
    // there, which must be migrated to the unified marker even when the PATH line
    // itself is unchanged (the common shims-only re-run). The line check (not a
    // full-block check) also leaves a hand-added identical line in place rather
    // than appending a duplicate.
    let has_legacy = existing.lines().any(|l| l.trim() == LEGACY_MARKER);
    if !has_legacy && existing.lines().any(|l| l.trim() == line) {
        return Ok(ProfileOutcome::AlreadyPresent(target.path.clone()));
    }
    if target.may_create {
        if let Some(parent) = target.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
    }
    // Strip any stale managed/legacy block first, then append the fresh one, so
    // re-running with a changed dir set (or an old `# nub shims` block) converges
    // to exactly one canonical block.
    let new = format!("{}\n{MANAGED_MARKER}\n{line}\n", strip_managed(&existing));
    match write_profile_atomically(&target.path, &new) {
        Ok(()) => Ok(ProfileOutcome::Added(target.path.clone())),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Ok(ProfileOutcome::Manual {
            line: line.to_string(),
        }),
        Err(e) => Err(e).with_context(|| format!("writing {}", target.path.display())),
    }
}

/// Replace `path`'s contents with `new` via temp + rename, so a torn write can
/// never truncate a shell profile. The rename targets the CANONICALIZED path — a
/// `~/.zshrc` that is a symlink into a dotfiles repo stays a symlink, with the
/// edit landing in the linked-to file; renaming onto the symlink path would
/// replace the link with a regular file and orphan the dotfiles copy. The
/// original file's permissions are copied over so a 600 profile stays 600.
fn write_profile_atomically(path: &Path, new: &str) -> std::io::Result<()> {
    let target = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let tmp = target.with_file_name(format!(
        "{}.nub-path-{}",
        target.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    std::fs::write(&tmp, new)?;
    if let Ok(meta) = std::fs::metadata(&target) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    if let Err(e) = std::fs::rename(&tmp, &target) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Re-derive the managed block across EVERY profile it may live in (all shells,
/// interactive + login files — the user may have switched shells). Call AFTER a
/// feature tears down its dir: any STILL-active dir is re-written, so unshimming
/// with a default set keeps `~/.nub/current` on PATH (and vice versa); when none
/// remain the block is dropped. Returns the changed files; profiles without our
/// block are left untouched. Touches only profiles, so it works even when the
/// official nub is gone.
pub fn remove_path_block() -> Result<Vec<PathBuf>> {
    let home = dirs_next::home_dir().context("cannot locate the home directory")?;
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    remove_path_block_from_profiles(&home, xdg.as_deref())
}

/// [`remove_path_block`] with the environment made explicit (the testable body).
pub fn remove_path_block_from_profiles(
    home: &Path,
    xdg_config: Option<&Path>,
) -> Result<Vec<PathBuf>> {
    let fish_base = xdg_config
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join(".config"));
    // The full set [`shell_profiles`] can write, across every shell: the zsh
    // pair (.zshrc interactive + .zshenv all-invocations), the bash trio
    // (.bashrc interactive + the .bash_profile/.profile login files), and fish.
    let candidates = [
        home.join(".zshrc"),
        home.join(".zshenv"),
        home.join(".bashrc"),
        home.join(".bash_profile"),
        home.join(".profile"),
        fish_base.join("fish").join("config.fish"),
    ];
    // Whatever's still active after the caller's teardown — re-added to any file
    // that carried our block, or omitted entirely (full strip) when none remain.
    let dirs = managed_path_dirs(home);
    let posix = posix_path_line(&dirs);
    let fish = fish_path_line(&dirs);
    let mut changed = Vec::new();
    for path in candidates {
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let stripped = strip_managed(&content);
        if stripped == content {
            continue; // no managed block in this file — leave it untouched
        }
        // config.fish takes the fish dialect; everything else POSIX.
        let line = if path.ends_with("config.fish") {
            fish.as_deref()
        } else {
            posix.as_deref()
        };
        let new = match line {
            Some(l) => format!("{stripped}\n{MANAGED_MARKER}\n{l}\n"),
            None => stripped,
        };
        write_profile_atomically(&path, &new)
            .with_context(|| format!("replacing {}", path.display()))?;
        changed.push(path);
    }
    Ok(changed)
}

/// Remove EVERY managed block — both the unified `# nub path` marker and the
/// legacy `# nub shims` one — leaving everything else byte-for-byte intact. A
/// hand-added PATH line without our marker is NOT removed (we only strip what we
/// wrote). Loops [`strip_one_block`] so a file carrying both a legacy and a
/// unified block (or two of either) is fully cleaned.
fn strip_managed(content: &str) -> String {
    let mut s = content.to_string();
    while let Some(stripped) = strip_one_block(&s) {
        s = stripped;
    }
    s
}

/// Remove ONE managed block (whichever managed/legacy marker appears first),
/// byte-exactly; `None` once none remain.
///
/// This is byte-exact string-slicing, NOT a line-split/rejoin, specifically so
/// the result preserves the ORIGINAL file's trailing-newline state: a profile
/// that had no trailing newline before the block was written must regain none
/// after it's removed (a line-rejoin would gain one). The block carries its own
/// leading `\n` separator and a trailing `\n` after the path line; we excise
/// that whole span, so whatever surrounded it — including "the file ended right
/// here, no newline" — is restored verbatim.
fn strip_one_block(content: &str) -> Option<String> {
    // The earliest marker (either dialect) sitting on its own line. We scan line
    // starts so an in-prose mention of the string can't be mistaken for it.
    let (marker_line_start, marker_len) = [MANAGED_MARKER, LEGACY_MARKER]
        .iter()
        .filter_map(|marker| {
            content
                .match_indices(marker)
                .map(|(idx, _)| idx)
                .find(|&idx| {
                    let at_line_start = idx == 0 || content.as_bytes()[idx - 1] == b'\n';
                    let rest = &content[idx + marker.len()..];
                    let to_eol_blank = rest.starts_with('\n') || rest.is_empty();
                    at_line_start && to_eol_blank
                })
                .map(|idx| (idx, marker.len()))
        })
        .min_by_key(|(idx, _)| *idx)?;

    // The block starts at the single `\n` separator written right before the
    // marker (if present — the marker can be the very first byte of the file).
    let block_start = if marker_line_start > 0 && content.as_bytes()[marker_line_start - 1] == b'\n'
    {
        marker_line_start - 1
    } else {
        marker_line_start
    };

    // The block end: past the marker's newline, then past the PATH line's
    // newline — but only consume that next line when it names one of OUR managed
    // dirs (defensive: a half-block whose line was hand-deleted stops at the
    // marker, and a user's own `.nub/`-containing PATH line is never eaten — it
    // must be exactly `.nub/shims` or `.nub/current`).
    let after_marker = marker_line_start + marker_len;
    let mut block_end = after_marker;
    if content.as_bytes().get(block_end) == Some(&b'\n') {
        block_end += 1; // the marker's trailing newline
        let path_line_end = content[block_end..]
            .find('\n')
            .map(|n| block_end + n + 1)
            .unwrap_or(content.len());
        let path_line = &content[block_end..path_line_end];
        if path_line.contains(".nub/shims") || path_line.contains(".nub/current") {
            block_end = path_line_end; // the PATH line + its newline
        }
    }

    let mut stripped = String::with_capacity(content.len());
    stripped.push_str(&content[..block_start]);
    stripped.push_str(&content[block_end..]);
    Some(stripped)
}

// ---------------------------------------------------------------------------
// Reachability + PATH fall-through
// ---------------------------------------------------------------------------

/// One shim name's which-style resolution, for the post-install warning
/// ("pnpm resolves to /opt/homebrew/bin/pnpm, which shadows the shim").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShimReachability {
    pub name: &'static str,
    /// The first executable PATH hit (shim dir included in the scan).
    /// `None` = the name resolves nowhere — the shim dir isn't on PATH yet.
    pub first_hit: Option<PathBuf>,
    /// The first hit IS the shim. `false` + a `first_hit` = shadowed by an
    /// earlier PATH entry; `false` + `None` = shim dir not on PATH.
    pub ok: bool,
}

/// Resolve each PM shim name against the current `PATH` (`nub` itself is not
/// checked — the official `~/.nub/bin/nub` resolving first is benign).
pub fn check_shims_reachable(shim_dir: &Path) -> Vec<ShimReachability> {
    check_shims_reachable_in(shim_dir, &std::env::var_os("PATH").unwrap_or_default())
}

/// [`check_shims_reachable`] against an explicit PATH (the testable body).
fn check_shims_reachable_in(shim_dir: &Path, path_var: &OsStr) -> Vec<ShimReachability> {
    let canon_shim = shim_dir.canonicalize().ok();
    PM_SHIM_NAMES
        .into_iter()
        .map(|name| {
            let first_hit = scan_path(name, path_var, None, None);
            let ok = first_hit
                .as_ref()
                .zip(canon_shim.as_deref())
                .is_some_and(|(hit, shim)| {
                    hit.parent().and_then(|p| p.canonicalize().ok()).as_deref() == Some(shim)
                });
            ShimReachability {
                name,
                first_hit,
                ok,
            }
        })
        .collect()
}

/// The unpinned/transparent fall-through: the first executable `invoked` on
/// PATH, SKIPPING the shim dir itself — the recursion guard (mirrors
/// `discovery::which_node`'s skip of nub's own shim dirs, but by canonical
/// path equality since `~/.nub/shims` is a fixed, possibly-symlinked dir).
/// `None` = a true PATH miss; the caller provisions a dynamic default.
pub fn find_system_pm(invoked: &str, shim_dir: &Path) -> Option<PathBuf> {
    find_system_pm_in(
        invoked,
        shim_dir,
        &std::env::var_os("PATH").unwrap_or_default(),
    )
}

/// [`find_system_pm`] against an explicit PATH (the testable body). Besides the
/// shim-dir skip, the running binary itself is skipped by file identity — the
/// last-ditch recursion guard for shim entries reachable through PATH spellings
/// the dir comparison can't see (relative entries, bind mounts).
fn find_system_pm_in(invoked: &str, shim_dir: &Path, path_var: &OsStr) -> Option<PathBuf> {
    let self_exe = std::env::current_exe().ok();
    scan_path(invoked, path_var, Some(shim_dir), self_exe.as_deref())
}

/// First executable hit for `name` across `path_var`'s entries, optionally
/// skipping one directory (compared canonicalized, so a symlinked PATH entry
/// to the shim dir is still skipped) and one file identity (inode-compared via
/// [`same_file`]). Empty PATH entries are skipped outright: POSIX treats them
/// as the cwd, which a fall-through must never search — an empty entry with
/// cwd == the shim dir would defeat the recursion guard (canonicalizing `""`
/// fails, so the dir comparison alone can't catch it).
fn scan_path(
    name: &str,
    path_var: &OsStr,
    skip_dir: Option<&Path>,
    skip_file: Option<&Path>,
) -> Option<PathBuf> {
    let skip = skip_dir.and_then(|d| d.canonicalize().ok());
    for dir in std::env::split_paths(path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        if let Some(skip) = skip.as_deref() {
            if dir.canonicalize().ok().as_deref() == Some(skip) {
                continue;
            }
        }
        for candidate in candidate_names(name) {
            let path = dir.join(candidate);
            if is_executable(&path) {
                if skip_file.is_some_and(|me| same_file(me, &path)) {
                    continue;
                }
                return Some(path);
            }
        }
    }
    None
}

/// When the running `nub` IS the shim dir's own hardlink (the dir is prepended
/// to PATH, so post-`nub pm shim` a bare `nub` resolves there), defer to the
/// SELF-OWNED official binary — the sibling `<~/.nub>/bin/nub`, never "whatever
/// nub turns up next on PATH". After a self-owned upgrade swaps that binary (new
/// inode) the shim-dir link still carries the OLD bytes, so without this
/// passthrough `nub` (including `nub pm shim` itself, the re-link) would run
/// stale code forever.
///
/// The target MUST be the sibling `bin/nub`, not a PATH scan. A PATH scan was
/// the original implementation and it is unsound: when the shim hardlink and the
/// upgraded `bin/nub` share an inode (the normal post-relink steady state, and
/// the state every fresh install starts in), the scan's "skip self by inode"
/// guard skipped the *correct* `bin/nub` and fell through to the FIRST OTHER nub
/// anywhere on PATH — a foreign npm-global / homebrew / dev-tree install, often
/// an OLDER version. The observed failure (2026-06-13): a self-owned upgrade to
/// 0.0.37 succeeded and relinked the shims, but a stale npm-global 0.0.31 lower
/// on PATH made `nub` (resolving to the shim) silently run 0.0.31 — `nub upgrade`
/// reported success while `nub -v` showed the old version. Resolving the sibling
/// directly removes the foreign-binary hazard entirely.
///
/// `None` (= run self) when: not running from the shim dir; the sibling
/// `bin/nub` doesn't exist (post-uninstall — keep running so `unshim` works); or
/// the sibling is the SAME file as self (already relinked / never upgraded — no
/// point exec'ing, and exec'ing self would loop).
pub fn nub_passthrough_target() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = shim_dir().ok()?;
    let canon_dir = dir.canonicalize().ok()?;
    if exe.parent()?.canonicalize().ok()? != canon_dir {
        return None;
    }
    // The official self-owned binary is the sibling of the shim dir: the shim dir
    // is `<install>/shims`, so `<install>/bin/nub` is the binary the shims track.
    let official = dir.parent()?.join("bin").join(nub_exe_name());
    if !is_executable(&official) {
        return None;
    }
    // Same file as self → the shim already points at the official binary (the
    // post-relink steady state). Run self rather than exec a copy of ourselves.
    if same_file(&exe, &official) {
        return None;
    }
    Some(official)
}

/// The on-disk file name of the `nub` executable for this platform — `nub` on
/// Unix, `nub.exe` on Windows.
fn nub_exe_name() -> &'static str {
    #[cfg(windows)]
    {
        "nub.exe"
    }
    #[cfg(unix)]
    {
        "nub"
    }
}

/// The on-disk spellings to probe per PATH dir. Windows additionally probes
/// the launcher extensions npm-on-Windows actually ships (`.cmd`): the
/// `.exe`-miss → `.cmd`-hit probe runs for real on the windows-latest CI leg
/// (nub-cli's `pm_shim_windows.rs` fall-through resolves a `pnpm.cmd`); the
/// `.bat` spelling is probed by the same loop but no test plants one.
#[cfg(unix)]
fn candidate_names(name: &str) -> [String; 1] {
    [name.to_string()]
}
#[cfg(windows)]
fn candidate_names(name: &str) -> [String; 3] {
    [
        format!("{name}.exe"),
        format!("{name}.cmd"),
        format!("{name}.bat"),
    ]
}

/// Executable-file check: on Unix a regular file with any exec bit; on Windows
/// existence (executability is extension-driven there).
fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        path.is_file()
    }
}

// ---------------------------------------------------------------------------
// Sibling bin resolution (the npx / pnpx seam)
// ---------------------------------------------------------------------------

/// Resolve a SIBLING bin entry (`npx` in the npm package, `pnpx` in pnpm,
/// `yarnpkg` in yarn) from a provisioned PM's primary bin path.
///
/// `provision_pm` returns the entry named for the PACKAGE
/// (`registry::bin_subpath`); a shim invoked as `npx` needs a different entry
/// from the same install. Rather than widening provisioning, this walks up
/// from the primary bin to the package root (the nearest ancestor carrying a
/// `package.json` — the store's normalized `<version>/package/` dir) and picks
/// the named entry from the cached manifest's bin map
/// (`registry::named_bin_subpath`) — the smallest seam, zero extra network.
pub fn sibling_bin(primary_bin: &Path, entry: &str) -> Result<PathBuf> {
    let pkg_root = primary_bin
        .ancestors()
        .skip(1)
        .find(|dir| dir.join("package.json").is_file())
        .with_context(|| {
            format!(
                "no package.json above {} to read a bin map from",
                primary_bin.display()
            )
        })?;
    let manifest_path = pkg_root.join("package.json");
    let manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&manifest_path)
            .with_context(|| format!("reading {}", manifest_path.display()))?,
    )
    .with_context(|| format!("parsing {}", manifest_path.display()))?;
    let subpath = super::registry::named_bin_subpath(&manifest, entry).with_context(|| {
        format!(
            "{} declares no bin entry named \"{entry}\"",
            manifest_path.display()
        )
    })?;
    let bin = pkg_root.join(subpath);
    if !bin.is_file() {
        bail!(
            "bin entry \"{entry}\" points at {}, which does not exist",
            bin.display()
        );
    }
    Ok(bin)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// The managed-dir set + its rendered lines for the PATH-block tests. A
    /// shims-only block (the common case); `managed_path_dirs` is exercised
    /// separately via the real-state paths. These mirror what
    /// `posix_path_line(SHIMS_DIRS)` / `fish_path_line(SHIMS_DIRS)` render, kept
    /// as literals so the expected profile bytes read at a glance.
    const SHIMS_DIRS: &[&str] = &["$HOME/.nub/shims"];
    const SHIMS_POSIX: &str = r#"export PATH="$HOME/.nub/shims:$PATH""#;
    const SHIMS_FISH: &str = "set -gx PATH $HOME/.nub/shims $PATH";

    /// Unique temp dir under the system temp root (mirrors `resolve.rs`'s
    /// `tmpdir` — never under $HOME, so nothing here can touch real profiles
    /// or the real ~/.nub/shims). The startup-nanos component keeps names
    /// unique ACROSS suite runs, not just within one: stale dirs are never
    /// cleaned, PIDs recycle, and a recycled-PID run re-entering a stale
    /// sibling finds last run's links/files (the tests/pm_shim.rs `tmp` flake
    /// — see its doc for the full post-mortem).
    fn tmpdir(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        static STARTED_NANOS: std::sync::OnceLock<u128> = std::sync::OnceLock::new();
        let nanos = STARTED_NANOS.get_or_init(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        });
        let dir = std::env::temp_dir().join(format!(
            "nub-shim-{tag}-{}-{nanos:x}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    fn write_exec(path: &Path, content: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, content).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn decision_matrix_is_strict_by_default_with_transparent_escapes() {
        use PinProvenance::*;
        use ShimDecision::*;
        use ShimName::*;
        let pnpm = PinState::Pinned {
            pm: Pm::Pnpm,
            provenance: PackageManagerField,
        };
        let npm = PinState::Pinned {
            pm: Pm::Npm,
            provenance: DevEngines,
        };
        let berry = PinState::Pinned {
            pm: Pm::YarnBerry,
            provenance: YarnPath,
        };
        let yarn1 = PinState::Pinned {
            pm: Pm::Yarn,
            provenance: PackageManagerField,
        };
        let nub = PinState::NubPinned {
            provenance: PackageManagerField,
        };
        let none = PinState::Unpinned;

        let cases: &[(ShimName, &PinState, Option<&str>, ShimDecision, &str)] = &[
            // Pinned + name match → run the pin, whatever the verb.
            (
                Pnpm,
                &pnpm,
                Some("install"),
                RunPinned {
                    pm: Pm::Pnpm,
                    bin_entry: "pnpm",
                },
                "name match runs the pinned PM",
            ),
            (
                Pnpm,
                &pnpm,
                Some("dlx"),
                RunPinned {
                    pm: Pm::Pnpm,
                    bin_entry: "pnpm",
                },
                "a transparent verb in a MATCHED project still runs the pin",
            ),
            (
                Yarnpkg,
                &yarn1,
                Some("install"),
                RunPinned {
                    pm: Pm::Yarn,
                    bin_entry: "yarnpkg",
                },
                "yarnpkg is yarn's alias — a name match with its own bin entry",
            ),
            (
                Npx,
                &npm,
                Some("cowsay"),
                RunPinned {
                    pm: Pm::Npm,
                    bin_entry: "npx",
                },
                "npx in an npm-pinned project runs the PINNED npm's npx",
            ),
            (
                Yarn,
                &berry,
                Some("install"),
                RunPinned {
                    pm: Pm::YarnBerry,
                    bin_entry: "yarn",
                },
                "yarn in a committed-yarnPath project execs the committed release (pm = YarnBerry)",
            ),
            // Pinned + name mismatch, not transparent → refuse.
            (
                Npm,
                &pnpm,
                Some("install"),
                Refuse {
                    pinned_pm: Pm::Pnpm,
                    provenance: PackageManagerField,
                },
                "npm install in a pnpm project refuses, naming pin + provenance",
            ),
            (
                Npm,
                &pnpm,
                None,
                Refuse {
                    pinned_pm: Pm::Pnpm,
                    provenance: PackageManagerField,
                },
                "a bare `npm` (no argv) is not transparent",
            ),
            (
                Npm,
                &pnpm,
                Some("--version"),
                Refuse {
                    pinned_pm: Pm::Pnpm,
                    provenance: PackageManagerField,
                },
                "a flag is not a transparent verb — strictness errs toward refusing",
            ),
            (
                Pnpm,
                &berry,
                Some("install"),
                Refuse {
                    pinned_pm: Pm::YarnBerry,
                    provenance: YarnPath,
                },
                "pnpm in a yarnPath project is a name mismatch",
            ),
            // Transparent escapes in a MISMATCHED project → the system PM, never the pin.
            (
                Npm,
                &pnpm,
                Some("create"),
                FallThrough { invoked: Npm },
                "`npm create vite` in a pnpm repo must work",
            ),
            (
                Npm,
                &pnpm,
                Some("init"),
                FallThrough { invoked: Npm },
                "init is transparent",
            ),
            (
                Npm,
                &pnpm,
                Some("exec"),
                FallThrough { invoked: Npm },
                "exec is transparent",
            ),
            (
                Yarn,
                &pnpm,
                Some("dlx"),
                FallThrough { invoked: Yarn },
                "dlx is transparent for yarn too",
            ),
            (
                Npx,
                &pnpm,
                Some("cowsay"),
                FallThrough { invoked: Npx },
                "the npx BINARY is always transparent",
            ),
            (
                Pnpx,
                &npm,
                None,
                FallThrough { invoked: Pnpx },
                "pnpx is always transparent, even with no argv",
            ),
            // Unpinned → fall through, always.
            (
                Pnpm,
                &none,
                Some("install"),
                FallThrough { invoked: Pnpm },
                "unpinned never refuses and never provisions the pin",
            ),
            (
                Npm,
                &none,
                Some("create"),
                FallThrough { invoked: Npm },
                "unpinned + transparent is still a plain fall-through",
            ),
            // nub-pinned (`pm use nub`) → a foreign-PM shim refuses to nub,
            // never provisions a competing PM just to have it reject the pin.
            (
                Pnpm,
                &nub,
                Some("install"),
                RefuseNubPinned {
                    provenance: PackageManagerField,
                },
                "pnpm install in a nub-pinned project refuses and redirects to nub",
            ),
            (
                Npm,
                &nub,
                None,
                RefuseNubPinned {
                    provenance: PackageManagerField,
                },
                "a bare `npm` in a nub-pinned project refuses (not transparent)",
            ),
            (
                Npm,
                &nub,
                Some("create"),
                FallThrough { invoked: Npm },
                "`npm create vite` still escapes in a nub-pinned project",
            ),
            (
                Npx,
                &nub,
                Some("cowsay"),
                FallThrough { invoked: Npx },
                "the npx binary is transparent in a nub-pinned project too",
            ),
        ];
        // Every row in this matrix is a TOP-LEVEL invocation (no PM running
        // above us). The nested-call relaxation is exercised separately in
        // `nested_mismatch_falls_through_top_level_still_refuses`.
        for (invoked, pin, arg, want, why) in cases {
            assert_eq!(
                decide(*invoked, pin, *arg, Nesting::TopLevel),
                *want,
                "{why} (invoked {invoked:?}, first arg {arg:?})"
            );
        }
    }

    #[test]
    fn nested_mismatch_falls_through_top_level_still_refuses() {
        use ShimDecision::*;
        use ShimName::*;
        let pnpm = PinState::Pinned {
            pm: Pm::Pnpm,
            provenance: PinProvenance::PackageManagerField,
        };

        // The bug: a pnpm postinstall shells out to `npm install` (a name
        // mismatch). TOP-LEVEL that refuses — the user typed it and can fix it.
        assert_eq!(
            decide(Npm, &pnpm, Some("install"), Nesting::TopLevel),
            Refuse {
                pinned_pm: Pm::Pnpm,
                provenance: PinProvenance::PackageManagerField,
            },
            "a top-level npm-in-a-pnpm-project mismatch stays strict"
        );
        // NESTED (a running PM set npm_config_user_agent) it must fall through,
        // not refuse — otherwise the pnpm-driven install breaks on its own hook.
        assert_eq!(
            decide(Npm, &pnpm, Some("install"), Nesting::Nested),
            FallThrough { invoked: Npm },
            "a nested mismatch falls through to the system PM instead of refusing"
        );
        // A nested SAME-PM call is unchanged: it still runs the pin. Nesting only
        // relaxes the mismatch refusal, never the run-the-pin path.
        assert_eq!(
            decide(Pnpm, &pnpm, Some("install"), Nesting::Nested),
            RunPinned {
                pm: Pm::Pnpm,
                bin_entry: "pnpm",
            },
            "a nested same-PM call still runs the pinned PM"
        );

        // The env reader: any non-empty PM marker means nested; absence/empty is
        // top-level. (Either npm_config_user_agent or npm_execpath suffices.)
        assert_eq!(
            Nesting::from_env(|k| (k == "npm_config_user_agent").then(|| "pnpm/9.0.0".into())),
            Nesting::Nested
        );
        assert_eq!(
            Nesting::from_env(|k| (k == "npm_execpath").then(|| "/x/npm-cli.js".into())),
            Nesting::Nested
        );
        assert_eq!(
            Nesting::from_env(|k| (k == "npm_config_user_agent").then(String::new)),
            Nesting::TopLevel,
            "an empty marker is not a running PM"
        );
        assert_eq!(Nesting::from_env(|_| None), Nesting::TopLevel);
    }

    #[test]
    fn safe_redirect_keeps_real_verbs_and_never_invents_a_missing_one() {
        let v = |args: &[&str]| args.iter().map(|s| s.to_string()).collect::<Vec<String>>();

        // A verb the pinned PM implements is suggested verbatim with its args.
        assert_eq!(
            safe_redirect(Pm::Pnpm, &v(&["install", "react"])).as_deref(),
            Some("pnpm install react")
        );
        // `ci` is npm-only: the OLD code blindly suggested `pnpm ci`, which is
        // not a real command. The redirect now drops to a verbless `use pnpm`
        // rather than fabricating a verb pnpm/yarn lack.
        assert_eq!(
            safe_redirect(Pm::Pnpm, &v(&["ci"])).as_deref(),
            Some("use pnpm"),
            "pnpm has no `ci` — suggest the PM, not a nonexistent verb"
        );
        assert_eq!(
            safe_redirect(Pm::YarnBerry, &v(&["ci"])).as_deref(),
            Some("use yarn"),
            "yarn has no `ci` either; Berry still prints `yarn`"
        );
        // npm DOES have `ci` — there the same-verb suggestion is honest.
        assert_eq!(
            safe_redirect(Pm::Npm, &v(&["ci"])).as_deref(),
            Some("npm ci")
        );
        // No verb to redirect → None, the caller drops its "run instead" line:
        // a bare `npm` names nothing to swap, and a flags-only `npm --version`
        // must not be echoed back as "run instead: pnpm --version".
        assert_eq!(safe_redirect(Pm::Pnpm, &[]), None);
        assert_eq!(
            safe_redirect(Pm::Pnpm, &v(&["--version"])),
            None,
            "a flags-only invocation has no honest redirect"
        );
    }

    #[cfg(unix)]
    #[test]
    fn install_shims_hardlinks_relinks_after_upgrade_and_survives_self_link() {
        use std::os::unix::fs::MetadataExt;
        let root = tmpdir("install");
        let bin = root.join("fake-nub");
        write_exec(&bin, "#!/bin/sh\necho v1\n");
        let shims = root.join("shims");

        // Fresh install: every PM name created as a hardlink (same inode, no copy).
        let first = install_shims_into(&shims, &bin).unwrap();
        assert_eq!(first.len(), SHIM_NAMES.len());
        let src_ino = std::fs::metadata(&bin).unwrap().ino();
        for shim in &first {
            assert_eq!(
                shim.action,
                ShimAction::Created,
                "{} must be fresh",
                shim.name
            );
            assert!(
                !shim.copied,
                "{} must be a hardlink on the same fs",
                shim.name
            );
            assert_eq!(
                std::fs::metadata(&shim.path).unwrap().ino(),
                src_ino,
                "{} must share the nub binary's inode",
                shim.name
            );
        }

        // Idempotent re-run against the same bytes: everything already current.
        let second = install_shims_into(&shims, &bin).unwrap();
        assert!(
            second.iter().all(|s| s.action == ShimAction::Current),
            "re-linking unchanged bytes must be a no-op, got {second:?}"
        );

        // "Upgrade": a NEW inode lands at the nub path → every entry relinks.
        std::fs::remove_file(&bin).unwrap();
        write_exec(&bin, "#!/bin/sh\necho v2\n");
        let new_ino = std::fs::metadata(&bin).unwrap().ino();
        assert_ne!(src_ino, new_ino, "the rewrite must produce a new inode");
        let third = install_shims_into(&shims, &bin).unwrap();
        for shim in &third {
            assert_eq!(
                shim.action,
                ShimAction::Relinked,
                "{} must relink",
                shim.name
            );
            assert_eq!(std::fs::metadata(&shim.path).unwrap().ino(), new_ino);
        }

        // Self-link guard: re-running with the nub binary path pointing AT an
        // existing shim entry (same inode) must take the same-bytes no-op path
        // and never delete its own link source — the `same_file` short-circuit.
        let fourth = install_shims_into(&shims, &shims.join("pnpm")).unwrap();
        assert!(
            fourth.iter().all(|s| s.action == ShimAction::Current),
            "self-link re-run leaves the already-linked entries alone, got {fourth:?}"
        );
        assert!(
            shims.join("pnpm").is_file() && shims.join("npm").is_file(),
            "the shim dir survives a re-link from one of its own entries"
        );
    }

    #[cfg(unix)]
    #[test]
    fn noexec_probe_never_false_alarms_and_detects_a_real_noexec_mount() {
        // Negative: the temp dir is exec-allowed on every dev/CI box — the
        // probe must not warn there. Best-effort contract: an unstattable
        // path is also "not noexec", never an error.
        assert!(
            !dir_is_noexec(&tmpdir("noexec")),
            "the temp dir must not read as noexec"
        );
        assert!(
            !dir_is_noexec(Path::new("/nonexistent-nub-noexec-probe")),
            "a missing path is a silent miss, not a warning"
        );

        // Positive: only reachable on a real noexec mount. Virtually every
        // Linux system has one (/proc, /sys/fs/cgroup, /dev/shm variants) —
        // find one in /proc/self/mounts and assert the probe sees it. macOS
        // has no noexec mount by default and creating one needs root, so the
        // positive arm is Linux-only (the Docker/CI legs run it).
        #[cfg(target_os = "linux")]
        {
            let mounts = std::fs::read_to_string("/proc/self/mounts").unwrap();
            let noexec_mount = mounts.lines().find_map(|line| {
                let mut fields = line.split_whitespace();
                let (_dev, point, _fs, opts) = (
                    fields.next()?,
                    fields.next()?,
                    fields.next()?,
                    fields.next()?,
                );
                (opts.split(',').any(|o| o == "noexec") && Path::new(point).is_dir())
                    .then(|| PathBuf::from(point))
            });
            match noexec_mount {
                Some(dir) => assert!(
                    dir_is_noexec(&dir),
                    "{} is mounted noexec but the probe missed it",
                    dir.display()
                ),
                // Honest skip beats a ceremonial fake — but say so in the log.
                None => eprintln!("no noexec mount on this system; positive arm not exercised"),
            }
        }
    }

    #[test]
    fn remove_shims_deletes_the_dir_and_is_idempotent() {
        let root = tmpdir("rm");
        let shims = root.join("shims");
        std::fs::create_dir_all(&shims).unwrap();
        std::fs::write(shims.join("pnpm"), "x").unwrap();
        assert!(
            remove_shims_from(&shims).unwrap(),
            "an existing dir is removed"
        );
        assert!(!shims.exists());
        assert!(
            !remove_shims_from(&shims).unwrap(),
            "a second removal reports nothing-to-do, not an error"
        );
    }

    #[test]
    fn shim_lock_is_exclusive_then_released_and_steals_a_stale_holder() {
        let root = tmpdir("lock");
        let shims = root.join("shims");
        let lockfile = root.join("shims.lock"); // sibling: <parent>/<name>.lock

        // First acquirer holds it; a second concurrent acquire of the SAME dir
        // sees a fresh lock and — rather than block the suite — returns None
        // (best-effort: proceed unlocked after the wait). We assert the held
        // lockfile exists while the guard is alive, and is gone after drop.
        let held = ShimLock::acquire(&shims).expect("first acquire wins");
        assert!(lockfile.is_file(), "the lockfile exists while held");
        drop(held);
        assert!(!lockfile.exists(), "drop releases (removes) the lockfile");

        // A STALE lock (mtime older than the window) is stolen: backdate the
        // file's mtime past LOCK_STALE and confirm acquire reclaims it.
        let f = std::fs::File::create(&lockfile).unwrap();
        let old = std::time::SystemTime::now() - (LOCK_STALE + std::time::Duration::from_secs(5));
        f.set_modified(old).unwrap();
        drop(f);
        let reclaimed = ShimLock::acquire(&shims).expect("a stale lock is stolen, not waited on");
        assert!(lockfile.is_file(), "the reclaimed lock is freshly created");
        drop(reclaimed);
        assert!(!lockfile.exists());
    }

    #[test]
    fn profile_block_appends_marked_block_idempotently_and_strips_exactly() {
        let home = tmpdir("profile");
        let zshrc = home.join(".zshrc");
        let zshenv = home.join(".zshenv");
        // A realistic interactive rc that ALREADY carries install.sh's own
        // `# nub` block — unshim must never strip that one.
        let original = "# mine\nexport PATH=\"$HOME/bin:$PATH\"\n\n# nub\nexport PATH=\"$HOME/.nub/bin:$PATH\"\n";
        std::fs::write(&zshrc, original).unwrap();

        // zsh writes the block to BOTH the interactive rc and `.zshenv` (the
        // file every zsh invocation reads — the non-interactive coverage). The
        // reported outcome is the interactive rc, the file the user sources.
        assert_eq!(
            add_path_block_for("zsh", &home, None, SHIMS_DIRS).unwrap(),
            ProfileOutcome::Added(zshrc.clone()),
            "the headline outcome names the interactive rc the user sources"
        );
        let with_block = std::fs::read_to_string(&zshrc).unwrap();
        assert_eq!(
            with_block,
            format!("{original}\n# nub path\n{SHIMS_POSIX}\n"),
            "the appended block must be byte-for-byte install.sh's shape"
        );
        // .zshenv was created from scratch and carries the same block — this is
        // what a GUI/IDE-spawned non-interactive zsh actually reads.
        assert_eq!(
            std::fs::read_to_string(&zshenv).unwrap(),
            format!("\n# nub path\n{SHIMS_POSIX}\n"),
            ".zshenv gets the block so non-interactive shells see the shims"
        );

        // Adding twice is a no-op on every target.
        assert_eq!(
            add_path_block_for("zsh", &home, None, SHIMS_DIRS).unwrap(),
            ProfileOutcome::AlreadyPresent(zshrc.clone())
        );
        assert_eq!(std::fs::read_to_string(&zshrc).unwrap(), with_block);

        // Removal strips BOTH files and restores the rc byte-for-byte — the
        // installer's `# nub` bin block left intact.
        let mut changed = remove_path_block_from_profiles(&home, None).unwrap();
        changed.sort();
        let mut want = vec![zshrc.clone(), zshenv.clone()];
        want.sort();
        assert_eq!(changed, want, "unshim sweeps both zsh files it wrote");
        assert_eq!(std::fs::read_to_string(&zshrc).unwrap(), original);
        assert_eq!(
            std::fs::read_to_string(&zshenv).unwrap(),
            "",
            ".zshenv (created solely for the block) is left empty after strip"
        );

        // Removing again: no block left, no files changed.
        assert!(
            remove_path_block_from_profiles(&home, None)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn profile_selection_covers_interactive_and_non_interactive_files_per_shell() {
        let home = tmpdir("select");

        // zsh: both .zshrc (interactive) and .zshenv (all invocations) are
        // created; the headline outcome is the interactive rc.
        let outcome = add_path_block_for("zsh", &home, None, SHIMS_DIRS).unwrap();
        assert_eq!(outcome, ProfileOutcome::Added(home.join(".zshrc")));
        let zsh_block = format!("\n# nub path\n{SHIMS_POSIX}\n");
        assert_eq!(
            std::fs::read_to_string(home.join(".zshrc")).unwrap(),
            zsh_block
        );
        assert_eq!(
            std::fs::read_to_string(home.join(".zshenv")).unwrap(),
            zsh_block,
            ".zshenv is wired so a non-interactive/GUI-spawned zsh sees the shims"
        );

        // fish: XDG_CONFIG_HOME wins, parents are created, the FISH line lands.
        let xdg = home.join("xdg");
        let fish_config = xdg.join("fish").join("config.fish");
        assert_eq!(
            add_path_block_for("fish", &home, Some(&xdg), SHIMS_DIRS).unwrap(),
            ProfileOutcome::Added(fish_config.clone())
        );
        let fish_content = std::fs::read_to_string(&fish_config).unwrap();
        assert!(
            fish_content.contains(SHIMS_FISH) && !fish_content.contains("export PATH"),
            "fish gets `set -gx`, never the posix line, got: {fish_content}"
        );

        // An unknown shell still can only be handled manually.
        assert_eq!(
            add_path_block_for("tcsh", &home, None, SHIMS_DIRS).unwrap(),
            ProfileOutcome::Manual {
                line: SHIMS_POSIX.to_string()
            }
        );
    }

    #[test]
    fn fresh_bash_home_creates_and_wires_a_login_profile_instead_of_punting() {
        // The breaker's should-fix: a fresh container HOME with no dotfile got
        // shims linked but NO PATH wiring (the old bash branch only edited an
        // EXISTING rc, then fell to Manual), leaving them silently dead — the
        // dominant CI/first-run case. Now bash CREATES ~/.profile so the shims
        // actually work.
        let home = tmpdir("fresh-bash");
        assert_eq!(
            add_path_block_for("bash", &home, None, SHIMS_DIRS).unwrap(),
            ProfileOutcome::Added(home.join(".profile")),
            "a fresh bash HOME must get a created+wired login profile, not Manual"
        );
        assert_eq!(
            std::fs::read_to_string(home.join(".profile")).unwrap(),
            format!("\n# nub path\n{SHIMS_POSIX}\n")
        );
        // .bashrc is NOT conjured (install.sh parity — interactive rc is only
        // touched when it already exists); the login file alone carries it.
        assert!(
            !home.join(".bashrc").exists(),
            "the interactive rc is never created out of thin air"
        );

        // With an existing .bashrc, BOTH it (interactive) and the login file get
        // the block; the headline outcome is the interactive rc.
        let home = tmpdir("bash-with-rc");
        std::fs::write(home.join(".bashrc"), "# rc\n").unwrap();
        assert_eq!(
            add_path_block_for("bash", &home, None, SHIMS_DIRS).unwrap(),
            ProfileOutcome::Added(home.join(".bashrc"))
        );
        assert!(
            std::fs::read_to_string(home.join(".bashrc"))
                .unwrap()
                .contains(SHIMS_POSIX),
            "the existing interactive rc is wired"
        );
        assert!(
            std::fs::read_to_string(home.join(".profile"))
                .unwrap()
                .contains(SHIMS_POSIX),
            "and the login file too, for non-interactive coverage"
        );

        // An existing .bash_profile is the login file chosen over .profile.
        let home = tmpdir("bash-with-profile");
        std::fs::write(home.join(".bash_profile"), "# hello\n").unwrap();
        assert_eq!(
            add_path_block_for("bash", &home, None, SHIMS_DIRS).unwrap(),
            ProfileOutcome::Added(home.join(".bash_profile")),
            "an existing .bash_profile is preferred as the login target"
        );
        assert!(
            !home.join(".profile").exists(),
            ".profile is not created when .bash_profile already exists"
        );
    }

    #[test]
    fn unshim_sweeps_every_profile_file_across_all_shells() {
        // unshim must strip from EVERY file add_path_block may have written —
        // across shells AND across the interactive/non-interactive split — since
        // the user may have switched shells since `nub pm shim` ran.
        let home = tmpdir("sweep");
        let xdg = home.join("xdg");

        add_path_block_for("zsh", &home, None, SHIMS_DIRS).unwrap(); // .zshrc + .zshenv
        add_path_block_for("fish", &home, Some(&xdg), SHIMS_DIRS).unwrap(); // config.fish
        std::fs::write(home.join(".bashrc"), "# rc\n").unwrap();
        add_path_block_for("bash", &home, None, SHIMS_DIRS).unwrap(); // .bashrc + .profile

        let mut changed = remove_path_block_from_profiles(&home, Some(&xdg)).unwrap();
        changed.sort();
        let mut want = vec![
            home.join(".zshrc"),
            home.join(".zshenv"),
            home.join(".bashrc"),
            home.join(".profile"),
            xdg.join("fish").join("config.fish"),
        ];
        want.sort();
        assert_eq!(
            changed, want,
            "every interactive + non-interactive file must be swept"
        );
        // A second sweep is a clean no-op.
        assert!(
            remove_path_block_from_profiles(&home, Some(&xdg))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn unshim_preserves_a_profile_with_no_trailing_newline() {
        // The breaker's byte-cleanliness nit: a profile that had NO trailing
        // newline before `nub pm shim` must regain none after `unshim`. The old
        // line-split/rejoin strip silently added one.
        let home = tmpdir("no-trailing-nl");
        let zshrc = home.join(".zshrc");
        let original = "export EDITOR=vi"; // deliberately no trailing newline
        std::fs::write(&zshrc, original).unwrap();

        add_path_block_for("zsh", &home, None, SHIMS_DIRS).unwrap();
        // Sanity: the block landed after the original's last byte.
        assert_eq!(
            std::fs::read_to_string(&zshrc).unwrap(),
            format!("{original}\n# nub path\n{SHIMS_POSIX}\n")
        );

        remove_path_block_from_profiles(&home, None).unwrap();
        assert_eq!(
            std::fs::read_to_string(&zshrc).unwrap(),
            original,
            "the no-trailing-newline state must survive shim+unshim byte-for-byte"
        );
    }

    #[test]
    fn managed_block_lists_dirs_in_fixed_priority_order_and_rewrites_in_place() {
        // The unified block's whole reason to exist: `~/.nub/shims` (PM shims)
        // outranks `~/.nub/current` (the default Node's bin) regardless of which
        // feature wired PATH first, so a shimmed npm keeps routing through nub.
        let home = tmpdir("order");
        let zshrc = home.join(".zshrc");
        assert_eq!(
            add_path_block_for("zsh", &home, None, &["$HOME/.nub/shims", "$HOME/.nub/current"])
                .unwrap(),
            ProfileOutcome::Added(zshrc.clone())
        );
        assert_eq!(
            std::fs::read_to_string(&zshrc).unwrap(),
            "\n# nub path\nexport PATH=\"$HOME/.nub/shims:$HOME/.nub/current:$PATH\"\n",
            "shims precede current in the one managed line"
        );

        // Re-running with a different dir set REWRITES the block in place (a
        // content change is not a no-op), converging on exactly one block.
        assert_eq!(
            add_path_block_for("zsh", &home, None, SHIMS_DIRS).unwrap(),
            ProfileOutcome::Added(zshrc.clone())
        );
        assert_eq!(
            std::fs::read_to_string(&zshrc).unwrap(),
            format!("\n# nub path\n{SHIMS_POSIX}\n"),
            "the block is rewritten, never duplicated"
        );
    }

    #[cfg(unix)]
    #[test]
    fn managed_path_dirs_reflects_on_disk_state_in_priority_order() {
        let home = tmpdir("managed-dirs");
        let nub = home.join(".nub");
        std::fs::create_dir_all(&nub).unwrap();

        assert!(managed_path_dirs(&home).is_empty(), "nothing active");

        std::fs::create_dir_all(nub.join("shims")).unwrap();
        assert_eq!(managed_path_dirs(&home), vec!["$HOME/.nub/shims"]);

        // current is a symlink to a NON-existent target (dangling) — it must still
        // count (symlink_metadata sees the link, not the target), and rank after
        // shims.
        std::os::unix::fs::symlink(home.join("store/bin"), nub.join("current")).unwrap();
        assert_eq!(
            managed_path_dirs(&home),
            vec!["$HOME/.nub/shims", "$HOME/.nub/current"],
            "shims outranks current; a dangling current still counts"
        );

        std::fs::remove_dir_all(nub.join("shims")).unwrap();
        assert_eq!(managed_path_dirs(&home), vec!["$HOME/.nub/current"]);
    }

    #[cfg(unix)]
    #[test]
    fn remove_re_adds_still_active_dirs_in_each_dialect() {
        // Unshim with a default still set: the block keeps ~/.nub/current rather
        // than being fully stripped — in both the posix and fish dialects.
        let home = tmpdir("remove-readd");
        let nub = home.join(".nub");
        std::fs::create_dir_all(&nub).unwrap();
        // current active, shims NOT (the just-removed one).
        std::os::unix::fs::symlink(home.join("store/bin"), nub.join("current")).unwrap();

        let zshrc = home.join(".zshrc");
        std::fs::write(
            &zshrc,
            "# nub path\nexport PATH=\"$HOME/.nub/shims:$HOME/.nub/current:$PATH\"\n",
        )
        .unwrap();
        let xdg = home.join("xdg");
        let fishrc = xdg.join("fish").join("config.fish");
        std::fs::create_dir_all(fishrc.parent().unwrap()).unwrap();
        std::fs::write(
            &fishrc,
            "# nub path\nset -gx PATH $HOME/.nub/shims $HOME/.nub/current $PATH\n",
        )
        .unwrap();

        let changed = remove_path_block_from_profiles(&home, Some(&xdg)).unwrap();
        assert!(changed.contains(&zshrc) && changed.contains(&fishrc));
        assert_eq!(
            std::fs::read_to_string(&zshrc).unwrap(),
            "\n# nub path\nexport PATH=\"$HOME/.nub/current:$PATH\"\n",
            "shims dropped (gone), current re-added in the posix dialect"
        );
        assert_eq!(
            std::fs::read_to_string(&fishrc).unwrap(),
            "\n# nub path\nset -gx PATH $HOME/.nub/current $PATH\n",
            "the fish profile is re-added in the fish dialect"
        );
    }

    #[test]
    fn add_migrates_a_legacy_shims_block_even_when_the_line_is_unchanged() {
        // A pre-unification user (legacy `# nub shims` marker, identical PATH line)
        // re-running `pm shim` must converge to the unified `# nub path` marker —
        // the line-equality idempotency check must not short-circuit the migration.
        let home = tmpdir("migrate");
        let zshrc = home.join(".zshrc");
        std::fs::write(&zshrc, format!("# keep\n\n# nub shims\n{SHIMS_POSIX}\n")).unwrap();

        let outcome = add_path_block_for("zsh", &home, None, SHIMS_DIRS).unwrap();
        assert_eq!(outcome, ProfileOutcome::Added(zshrc.clone()));
        assert_eq!(
            std::fs::read_to_string(&zshrc).unwrap(),
            format!("# keep\n\n# nub path\n{SHIMS_POSIX}\n"),
            "the legacy marker migrates to the unified one, the PATH line unchanged"
        );
    }

    #[test]
    fn strip_managed_is_byte_exact_across_block_positions_and_newline_states() {
        let block = format!("\n# nub path\n{SHIMS_POSIX}\n");

        // No block present → unchanged.
        assert_eq!(strip_managed("just some config\n"), "just some config\n");
        // A `# nub` (installer) block is NOT a managed marker → untouched.
        let installer = "# nub\nexport PATH=\"$HOME/.nub/bin:$PATH\"\n";
        assert_eq!(strip_managed(installer), installer);

        // Original with trailing newline: restored exactly.
        assert_eq!(strip_managed(&format!("a\nb\n{block}")), "a\nb\n");
        // Original without trailing newline: restored exactly (no NL gained).
        assert_eq!(strip_managed(&format!("a\nb{block}")), "a\nb");
        // Empty original (a file created solely for the block): back to empty.
        assert_eq!(strip_managed(&block), "");
        // Block followed by later user content: only our bytes are excised.
        assert_eq!(
            strip_managed(&format!("a\n{block}c\n")),
            "a\nc\n",
            "content appended after our block survives the strip"
        );
        // A half-block (marker but the line was hand-deleted) stops at the marker
        // rather than eating the unrelated next line.
        assert_eq!(
            strip_managed("a\n\n# nub path\nunrelated line\n"),
            "a\nunrelated line\n",
            "a marker without our PATH line must not consume the following line"
        );
        // A user's OWN `.nub/`-containing PATH line under a half-block is NOT eaten
        // — only an exact `.nub/shims` or `.nub/current` line is ours to consume.
        assert_eq!(
            strip_managed("# nub path\nexport PATH=\"/opt/.nub/bin:$PATH\"\nKEEP=1\n"),
            "export PATH=\"/opt/.nub/bin:$PATH\"\nKEEP=1\n",
            "a foreign .nub/ PATH line survives — only our managed dirs are consumed"
        );
        // Migration: the legacy `# nub shims` block is stripped the same way, and
        // a file carrying BOTH a legacy and a unified block is fully cleaned.
        let legacy = format!("\n# nub shims\n{SHIMS_POSIX}\n");
        assert_eq!(strip_managed(&format!("x\n{legacy}")), "x\n");
        assert_eq!(strip_managed(&format!("x\n{legacy}{block}")), "x\n");
    }

    #[cfg(unix)]
    #[test]
    fn find_system_pm_skips_the_shim_dir_and_requires_the_exec_bit() {
        let root = tmpdir("scan");
        let shims = root.join("shims");
        let system = root.join("system");
        std::fs::create_dir_all(&shims).unwrap();
        std::fs::create_dir_all(&system).unwrap();
        write_exec(&shims.join("pnpm"), "#!/bin/sh\n");
        write_exec(&system.join("pnpm"), "#!/bin/sh\n");

        // PATH reaches the shim dir through a SYMLINK — canonical-path
        // equality must still skip it (the recursion guard).
        let alias = root.join("alias");
        std::os::unix::fs::symlink(&shims, &alias).unwrap();
        let path_var = std::env::join_paths([alias.clone(), system.clone()]).unwrap();
        assert_eq!(
            find_system_pm_in("pnpm", &shims, &path_var),
            Some(system.join("pnpm")),
            "the shim dir (reached via symlink) is skipped; the system pnpm is the hit"
        );

        // A file without the exec bit is not a hit.
        std::fs::write(system.join("npm"), "not executable").unwrap();
        assert_eq!(
            find_system_pm_in("npm", &shims, &path_var),
            None,
            "a mode-644 file must not satisfy the PATH scan"
        );

        // A true PATH miss is None — the caller's provision-a-default signal.
        assert_eq!(find_system_pm_in("yarn", &shims, &path_var), None);
    }

    #[cfg(unix)]
    #[test]
    fn scan_skips_empty_path_entries_and_the_running_binary() {
        let root = tmpdir("scan-guard");
        let system = root.join("system");
        std::fs::create_dir_all(&system).unwrap();
        write_exec(&system.join("pnpm"), "#!/bin/sh\n");

        // An EMPTY PATH entry means cwd in POSIX lookup — a fall-through must
        // never search it (with cwd == the shim dir it would re-enter the shim
        // forever; canonicalizing "" fails, so the skip-dir comparison alone
        // can't catch it). The entry is skipped, the later real entry wins.
        // (The cwd-equals-shim-dir loop itself is exercised end-to-end in
        // nub-cli's pm_shim integration tests, where the CHILD's cwd is
        // controlled — mutating this process's cwd would race parallel tests.)
        let path_var: std::ffi::OsString = format!(":{}", system.display()).into();
        assert_eq!(
            scan_path("pnpm", &path_var, None, None),
            Some(system.join("pnpm")),
            "the empty entry contributes nothing; the real entry is the hit"
        );

        // skip_file: a candidate that IS the given file (same inode, any path
        // spelling) is passed over — the last-ditch self-recursion guard.
        let me = system.join("pnpm");
        let hardlinked = root.join("elsewhere");
        std::fs::create_dir_all(&hardlinked).unwrap();
        std::fs::hard_link(&me, hardlinked.join("pnpm")).unwrap();
        let path_var = std::env::join_paths([hardlinked.clone(), system.clone()]).unwrap();
        assert_eq!(
            scan_path("pnpm", &path_var, None, Some(&me)),
            None,
            "both candidates are the same inode as skip_file — no hit"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unshim_edits_through_a_symlinked_profile_without_replacing_the_link() {
        let home = tmpdir("symlink-profile");
        // A dotfiles setup: ~/.zshrc is a symlink into a repo checkout.
        let dotfiles = home.join("dotfiles");
        std::fs::create_dir_all(&dotfiles).unwrap();
        let real = dotfiles.join("zshrc");
        std::fs::write(
            &real,
            format!("export EDITOR=vi\n\n# nub path\n{SHIMS_POSIX}\n"),
        )
        .unwrap();
        std::os::unix::fs::symlink(&real, home.join(".zshrc")).unwrap();

        let changed = remove_path_block_from_profiles(&home, None).unwrap();
        assert_eq!(changed, vec![home.join(".zshrc")]);
        assert!(
            home.join(".zshrc").symlink_metadata().unwrap().is_symlink(),
            "the profile must still be a symlink — replacing it orphans the dotfiles copy"
        );
        assert_eq!(
            std::fs::read_to_string(&real).unwrap(),
            "export EDITOR=vi\n",
            "the block is stripped from the linked-to file"
        );
    }

    #[cfg(unix)]
    #[test]
    fn reachability_reports_the_first_hit_and_flags_shadowing() {
        let root = tmpdir("reach");
        let shims = root.join("shims");
        let brew = root.join("brew");
        std::fs::create_dir_all(&brew).unwrap();
        std::fs::create_dir_all(&shims).unwrap();
        for name in PM_SHIM_NAMES {
            write_exec(&shims.join(name), "#!/bin/sh\n");
        }
        write_exec(&brew.join("pnpm"), "#!/bin/sh\n"); // a Homebrew pnpm earlier on PATH

        let path_var = std::env::join_paths([brew.clone(), shims.clone()]).unwrap();
        let report = check_shims_reachable_in(&shims, &path_var);
        let by_name = |n: &str| report.iter().find(|r| r.name == n).unwrap();
        let pnpm = by_name("pnpm");
        assert_eq!(
            pnpm.first_hit.as_deref(),
            Some(brew.join("pnpm").as_path()),
            "the shadowing binary is named so the CLI can print the exact fix"
        );
        assert!(!pnpm.ok, "a shadowed shim is not ok");
        let npm = by_name("npm");
        assert!(npm.ok, "an unshadowed shim resolves to itself: {npm:?}");

        // Shim dir not on PATH at all: no hit, not ok.
        let off_path = std::env::join_paths([brew]).unwrap();
        let report = check_shims_reachable_in(&shims, &off_path);
        let npm = report.iter().find(|r| r.name == "npm").unwrap();
        assert_eq!(npm.first_hit, None);
        assert!(!npm.ok);
    }

    #[test]
    fn sibling_bin_picks_the_named_entry_from_the_cached_manifest() {
        // The store shape provision_pm produces: <version>/package/{package.json,bin/*}.
        let pkg = tmpdir("sibling")
            .join("pm")
            .join("pnpm")
            .join("9.5.0")
            .join("package");
        std::fs::create_dir_all(pkg.join("bin")).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            r#"{ "name": "pnpm", "bin": { "pnpm": "bin/pnpm.cjs", "pnpx": "bin/pnpx.cjs", "ghost": "bin/ghost.cjs" } }"#,
        )
        .unwrap();
        std::fs::write(pkg.join("bin/pnpm.cjs"), "// pnpm\n").unwrap();
        std::fs::write(pkg.join("bin/pnpx.cjs"), "// pnpx\n").unwrap();
        let primary = pkg.join("bin/pnpm.cjs");

        assert_eq!(
            sibling_bin(&primary, "pnpx").unwrap(),
            pkg.join("bin/pnpx.cjs"),
            "the pnpx entry resolves from pnpm's primary bin"
        );
        assert_eq!(
            sibling_bin(&primary, "pnpm").unwrap(),
            primary,
            "asking for the package-named entry round-trips to the primary"
        );

        // A missing entry errors naming it.
        let err = sibling_bin(&primary, "nope").unwrap_err().to_string();
        assert!(
            err.contains("\"nope\""),
            "a missing bin entry must be named, got: {err}"
        );
        // An entry whose file never landed errors naming the dead path.
        let err = sibling_bin(&primary, "ghost").unwrap_err().to_string();
        assert!(
            err.contains("ghost.cjs") && err.contains("does not exist"),
            "a dangling bin entry must name the missing file, got: {err}"
        );
    }
}
