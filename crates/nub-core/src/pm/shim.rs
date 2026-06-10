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
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::Pm;

// ---------------------------------------------------------------------------
// Shim names and the decision core
// ---------------------------------------------------------------------------

/// The binaries the shim dir intercepts. `nub` itself is linked too (see
/// [`SHIM_NAMES`]) but is dispatched by the existing `Argv0` machinery, not by
/// this enum — these are only the PM-shaped names.
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
    Pinned { pm: Pm, provenance: PinProvenance },
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
///   - empty argv → just the bare PM name (`pnpm`);
///   - a first verb the pinned PM also implements → the project PM with the SAME
///     verb and its remaining args (`pnpm install react`);
///   - a first verb the pinned PM does NOT implement → just `use <pinned-pm>`,
///     with no synthesized verb mapping (`pnpm ci` → "use pnpm").
///
/// Returns the redirect TEXT (without the surrounding message framing) so the
/// CLI owns the prose and this stays a pure, unit-testable decision.
pub fn safe_redirect(pinned: Pm, args: &[String]) -> String {
    let Some(verb) = args.first() else {
        return pinned.to_string();
    };
    if verb_absent_on(pinned, verb) {
        // No honest same-verb suggestion exists — don't fabricate one.
        format!("use {pinned}")
    } else {
        format!("{pinned} {}", args.join(" "))
    }
}

// ---------------------------------------------------------------------------
// Shim dir management
// ---------------------------------------------------------------------------

/// The PM names the shim dir intercepts (also the reachability-check set —
/// `nub` is deliberately not checked there: the official `~/.nub/bin/nub`
/// resolving first is benign, both are nub).
pub const PM_SHIM_NAMES: [&str; 6] = ["npm", "npx", "pnpm", "pnpx", "yarn", "yarnpkg"];

