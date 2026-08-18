//! The built-in default ENTRIES — the secret deny-set and trusted-host allows the
//! compiler emits into an ordered list (for `sandbox: true`'s base, the build-jail
//! preset, and the `$trusted`/`$tooldirs` sets). Per .fray/sandbox.md "Built-in
//! defaults are just default ENTRIES, not a floor": these are ordinary last-match-wins
//! entries, so a later user rule can override any of them.
//!
//! The data (secret paths/globs, browser/wallet dirs) is ported verbatim from the
//! reviewed `secrets.rs` in the salvage branches — the §8.5 attack→capability
//! mapping. It is DATA, re-homed under the fresh policy model.

use crate::matcher::path::{
    Homes, canonicalize_glob_prefix, canonicalize_including_nonexistent, expand_symbolic,
    normalize_slashes,
};
use crate::policy::{CanonGlob, Effect, FsAccess, FsOrigin, FsRule};
use std::path::Path;

/// Secret-bearing paths to DENY-READ, resolved under the home anchors. Classic
/// creds, VCS/cloud tokens, the 2024–26 crypto-wallet wave, browser profiles, and
/// the macOS Keychain. Each becomes a subtree Deny entry (path + `path/**`).
const SECRET_READ_RELPATHS: &[&str] = &[
    // classic credentials
    ".ssh",
    ".gnupg",
    ".aws",
    ".netrc",
    ".git-credentials",
    ".config/git/credentials",
    ".docker/config.json",
    ".kube",
    ".config/gcloud",
    ".config/gh",
    ".config/hub",
    ".npmrc",
    ".pgpass",
    ".pypirc",
    // crypto wallets / keystores
    ".config/solana",
    ".config/sui",
    ".aptos",
    ".electrum",
    ".ethereum/keystore",
    ".bitcoin",
    // macOS Keychain (harmless path elsewhere)
    "Library/Keychains",
    // browser profile/cookie dirs (wallet-extension state + session cookies)
    "Library/Application Support/Google/Chrome",
    "Library/Application Support/BraveSoftware",
    "Library/Application Support/Firefox",
    "Library/Application Support/Microsoft Edge",
    ".config/google-chrome",
    ".config/BraveSoftware",
    ".mozilla/firefox",
    ".config/microsoft-edge",
];

/// The default secret-FILE READ-deny globs: any file whose BASENAME starts with `.env`
/// (`.env`, `.env.local`, `.env.production`, `.envrc`, …) OR is an npm config file, at
/// any depth. `.env*` holds the exact secrets the sandbox scrubs from the env; a
/// project-local `.npmrc` (`./.npmrc`, `packages/x/.npmrc`) can hardcode a registry
/// token and would otherwise fall inside the project `./` read grant (the home-anchored
/// `~/.npmrc` deny does not cover it). Both are denied by DEFAULT on every read-granting
/// fs policy (an unconditional floor) — see
/// [`env_deny_leaf_rules`]/[`env_deny_subtree_rules`] and the injection in
/// `fold::finalize_env_deny`. Denying reads is near-zero-breakage: legit code reads
/// secrets via the injected process env / the PM's constructed registry config, not by
/// `fs.read()`-ing the file inside a lifecycle script.
///
/// npm's BUILTIN config file is `<npm>/npmrc` with NO leading dot, so the `.npmrc` globs
/// miss it. It sits at `<node-root>/lib/node_modules/npm/npmrc` — inside the subtree the
/// build jail grants to make `npm`/`npx` resolvable — and while it usually holds only
/// shipped defaults, a managed install can put an auth token there. Denying it keeps the
/// floor consistent: every OTHER npmrc in npm's hierarchy is already denied, so leaving
/// the one undotted spelling readable was an artifact of keying on the dot. Matched at
/// its canonical location rather than by bare basename, which would also deny unrelated
/// files a project happens to call `npmrc`.
///
/// NO LONGER ENFORCED FOR THE BUILD JAIL, and the machinery that used to enforce it there
/// is inert. `preset::enforce_pure_allowlist` strips every deny from a build-jail policy, so
/// `requires_deny_search_roots` returns false for it and the Linux mask walk never runs —
/// which also makes `pm_engine::build_jail`'s `npm_builtin_config_deny_root` seeding (added
/// solely to aim that walk at npm's own dir past `DENY_WALK_SKIP_DIRS`) dead on that path.
///
/// This glob is IRREDUCIBLE under a pure allowlist and is the one build-jail residual with a
/// real credential behind it: the jail must grant `<node-root>/lib/node_modules` wholesale so
/// `npm`/`npx`/`corepack` resolve, `npmrc` sits inside that tree, and withholding one file
/// from inside a granted subtree is precisely the deny-in-grant shape Landlock and Windows
/// AppContainer cannot express. Narrowing the grant instead would mean enumerating npm's tree
/// to exclude one entry — an enumerate-to-exclude scan, which breaks on anything created after
/// the scan and is not the model. Closing it properly means not granting npm's tree at all
/// (e.g. resolving `npm`/`npx` through a shim), which is a separate design change.
///
/// The band still applies in full to every NON-build-jail policy, where it is enforced as
/// before — Linux by masking a granted path, macOS by an SBPL deny.
///
/// The set splits into a LEAF band ([`ENV_DENY_LEAF_GLOBS`] — the file itself) and a
/// SUBTREE band ([`ENV_DENY_SUBTREE_GLOBS`] — `**/.env*/**`, covering a `.env.d/`-style
/// DIRECTORY of per-target secret files; an npmrc is always a file, so it has no subtree
/// twin). Both are appended as the LAST entries so the block is UNCONDITIONAL — no
/// directory grant, glob, or exact allow can reopen a denied file or a `.env*/`
/// directory's contents (sandbox.mdx "`.env` files are always blocked"). See
/// `fold::finalize_env_deny`. Each glob carries a rootless twin mirroring it for a
/// depth-0 match; canonical candidates are absolute, so `**/…` is the form that bites.
///
/// These two arrays are the SINGLE SOURCE OF TRUTH for the floor: [`env_deny_floor_start`]
/// recognizes it POSITIONALLY (last N entries, leaf band then subtree band) and the Linux
/// backend's `is_builtin_env_glob` by membership, and both derive from these constants
/// rather than restating them — a hand-copied list silently desynced on every edit here.
pub(crate) const ENV_DENY_LEAF_GLOBS: &[&str] = &[
    "**/.env*",
    ".env*",
    "**/.npmrc",
    ".npmrc",
    "**/node_modules/npm/npmrc",
    "node_modules/npm/npmrc",
];
pub(crate) const ENV_DENY_SUBTREE_GLOBS: &[&str] = &["**/.env*/**", ".env*/**"];

/// Case-insensitive substring test for a secret name-word anywhere in a key. Used by
/// [`is_npm_config_credential`] for the registry-credential family.
pub fn word_in_substr(word: &str, key: &str) -> bool {
    key.to_ascii_uppercase()
        .contains(&word.to_ascii_uppercase())
}

/// Whole-segment (case-insensitive) test. The key is split on `_`/`-`/`.` and the word
/// must EQUAL one segment, so `key` hits `NPM_CONFIG_KEY` but misses `KEYTAR`. Used by
/// [`is_npm_config_credential`] for the registry-credential family.
pub fn word_is_segment(word: &str, key: &str) -> bool {
    let w = word.to_ascii_uppercase();
    segments(key).contains(&w)
}

/// Split a name into non-empty, upper-cased segments on `_`/`-`/`.` boundaries.
fn segments(s: &str) -> Vec<String> {
    s.split(['_', '-', '.'])
        .filter(|seg| !seg.is_empty())
        .map(str::to_ascii_uppercase)
        .collect()
}

/// Build the default secret-PATH read DENY entries (`~/.ssh`, `~/.aws`, wallets, …).
/// Emitted into `sandbox: true`'s fs base (`fold::secure_default_fs`) and re-asserted by
/// the build-jail preset. Deny access is neutral (Read). The depth-independent `.env*`
/// denies are handled SEPARATELY (the env-deny bands, injected unconditionally as the
/// last entries on every read-granting policy) — see `fold::fold_fs`.
pub fn secret_read_denies(homes: &Homes) -> Vec<FsRule> {
    let mut out = Vec::new();
    for rel in SECRET_READ_RELPATHS {
        let anchored = format!("~/{rel}");
        for g in subtree_globs(&expand_symbolic(&anchored, homes)) {
            out.push(deny(g));
        }
    }
    out
}

/// The LEAF secret-file READ-deny entries ([`ENV_DENY_LEAF_GLOBS`]) — the `.env*` /
/// `.npmrc` file itself. Depth-independent (matched by basename anywhere), NOT anchored
/// under any root. Appended as a trailing band so it beats every prior allow — a broad
/// dir-allow, a glob, OR an exact-file allow — and cannot be reopened (the unconditional
/// floor).
pub(crate) fn env_deny_leaf_rules() -> Vec<FsRule> {
    ENV_DENY_LEAF_GLOBS
        .iter()
        .map(|g| deny(g.to_string()))
        .collect()
}

/// Index where the builtin secret-file floor begins in `entries` — the boundary between
/// the user/default band and the two bands `fold::finalize_env_deny` appends LAST. `None`
/// when the floor is absent (a fully-relaxed or no-read policy) or no longer trailing.
///
/// Exists so a post-fold pass that needs to add band-1 entries can splice BEFORE the floor
/// instead of appending after it. Displacing the floor is not a matching bug (deny order
/// among denies is irrelevant) but it defeats this positional recognizer, which the Linux
/// backend calls to tell an explicit USER `.env` deny from the builtin floor when planning
/// the dotenv mask kind — a floor no longer trailing reads as "absent", silently
/// downgrading an explicit deny's genuinely-unreadable mask to the present-but-empty
/// dotenv shape.
#[cfg(target_os = "linux")]
pub(crate) fn env_deny_floor_start(entries: &[FsRule]) -> Option<usize> {
    let floor_len = ENV_DENY_LEAF_GLOBS.len() + ENV_DENY_SUBTREE_GLOBS.len();
    let start = entries.len().checked_sub(floor_len)?;
    ENV_DENY_LEAF_GLOBS
        .iter()
        .chain(ENV_DENY_SUBTREE_GLOBS)
        .enumerate()
        .all(|(offset, glob)| {
            let rule = &entries[start + offset];
            rule.effect == Effect::Deny && rule.matcher.as_str() == *glob
        })
        .then_some(start)
}

/// The SUBTREE `.env*` READ-deny entries ([`ENV_DENY_SUBTREE_GLOBS`]) — the CONTENTS of
/// a `.env*`-named directory. Injected as the LAST band, so it is unconditionally
/// authoritative for a `.env.d/`-style secret directory: nothing reopens its children.
pub(crate) fn env_deny_subtree_rules() -> Vec<FsRule> {
    ENV_DENY_SUBTREE_GLOBS
        .iter()
        .map(|g| deny(g.to_string()))
        .collect()
}

/// The policy SOURCE-FILE self-exclusion DENY — an EXACT-path deny (read AND write) on ONE
/// file the sandbox rules were sourced from (the caller emits one per `ctx.policy_files`
/// entry), so a sandboxed process can neither read nor tamper with the policy that confines
/// it. Canonicalized through the EXACT same
/// path the matcher runs a candidate through (`canonicalize_including_nonexistent` — resolve
/// symlinks / firmlinks, strip the Windows `\\?\` verbatim prefix, survive a non-existent
/// tail) then slash-normalized, so (a) a grant reaching the file via a different spelling
/// still hits this deny, and (b) the rule string is byte-identical to the candidate string
/// the matcher builds — a plain literal, never the verbatim prefix whose `?` would read as a
/// glob metachar and break the match. Deny access is the inert canonical [`FsAccess::DENY`].
/// Injected as the last user/default entry BEFORE the `.env*` floor (see
/// `fold::finalize_policy_file_deny`), so last-match-wins beats any prior allow — including a
/// broad `fs: ["."]`.
///
/// The literal path is glob-ESCAPED ([`globset::escape`]) before it becomes the pattern: a
/// real path may contain glob metachars (`[id]`/`[...slug]` Next.js segments, a `{a,b}`
/// directory), and an unescaped `[`/`{`/`*`/`?` would be read as a pattern that does NOT
/// match the literal candidate — a silent fail-OPEN leaving the policy file readable. The
/// candidate side is a literal subject string, so escaping ONLY the deny pattern matches it
/// exactly with no sibling over-match.
pub(crate) fn policy_file_deny_rule(policy_file: &Path) -> FsRule {
    let canon = canonicalize_including_nonexistent(policy_file);
    let normalized = normalize_slashes(&canon.to_string_lossy());
    FsRule {
        matcher: CanonGlob(globset::escape(&normalized)),
        effect: Effect::Deny,
        access: FsAccess::DENY,
        origin: FsOrigin::Authored,
    }
}

/// The generous read base entry: allow everything, then the secret denies (added
/// by the caller after this) tighten it. Emitted for the wrapper `true` /
/// spread-of-defaults read posture.
pub fn generous_read_allow() -> FsRule {
    FsRule {
        matcher: CanonGlob("**".to_string()),
        effect: Effect::Allow,
        access: FsAccess::Read,
        origin: FsOrigin::Authored,
    }
}

/// A subtree grant expands to two globs — the node itself and everything under
/// it — so a bare path like `~/.ssh` denies both `~/.ssh` and `~/.ssh/id_rsa`.
/// A pattern already carrying a glob metachar is emitted as-is (no `/**` suffix).
pub fn subtree_globs(expanded: &str) -> Vec<String> {
    if expanded.contains(['*', '?', '[', '{']) {
        return vec![expanded.to_string()];
    }
    // `/` is the whole-filesystem spelling every backend recognizes (`is_whole_fs` /
    // `is_whole_root` accept it), but the globs it expands to — `""` and `/**` — are
    // drive-less, and a drive-less path matches nothing on Windows. So `fs: { "/": "r" }`
    // compiled to rules no candidate could hit and the grant silently evaporated, while
    // the `""` half reached `literal_subtree` as a Some(empty path) the Windows ACL
    // planner would try to grant. `**` is the drive-agnostic spelling of the same rule and
    // is already classified as whole-fs everywhere. Matched exactly, not via the trimmed
    // form: `//` and a slash-normalized bare `\\` are not root spellings on Windows.
    #[cfg(windows)]
    if expanded == "/" {
        return vec!["**".to_string()];
    }
    let trimmed = expanded.trim_end_matches('/');
    vec![trimmed.to_string(), format!("{trimmed}/**")]
}

/// Whole-disk READ for the build jail, emitted as an ALLOW-SET THAT EXCLUDES THE SECRET
/// SUBTREES — the replacement for a bare `**` read allow.
///
/// ⛔ WHY NOT `**` PLUS A DENY. The build jail compiles to a pure allowlist
/// (`preset::enforce_pure_allowlist`, the LAST step of its compile) because Landlock has no
/// deny primitive at any ABI and a deny-ACE naming an AppContainer's own SID is inert against
/// that AppContainer's child. Every deny is therefore stripped, and a `**` read allow hands a
/// jailed lifecycle script every secret this module names. MEASURED on macOS arm64, two arms
/// differing only in the grant: with no catalog entry a script gets EPERM on `~/.ssh`; under
/// `read:"disk"` it reads the file.
///
/// An allowlist cannot subtract, so the exclusion is expressed POSITIVELY: descend from `/`,
/// and at each level allow every child outright EXCEPT the ones that lead to a secret, which
/// are recursed into instead. A path the walk never names is simply never granted.
///
/// ⛔ THIS CLOSES THE `$HOME`-ANCHORED SUBTREES ONLY, AND THAT IS A REAL LIMIT RATHER THAN AN
/// OVERSIGHT. [`ENV_DENY_LEAF_GLOBS`] is a depth-INDEPENDENT basename match — `.env*` under any
/// root at any depth — and no finite allow set expresses "everything except files named `.env*`
/// anywhere". That band cannot be recovered here; preserving it needs a per-backend rendering
/// step, because the compile is currently backend-agnostic.
pub fn disk_minus_secrets_read_allows(homes: &Homes) -> Vec<FsRule> {
    let mut out = Vec::new();
    descend_allowing_all_but(
        Path::new("/"),
        &secret_paths(homes),
        FsAccess::Read,
        &mut out,
    );
    out
}

/// The `userHome` SCOPE's allow set: everything under `$HOME` except the secret subtrees, at
/// `access`. The same complement idiom as [`disk_minus_secrets_read_allows`], one root down.
///
/// ⛔ THIS EXISTS BECAUSE THE SCOPE USED TO BE A BARE `push_rw($HOME)` AND THAT HANDED OVER
/// `~/.ssh`. MEASURED on macOS with a fresh override-feature binary and a passing negative
/// control: `{"network":true}` gave `REAL_SSH_EPERM`, while `read:{userHome}` and
/// `write:{userHome}` both gave `REAL_SSH_READABLE`. `enforce_pure_allowlist` drops every deny on
/// all three platforms, so the secret floor could never have survived to override a `$HOME`
/// subtree allow — the exclusion has to be built into the allow set or it does not exist.
///
/// It matches the intent `CuratedGrant::home_paths` already documents for v1 ("it reads nothing
/// else under `$HOME`") and the prior art that comment cites: Homebrew grants the real `$HOME`
/// with `deny_read_home` plus a curated allowlist. v1 never had the hole because it only ever
/// grants NAMED home directories; v2's scope is what reintroduced it.
///
/// WRITE gets the same complement as READ, deliberately. A script that can WRITE `~/.ssh` can
/// append its own key to `authorized_keys`, which is persistence rather than a build need.
///
/// Gated on the FEATURE alone, not `any(feature, test)` like its sibling: `apply_v2_grant` is its
/// only caller, and both are now compiled into every build because every build bakes a v2 catalog —
/// so the gate that used to keep this callerless (and tripping `-D warnings`) is gone.
pub fn home_minus_secrets_allows(homes: &Homes, access: FsAccess) -> Vec<FsRule> {
    let mut out = Vec::new();
    descend_allowing_all_but(&homes.home, &secret_paths(homes), access, &mut out);
    out
}

/// Every path the complement walks must leave out: the `$HOME`-anchored subtree secrets, plus
/// the `.env*`/`.npmrc` leaves resolved to real paths.
///
/// ⛔ THE `.env` BAND, RECOVERED AS FAR AS AN ALLOWLIST CAN CARRY IT. [`ENV_DENY_LEAF_GLOBS`] is a
/// depth-INDEPENDENT basename match, and no finite allow set expresses "everything except files
/// named `.env*` anywhere" — so the band cannot be preserved in general. What CAN be preserved is
/// the case that actually matters: a dependency's install script reading the CONSUMING PROJECT's
/// `.env`. Those files exist on disk at compile time, so they resolve to exact paths the walk can
/// exclude by name.
///
/// MEASURED before this existed: under a `read:"disk"` grant a jailed lifecycle script read the
/// project's `.env` in full. Enumerated rather than globbed because the walk compares real paths.
///
/// ⛔ EXCLUDING MORE THAN NEEDED IS THE SAFE DIRECTION HERE, and the asymmetry is the reason: it
/// withholds a file the jail denies for every other package anyway, and §0's "over-granting is
/// safe, under-granting breaks installs" is about capabilities a build NEEDS, which a credential
/// file is not.
fn secret_paths(homes: &Homes) -> Vec<std::path::PathBuf> {
    let mut secrets: Vec<std::path::PathBuf> = SECRET_READ_RELPATHS
        .iter()
        .map(|rel| homes.home.join(rel))
        .collect();
    for root in [&homes.project, &homes.home] {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            if is_env_secret_name(&entry.file_name().to_string_lossy()) {
                secrets.push(entry.path());
            }
        }
    }
    secrets
}

/// One level of [`disk_minus_secrets_read_allows`]. `dir` itself is granted NON-recursively so
/// the child grants below it are reachable — traversing a parent is a precondition for opening
/// anything beneath it, and granting the parent's whole subtree here would re-admit the very
/// secrets the walk exists to leave out.
///
/// Recursion is bounded by the depth of the secret paths themselves (three segments at most),
/// because only a directory that LEADS to a secret is ever descended into. An unreadable
/// directory is skipped rather than failing the compile: the jailed script could not have read
/// it either, so omitting it withholds nothing it would otherwise have had.
fn descend_allowing_all_but(
    dir: &Path,
    secrets: &[std::path::PathBuf],
    access: FsAccess,
    out: &mut Vec<FsRule>,
) {
    // ⛔⛔ NEVER SELF-GRANT THE FILESYSTEM ROOT, AND THIS IS NOT A MICRO-OPTIMISATION — IT WAS THE
    // BUG. `canonicalize_glob_prefix("/")` collapses to the EMPTY STRING, and `""` is one of the
    // spellings `is_whole_root` accepts, so emitting it handed back the exact whole-root read this
    // walk exists to avoid. MEASURED: with the root self-grant present the unit tests passed and a
    // jailed script under `read:"disk"` STILL read `~/.ssh` — all 810 careful exclusions undone by
    // the first rule. Children stay reachable without it: Landlock and Seatbelt both grant a
    // subtree without needing a separate rule for its ancestors.
    if dir.parent().is_some() {
        out.push(allow_at(dir.to_string_lossy().into_owned(), access));
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let child = entry.path();
        if secrets.contains(&child) {
            continue;
        }
        if is_unrepresentable_grant(&child) {
            continue;
        }
        if secrets.iter().any(|secret| secret.starts_with(&child)) {
            descend_allowing_all_but(&child, secrets, access, out);
        } else {
            out.push(allow_at(format!("{}/**", child.to_string_lossy()), access));
        }
    }
}

/// Whether the walk must leave `child` out entirely because no grant it could emit would be
/// REPRESENTABLE to the Linux mount planner.
///
/// The asymmetry that decides this is the whole point: emitting an unrepresentable path does not
/// merely widen or narrow one directory, it makes `compile_mount_plan` return `Err` for the WHOLE
/// policy — and the Linux build jail is Landlock-or-nothing and fail-closed, so the lifecycle
/// script then does not run at all. Skipping costs one directory's readability, which a build
/// almost never needs. So this fails toward omission.
///
/// Two cases, both MEASURED against the real planner rather than reasoned about:
///
/// - A RESERVED KERNEL TREE ([`RESERVED_KERNEL_TREES`]). The walk names every non-secret child of
///   `/`, so it emitted `/dev/**` on macOS and `/dev/**` + `/proc/**` + `/sys/**` on Linux, and
///   the mount plan refused. This one is also right on SECURITY grounds independent of the
///   mechanism: reading `/proc` wholesale is a same-uid disclosure channel — every other
///   process's `environ` and `cmdline`, the user's shell and editor included — and
///   `linux_landlock`'s `PROC_READ_PATHS` grants eight specific global files precisely to avoid
///   that. A `/proc/**` grant here would undo that narrowing in one rule. Nothing is lost by
///   omitting these: the device nodes and `/proc` files a toolchain needs are granted as explicit
///   leaf rules by the backend, independent of this walk.
/// - A NAME CARRYING A GLOB METACHARACTER. The walk emits an unescaped literal and the planner
///   demands a bounded one, so a single real directory called `weird[1]name` anywhere the walk
///   enumerates took the entire rung down. Skipping the child rather than the descent is
///   deliberate: nothing under it is granted either, so a secret beneath such a directory stays
///   unreachable rather than being silently admitted.
fn is_unrepresentable_grant(child: &Path) -> bool {
    if RESERVED_KERNEL_TREES
        .iter()
        .any(|root| child == Path::new(root))
    {
        return true;
    }
    child.to_string_lossy().contains(['*', '?', '[', '{'])
}

/// See [`is_reserved_kernel_tree`]. Shared with the Linux mount planner so the set the walk omits
/// and the set the planner refuses cannot drift apart. Ungated: the walk is now compiled on every
/// target because every build bakes a v2 catalog, so the old union-of-consumers' cfg would have to
/// name every target anyway.
pub(crate) const RESERVED_KERNEL_TREES: &[&str] = &["/proc", "/sys", "/dev"];

/// Does this FILE NAME belong to the builtin env-secret floor? DERIVED from
/// [`ENV_DENY_LEAF_GLOBS`] rather than restating its members, for exactly the reason that
/// constant's own doc gives: a hand-copied list silently desyncs on every edit there. Each glob's
/// LEAF is taken (`**/.env*` → `.env*`), and a trailing `*` becomes a prefix test.
///
/// `node_modules/npm/npmrc` reduces to the bare name `npmrc`, so a file called exactly that is
/// treated as a secret wherever it sits. That is broader than the glob and deliberately so: the
/// only consequence is withholding READ of a file the jail denies for every other package anyway.
fn is_env_secret_name(name: &str) -> bool {
    ENV_DENY_LEAF_GLOBS.iter().any(|glob| {
        let leaf = glob.rsplit('/').next().unwrap_or(glob);
        match leaf.strip_suffix('*') {
            Some(prefix) => name.starts_with(prefix),
            None => name == leaf,
        }
    })
}

/// A speculative Allow at `access`. [`FsOrigin::Speculative`] because the complement walk names
/// real paths on a real host, and `compile_mount_plan` REFUSES a missing AUTHORED source.
fn allow_at(glob: String, access: FsAccess) -> FsRule {
    FsRule {
        matcher: CanonGlob(canonicalize_glob_prefix(&glob)),
        effect: Effect::Allow,
        access,
        origin: FsOrigin::Speculative,
    }
}

fn deny(glob: String) -> FsRule {
    FsRule {
        matcher: CanonGlob(canonicalize_glob_prefix(&glob)),
        effect: Effect::Deny,
        access: FsAccess::DENY,
        origin: FsOrigin::Authored,
    }
}

/// Non-secret operational env keys that pass through in the `sandbox: true`
/// curated baseline: PATH + system/locale/toolchain-discovery vars + the
/// build-hint `npm_config_*` subset. Ambient secrets never ride this list. The
/// exact baseline is the deferred build-jail thread's product surface; this is a
/// usable, safe default for the frontend-less engine.
///
/// The Windows container-essential block (`SystemRoot` … `PROCESSOR_ARCHITECTURE`)
/// is load-bearing: `CreateProcessW` with a constructed environment block that
/// omits `SystemRoot` fails `ERROR_ENVVAR_NOT_FOUND` (the loader resolves system
/// DLLs relative to it), and a normal Windows exe (node.exe) needs the
/// `USERPROFILE`/`APPDATA`/`LOCALAPPDATA` family to resolve its home/temp/config.
/// The `ProgramFiles`/`ProgramFiles(x86)`/`ProgramW6432`/`ProgramData` OS-location
/// vars are non-secret and load-bearing for native builds: node-gyp's Python and
/// toolchain discovery reads them (`find-python.js`), falling back to a wrong path on
/// a non-C: / relocated install when they are absent. These names never appear on unix
/// (the filter is over the ambient env, so the baseline stays OS-appropriate without a
/// `cfg`).
const BASELINE_ENV_EXACT: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "PWD",
    "TERM",
    "TZ",
    "LANG",
    "LC_ALL",
    "TMPDIR",
    "TEMP",
    "TMP",
    // Windows container-essential (see the doc note above).
    "SystemRoot",
    "SystemDrive",
    "windir",
    "ComSpec",
    "PATHEXT",
    "USERPROFILE",
    "LOCALAPPDATA",
    "APPDATA",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
    // Windows OS-location vars for native-build toolchain discovery (see doc note).
    "ProgramFiles",
    "ProgramFiles(x86)",
    "ProgramW6432",
    "ProgramData",
];
const BASELINE_ENV_PREFIXES: &[&str] = &["LC_", "npm_config_"];

/// The `npm_config_` prefix carries BOTH build hints (kept) and registry
/// CREDENTIALS (must never reach sandboxed code). See [`is_npm_config_credential`].
const NPM_CONFIG_PREFIX: &str = "npm_config_";

/// Unambiguous credential words in an `npm_config_*` key — long/specific enough that
/// a case-insensitive SUBSTRING hit has no realistic collision with a node-gyp /
/// node-pre-gyp build hint (none of which embed these). Case-insensitive substring
/// discipline (via [`word_in_substr`]), scoped to the registry-credential family.
const NPM_CRED_SUBSTR_TOKENS: &[&str] = &[
    "token",
    "secret",
    "password",
    "passwd",
    "credential",
    "apikey",
];

/// Short/ambiguous credential words matched ONLY as a whole `_`/`-`/`.` segment, so
/// `npm_config_key` (npm's inline registry client SSL key) and `npm_config_api_key`
/// scrub while a package binary-host hint whose name merely contains the letters
/// (`npm_config_keytar_binary_host_mirror`, `..._monkey_...`) is spared. `auth` is
/// deliberately NOT here — it stays the anchored `_auth` test below so `always-auth` /
/// `_author` are not swept.
const NPM_CRED_SEGMENT_TOKENS: &[&str] = &["key"];

/// Whether an `npm_config_*` key (given the part AFTER the `npm_config_` prefix) is a
/// registry CREDENTIAL rather than a build hint — such keys must never reach sandboxed
/// lifecycle code (.fray/sandbox.md thread #6: registry auth never rides the lifecycle
/// env). Three tiers, all case-insensitive + delimiter-aware. First, the anchored legacy
/// markers: `_auth*` (the leading `_` spares `always-auth` / `_author`) and `email` as the
/// whole key or a registry-scoped `:email` suffix (an unanchored `email` would wrongly
/// scrub `npm_config_nodemailer_binary_host_mirror`). Then the unambiguous credential
/// words ([`NPM_CRED_SUBSTR_TOKENS`]) anywhere in the key — catching `password` /
/// `_authToken` / scoped `//host/:_password` and the undelimited `foo_token` / `my_secret`
/// forms an exact-segment rule would miss. Finally the short `key` family as a whole
/// segment ([`NPM_CRED_SEGMENT_TOKENS`]). Kept build hints
/// (`target`/`arch`/`runtime`/`nodedir`/`python`/`*_binary_host_mirror`/…) match none.
/// Best-effort per §8: the rare native package literally named after a credential word
/// loses its binary-host MIRROR hint (falling back to the default host), acceptable next
/// to leaking a token.
fn is_npm_config_credential(remainder: &str) -> bool {
    let r = remainder.to_ascii_lowercase();
    if r.contains("_auth") || r == "email" || r.ends_with(":email") {
        return true;
    }
    if NPM_CRED_SUBSTR_TOKENS
        .iter()
        .any(|w| word_in_substr(w, remainder))
    {
        return true;
    }
    NPM_CRED_SEGMENT_TOKENS
        .iter()
        .any(|w| word_is_segment(w, remainder))
}