/// Everything [`install_shims`] links: the PM names plus a `nub` hardlink, so
/// `nub pm unshim` still resolves after the official binary is uninstalled
/// (the inode survives while any hardlink remains).
pub const SHIM_NAMES: [&str; 7] = ["npm", "npx", "pnpm", "pnpx", "yarn", "yarnpkg", "nub"];

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
/// is already current. On Windows, replacing a RUNNING shim `.exe` fails (the
/// OS locks executing images) — unverified here, for the future CI leg.
pub fn install_shims_into(dir: &Path, nub_binary: &Path) -> Result<Vec<InstalledShim>> {
    // Serialize concurrent writers (a re-link racing a `pnpm -r` shim storm) on
    // a best-effort advisory lock — see [`ShimLock`]. Held until this returns.
    let _lock = ShimLock::acquire(dir);
    std::fs::create_dir_all(dir).with_context(|| format!("creating shim dir {}", dir.display()))?;
    let mut report = Vec::with_capacity(SHIM_NAMES.len());
    for name in SHIM_NAMES {
        let target = dir.join(shim_file_name(name));
        // Same bytes already (covers the self-link case: re-running `nub pm
        // shim` FROM the shim dir's own nub after the official binary was
        // removed — remove-then-link would delete the link's source).
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

// ---------------------------------------------------------------------------
// Shell-profile PATH block (the install.sh mechanism, ported)
// ---------------------------------------------------------------------------

/// The PATH lines, exactly install.sh's shape (`$HOME`-relative so the profile
/// stays portable across machines), pointing at the SHIMS dir.
pub const SHIMS_POSIX_PATH_LINE: &str = r#"export PATH="$HOME/.nub/shims:$PATH""#;
pub const SHIMS_FISH_PATH_LINE: &str = "set -gx PATH $HOME/.nub/shims $PATH";

/// The block's marker comment. install.sh writes `# nub` above its `~/.nub/bin`
/// line; this is deliberately DISTINCT so `nub pm unshim` strips exactly the
/// shims block and never the installer's.
const BLOCK_MARKER: &str = "# nub shims";

/// Outcome of [`add_path_block`], for the CLI's "what changed" report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileOutcome {
    /// The marked block was appended to this profile (CLI prints a source hint).
    Added(PathBuf),
    /// The profile already carries the PATH line — adding twice is a no-op.
    AlreadyPresent(PathBuf),
    /// No known profile exists / is writable for this shell — the CLI prints
    /// `line` as "add this to your shell config yourself" and exits 0.
    Manual { line: &'static str },
}

/// Append the marked PATH block to the current shell's profile, mirroring
/// install.sh's file selection (`$SHELL` basename → zsh: `~/.zshrc`, created
/// if missing; bash: the first existing+writable of `~/.bashrc`,
/// `~/.bash_profile`, never created; fish: `$XDG_CONFIG_HOME|~/.config`
/// `/fish/config.fish`, created if missing; anything else → manual).
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
    add_path_block_for(&shell, &home, xdg.as_deref())
}

/// [`add_path_block`] with the environment made explicit (the testable body).
pub fn add_path_block_for(
    shell: &str,
    home: &Path,
    xdg_config: Option<&Path>,
) -> Result<ProfileOutcome> {
    match shell_profile(shell, home, xdg_config) {
        Some(target) => append_block(&target),
        None => Ok(ProfileOutcome::Manual {
            line: SHIMS_POSIX_PATH_LINE,
        }),
    }
}

/// One shell's profile target: which file, which line dialect, and whether a
/// missing file may be created (install.sh creates for zsh/fish, never bash).
struct ProfileTarget {
    path: PathBuf,
    line: &'static str,
    may_create: bool,
}

fn shell_profile(shell: &str, home: &Path, xdg_config: Option<&Path>) -> Option<ProfileTarget> {
    match shell {
        "zsh" => Some(ProfileTarget {
            path: home.join(".zshrc"),
            line: SHIMS_POSIX_PATH_LINE,
            may_create: true,
        }),
        // install.sh: `[[ -w $f ]]` — the file must EXIST and be writable;
        // an unwritable .bashrc falls through to .bash_profile.
        "bash" => [".bashrc", ".bash_profile"]
            .iter()
            .map(|f| home.join(f))
            .find(|p| p.is_file() && appendable(p))
            .map(|path| ProfileTarget {
                path,
                line: SHIMS_POSIX_PATH_LINE,
                may_create: false,
            }),
        "fish" => {
            let base = xdg_config
                .map(Path::to_path_buf)
                .unwrap_or_else(|| home.join(".config"));
            Some(ProfileTarget {
                path: base.join("fish").join("config.fish"),
                line: SHIMS_FISH_PATH_LINE,
                may_create: true,
            })
        }
        _ => None,
    }
}

/// Writability probe: opening for append is the honest check (touches nothing).
fn appendable(path: &Path) -> bool {
    std::fs::OpenOptions::new().append(true).open(path).is_ok()
}

/// Append `\n# nub shims\n<line>\n` — byte-for-byte what install.sh's three
/// `echo`s produce for its own block. Idempotency keys on the PATH line itself
/// (trimmed line equality), so a hand-added identical line also counts as
/// present — and is then deliberately NOT ours to remove.
fn append_block(target: &ProfileTarget) -> Result<ProfileOutcome> {
    let existing = match std::fs::read_to_string(&target.path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if !target.may_create {
                return Ok(ProfileOutcome::Manual { line: target.line });
            }
            String::new()
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return Ok(ProfileOutcome::Manual { line: target.line });
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", target.path.display())),
    };
    if existing.lines().any(|l| l.trim() == target.line) {
        return Ok(ProfileOutcome::AlreadyPresent(target.path.clone()));
    }
    if target.may_create {
        if let Some(parent) = target.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
    }
    let mut file = match std::fs::OpenOptions::new()
        .append(true)
        .create(target.may_create)
        .open(&target.path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return Ok(ProfileOutcome::Manual { line: target.line });
        }
        Err(e) => return Err(e).with_context(|| format!("opening {}", target.path.display())),
    };
    write!(file, "\n{BLOCK_MARKER}\n{}\n", target.line)
        .with_context(|| format!("appending to {}", target.path.display()))?;
    Ok(ProfileOutcome::Added(target.path.clone()))
}

/// Strip the marked block from EVERY known profile (zsh + both bash files +
/// fish), not just the current `$SHELL`'s — the user may have switched shells
/// since `nub pm shim` ran. Returns the files that changed. Unreadable /
/// missing profiles are skipped (nothing of ours to strip); write failures
/// propagate. Must keep working when the official nub is gone — it touches
/// only profile files, never `current_exe`'s install dir.
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
    let candidates = [
        home.join(".zshrc"),
        home.join(".bashrc"),
        home.join(".bash_profile"),
        fish_base.join("fish").join("config.fish"),
    ];
    let mut changed = Vec::new();
    for path in candidates {
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Some(stripped) = strip_block(&content) else {
            continue;
        };
        // Temp + rename: a torn write must never truncate a shell profile.
        // The rename targets the CANONICALIZED path — a `~/.zshrc` that is a
        // symlink into a dotfiles repo must stay a symlink, with the edit
        // landing in the linked-to file; renaming onto the symlink path would
        // replace the link with a regular file and orphan the dotfiles copy.
        // Permissions are copied over so a 600 profile stays 600.
        let target = path.canonicalize().unwrap_or_else(|_| path.clone());
        let tmp = target.with_file_name(format!(
            "{}.nub-unshim-{}",
            target.file_name().unwrap_or_default().to_string_lossy(),
            std::process::id()
        ));
        std::fs::write(&tmp, &stripped).with_context(|| format!("writing {}", tmp.display()))?;
        if let Ok(meta) = std::fs::metadata(&target) {
            let _ = std::fs::set_permissions(&tmp, meta.permissions());
        }
        if let Err(e) = std::fs::rename(&tmp, &target) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e).with_context(|| format!("replacing {}", target.display()));
        }
        changed.push(path);
    }
    Ok(changed)
}

/// Remove exactly what [`append_block`] wrote: the marker line, the PATH line
/// under it (only when it really names `.nub/shims`), and the one blank
/// separator line above. `None` = no block, file untouched. A hand-added PATH
/// line without the marker is NOT removed — we only strip what we wrote.
fn strip_block(content: &str) -> Option<String> {
    let lines: Vec<&str> = content.split('\n').collect();
    let mut kept: Vec<&str> = Vec::with_capacity(lines.len());
    let mut changed = false;
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim() == BLOCK_MARKER {
            changed = true;
            if kept.last().is_some_and(|l| l.trim().is_empty()) {
                kept.pop(); // the blank separator the block was appended with
            }
            i += 1; // the marker
            if i < lines.len() && lines[i].contains(".nub/shims") {
                i += 1; // our PATH line
            }
            continue;
        }
        kept.push(lines[i]);
        i += 1;
    }
    changed.then(|| kept.join("\n"))
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
/// real binary found past the shim dir: after an upgrade swaps the official
/// `nub` (new inode), the shim-dir link still carries the OLD bytes — without
/// this passthrough, `nub` (including `nub pm shim` itself, the re-link) would
/// run stale code forever. `None` = not running from the shim dir, or no other
/// `nub` exists (post-uninstall: keep running, `unshim` must still work), or
/// the found binary is the same file (no upgrade happened — no point exec'ing).
pub fn nub_passthrough_target() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = shim_dir().ok()?;
    let canon_dir = dir.canonicalize().ok()?;
    if exe.parent()?.canonicalize().ok()? != canon_dir {
        return None;
    }
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    // skip_file = self: a same-inode hit (official binary not yet upgraded)
    // means there is nothing fresher to defer to.
    scan_path("nub", &path_var, Some(&dir), Some(&exe))
}