/// Build the curated-baseline child env from the ambient env (the `sandbox: true`
/// / build-jail env posture). Only the non-secret operational allowlist passes.
pub fn curated_baseline_env(
    ambient: &std::collections::BTreeMap<String, String>,
) -> std::collections::BTreeMap<String, String> {
    ambient
        .iter()
        .filter(|(k, _)| baseline_allows(k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Whether an env key is credential-shaped. Under the build-jail's default-deny
/// allowlist ([`build_jail_env_allowed`]) this is a BELT-AND-SUSPENDERS reject, not
/// the primary control: the allowlist already admits only known-safe namespaces, and
/// this rejects any credential-shaped name that somehow rides an allowed prefix in. It
/// also backs [`baseline_allows`]'s `npm_config_*` carve-out. Two tiers, both
/// delimiter/case-aware: an `npm_config_*` key routes to the registry-credential
/// predicate ([`is_npm_config_credential`], which also scrubs `_auth*`/`email`/`key`),
/// and any other key is scrubbed when it CONTAINS an unambiguous credential word
/// (`token`/`secret`/`password`/`passwd`/`credential`/`apikey`) or carries `auth` as a
/// delimited segment (`AUTH_TOKEN`/`GITHUB_AUTH`, sparing `AUTHOR`). Best-effort by
/// NAME — what actually keeps the general credential family (`*_API_KEY`,
/// `AWS_ACCESS_KEY_ID`, `PRIVATE_KEY`, `DATABASE_URL`, …) out is the allowlist NOT
/// admitting them, since a denylist structurally misses names with no credential word.
pub fn is_credential_env_key(key: &str) -> bool {
    if let Some(rest) = strip_prefix_ci(key, NPM_CONFIG_PREFIX) {
        return is_npm_config_credential(rest);
    }
    if NPM_CRED_SUBSTR_TOKENS
        .iter()
        .any(|w| word_in_substr(w, key))
    {
        return true;
    }
    word_is_segment("auth", key)
}

/// Env-name PREFIXES admitted into the build-jail lifecycle env on top of
/// [`baseline_allows`]: the package-metadata and lifecycle namespaces npm/aube set for
/// a dependency's build script. `npm_config_*` (with its credential carve-out) and
/// `LC_*` already come through [`baseline_allows`]; these are the build-jail additions.
const BUILD_JAIL_EXTRA_PREFIXES: &[&str] = &["npm_package_", "npm_lifecycle_"];

/// Exact env names admitted into the build-jail lifecycle env on top of
/// [`baseline_allows`]. Three groups:
///  - Lifecycle re-invocation / install-root discovery: `NODE` + `npm_node_execpath`
///    (a build script re-runs the SAME node), `INIT_CWD` (npm's invocation dir).
///  - Proxy config for prebuilt-binary fetchers (node-pre-gyp / prebuild-install),
///    both cases — POSIX tools split by convention (curl reads lowercase, others upper).
///  - Non-secret native-toolchain config a real addon linking a system library needs
///    (node-gyp/gyp/make/cc/ld honor these). The runtime-linker-injection family
///    (`LD_LIBRARY_PATH`/`DYLD_*`) is DELIBERATELY EXCLUDED: link-time lib dirs ride
///    `LDFLAGS`, and `*_LIBRARY_PATH` is a runtime-injection surface, not a build need.
///
/// PATH/HOME/TMPDIR/TEMP/TMP + the OS-essential + locale/operational set come through
/// [`baseline_allows`]; of those, only PATH/HOME/TMPDIR were EMPIRICALLY load-bearing
/// for a from-source node-gyp compile (HOME anchors the header cache on a clean host).
///
/// `__NUB_NODE_GYP_EXE` is DELIBERATELY absent. The engine stamps it so its lazy
/// `npm_config_node_gyp` shim can trampoline back through the PM binary
/// (`<exe> __node-gyp-bootstrap <dir>`), but that trampoline is neither needed nor safe
/// here: the engine bootstraps the real node-gyp OUTSIDE the jail and prepends its
/// `$tooldirs`-readable bin dir to the lifecycle PATH, and the shim resolves that
/// bootstrapped copy DIRECTLY from its own location, never by a PATH lookup — measured
/// end-to-end, a from-source addon links under the jail through both the bare-`node-gyp`
/// and the `npm_config_node_gyp` route.
///
/// A PATH lookup is the LAST-RESORT branch, not the mechanism, and `8f7ac1033f` is why:
/// PATH is exactly the namespace npm's run-script rewrites, so resolving `node-gyp` by
/// name walked into npm's own stub, which re-exports the inherited `npm_config_node_gyp`
/// — i.e. back into this shim. Withholding the var is what made that cycle the ONLY
/// reachable branch here, so the jail was causal for BUG-C via this very scrub.
/// Admitting the var would REPLACE that working fallback with an exec of the PM binary,
/// which no read grant covers, and the shim does not guard that call. Whether that is
/// survivable is BACKEND-DEPENDENT, so it must not be assumed: under macOS Seatbelt the
/// binary is unreadable yet still executable (the base profile allows `process-exec`
/// outright), and the trampoline was measured to succeed; under a Linux bwrap namespace
/// an ungranted path is simply not mounted, so the same exec is ENOENT and the compile
/// hard-fails with no fallback left. Admitting the var therefore requires read-granting
/// the binary in the same change — see `build_jail_withholds_the_node_gyp_trampoline_exe`.
const BUILD_JAIL_EXTRA_EXACT: &[&str] = &[
    // ⛔⛔ THE JAIL STAMPS ALL THREE, and without these entries the stamps were INERT — this
    // scrub dropped them before the child ever saw them. Same class as `NODE_OPTIONS` below:
    // safe ONLY because `build_jail.rs` writes nub's own value into `ambient` (and purges every
    // case-variant) before the scrub runs, so an ambient user value is already overwritten and
    // cannot ride the entry in. The entry and the stamp move together, in both directions.
    //
    // MEASURED, which is how the inertness surfaced: electron-chromedriver@43.2.0 re-measured on
    // 0d9c2c575b — a binary that CARRIES `redirect_electron_cache` — stayed at 55 cells
    // write:"disk", and its restored-over-runner-up paths were still the DEFAULT location:
    //     home/AppData/Local/electron/Cache/<sha>/chromedriver-v43.2.0-win32-x64.zip
    //     home/AppData/Local/electron/Cache/<sha>/SHASUMS256.txt
    // The consumer does forward the knob (`download-chromedriver.js:12` passes
    // `cacheRoot: process.env.electron_config_cache`) and the redirect's target sits under
    // `$cache/nub/pm/tools`, which is granted at EVERY rung — so had the variable arrived, the
    // download would have landed somewhere already granted. It did not arrive.
    //
    // ⛔ `npm_config_prefix` needs NO entry: it rides `baseline_allows`' `npm_config_*` prefix
    // carve-out. That asymmetry is exactly why one of the three redirects appeared to work while
    // the other two did not — it is not evidence that their premises were wrong.
    "electron_config_cache",
    "ELECTRON_CACHE",
    "PLAYWRIGHT_BROWSERS_PATH",
    "NODE",
    // The jail STAMPS this (`build_jail.rs`), so it must survive the scrub. A dependency's
    // lifecycle script runs on vanilla Node: nub's preload is a developer-facing
    // augmentation a published postinstall never asked for, and loading nub's runtime —
    // including the dlopen'd native addon — into untrusted code is surface the jail exists
    // to remove. Bubblewrap already behaved this way by accident (its mount view hides
    // nub's runtime dir, so preload discovery found nothing and degraded); Landlock leaves
    // the dir VISIBLE-but-unreadable, so discovery succeeded and the read then hard-failed.
    "NODE_COMPAT",
    // Also STAMPED by the jail, and in the same silent-if-dropped class as the MSVC trio
    // below: it names the Node the package's own pin chain asked for, resolved OUT of the
    // jail because the confined shim can reach none of discovery's answers — not `~/.nvm`,
    // not nub's store, not nodejs.org. Withheld, the child re-runs that walk and fails
    // closed on a version the host may already have unpacked. The value is one nub itself
    // resolved, never an ambient a dependency could aim elsewhere: `build_jail.rs`
    // overwrites the key unconditionally whenever the pin chain yields a pin.
    "NODE_EXECUTABLE",
    "npm_node_execpath",
    "INIT_CWD",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "CC",
    "CXX",
    "CPP",
    "LD",
    "AR",
    "AS",
    "NM",
    "RANLIB",
    "STRIP",
    "CFLAGS",
    "CXXFLAGS",
    "CPPFLAGS",
    "LDFLAGS",
    "CPATH",
    "C_INCLUDE_PATH",
    "CPLUS_INCLUDE_PATH",
    "LIBRARY_PATH",
    "PKG_CONFIG",
    "PKG_CONFIG_PATH",
    "MAKE",
    "MAKEFLAGS",
    "PYTHON",
    "GYP_DEFINES",
    // macOS SDK / deployment-target overrides a real native build sets (non-secret;
    // absent → the toolchain's default SDK/min-OS, so a package pinning either needs them).
    "SDKROOT",
    "MACOSX_DEPLOYMENT_TARGET",
    // The Windows MSVC trio `node-gyp`'s `findVisualStudio` short-circuits on. WITHOUT THESE
    // THE JAIL CANNOT BUILD FROM SOURCE AT ALL: its only other discovery routes are a COM
    // server an AppContainer may not activate and a PowerShell module it cannot see, so the
    // pre-resolved answer `pm_engine::jail_msvc` stamps is the whole mechanism. Non-secret
    // toolchain path/version pointers, the same class as `SDKROOT` and `PYTHON` above, so an
    // ambient value (a developer command prompt's) rides in on the same terms — which is also
    // the behaviour node-gyp is built around. Inert on POSIX, where nothing sets them, exactly
    // as the two macOS names above are.
    "VCINSTALLDIR",
    "VSCMD_VER",
    "WindowsSDKVersion",
];

/// Whether an env key is admitted into the build-jail's lifecycle env — the
/// DEFAULT-DENY allowlist that replaced the old credential denylist. A denylist
/// structurally missed the general credential family (`OPENAI_API_KEY`,
/// `AWS_ACCESS_KEY_ID`, `PRIVATE_KEY`, `SSH_KEY`, `GH_PAT`, `DATABASE_URL`, …) — none
/// carry a credential WORD, so all passed straight through. This admits the curated
/// baseline ([`baseline_allows`] — OS/startup essentials, locale/operational vars,
/// `npm_config_*` minus registry credentials) plus the build-jail additions
/// ([`BUILD_JAIL_EXTRA_PREFIXES`]/[`BUILD_JAIL_EXTRA_EXACT`]); everything else is
/// DENIED. [`is_credential_env_key`] is applied first as a belt-and-suspenders reject
/// so a credential-shaped name can never ride an allowed prefix in. Case-sensitive on
/// POSIX, case-insensitive on Windows (the env-name contract).
pub fn build_jail_env_allowed(key: &str) -> bool {
    if is_credential_env_key(key) {
        return false;
    }
    if baseline_allows(key) {
        return true;
    }
    #[cfg(windows)]
    {
        // WINDOWS ONLY, and safe ONLY because the jail STAMPS it: `build_jail.rs` writes
        // nub's own value into `ambient` before this scrub runs, so an ambient user value is
        // already overwritten and can never ride this entry in. `NODE_OPTIONS` carries
        // `--import`, so admitting an ambient one would hand a dependency's lifecycle script
        // an arbitrary-code channel. The entry and the stamp move together, in both
        // directions: this was removed once when the stamp was withdrawn, and comes back with
        // the stdio shim (`windows_build_jail_node_options`). No other platform stamps it, and
        // none may admit it.
        if key.eq_ignore_ascii_case("NODE_OPTIONS") {
            return true;
        }
        BUILD_JAIL_EXTRA_EXACT
            .iter()
            .any(|e| e.eq_ignore_ascii_case(key))
            || BUILD_JAIL_EXTRA_PREFIXES.iter().any(|p| {
                key.get(..p.len())
                    .is_some_and(|s| s.eq_ignore_ascii_case(p))
            })
    }
    #[cfg(not(windows))]
    {
        BUILD_JAIL_EXTRA_EXACT.contains(&key)
            || BUILD_JAIL_EXTRA_PREFIXES.iter().any(|p| key.starts_with(p))
    }
}

/// The `NODE_OPTIONS` route to the Windows realpath defect. NOT SHIPPED — retained as the
/// measured record of a repair that works and is still the wrong one, and as the string the
/// branch-scoped probe keeps measuring.
///
/// THE DEFECT. Node's JS `realpathSync` walks a path component by component and, on Windows,
/// `lstat`s the VOLUME ROOT first. The jail grants leaf-only and leans on traverse-bypass
/// (`SeChangeNotifyPrivilege` + `FILE_DEVICE_ALLOW_APPCONTAINER_TRAVERSAL`), which exempts
/// INTERMEDIATE components of a single open — it does not make an ancestor openable as a
/// TARGET. So every `require()` of an absolute path dies on `EPERM: lstat 'C:\'`.
///
/// WHAT SHIPS INSTEAD — the ancestor chain, repaired as far as an unprivileged token reaches:
/// a non-inherited traverse ACE, written wherever the user holds `WRITE_DAC`, which is
/// `%USERPROFILE%` and below (`ancestor_chain` in `backend/windows.rs`). `C:\` and `C:\Users`
/// stay UNREPAIRED — measured refused across three images including a genuine workstation.
/// A second prong once requested the capability SIDs those two roots already carry; it never
/// took effect (`NtCreateLowBoxToken` refuses the `S-1-15-3-65536-…` AppSilo RID class, and the
/// launch fell back on every attempt in both principals) and has been DELETED. Raw capability
/// SIDs are indeed requestable unprivileged — `internetClient` is — but not these, so do not
/// read the privilege point as making those two roots reachable.
///
/// WHICH MEANS A REALPATH WALK STILL DIES ON ITS FIRST COMPONENT de-elevated, whatever the ACE
/// half repaired further down. That is why this preload exists, and why it tolerates a refused
/// STRICT ANCESTOR of a granted root rather than any refused component.
///
/// WHY NOT REDIRECT REALPATH AT ITS NATIVE TWIN. That was the first candidate, and it is
/// REFUTED by measurement: `fs.realpathSync.native` is refused under this jail too, with
/// `EPERM ... realpath` on a file the jail GRANTED and Node can `readFileSync` in the same
/// breath (run 30460192608, re-measured with attribution in 30513204884 — both images, every
/// path shape tried, including one whose whole ancestor chain is AAP-granted bar `C:\`).
///
/// The REASON stated here was wrong, and the corrected one matters because it closes a route
/// rather than leaving one open. It said `GetFinalPathNameByHandleW` needs more than the leaf
/// handle the jail allows; the documented per-component sensitivity of that call is scoped to
/// SMB, which on local NTFS would have left it available. The refusal is EARLIER: libuv's
/// `fs__realpath` opens with `dwShareMode=0`, so if its `CreateFileW` had succeeded, holding
/// the file open elsewhere would turn the retry into ERROR_SHARING_VIOLATION — libuv's `EBUSY`.
/// Held, it is still `EPERM`, i.e. ERROR_ACCESS_DENIED, so the OPEN is what the LowBox check
/// refuses and the normalized-name query is never reached. `fs.openSync` on the same leaf
/// succeeds in the same arm, which is what makes that a statement about libuv's call shape
/// rather than about the file.
///
/// WHAT IT WOULD DO. `--preserve-symlinks-main` clears the realpath in `resolveMainPath`
/// (`internal/modules/run_main.js`) and `--preserve-symlinks` clears the ones in `_findPath`
/// and the ESM `finalizeResolution`, so module resolution never walks a path to the volume
/// root. Measured on Windows CI (run 30463527647), that is sufficient: the entry point runs,
/// dependency `require`s resolve, and a lifecycle script body completes under the real
/// build-jail policy.
///
/// WHY IT IS NOT STAMPED ANYWAY — the disqualifying measurement. nub's DEFAULT node-linker is
/// `Isolated` (`aube-linker/src/lib.rs`), which materialises each package in its own store
/// cell and wires dependencies as symlinks. `--preserve-symlinks` makes a dependency resolve
/// under its LINK path, so the parent-directory walk from `node_modules/<pkg>` skips the store
/// cell that holds that package's private dependencies and lands on the project's top-level
/// `node_modules` instead. Against a fixture mirroring the real
/// `.aube/<dep_path>/node_modules/<name>` layout, a package whose private dependency is
/// `bar@2.0.0` resolved `bar@1.0.0` — the unrelated top-level copy — and threw NOTHING:
///
/// ```text
/// without the flag:  foo sees bar@2.0.0
/// with the flag:     foo sees bar@1.0.0
/// ```
///
/// A lifecycle script that builds against the wrong dependency version and exits 0 is worse
/// than one that cannot start, so this stays unwired even now that a working repair exists
/// beside it. (`preserve_symlinks_isolated_layout` is the standing regression test; if it ever
/// stops reproducing, this becomes available again and the test says so.)
///
/// AND THE OBVIOUS SALVAGE IS CLOSED TOO — do not re-derive it. The wrong-version hazard above is
/// attributable to `--preserve-symlinks` ALONE (`preserve_symlinks_isolated_layout` measures
/// main-only leaving the same fixture correct), so `--preserve-symlinks-main` by itself looks like
/// a repair that dodges the disqualification. It is not, because it does not repair enough:
/// `Module._findPath` realpaths every NON-main resolution unless `--preserve-symlinks` is set, so
/// main-only clears `resolveMainPath` and then every `require()` dies `EPERM` anyway. There is no
/// cache to lean on — `toRealPath` memoises only what a SUCCESSFUL walk populated, and under this
/// jail none succeeds. `realpath_unavailable_resolution` measures both halves (entry point runs,
/// requires fail) against a process where realpath is refused exactly as the AppContainer refuses
/// it. So the flag pair is the only configuration that WORKS and it is disqualified, while the one
/// configuration that is not disqualified does not work.
#[cfg(windows)]
pub fn windows_realpath_node_options() -> String {
    "--preserve-symlinks-main --preserve-symlinks".to_string()
}

/// Retained for the probe's differential arm: the refuted native-realpath shim, kept so
/// `windows_realpath_ancestors` measures the SAME string that was rejected rather than a
/// restatement of it, and so a future Node that grants `GetFinalPathNameByHandleW` under an
/// AppContainer can be re-tested against it directly.
///
/// `data:` is load-bearing: `defaultResolve` short-circuits on that protocol before any
/// filesystem access, which is the only way a preload can be delivered into a jail whose
/// realpath is broken. `--import` preloads run AFTER `resolveMainPath`, which is why the
/// entry point still needs `--preserve-symlinks-main` alongside.
#[cfg(windows)]
#[doc(hidden)]
pub fn windows_native_realpath_shim_node_options() -> String {
    // Percent-encoded because NODE_OPTIONS is whitespace-separated; only space and quote
    // need it here.
    let shim = "import fs from \"node:fs\";\
                const n=fs.realpathSync.native;n.native=n;fs.realpathSync=n;"
        .replace(' ', "%20")
        .replace('"', "%22");
    format!("--preserve-symlinks-main --import data:text/javascript,{shim}")
}

/// The JS the Windows build jail preloads into every confined Node. Kept as a FILE rather
/// than a Rust string so it stays readable, greppable and lintable; the delivery encoding is
/// [`build_jail_node_options`]'s job.
///
/// Not `#[cfg(windows)]`: only the STAMPING DECISION is Windows-specific (see
/// [`windows_build_jail_node_options`]), and gating the bytes as well would make the
/// composition untestable on the machines people develop on. An unused `include_str!` costs a
/// string in the binary, not a behaviour.
const WINDOWS_STDIO_SHIM: &str = include_str!("../backend/windows_stdio_shim.js");

/// Drop whole-line comments and indentation before a payload is encoded. Applied to EVERY
/// stamped shim — stdio, net gate and realpath — so the sources stay densely commented (which is
/// where their provenance lives) while the delivered payload does not carry the prose.
///
/// THIS IS A BUDGET, NOT A TIDY-UP. The stamp is by far the largest thing in the child's
/// environment block, and how much block a `CreateProcessW` child will accept is not a number we
/// have a documented ceiling for (see `stamped_node_options_fits_the_env_block` — the widely-cited
/// 32767 belongs to other API paths, not to `lpEnvironment`). That test pins the margin so an
/// innocent-looking edit cannot quietly push the stamp past anything we have measured to launch.
///
/// WHOLE LINES ONLY, and NOT a step on the way to a character-level stripper. Deciding what a
/// mid-line `/` means is context-sensitive grammar — regex-literal versus division is settled by
/// the PARSER's expectation state, not lexically, which is why doing it properly costs a full
/// lexer coupled to a parser (oxc's `lexer/regex.rs` is entered from the parser, not from a
/// standalone scan). Reaching for one here would trade a dead-simple transform for a parser whose
/// failure mode is a silently half-valid module evaluated inside a confined child, and the payoff
/// is nil: line-leading comments are already over half of every shim (55-62%).
///
/// ITS PRECONDITION, machine-checked below rather than promised in prose: no string or template
/// literal may span a newline. Trimming and dropping whole lines is otherwise exactly
/// semantics-preserving — comments and blank lines are already whitespace to the parser, and one
/// newline per surviving line is kept, so ASI is untouched.
fn strip_js_comments(src: &str) -> String {
    let out = src
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    // Checked on the OUTPUT, where the comment lines that legitimately carry lone backticks
    // (prose quoting an identifier) are already gone, so only code is measured.
    debug_assert!(
        out.lines().all(|l| l.matches('`').count() % 2 == 0) && !out.contains("\\\n"),
        "strip_js_comments is unsound where a string or template literal spans a newline"
    );
    out
}

/// The JS actually delivered to a confined Node.
pub fn build_jail_stdio_preload_js() -> String {
    strip_js_comments(WINDOWS_STDIO_SHIM)
}

/// The `NODE_OPTIONS` the Windows build jail STAMPS over any ambient value, delivering the
/// `child_process` stdio shim ([`WINDOWS_STDIO_SHIM`]) and the per-package network gate
/// ([`net_gate_node_options`]) as two `--import` terms on one value.
///
/// WHY A STAMP AND NOT A GRANT. The blocker is the OBJECT NAMESPACE, not a permission: under
/// one policy and one grant set, `\\.\pipe\LOCAL\…` was CREATED while `\\.\pipe\…` was
/// REFUSED (run 30473523088). Global NPFS is closed to a LowBox token, the AppContainer's
/// private namespace is open, and libuv spells only the former. No filesystem rule reaches
/// `\Device\NamedPipe` — a maximally loose policy still hung — so there is nothing to grant.
///
/// WHY IT MUST OVERWRITE. `NODE_OPTIONS` carries `--import`. Admitting the key to the
/// lifecycle env allowlist ([`build_jail_env_allowed`]) is only sound because this value
/// replaces whatever the user's environment held; the two are wired together deliberately and
/// must be removed together if either goes.
///
/// `data:` is load-bearing twice over: `defaultResolve` short-circuits on that protocol before
/// touching the filesystem, so the preload needs no grant of its own and cannot be tampered
/// with by the package it confines.
///
/// REQUIRES `--import`, i.e. Node 20.6+. The caller gates on the interpreter's version rather
/// than stamping blind — an unknown flag in `NODE_OPTIONS` aborts Node at startup, which would
/// turn a missing repair into a broken install.
#[cfg(windows)]
pub fn windows_build_jail_node_options(
    package_name: Option<&str>,
    package_version: Option<&str>,
) -> String {
    build_jail_node_options(package_name, package_version)
}

/// Both build-jail preloads on one `NODE_OPTIONS`, as two `--import` terms.
///
/// ORDER IS NOT SIGNIFICANT. Both shims patch the same `child_process` seams, and each is
/// idempotent behind its own `globalThis` sentinel, so either order composes — measured 6/6 by
/// `probe/net-gate/compose-check.cjs` and pinned by `tests/net_gate_semantics.rs`.
///
/// Platform-independent for the same reason [`WINDOWS_STDIO_SHIM`] is: what is Windows-specific
/// is the DECISION to stamp, which lives in [`windows_build_jail_node_options`] and in the
/// lifecycle env allowlist that admits `NODE_OPTIONS` only there.
pub fn build_jail_node_options(
    package_name: Option<&str>,
    package_version: Option<&str>,
) -> String {
    format!(
        "{} {}",
        data_url_import(&strip_js_comments(WINDOWS_STDIO_SHIM)),
        net_gate_node_options(package_name, package_version)
    )
}

/// The JS that repairs `fs.realpath*` inside a confined Node. Kept as a FILE for the same
/// reasons [`WINDOWS_STDIO_SHIM`] is, and un-gated for the same reason: the repair is a Windows
/// fact, but whether the walk reproduces Node's own resolution is not, and gating the bytes
/// would leave the part most likely to break untestable off Windows.
const WINDOWS_REALPATH_SHIM: &str = include_str!("../backend/windows_realpath_shim.js");

/// The token the realpath shim reserves for the jail's granted anchors. Substituted, never
/// appended — the shim reads it as a bare initializer, so a MISSING placeholder yields a module
/// that throws on an undefined identifier and aborts the confined Node at startup rather than
/// degrading to one whose tolerance rule is scoped to nothing.
const REALPATH_ROOTS_PLACEHOLDER: &str = "__NUB_REALPATH_ROOTS_JSON__";

/// The `--import` term repairing module resolution under the AppContainer, plus the
/// `--preserve-symlinks-main` the entry point needs alongside it.
///
/// THE DEFECT. Node's JS `realpathSync` opens every path prefix as a TARGET, starting with the
/// volume root. Bypass-traverse exempts INTERMEDIATE components of one open; it does not make
/// an ancestor openable as a target. So `lstat 'C:\'` is refused and every `require()` of an
/// absolute path dies `EPERM` on a file the same process can `readFileSync`.
///
/// WHY THIS EXISTS ALONGSIDE THE BACKEND'S ANCESTOR REPAIR, not instead of it. `windows.rs`
/// writes a non-inherited traverse ACE on each ancestor the unprivileged user can write one
/// on, and that repair is best-effort BY DESIGN: a refused ACE write is skipped. Above the
/// profile nothing repairs the chain at all — no standard user can re-ACE `C:\` or `C:\Users`,
/// and the capability-SID route that once covered them is kernel-refused and has since been
/// deleted rather than kept as an inert hedge. This term is what keeps the
/// jail working in exactly those cases, and it costs nothing when the ancestor repair did land:
/// with every `lstat` succeeding, the tolerance rule never fires and the walk is Node's own.
///
/// WHY NOT THE PRESERVE-SYMLINKS PAIR, which also works. Under nub's default `Isolated` linker
/// `--preserve-symlinks` resolves a dependency under its LINK path, so the parent walk skips the
/// store cell holding that package's private dependencies and silently binds an unrelated
/// top-level version — see [`windows_realpath_node_options`], which stays unwired for that
/// reason. This repair keeps resolution intact instead: it only asserts that a component the
/// jail refuses to interrogate is a plain directory, and those are exactly the ones above every
/// grant.
///
/// `--preserve-symlinks-main` IS still required: `--import` preloads run inside
/// `executeUserEntryPoint`, after `resolveMainPath` (`internal/modules/run_main.js`), so the
/// entry point's own realpath happens before the shim exists. Its known residual — an entry
/// reached THROUGH a symlink roots the `node_modules` walk at the link path — does not arise for
/// a lifecycle spawn: aube hands the script a store-cell REAL path as `package_dir`
/// (`materialized_pkg_dir`, used as `current_dir` in `install/lifecycle.rs`), so no lifecycle
/// entry arrives through a link.
///
/// An empty `roots` yields an empty string: with nothing to scope the tolerance to, the shim
/// would be inert anyway, and stamping an inert preload only widens the `NODE_OPTIONS` surface.
///
/// EVERY ROOT IS STAMPED IN BOTH SPELLINGS ON WINDOWS — see [`with_alternate_spellings`]. The
/// shim's tolerance rule is a string prefix test, so a root and a walked component that name the
/// same directory in different spellings do not match, and the tolerance silently never fires.
/// Each root, plus its canonical filesystem spelling where that differs — Windows only.
///
/// WHY THIS IS NOT COSMETIC, measured. The shim decides whether to tolerate a refused component
/// by testing whether it is a strict path-boundary PREFIX of a root, over lowercased
/// `path.resolve` output. That is a STRING test, so two spellings of one directory do not match,
/// and Windows hands a process both: `%TEMP%` arrives 8.3-SHORT (`C:\Users\RUNNER~1\…`) while the
/// working directory and a junction's `readlink` target arrive LONG (`C:\Users\runneradmin\…`).
/// Whichever spelling the roots carry, the walk meets the other one and the tolerance never
/// fires — a silent, spelling-dependent failure rather than a loud one.
///
/// Measured both directions on one fixture, de-elevated, realpath preload stamped, run
/// 30569197328: SHORT-form roots lose the bare-specifier-through-a-junction cell on
/// `EPERM … lstat 'C:\Users\runneradmin'`; LONG-form roots lose that cell AND the absolute-entry
/// cell AND npm's own entry, all on `EPERM … lstat 'C:\Users\RUNNER~1'`. Same fixture, same
/// jail, opposite spellings, both thrown from `lstatOrTolerate`.
///
/// The `\\?\` verbatim prefix is a THIRD spelling of the same problem, already handled inside the
/// shim (`stripLongPrefix`) after it measured as `native-longpath-granted=ERR`. This is the same
/// bug class one level out, fixed where the roots are chosen rather than where they are compared,
/// so the shim keeps one comparison rule instead of accreting per-spelling special cases.
///
/// Adding a spelling cannot widen the jail: the tolerance only ever asserts that a component the
/// OS refused to interrogate is a plain directory, and both spellings name the same directory.
/// Non-Windows is returned untouched — 8.3 aliasing is a Windows fact, and canonicalizing on a
/// POSIX host would resolve symlinks and quietly change which paths the rule covers.
fn with_alternate_spellings(roots: &[std::path::PathBuf]) -> Vec<std::path::PathBuf> {
    #[cfg(not(windows))]
    {
        roots.to_vec()
    }
    #[cfg(windows)]
    {
        let mut out: Vec<std::path::PathBuf> = Vec::with_capacity(roots.len() * 2);
        for root in roots {
            if !out.contains(root) {
                out.push(root.clone());
            }
            // A root under a not-yet-created directory is the COMMON case for a build, and bare
            // `canonicalize` errs on it — keeping only the as-built spelling, so a walk that meets
            // the other spelling of an existing ancestor still refuses. Resolve the longest
            // existing prefix and re-apply the tail lexically instead; the same shape Bazel uses.
            let canonical = crate::matcher::path::canonicalize_including_nonexistent(root);
            if !out.contains(&canonical) {
                out.push(canonical);
            }
        }
        out
    }
}

pub fn realpath_shim_node_options(roots: &[std::path::PathBuf]) -> String {
    if roots.is_empty() {
        return String::new();
    }
    let roots = with_alternate_spellings(roots);
    let json = serde_json::to_string(
        &roots
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
    )
    .expect("a list of strings always serializes");
    // Stripped BEFORE substitution so the injected roots never pass through the stripper; the
    // assertion below then doubles as the check that stripping did not eat the placeholder line.
    let js = strip_js_comments(WINDOWS_REALPATH_SHIM).replace(REALPATH_ROOTS_PLACEHOLDER, &json);
    debug_assert!(
        !js.contains(REALPATH_ROOTS_PLACEHOLDER),
        "windows_realpath_shim.js must contain exactly one {REALPATH_ROOTS_PLACEHOLDER}"
    );
    format!("--preserve-symlinks-main {}", data_url_import(&js))
}

/// The JS enforcing per-package egress inside the confined Node. Kept as a FILE rather than a
/// Rust string so it stays readable, greppable and lintable.
const NET_GATE_SHIM: &str = include_str!("../backend/net_gate_shim.js");

/// The token the shim reserves for its compiled policy. Substituted, never appended: the shim
/// reads it as a bare initializer, so a MISSING placeholder yields a module that throws on an
/// undefined identifier — which aborts the confined Node at startup rather than degrading to
/// an ungated one. [`net_gate_node_options`] therefore asserts the substitution happened.
const NET_GATE_POLICY_PLACEHOLDER: &str = "__NUB_NET_POLICY_JSON__";

/// The `--import` term delivering the per-package network gate for `package_name`.
///
/// WHAT THIS IS, PLAINLY: userland, and NOT a security boundary. It patches
/// `net.Socket.prototype.connect`, `dns`, `dgram` and the `child_process` seams inside the
/// confined Node, so a native addon opening a raw socket bypasses it entirely, and so does any
/// client that ignores proxy env (`curl --noproxy '*'`, a static binary, Windows PowerShell
/// 5.1, which reads HKCU proxy settings rather than the environment — a named, accepted
/// residual). Do not describe it as an OS-enforced guarantee.
///
/// ⛔ THE OLD JUSTIFICATION FOR ACCEPTING THE POWERSHELL RESIDUAL — "no corpus package uses
/// PowerShell as a lifecycle entry" — IS FALSE, measured 2026-08-04. `dprint@0.19.2`'s postinstall
/// runs `install.ps1`, whose body is `Invoke-WebRequest $DprintUri -OutFile $DprintZip`: a
/// PowerShell download as the lifecycle entry point, exactly the shape the claim ruled out.
///
/// What actually contains it on Windows is a DIFFERENT mechanism, and it is worth stating so the
/// residual is not re-justified on the wrong grounds: inside the AppContainer that PowerShell
/// cannot resolve its own cmdlets at all — the corpus logs show
/// `Invoke-WebRequest … CommandNotFoundException` in 100% of dprint's non-control cells, i.e. at
/// EVERY grant level — which is the same "a PowerShell module it cannot see" limitation noted on
/// `OS_ESSENTIAL_ENV` above. So the bypass is unreachable in practice HERE, by accident of the
/// LowBox rather than by design, and nothing about the gate itself prevents it. On a platform
/// whose backend does not decline PowerShell this residual would be live.
///
/// What it DOES buy is the shape the threat actually has. Shai-Hulud grew by publishing a new
/// lifecycle hook into packages that never had one, phoning home with plain
/// `https.get`/`fetch`/`axios`. All of that is denied for any package the catalog does not
/// name, on all three platforms and at both Node tiers. To spread, a worm must now ship and
/// load a per-platform native socket addon — a far smaller and far more conspicuous blast
/// radius than one line of JS. Against nub's 344-package corpus the preload reaches 178 of the
/// 179 packages that contact any host, the exception being a POSIX `.sh` that does not run on
/// Windows at all.
///
/// THE POLICY IS A PER-PACKAGE BOOLEAN, sourced from package identity
/// ([`super::build_jail_net_allowed`]). There is deliberately no host list: see that module for
/// why per-host permissioning was dropped rather than deferred.
///
/// PLATFORM-INDEPENDENT on purpose, though only the Windows jail stamps it today (the one
/// platform with no unprivileged OS egress lever; Linux has a seccomp `AF_INET` ceiling and
/// macOS denies outright in Seatbelt — NO proxy on any platform, because the jail's egress
/// decision is a per-package boolean and there is nothing to route). Nothing branches on OS, so serving as a
/// defence-in-depth layer elsewhere needs no porting — and keeping it un-gated is what makes
/// the behaviour testable off Windows.
pub fn net_gate_node_options(package_name: Option<&str>, package_version: Option<&str>) -> String {
    let policy = serde_json::json!({
        "package": package_name,
        // ⛔ THE SHARED DECISION, NOT THE v1 TABLE. This called
        // `package_network::build_jail_net_allowed` directly, which knows nothing of the v2 catalog or
        // the baseline — so on Windows, where this shim IS the egress enforcement, an uncatalogued
        // package was denied the network although `baseline_caps().network` grants it. Measured: 17 of
        // the 86 win32 jail-blaming records die on
        // `blocked network access to node-precompiled-binaries.grpc.io by grpc`, and `grpc` has no
        // catalog entry at all.
        "allow": super::preset::build_jail_net_allowed_for(package_name, package_version),
    });

    // Stripped before substitution, for the reason given in `realpath_shim_node_options`.
    let js = strip_js_comments(NET_GATE_SHIM).replace(
        NET_GATE_POLICY_PLACEHOLDER,
        &serde_json::to_string(&policy).expect("a policy of strings and bools always serializes"),
    );
    debug_assert!(
        !js.contains(NET_GATE_POLICY_PLACEHOLDER),
        "net_gate_shim.js must contain exactly one {NET_GATE_POLICY_PLACEHOLDER}"
    );
    data_url_import(&js)
}

/// An `--import` term carrying `js` as a base64 `data:` module.
///
/// BASE64, NOT PERCENT-ENCODING — and the reason is the ALPHABET, not the size. `NODE_OPTIONS`
/// is split on WHITESPACE and each term re-parsed by Node's own option reader, so a payload that
/// can contain whitespace does not corrupt a URL that Node then rejects — it silently truncates
/// the preload into a half-loaded shim, or into an option fragment Node aborts on. Base64's
/// alphabet contains no whitespace at all, which makes that class of failure structurally
/// unreachable rather than merely avoided by careful escaping. It is also more compact, but only
/// modestly: MEASURED on both shims together back when neither was stripped, 37,227 chars against
/// 47,261 percent-encoded, i.e. 1.27×. The ratio is a property of the alphabets and survives
/// [`strip_js_comments`]; the absolutes do not. (A 2.6× figure circulated earlier; it compared
/// base64 with comments STRIPPED against percent-encoding with comments kept, which is not the
/// same input twice.)
///
/// NEITHER IS A FIX FOR A SIZE LIMIT, because there is no limit here to fix. The documented
/// 32,767-character Windows cap governs `SetEnvironmentVariable`, NOT an environment BLOCK handed
/// to `CreateProcess`, which is how Node spawns: real `windows-latest` accepted a 49,381-char
/// percent-encoded pair and armed the gate (run 30503464536), which is why the "payload exceeds
/// the cap" blocker was retracted. Do not reintroduce it.
fn data_url_import(js: &str) -> String {
    use base64::Engine as _;
    format!(
        "--import data:text/javascript;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(js)
    )
}

/// The build-jail ENV posture (D1): a DEFAULT-DENY allowlist over the effective child
/// env the unconfined lifecycle spawn would have had. Only the known build namespace
/// ([`build_jail_env_allowed`]) is admitted; every other key — including the whole
/// general credential family a denylist structurally missed — is WITHHELD. The
/// returned policy enforces, carries the admitted keys as `constructed`, and records
/// the withheld ones (sorted — `BTreeMap` iteration order — for a stable failure hint).
pub fn lifecycle_scrubbed_env(
    ambient: &std::collections::BTreeMap<String, String>,
) -> crate::policy::EnvPolicy {
    let mut constructed = std::collections::BTreeMap::new();
    let mut withheld = Vec::new();
    for (key, value) in ambient {
        if build_jail_env_allowed(key) {
            constructed.insert(key.clone(), value.clone());
        } else {
            withheld.push(key.clone());
        }
    }
    crate::policy::EnvPolicy {
        resolved: true,
        enforce: true,
        constructed,
        schema: Vec::new(),
        withheld,
        // The allowlist admits only known-safe namespaces (a credential-shaped name is
        // rejected by is_credential_env_key), so nothing kept in `constructed` is a
        // secret — the output redactor has nothing to scrub.
        sensitive_keys: Vec::new(),
    }
}

/// OS-STARTUP-mechanism env names a sandboxed child needs merely to EXIST — the
/// spawning OS's own bootstrap essentials, per OS. Distinct from (and far narrower
/// than) [`BASELINE_ENV_EXACT`]: the baseline is "what a build child needs to
/// operate usefully" (PATH/HOME/USERPROFILE/npm hints); THIS is "what any child
/// needs before it can run at all." All names here are non-secret path/topology
/// pointers, so injecting their real ambient values does NOT breach the deny-all
/// floor (which denies USER/ambient env + secrets, not OS mechanism). Grounded in
/// `wiki/research/sandbox-os-essentials-env.md` (libuv `required_vars[]` + prior-art
/// survey) alongside nub's Windows-VM subset pin.
///
/// POSIX (macOS + Linux): EMPTY — an absolute-path `execve` starts with an empty
/// environ (`node`/`sh`/`true` all rc=0; `os.tmpdir()` falls back to `/tmp`), so the
/// floor injects nothing regardless of ambient contents.
///
/// Windows: `{SystemRoot, SystemDrive, TEMP, TMP, LOCALAPPDATA}`. Provenance:
///  - `SystemRoot`, `SystemDrive`, `TEMP` — libuv's own Windows `required_vars[]`
///    strict-essentials (`deps/uv/src/win/process.c`): winsock's `WSAStartup` fails
///    without `SystemRoot`; some APIs reference `TEMP`; `SystemDrive` is the
///    loader/CLR essential a managed child (`powershell.exe`) resolves System32 from.
///  - `TMP` — the other half of Node's `os.tmpdir()` fallback pair (`lib/os.js`:
///    `TEMP || TMP || <SystemRoot>\temp`); without the pair, AppContainer temp work
///    lands in a non-writable dir.
///  - `LOCALAPPDATA` — the AppContainer essential. The ENFORCING path (fs/net
///    confined → a LowBox AppContainer) resolves the per-container profile dir
///    (`%LOCALAPPDATA%\Packages\…`) from the env, so a block missing it fails
///    `CreateProcessW` with `ERROR_ENVVAR_NOT_FOUND` (203). The VM subset sweep
///    pinned it: `{SystemRoot}` and `{SystemRoot,USERPROFILE}` both fail 203,
///    `{SystemRoot,LOCALAPPDATA}` is the smallest that starts. It embeds the OS
///    username, but that disclosure is REDUNDANT (the child runs AS that user and
///    can read its own username) and empirically REQUIRED — the real value is the
///    minimal correct choice (a synthetic non-disclosing value needs a writable
///    scratch dir + fs grant for zero privacy gain).
///
/// The `SystemDrive`/`TEMP`/`TMP` widen over the earlier VM-pinned `{SystemRoot,
/// LOCALAPPDATA}` minimum is libuv-+-`os.tmpdir()`-grounded, not re-pinned on the VM;
/// a `windows-latest` conformance run at main-merge validates it end-to-end. libuv's
/// Cygwin-subprocess-compat vars (`USERNAME`/`USERDOMAIN`/`LOGONSERVER`/…) are NOT on
/// the floor — subprocess-compat, not start-essential, and identity-bearing.
///
/// `#[cfg]`-gated on the SPAWNING OS (= the child's OS) so a POSIX floor provably
/// injects nothing regardless of ambient contents, while the selection logic stays
/// host-independently testable via [`os_essential_env_from`].
#[cfg(windows)]
const OS_ESSENTIAL_ENV: &[&str] = &["SystemRoot", "SystemDrive", "TEMP", "TMP", "LOCALAPPDATA"];
#[cfg(not(windows))]
const OS_ESSENTIAL_ENV: &[&str] = &[];

/// Select the OS-essential names present in `ambient`, matched case-insensitively
/// (Windows env names are case-insensitive by OS contract — `SYSTEMROOT` and
/// `SystemRoot` are the same var — and the child keeps the ambient's actual cased
/// key + real value). Split from [`os_essential_env`] so the selection is unit-
/// testable on any host by passing an explicit name list.
fn os_essential_env_from(
    ambient: &std::collections::BTreeMap<String, String>,
    names: &[&str],
) -> std::collections::BTreeMap<String, String> {
    let mut selected = std::collections::BTreeMap::new();
    for (key, value) in ambient {
        if names.iter().any(|name| name.eq_ignore_ascii_case(key)) {
            // This helper models Windows selection even on a POSIX test host.
            // A malformed synthetic map can contain aliases that a real Windows
            // environment cannot; keep one deterministic logical entry anyway.
            insert_env_with_case(&mut selected, key.clone(), value.clone(), true);
        }
    }
    selected
}

/// The OS-essential env for the spawning OS, read from the host ambient env at
/// compile time. Only the whitelisted NAMES are admitted; their VALUES come from
/// the real ambient env, and an essential absent from the host is skipped (never
/// fabricated).
pub fn os_essential_env(
    ambient: &std::collections::BTreeMap<String, String>,
) -> std::collections::BTreeMap<String, String> {
    os_essential_env_from(ambient, OS_ESSENTIAL_ENV)
}

/// Environment variable names follow the spawning OS's name contract: Windows
/// folds ASCII case, while POSIX keeps it significant.
pub(crate) fn env_key_eq(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

/// Whether `env` already contains `key`, honoring the spawning OS's env-name
/// contract. This is deliberately not a plain `BTreeMap::contains_key`: Windows
/// treats `PATH` and `Path` as one variable even though Rust's map does not.
pub(crate) fn env_contains_key(
    env: &std::collections::BTreeMap<String, String>,
    key: &str,
) -> bool {
    env.keys().any(|existing| env_key_eq(existing, key))
}

/// Insert one environment value while preserving one logical Windows key. A
/// literal entry is folded before ambient source values, so its spelling and value
/// replace any same-folded ambient entry rather than serializing both aliases.
pub(crate) fn insert_env(
    env: &mut std::collections::BTreeMap<String, String>,
    key: String,
    value: String,
) {
    insert_env_with_case(env, key, value, cfg!(windows));
}

fn insert_env_with_case(
    env: &mut std::collections::BTreeMap<String, String>,
    key: String,
    value: String,
    case_insensitive: bool,
) {
    if case_insensitive {
        env.retain(|existing, _| !existing.eq_ignore_ascii_case(&key));
    }
    env.insert(key, value);
}

/// Add the OS bootstrap variables after every constraining env fold. These are
/// mechanism values, not user-provided capabilities: Windows must retain them for
/// `CreateProcessW`/AppContainer startup even when an array or object allowlist
/// otherwise excludes them. Existing policy entries win, including an explicit
/// literal override.
pub(crate) fn add_os_essential_env(
    policy: &mut crate::policy::EnvPolicy,
    ambient: &std::collections::BTreeMap<String, String>,
) {
    // `EnvPolicy` is public IR, so normalize even a direct caller's synthetic
    // map. Normal compiler folds have already made literal-over-ambient choice
    // explicit; this only gives an ambiguous externally-built alias pair one
    // deterministic Windows representation.
    let constructed = std::mem::take(&mut policy.constructed);
    for (key, value) in constructed {
        insert_env(&mut policy.constructed, key, value);
    }
    for (key, value) in os_essential_env(ambient) {
        if !env_contains_key(&policy.constructed, &key) {
            insert_env(&mut policy.constructed, key, value);
        }
    }
    policy.withheld = ambient
        .keys()
        .filter(|key| !env_contains_key(&policy.constructed, key))
        .cloned()
        .collect();
}

/// The strip-all env FLOOR: an enforcing env that WITHHOLDS all user/ambient env
/// but injects the minimal OS-startup essentials so the child spawns reliably
/// instead of only where the OS tolerates an empty block. Injecting these does NOT
/// breach the deny-all floor — the floor denies USER/ambient env and secrets; the
/// essentials are OS MECHANISM (where Windows is installed / how its loader finds
/// System32), never user config or a credential. Single source of truth for both
/// strip-all constructors: the complete-statement floor (`floor_env`) and the
/// explicit `env: false`.
pub fn strip_all_env(
    ambient: &std::collections::BTreeMap<String, String>,
) -> crate::policy::EnvPolicy {
    let constructed = os_essential_env(ambient);
    let withheld = ambient
        .keys()
        .filter(|k| !env_contains_key(&constructed, k))
        .cloned()
        .collect();
    crate::policy::EnvPolicy {
        resolved: true,
        enforce: true,
        constructed,
        schema: Vec::new(),
        withheld,
        // OS-essential baseline carries no user secrets.
        sensitive_keys: Vec::new(),
    }
}

/// Case-insensitive prefix strip: returns the remainder after `prefix` if `key`
/// starts with it (ignoring ASCII case), else `None`. Used to gate the credential
/// carve-out uniformly across platforms.
fn strip_prefix_ci<'a>(key: &'a str, prefix: &str) -> Option<&'a str> {
    key.get(..prefix.len())
        .filter(|head| head.eq_ignore_ascii_case(prefix))
        .map(|_| &key[prefix.len()..])
}

/// Whether a key is in the curated baseline. Case-SENSITIVE on unix (POSIX env
/// keys are); case-INSENSITIVE on Windows, where env names are case-insensitive by
/// OS contract and a process may report `SYSTEMROOT` or `SystemRoot` — an
/// exact-case miss would drop a container-essential var and re-open the
/// `ERROR_ENVVAR_NOT_FOUND` spawn failure the baseline exists to prevent.
///
/// Public because [`curated_baseline_env`] uses it as the match predicate for
/// `sandbox: true`'s env — the single source of truth for the curated allowlist,
/// never a drifting reimplementation.
pub fn baseline_allows(key: &str) -> bool {
    // Registry credential keys ride the build-hint `npm_config_*` prefix; scrub them
    // before the prefix pass would admit them. Case-insensitive prefix match so a
    // Windows-cased `NPM_CONFIG_//…:_authToken` is caught too (env names are
    // case-insensitive there); on unix npm always emits the lowercase prefix, so a CI
    // match only ever affects `npm_config_`-shaped keys and never widens the allow.
    if let Some(rest) = strip_prefix_ci(key, NPM_CONFIG_PREFIX)
        && is_npm_config_credential(rest)
    {
        return false;
    }
    #[cfg(windows)]
    {
        BASELINE_ENV_EXACT
            .iter()
            .any(|e| e.eq_ignore_ascii_case(key))
            || BASELINE_ENV_PREFIXES.iter().any(|p| {
                key.get(..p.len())
                    .is_some_and(|s| s.eq_ignore_ascii_case(p))
            })
    }
    #[cfg(not(windows))]
    {
        BASELINE_ENV_EXACT.contains(&key)
            || BASELINE_ENV_PREFIXES.iter().any(|p| key.starts_with(p))
    }
}

#[cfg(test)]
mod tests {
    /// ⛔ EVERY KEY THE JAIL STAMPS MUST SURVIVE THIS SCRUB — the entry and the stamp move
    /// together, in BOTH directions, and this is the guard for the reverse direction.
    ///
    /// Without their `BUILD_JAIL_EXTRA_EXACT` entries the tool-cache redirects in
    /// `build_jail.rs` were INERT: the scrub dropped them before the child saw them, so
    /// `@electron/get` and playwright kept their DEFAULT cache roots — outside every rung on
    /// Windows, where those roots hang off `LOCALAPPDATA` rather than the redirected `HOME` —
    /// and the packages walked the ladder to `write:"disk"`. The failure is silent from both
    /// ends: the stamp looks landed and the redirect looks correct.
    ///
    /// The list below is every literal `ambient.insert` in `build_jail.rs`. Two more stamps
    /// are deliberately absent because they need no entry — `npm_config_prefix` and
    /// `npm_config_python` ride `baseline_allows`' `npm_config_*` PREFIX carve-out. That
    /// asymmetry is exactly why one redirect appeared to work while two did not.
    #[test]
    fn the_build_jail_admits_every_key_it_stamps() {
        for key in [
            "electron_config_cache",
            "ELECTRON_CACHE",
            "PLAYWRIGHT_BROWSERS_PATH",
            "NODE_COMPAT",
            "NODE_EXECUTABLE",
            "npm_node_execpath",
        ] {
            assert!(
                build_jail_env_allowed(key),
                "{key} is stamped by build_jail.rs but scrubbed before the child sees it, \
                 which makes the redirect inert"
            );
        }
        // CONTROL — the gate must still REJECT, or the assertions above prove nothing.
        assert!(
            !build_jail_env_allowed("ELECTRON_MIRROR_SECRET_TOKEN"),
            "the allowlist must not admit arbitrary keys"
        );
        assert!(
            !build_jail_env_allowed("NODE_GYP_FORCE_PYTHON"),
            "a key documented as withheld must stay withheld"
        );
    }
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn homes() -> Homes {
        Homes {
            home: PathBuf::from("/testhome"),
            tmp: PathBuf::from("/testtmp"),
            cache: PathBuf::from("/testhome/.cache"),
            project: PathBuf::from("/proj"),
        }
    }

    /// `NODE_OPTIONS` is whitespace-separated, so an over-long or under-encoded payload does
    /// not produce a malformed URL that Node rejects — it produces a TRUNCATED preload that
    /// silently loads half a shim, or an option fragment Node aborts on. Both are quiet, so
    /// the encoding is asserted rather than eyeballed.
    #[test]
    fn both_shims_compose_on_one_value() {
        let stamped = build_jail_node_options(Some("chalk"), Some("5.6.2"));
        let terms: Vec<&str> = stamped.split(' ').collect();
        assert_eq!(
            terms.len(),
            4,
            "expected two `--import <url>` pairs and nothing else: {stamped}"
        );
        assert_eq!(terms[0], "--import");
        assert_eq!(terms[2], "--import");
        for url in [terms[1], terms[3]] {
            assert!(url.starts_with("data:text/javascript;base64,"));
        }
        // The whole payload must arrive, not merely its opening — an identifier from each
        // shim's tail is what makes a truncation visible. Decoded rather than matched against
        // a re-encoding, because base64 is offset-sensitive: the encoding of a substring is
        // not generally a substring of the encoding.
        assert!(decode_import(terms[1]).contains("writableScratchDir"));
        assert!(decode_import(terms[3]).contains("ERR_NUB_JAIL_NET_DENIED"));
    }

    /// Round-trips an `--import data:…;base64,…` term back to its JS, so an assertion can be
    /// written against what the confined Node will actually EVALUATE rather than against the
    /// opaque encoding of it.
    fn decode_import(url: &str) -> String {
        use base64::Engine as _;
        let b64 = url
            .strip_prefix("data:text/javascript;base64,")
            .expect("a base64 data: URL");
        String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(b64)
                .expect("the generator emitted valid base64"),
        )
        .expect("the shim is UTF-8")
    }

    /// The placeholder must be SUBSTITUTED, and the delivered module must carry a real policy
    /// literal. A missed substitution is the quiet catastrophe: the shim reads
    /// `__NUB_NET_POLICY_JSON__` as a bare initializer, so an unsubstituted payload throws on
    /// an undefined identifier and aborts the confined Node at startup.
    #[test]
    fn the_compiled_policy_is_substituted_into_the_delivered_module() {
        let js = decode_import(
            net_gate_node_options(Some("chalk"), Some("5.6.2"))
                .split_once(' ')
                .unwrap()
                .1,
        );
        assert!(
            !js.contains(NET_GATE_POLICY_PLACEHOLDER),
            "the policy placeholder survived into the delivered module"
        );
        assert!(
            // `chalk` carries no catalog entry, so its `allow` is the BASELINE's network value rather
            // than a hardcoded false — see `an_unlisted_package_takes_the_baseline_and_a_listed_one_is_
            // admitted_wholesale` for why that changed and what it was breaking on Windows.
            js.contains(&format!(
                r#"const POLICY = {{"package":"chalk","allow":{}}}"#,
                crate::catalog_v2::baseline_caps().network
            )),
            "{js}"
        );
    }

    /// The compiled policy line, as the confined Node will evaluate it.
    fn compiled_policy(package_name: Option<&str>, package_version: Option<&str>) -> String {
        decode_import(
            net_gate_node_options(package_name, package_version)
                .split_once(' ')
                .unwrap()
                .1,
        )
        .lines()
        .find(|l| l.starts_with("const POLICY = "))
        .expect("the policy line")
        .to_string()
    }

    /// THE GATE IS A BOOLEAN, AND IT NOW RESOLVES THROUGH THE SAME DECISION AS THE COMPILED NET AXIS.
    ///
    /// ⛔⛔ THIS ASSERTED "UNLISTED ⇒ DENIED" AND THAT WAS A SHIPPING BUG, not merely a stale test. The
    /// gate called `package_network::build_jail_net_allowed` — the v1 table — so it knew nothing of the v2
    /// catalog or of `baseline_caps().network`. On Windows this shim IS the egress enforcement, so an
    /// uncatalogued package was denied the network there although the baseline grants it. Measured: 17 of
    /// the 86 win32 records whose verdict blames the jail die on
    /// `blocked network access to node-precompiled-binaries.grpc.io by grpc`, and `grpc` has no entry.
    ///
    /// An unlisted package therefore takes the BASELINE now. What the test still pins is that the answer
    /// is a boolean with no host list in either direction — per-host permissioning is out of scope, and a
    /// stray `hosts` key is the tell that it came back.
    #[test]
    fn an_unlisted_package_takes_the_baseline_and_a_listed_one_is_admitted_wholesale() {
        let baseline_allows = crate::catalog_v2::baseline_caps().network;
        let want = if baseline_allows {
            r#""allow":true"#
        } else {
            r#""allow":false"#
        };
        for unlisted in [None, Some("chalk"), Some("definitely-not-in-the-catalog")] {
            let line = compiled_policy(unlisted, Some("1.0.0"));
            assert!(
                line.contains(want),
                "{unlisted:?} has no catalog entry, so the gate must match the baseline \
                 (network={baseline_allows}): {line}"
            );
        }
        // ⛔⛔ THE GATE MUST AGREE WITH THE COMPILED NET AXIS, PACKAGE FOR PACKAGE — that agreement IS
        // the invariant, and asserting `allow:true` for every v1-listed name was asserting the opposite.
        // While a v2 catalog is in force it is authoritative, and a v2 entry may DELIBERATELY grant less
        // than v1's table did: `@apollo/protobufjs` is listed in v1 and denied by its v2 entry, which is
        // the catalog tightening a high-value target exactly as intended. A test that demands v1's answer
        // would force the gate back onto the v1 table, which is the bug this whole change removes.
        //
        // Covering three classes on purpose: a v1-listed name, an uncatalogued one, and `None`. Each must
        // match `build_jail_net_allowed_for`, whatever that answers.
        let mut agree = vec![
            (None, "1.0.0".to_string()),
            (Some("definitely-not-catalogued"), "1.0.0".to_string()),
        ];
        agree.extend(
            super::super::PACKAGE_NETWORK_ALLOWED
                .iter()
                .map(|(name, range)| (Some(*name), admitted_version(range).to_string())),
        );
        for (pkg, version) in agree {
            let want = crate::compiler::preset::build_jail_net_allowed_for(pkg, Some(&version));
            let line = compiled_policy(pkg, Some(&version));
            assert!(
                line.contains(&format!(r#""allow":{want}"#)),
                "{pkg:?}@{version}: the node-level gate must match the compiled net axis \
                 (build_jail_net_allowed_for said {want}): {line}"
            );
        }

        // Neither direction may compile a host list; per-host permissioning is out of scope,
        // and a stray `hosts` key would be the tell that it came back.
        let mut probes = vec![(None, "1.0.0"), (Some("chalk"), "5.6.2")];
        probes.extend(
            super::super::PACKAGE_NETWORK_ALLOWED
                .iter()
                .map(|(name, range)| (Some(*name), admitted_version(range))),
        );
        for (pkg, version) in probes {
            assert!(
                !compiled_policy(pkg, Some(version)).contains("hosts"),
                "egress is a per-package boolean; a host list must not be compiled: {pkg:?}"
            );
        }
    }

    /// A version the given catalog scope admits — any version for an unscoped entry.
    fn admitted_version(range: &Option<&str>) -> &'static str {
        let Some(range) = range else {
            return "1.2.3";
        };
        let req = semver::VersionReq::parse(range).expect("build.rs validated it");
        ["0.0.1", "0.1.0", "1.0.0", "9999.0.0"]
            .into_iter()
            .find(|v| req.matches(&semver::Version::parse(v).expect("literal")))
            .unwrap_or_else(|| panic!("`{range}` matches none of the probe versions"))
    }

    /// Base64's alphabet contains no whitespace, which is the structural reason the payload
    /// survives `NODE_OPTIONS`' whitespace split. Asserted on the generator rather than left
    /// to the encoder's reputation, because a switch back to percent-encoding (or to any
    /// encoding with a space in its alphabet) reintroduces silent truncation.
    #[test]
    fn the_delivered_term_is_whitespace_free_after_the_flag() {
        let term = net_gate_node_options(Some("chalk"), Some("5.6.2"));
        let (flag, url) = term.split_once(' ').expect("flag then payload");
        assert_eq!(flag, "--import");
        assert!(
            !url.chars().any(char::is_whitespace),
            "the data: URL must survive NODE_OPTIONS' whitespace split"
        );
    }

    /// The stamp is the largest thing in a jailed child's environment block by an order of
    /// magnitude, so a runaway payload is worth catching — but the ceiling is EMPIRICAL, not a
    /// documented API limit. This test previously asserted 26000 against "a `CreateProcessW`
    /// environment block is capped at 32767 characters in total"; that premise is REFUTED. The
    /// 32767 figure attaches to `lpCommandLine` and to `SetEnvironmentVariable`'s per-variable
    /// maximum, not to the `lpEnvironment` block nub passes straight through under
    /// `CREATE_UNICODE_ENVIRONMENT`. Measured: the full production stamp launched confined inside a
    /// 56790-character block (~1.73x the supposed cap) with the CHILD reporting all three preload
    /// sentinels — `__nubJailRealpathShim`, `__nubJailStdioShim`, `__nubJailNetGate` — true off its
    /// own `globalThis`, reproduced across both principals in runs 30568739579, 30568304451 and
    /// 30569197328. HONEST RESIDUAL: what that establishes is that 56790 STARTS, not that no cap
    /// exists above it.
    ///
    /// MEASURE THE WHOLE STAMP. Production carries three `--import` terms — `build_jail_node_options`
    /// composes stdio + net gate, then `build_jail.rs` appends the realpath term — and this test
    /// used to size the stdio term ALONE, which is why an earlier shrink of one term read as
    /// sufficient and was not. The realpath term EMBEDS the granted roots, so its size is
    /// deployment-dependent; the roots below are deep and multi-root on purpose, because shallow
    /// ones understate a real jail.
    ///
    /// THE NUMBER, and why it is far below the 56790 that launched. All three shims now go
    /// through [`strip_js_comments`], which took the stamp from ~55.6k to ~33.8k; a budget still
    /// pinned near the measured ceiling would leave ~22k of slack and stop guarding anything. So
    /// this tracks the payload rather than the ceiling — it is set just over the composition
    /// below, whose Windows form is the larger one because [`with_alternate_spellings`] can
    /// double the roots (~34.3k worst case). The ceiling remains the real constraint and is not
    /// re-measured here; it is simply no longer the binding number.
    ///
    /// If a future edit trips this, the first question is what grew, not whether to raise the
    /// budget — the cheap reclaim (comments) is already spent, so growth here is real payload.
    ///
    /// IT TRIPPED ONCE, and the answer is recorded here so the next reader need not re-derive it.
    /// The binding seam (`installBindingSeam` in `windows_realpath_shim.js`) took the realpath
    /// payload from 6,588 to 9,468 base64 characters and the whole stamp from 33,834 to 36,714,
    /// against a budget of 36,000. That growth IS the repair — it is the layer that reaches the
    /// ESM resolver, which the fs-layer replacement provably cannot — so the ratchet moved rather
    /// than the code. Slack over the composition is kept at roughly what it was (~2.2k), and the
    /// EMPIRICAL ceiling is untouched and still far away: 56,790 characters launched a confined
    /// child with all three sentinels reported.
    #[test]
    fn stamped_node_options_fits_the_env_block() {
        const BUDGET: usize = 39_000;
        let roots: Vec<std::path::PathBuf> = [
            r"C:\Users\runneradmin\AppData\Local\nub\store\v1\registry.npmjs.org\esbuild\0.21.5\node_modules\esbuild",
            r"C:\Users\runneradmin\work\monorepo\packages\web-app\node_modules\.pnpm\esbuild@0.21.5\node_modules\esbuild",
            r"C:\Users\RUNNER~1\AppData\Local\Temp\nub-build-jail-scratch-8f3c1a2b",
        ]
        .iter()
        .map(std::path::PathBuf::from)
        .collect();
        let stamped = format!(
            "{} {}",
            build_jail_node_options(Some("esbuild"), Some("0.21.5")),
            realpath_shim_node_options(&roots)
        );
        assert!(
            stamped.len() <= BUDGET,
            "the stamped NODE_OPTIONS is {} chars, over the {BUDGET} budget; the largest env \
             block measured to launch a confined child is 56790 chars and must still fit PATH \
             and npm_config_*",
            stamped.len()
        );

        // Asserted on what the child will EVALUATE, not on the constants, so a call site that
        // forgets the stripper is caught alongside a stripper that stops working. Stripping must
        // remove PROSE and nothing else: a tail identifier per shim proves the code survived past
        // the point a truncation would bite, and the absence of a line-leading `//` proves the
        // prose did not.
        let payloads: Vec<String> = stamped
            .split(' ')
            .filter(|t| t.starts_with("data:"))
            .map(decode_import)
            .collect();
        assert_eq!(payloads.len(), 3, "stdio, net gate and realpath: {stamped}");
        // The realpath marker tracks the LAST thing in that shim, which is now the binding seam's
        // Node-18 ctx branch — `fs.promises.realpath` moved into the middle when the seam landed
        // and would no longer catch a truncation of everything after it.
        for (payload, tail) in payloads.iter().zip([
            "writableScratchDir",
            "origCpSpawnSync",
            "ctx.syscall = \"realpath\"",
        ]) {
            assert!(payload.contains(tail), "{tail} missing from a payload");
            assert!(
                !payload.lines().any(|line| line.starts_with("//")),
                "whole-line comments must be gone from every delivered payload"
            );
        }
    }

    #[test]
    fn baseline_keeps_windows_essentials_drops_secrets() {
        let ambient: BTreeMap<String, String> = [
            ("PATH", "/bin"),
            ("USERPROFILE", "C:/Users/me"),
            ("LOCALAPPDATA", "C:/Users/me/AppData/Local"),
            ("APPDATA", "C:/Users/me/AppData/Roaming"),
            ("NUMBER_OF_PROCESSORS", "8"),
            ("PROCESSOR_ARCHITECTURE", "AMD64"),
            ("SystemRoot", "C:/Windows"),
            ("ProgramFiles", "C:/Program Files"),
            ("ProgramFiles(x86)", "C:/Program Files (x86)"),
            ("ProgramW6432", "C:/Program Files"),
            ("ProgramData", "C:/ProgramData"),
            ("MY_SECRET_TOKEN", "leak"),
            ("AWS_SECRET_ACCESS_KEY", "leak"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let out = curated_baseline_env(&ambient);
        for k in [
            "PATH",
            "USERPROFILE",
            "LOCALAPPDATA",
            "APPDATA",
            "NUMBER_OF_PROCESSORS",
            "PROCESSOR_ARCHITECTURE",
            "SystemRoot",
            // OS-location vars native-build toolchain discovery reads (F1 regression fix).
            "ProgramFiles",
            "ProgramFiles(x86)",
            "ProgramW6432",
            "ProgramData",
        ] {
            assert!(out.contains_key(k), "baseline must keep {k}");
        }
        assert!(
            !out.contains_key("MY_SECRET_TOKEN"),
            "secret not in baseline"
        );
        assert!(
            !out.contains_key("AWS_SECRET_ACCESS_KEY"),
            "aws secret not in baseline"
        );
    }

    fn ambient(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn os_essential_selection_is_case_insensitive_and_value_preserving() {
        // Host-independent: exercise the selection with an explicit name list so the
        // Windows contract (case-insensitive names, real ambient value kept) is proven
        // on any dev host. A non-essential name is never admitted.
        let env = ambient(&[
            ("SYSTEMROOT", "C:/Windows"), // upper-cased ambient key still matches
            ("windir", "C:/Windows"),
            ("SECRET_TOKEN", "leak"),
        ]);
        let out = os_essential_env_from(&env, &["SystemRoot", "windir"]);
        assert_eq!(
            out.get("SYSTEMROOT").map(String::as_str),
            Some("C:/Windows"),
            "case-insensitive name match keeps the ambient key + value"
        );
        assert!(out.contains_key("windir"));
        assert!(
            !out.contains_key("SECRET_TOKEN"),
            "a non-essential (secret) name is never injected"
        );
    }

    #[test]
    fn windows_essential_selection_deduplicates_case_aliases() {
        // This is intentionally host-runnable: the selector models Windows
        // case-folding even when the test process itself is POSIX.
        let env = ambient(&[
            ("SYSTEMROOT", "C:/old"),
            ("SystemRoot", "C:/Windows"),
            ("TEMP", "C:/Temp"),
        ]);
        let out = os_essential_env_from(&env, &["SystemRoot", "TEMP"]);
        assert_eq!(out.len(), 2, "Windows aliases are one logical env key");
        assert_eq!(
            out.get("SystemRoot").map(String::as_str),
            Some("C:/Windows")
        );
        assert_eq!(out.get("TEMP").map(String::as_str), Some("C:/Temp"));
    }

    #[test]
    fn windows_env_insert_replaces_same_folded_ambient_key() {
        // The literal insertion seam is host-runnable with the Windows case rule
        // passed explicitly. It protects both the compiler's constructed map and
        // the Windows launch block from `Path`/`PATH` duplication.
        let mut env = ambient(&[("Path", "ambient")]);
        insert_env_with_case(&mut env, "PATH".to_string(), "literal".to_string(), true);
        assert_eq!(env.len(), 1);
        assert_eq!(env.get("PATH").map(String::as_str), Some("literal"));
    }

    #[test]
    fn strip_all_injects_only_essentials_and_withholds_the_rest() {
        // The security property that matters: an ambient secret must NEVER ride the
        // strip-all floor's constructed env; only whitelisted OS essentials do, and
        // everything else is recorded withheld. On POSIX the essential set is empty,
        // so `constructed` is empty and even a `SystemRoot`-named ambient var is
        // withheld — the floor injects nothing where the OS needs nothing.
        let env = ambient(&[
            ("SystemRoot", "C:/Windows"),
            ("SystemDrive", "C:"),
            ("TEMP", "C:/Users/me/AppData/Local/Temp"),
            ("TMP", "C:/Users/me/AppData/Local/Temp"),
            ("LOCALAPPDATA", "C:/Users/me/AppData/Local"),
            ("AWS_SECRET_ACCESS_KEY", "leak"),
            ("GITHUB_TOKEN", "leak"),
            ("PATH", "/bin"),
        ]);
        let p = strip_all_env(&env);
        assert!(p.enforce, "strip-all always enforces");
        // No secret or user-config var ever appears in the constructed child env.
        for secret in ["AWS_SECRET_ACCESS_KEY", "GITHUB_TOKEN", "PATH"] {
            assert!(
                !p.constructed.contains_key(secret),
                "{secret} must never ride the strip-all floor"
            );
            assert!(
                p.withheld.contains(&secret.to_string()),
                "{secret} must be recorded withheld"
            );
        }
        #[cfg(windows)]
        {
            for essential in ["SystemRoot", "SystemDrive", "TEMP", "TMP", "LOCALAPPDATA"] {
                assert!(
                    p.constructed.contains_key(essential),
                    "Windows floor injects the OS-startup essential {essential}"
                );
                assert!(
                    !p.withheld.contains(&essential.to_string()),
                    "an injected essential is provided, not withheld"
                );
            }
        }
        #[cfg(not(windows))]
        {
            assert!(
                p.constructed.is_empty(),
                "POSIX floor injects no essentials (empty-env exec starts fine)"
            );
            assert!(
                p.withheld.contains(&"SystemRoot".to_string()),
                "on POSIX a SystemRoot-named ambient var is withheld, not injected"
            );
        }
    }

    #[test]
    fn npm_config_build_hints_pass_but_credentials_scrubbed() {
        // The `npm_config_*` family passes build hints through, but registry auth
        // rides the same prefix and must be scrubbed — thread #6. Both the bare
        // legacy keys and the registry-scoped `//host/:_auth…` forms are excluded.
        // Build hints (kept) — incl. two regression guards for false positives: a
        // package whose name embeds "email" (`nodemailer`) must survive the anchored
        // `email` marker, and a package whose name embeds "key" (`keytar`) must survive
        // the whole-SEGMENT `key` rule. `always-auth` is `-auth`, not `_auth` — kept.
        let hints = [
            "npm_config_target",
            "npm_config_arch",
            "npm_config_target_arch",
            "npm_config_runtime",
            "npm_config_nodedir",
            "npm_config_python",
            "npm_config_build_from_source",
            "npm_config_registry",
            "npm_config_sharp_binary_host",
            "npm_config_nodemailer_binary_host_mirror",
            "npm_config_keytar_binary_host_mirror",
            "npm_config_always-auth",
        ];
        // Credentials (scrubbed) — the anchored legacy markers, the broadened
        // credential-word set (token/secret/password/passwd/credential/apikey), and
        // the short `key` family as a delimited segment. Covers undelimited, hyphen,
        // and dot forms an exact-segment rule would miss.
        let creds = [
            "npm_config__auth",
            "npm_config__authToken",
            "npm_config__password",
            "npm_config_email",
            "npm_config_//registry.npmjs.org/:_authToken",
            "npm_config_//registry.npmjs.org/:_password",
            "npm_config_//registry.npmjs.org/:_auth",
            "npm_config_password",
            "npm_config_passwd",
            "npm_config_foo_token",
            "npm_config_authtoken",
            "npm_config_my_secret",
            "npm_config_credential",
            "npm_config_apikey",
            "npm_config_api_key",
            "npm_config_signing_key",
            "npm_config_key",
            "npm_config_my-token",
            "npm_config_x.secret.y",
        ];
        let ambient: BTreeMap<String, String> = hints
            .iter()
            .chain(creds.iter())
            .map(|k| (k.to_string(), "v".to_string()))
            .collect();
        let out = curated_baseline_env(&ambient);
        for k in hints {
            assert!(out.contains_key(k), "build hint {k} must pass");
        }
        for k in creds {
            assert!(!out.contains_key(k), "credential {k} must be scrubbed");
        }
    }

    /// The MSVC trio is the one allowlist entry whose absence is SILENT: nub would resolve
    /// Visual Studio, stamp the answer, and the scrub would drop it on the way into the child,
    /// leaving node-gyp back on the COM server an AppContainer cannot activate — with no error
    /// anywhere to say why. `WindowsSDKVersion` additionally has to survive
    /// [`is_credential_env_key`], which rejects a `key`-shaped segment.
    #[test]
    fn the_msvc_trio_reaches_the_jailed_child() {
        for key in ["VCINSTALLDIR", "VSCMD_VER", "WindowsSDKVersion"] {
            assert!(
                build_jail_env_allowed(key),
                "{key} must reach node-gyp or the Visual Studio pre-resolution is inert"
            );
        }
    }

    /// `NODE_EXECUTABLE` fails the same silent way, and its symptom is worse than inertness:
    /// the child re-runs nub's version discovery inside the jail, where the only granted
    /// interpreter is the one that already failed to satisfy the pin and every other answer
    /// — `~/.nvm`, nub's store, nodejs.org — is out of reach, so it hard-errors
    /// `ERR_NUB_NODE_PROVISION_FAILED` on a version the host may already have. Withholding it
    /// turns the out-of-jail resolution into a download nothing consumes.
    #[test]
    fn the_pre_resolved_node_reaches_the_jailed_child() {
        assert!(
            build_jail_env_allowed("NODE_EXECUTABLE"),
            "NODE_EXECUTABLE must reach the child or the confined shim re-resolves and fails closed"
        );
    }

    #[test]
    fn os_essential_floor_is_the_libuv_grounded_set() {
        // The exact per-OS floor NAME set — the contract from
        // wiki/research/sandbox-os-essentials-env.md. Windows keeps libuv's three
        // strict-essentials + os.tmpdir()'s TMP + the AppContainer LOCALAPPDATA;
        // POSIX keeps nothing (empty-environ exec starts fine).
        #[cfg(windows)]
        {
            let expected: &[&str] = &["SystemRoot", "SystemDrive", "TEMP", "TMP", "LOCALAPPDATA"];
            assert_eq!(OS_ESSENTIAL_ENV, expected);
        }
        #[cfg(not(windows))]
        assert!(
            OS_ESSENTIAL_ENV.is_empty(),
            "POSIX floor is empty (macOS + Linux start from an empty environ)"
        );
    }

    #[test]
    fn lifecycle_scrubbed_env_is_a_default_deny_allowlist() {
        // The build-jail env posture (D1) is a DEFAULT-DENY allowlist: admit the known
        // build namespace (PATH/HOME/TMPDIR floor, NODE re-invocation, npm_package_*/
        // npm_lifecycle_*/npm_config_* build env, proxy, non-secret toolchain config),
        // WITHHOLD everything else — crucially the general credential family a denylist
        // structurally missed (no credential WORD → passed straight through before).
        let ambient = ambient(&[
            // Lifecycle essentials — must be ADMITTED.
            ("PATH", "/bin"),
            ("HOME", "/home/u"),
            ("TMPDIR", "/tmp"),
            ("NODE", "/n/node"),
            // The jail STAMPS this to keep a dependency's script on vanilla Node; the
            // scrubber dropping it would silently restore augmentation, and with it the
            // unreadable-preload abort under Landlock.
            ("NODE_COMPAT", "1"),
            ("npm_node_execpath", "/n/node"),
            ("INIT_CWD", "/proj"),
            ("npm_package_name", "left-pad"),
            ("npm_lifecycle_event", "install"),
            ("npm_config_registry", "https://r/"),
            ("npm_config_target_arch", "arm64"),
            ("https_proxy", "http://proxy:8080"),
            ("CFLAGS", "-O2"),
            ("PKG_CONFIG_PATH", "/opt/lib/pkgconfig"),
            ("SDKROOT", "/sdk"),
            ("MACOSX_DEPLOYMENT_TARGET", "11.0"),
            ("ProgramFiles(x86)", "C:/Program Files (x86)"),
            // The *_KEY / *_API_KEY family the OLD denylist let through — must be WITHHELD.
            ("OPENAI_API_KEY", "sk-x"),
            ("AWS_ACCESS_KEY_ID", "AKIA"),
            ("PRIVATE_KEY", "-----BEGIN"),
            ("SSH_KEY", "ssh-rsa"),
            ("GH_PAT", "ghp_x"),
            ("DATABASE_URL", "postgres://u:p@h/db"),
            // Credential-word / registry-auth family — must be WITHHELD.
            ("NPM_TOKEN", "t"),
            ("GITHUB_TOKEN", "t"),
            ("AWS_SECRET_ACCESS_KEY", "t"),
            ("MY_PASSWORD", "t"),
            ("AUTH_HEADER", "t"),
            ("npm_config_//registry.npmjs.org/:_authToken", "t"),
            // An unrelated ambient var with no build role — DENIED by default-deny.
            ("EDITOR", "vim"),
        ]);
        let p = lifecycle_scrubbed_env(&ambient);
        assert!(p.enforce && p.resolved);
        for kept in [
            "PATH",
            "HOME",
            "TMPDIR",
            "NODE",
            "NODE_COMPAT",
            "npm_node_execpath",
            "INIT_CWD",
            "npm_package_name",
            "npm_lifecycle_event",
            "npm_config_registry",
            "npm_config_target_arch",
            "https_proxy",
            "CFLAGS",
            "PKG_CONFIG_PATH",
            "SDKROOT",
            "MACOSX_DEPLOYMENT_TARGET",
            "ProgramFiles(x86)",
        ] {
            assert!(p.constructed.contains_key(kept), "must admit {kept}");
        }
        for denied in [
            "OPENAI_API_KEY",
            "AWS_ACCESS_KEY_ID",
            "PRIVATE_KEY",
            "SSH_KEY",
            "GH_PAT",
            "DATABASE_URL",
            "NPM_TOKEN",
            "GITHUB_TOKEN",
            "AWS_SECRET_ACCESS_KEY",
            "MY_PASSWORD",
            "AUTH_HEADER",
            "npm_config_//registry.npmjs.org/:_authToken",
            "EDITOR",
        ] {
            assert!(
                !p.constructed.contains_key(denied),
                "must withhold {denied}"
            );
            assert!(p.withheld.contains(&denied.to_string()));
        }
    }

    /// Nub runs npm lifecycle scripts through cmd.exe on Windows, so the build jail's
    /// ambient snapshot carries cmd's hidden `=C:`/`=ExitCode` per-drive entries. A
    /// `=`-named key cannot round-trip a `KEY=VALUE` block and the backend rejects one
    /// outright, so this pins that every posture builder DROPS them by allowlist —
    /// i.e. the build jail was never able to reach that reject.
    #[test]
    fn env_postures_drop_cmd_exe_shell_positional_keys() {
        let ambient = ambient(&[
            ("=C:", "C:\\Users\\me"),
            ("=D:", "D:\\work"),
            ("=ExitCode", "00000000"),
            ("SystemRoot", "C:/Windows"),
            ("PATH", "C:/Windows/System32"),
        ]);
        for key in ["=C:", "=D:", "=ExitCode"] {
            assert!(!build_jail_env_allowed(key), "build jail must deny {key}");
        }
        let jail = lifecycle_scrubbed_env(&ambient);
        for posture in [
            &jail.constructed,
            &curated_baseline_env(&ambient),
            &strip_all_env(&ambient).constructed,
        ] {
            assert!(
                !posture.keys().any(|k| k.starts_with('=')),
                "no posture may construct a `=`-named key: {posture:?}"
            );
        }
        assert!(
            jail.constructed.contains_key("PATH"),
            "the drop is name-scoped, not a blanket scrub"
        );
    }

    /// The env channel a from-source native compile rides, pinned by name: losing any one
    /// of these breaks an offline in-jail node-gyp build, which no other test would catch.
    #[test]
    fn build_jail_admits_the_node_gyp_compile_channel() {
        let ambient = ambient(&[
            ("PATH", "/tools/node-gyp/.bin:/usr/bin"),
            ("npm_config_node_gyp", "/cache/tools/node-gyp/node-gyp.js"),
            ("npm_config_nodedir", "/nodes/v26.5.0"),
            ("npm_node_execpath", "/nodes/v26.5.0/bin/node"),
            ("NODE", "/shim/node"),
        ]);
        let p = lifecycle_scrubbed_env(&ambient);
        for kept in [
            "PATH",
            "npm_config_node_gyp",
            "npm_config_nodedir",
            "npm_node_execpath",
            "NODE",
        ] {
            assert!(
                p.constructed.contains_key(kept),
                "{kept} is load-bearing for an offline node-gyp compile and must be admitted"
            );
        }
    }

    /// `__NUB_NODE_GYP_EXE` stays WITHHELD, and that is what keeps offline compiles working
    /// — see [`BUILD_JAIL_EXTRA_EXACT`]. The engine's `npm_config_node_gyp` shim only
    /// trampolines back through the PM binary when this var is present; withheld, it falls
    /// back to `node-gyp` on the lifecycle PATH, which the jail does grant. Admitting it
    /// without a matching read grant for the PM binary would turn a working fallback into
    /// an exec of an ungranted path, so this assertion is a tripwire on that pairing, not
    /// incidental coverage of the default-deny shape.
    ///
    /// The names are nub's own (`__NUB_*`, from the embedder profile's
    /// `internal_env_prefix`), NOT the engine's `AUBE_*`: the jail reasons about them by
    /// name, and the embedded engine's brand is never nub's env surface. This crate has
    /// no aube dependency by design, so the spelling is duplicated here rather than
    /// imported — the profile's compile-time assertion in `pm_engine::identity` pins the
    /// other side.
    #[test]
    fn build_jail_withholds_the_node_gyp_trampoline_exe() {
        let ambient = ambient(&[
            ("__NUB_NODE_GYP_EXE", "/usr/local/bin/nub"),
            ("__NUB_NODE_GYP_PROJECT_DIR", "/proj"),
        ]);
        let p = lifecycle_scrubbed_env(&ambient);
        for withheld in ["__NUB_NODE_GYP_EXE", "__NUB_NODE_GYP_PROJECT_DIR"] {
            assert!(
                !p.constructed.contains_key(withheld),
                "{withheld} must stay withheld unless the PM binary is read-granted too"
            );
        }
    }

    #[test]
    fn is_credential_env_key_spares_legit_build_vars() {
        // False-positive guards: an npm build hint whose name embeds a credential word
        // survives (npm_config_* routes to the registry-credential predicate), and a
        // bare `AUTHOR`/`AUTHORS` var is not swept by the `auth`-segment rule.
        for legit in [
            "PATH",
            "NODE_OPTIONS",
            "npm_config_target",
            "npm_config_keytar_binary_host_mirror",
            "npm_package_author",
            "AUTHOR",
            "AUTHORS",
        ] {
            assert!(!is_credential_env_key(legit), "{legit} is not a credential");
        }
        for cred in [
            "NPM_TOKEN",
            "SECRET_VALUE",
            "MY_PASSWORD",
            "AUTH_TOKEN",
            "GITHUB_AUTH",
            "npm_config__authToken",
        ] {
            assert!(is_credential_env_key(cred), "{cred} is a credential");
        }
    }

    #[test]
    fn secret_path_denies_are_home_anchored_and_exclude_dotenv() {
        let globs: Vec<String> = secret_read_denies(&homes())
            .into_iter()
            .map(|r| r.matcher.as_str().to_string())
            .collect();
        // Home-anchored secret files/dirs appear as subtree denies (substring match
        // tolerates OS firmlink canonicalization of the fake home prefix).
        for frag in [".gnupg", ".pgpass", ".pypirc", ".config/git/credentials"] {
            assert!(
                globs.iter().any(|g| g.contains(frag)),
                "missing home secret deny containing {frag}"
            );
        }
        // `.env*` is NOT in the secret-PATH set — it is injected separately (with the
        // exact-file override precedence) via the env-deny bands, so it must not appear here.
        assert!(
            globs.iter().all(|g| !g.contains(".env")),
            "`.env*` must be handled by the env-deny bands, not the secret-path splice"
        );
    }

    #[test]
    fn env_deny_bands_split_leaf_and_subtree_as_deny() {
        let leaf = env_deny_leaf_rules();
        let subtree = env_deny_subtree_rules();
        // Every rule is a Deny with the canonical inert access.
        assert!(
            leaf.iter()
                .chain(&subtree)
                .all(|r| r.effect == Effect::Deny)
        );
        let leaf_globs: Vec<&str> = leaf.iter().map(|r| r.matcher.as_str()).collect();
        let subtree_globs: Vec<&str> = subtree.iter().map(|r| r.matcher.as_str()).collect();
        // The LEAF band denies the secret FILE itself; the SUBTREE band denies a
        // `.env*/`-directory's contents. The split is what lets an exact-file allow sit
        // between them (leaf-deny → allow → subtree-deny-last). An npmrc is a file with
        // no subtree twin, so it appears only in the leaf band.
        //
        // EXACT, ORDERED equality — not membership. This is the crate's only content pin
        // on the floor, and both consumers read it POSITIONALLY, so a reorder or a
        // PREPEND is as much a break as a removal and a subset check sees none of them.
        // Widening the floor is a deliberate security change: land it here first, then
        // follow the restated literal in `tests/compiler.rs`, which cannot reach these
        // crate-private arrays.
        assert_eq!(
            leaf_globs,
            [
                "**/.env*",
                ".env*",
                "**/.npmrc",
                ".npmrc",
                "**/node_modules/npm/npmrc",
                "node_modules/npm/npmrc",
            ]
        );
        assert_eq!(subtree_globs, ["**/.env*/**", ".env*/**"]);
    }

    /// npm's builtin config is the one file in its config hierarchy spelled without a
    /// leading dot, so the `.npmrc` globs miss it — and it sits inside the subtree the
    /// build jail grants so `npm`/`npx` resolve. A managed install can put an auth token
    /// there, so the floor must cover it.
    ///
    /// This pins the POLICY only. Whether a backend enforces it is separate: macOS does,
    /// Linux does not reach this path with a mask (see the `ENV_DENY_LEAF_GLOBS` doc), so
    /// a green run here is not evidence of Linux enforcement.
    #[test]
    fn the_floor_denies_npms_undotted_builtin_config_in_policy() {
        // An allow-by-default set with only the floor on top: whatever the floor denies
        // is denied no matter how permissive the grants above it were.
        let set = crate::policy::FsRuleSet {
            entries: env_deny_leaf_rules()
                .into_iter()
                .chain(env_deny_subtree_rules())
                .collect(),
            default_effect: Effect::Allow,
        };
        let matcher = crate::matcher::path::PathMatcher::new(&set);
        for denied in [
            "/opt/node/lib/node_modules/npm/npmrc",
            "/home/u/.nvm/versions/node/v26.5.0/lib/node_modules/npm/npmrc",
        ] {
            assert_eq!(
                matcher.decide(Path::new(denied)).effect,
                Effect::Deny,
                "npm's builtin config must be denied: {denied}"
            );
        }
        // Scoped to npm's own location — an unrelated file a project calls `npmrc`
        // (a template, a distro's `etc/npmrc`) is not swept up by the floor.
        assert_eq!(
            matcher.decide(Path::new("/srv/app/config/npmrc")).effect,
            Effect::Allow,
            "the floor must not deny an unrelated file named `npmrc`"
        );
    }
}