/// The on-disk spellings to probe per PATH dir. Windows additionally probes
/// the launcher extensions npm-on-Windows actually ships (`.cmd`) — honest
/// cfg-gated code, runtime-unverified until the Windows CI leg (PATHEXT
/// resolution can't be exercised from macOS/Linux).
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

    /// Unique temp dir under the system temp root (mirrors `resolve.rs`'s
    /// `tmpdir` — never under $HOME, so nothing here can touch real profiles
    /// or the real ~/.nub/shims).
    fn tmpdir(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "nub-shim-{tag}-{}-{}",
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
            safe_redirect(Pm::Pnpm, &v(&["install", "react"])),
            "pnpm install react"
        );
        // `ci` is npm-only: the OLD code blindly suggested `pnpm ci`, which is
        // not a real command. The redirect now drops to a verbless `use pnpm`
        // rather than fabricating a verb pnpm/yarn lack.
        assert_eq!(
            safe_redirect(Pm::Pnpm, &v(&["ci"])),
            "use pnpm",
            "pnpm has no `ci` — suggest the PM, not a nonexistent verb"
        );
        assert_eq!(
            safe_redirect(Pm::YarnBerry, &v(&["ci"])),
            "use yarn",
            "yarn has no `ci` either; Berry still prints `yarn`"
        );
        // npm DOES have `ci` — there the same-verb suggestion is honest.
        assert_eq!(safe_redirect(Pm::Npm, &v(&["ci"])), "npm ci");
        // No argv → bare PM name (nothing to swap).
        assert_eq!(safe_redirect(Pm::Pnpm, &[]), "pnpm");
    }

    #[cfg(unix)]
    #[test]
    fn install_shims_hardlinks_relinks_after_upgrade_and_survives_self_link() {
        use std::os::unix::fs::MetadataExt;
        let root = tmpdir("install");
        let bin = root.join("fake-nub");
        write_exec(&bin, "#!/bin/sh\necho v1\n");
        let shims = root.join("shims");

        // Fresh install: all 7 names created as hardlinks (same inode, no copy).
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

        // Post-uninstall: the official nub is gone; re-running FROM the shim
        // dir's own `nub` hardlink must not delete its own link source.
        std::fs::remove_file(&bin).unwrap();
        let fourth = install_shims_into(&shims, &shims.join("nub")).unwrap();
        assert!(
            fourth.iter().all(|s| s.action == ShimAction::Current),
            "self-link re-run leaves the already-linked entries alone, got {fourth:?}"
        );
        assert!(
            shims.join("nub").is_file() && shims.join("pnpm").is_file(),
            "the shim dir survives a re-link from its own nub"
        );
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
        // A realistic profile that ALREADY carries install.sh's own `# nub`
        // block — unshim must never strip that one.
        let original = "# mine\nexport PATH=\"$HOME/bin:$PATH\"\n\n# nub\nexport PATH=\"$HOME/.nub/bin:$PATH\"\n";
        std::fs::write(&zshrc, original).unwrap();

        assert_eq!(
            add_path_block_for("zsh", &home, None).unwrap(),
            ProfileOutcome::Added(zshrc.clone())
        );
        let with_block = std::fs::read_to_string(&zshrc).unwrap();
        assert_eq!(
            with_block,
            format!("{original}\n# nub shims\n{SHIMS_POSIX_PATH_LINE}\n"),
            "the appended block must be byte-for-byte install.sh's shape"
        );

        // Adding twice is a no-op.
        assert_eq!(
            add_path_block_for("zsh", &home, None).unwrap(),
            ProfileOutcome::AlreadyPresent(zshrc.clone())
        );
        assert_eq!(std::fs::read_to_string(&zshrc).unwrap(), with_block);

        // Removal restores the original byte-for-byte — the installer's
        // `# nub` bin block included.
        assert_eq!(
            remove_path_block_from_profiles(&home, None).unwrap(),
            vec![zshrc.clone()]
        );
        assert_eq!(std::fs::read_to_string(&zshrc).unwrap(), original);

        // Removing again: no block left, no files changed.
        assert!(
            remove_path_block_from_profiles(&home, None)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn profile_selection_mirrors_install_sh_per_shell() {
        let home = tmpdir("select");

        // zsh: a missing .zshrc is created (install.sh's `! -f` arm).
        let outcome = add_path_block_for("zsh", &home, None).unwrap();
        assert_eq!(outcome, ProfileOutcome::Added(home.join(".zshrc")));
        assert_eq!(
            std::fs::read_to_string(home.join(".zshrc")).unwrap(),
            format!("\n# nub shims\n{SHIMS_POSIX_PATH_LINE}\n")
        );

        // fish: XDG_CONFIG_HOME wins, parents are created, the FISH line lands.
        let xdg = home.join("xdg");
        let fish_config = xdg.join("fish").join("config.fish");
        assert_eq!(
            add_path_block_for("fish", &home, Some(&xdg)).unwrap(),
            ProfileOutcome::Added(fish_config.clone())
        );
        let fish_content = std::fs::read_to_string(&fish_config).unwrap();
        assert!(
            fish_content.contains(SHIMS_FISH_PATH_LINE) && !fish_content.contains("export PATH"),
            "fish gets `set -gx`, never the posix line, got: {fish_content}"
        );

        // bash: only ever appends to an EXISTING rc file — none here → manual.
        assert_eq!(
            add_path_block_for("bash", &home, None).unwrap(),
            ProfileOutcome::Manual {
                line: SHIMS_POSIX_PATH_LINE
            }
        );
        // .bash_profile exists (no .bashrc) → it is the one chosen.
        std::fs::write(home.join(".bash_profile"), "# hello\n").unwrap();
        assert_eq!(
            add_path_block_for("bash", &home, None).unwrap(),
            ProfileOutcome::Added(home.join(".bash_profile"))
        );

        // An unknown shell can only be handled manually.
        assert_eq!(
            add_path_block_for("tcsh", &home, None).unwrap(),
            ProfileOutcome::Manual {
                line: SHIMS_POSIX_PATH_LINE
            }
        );

        // unshim sweeps EVERY known profile in one call, whatever $SHELL is now.
        let changed = remove_path_block_from_profiles(&home, Some(&xdg)).unwrap();
        assert_eq!(
            changed.len(),
            3,
            "zsh + bash_profile + fish must all be stripped, got {changed:?}"
        );
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
            format!("export EDITOR=vi\n\n{BLOCK_MARKER}\n{SHIMS_POSIX_PATH_LINE}\n"),
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
