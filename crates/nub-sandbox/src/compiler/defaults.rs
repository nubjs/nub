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
use crate::policy::{CanonGlob, Effect, FsAccess, FsRule};
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

/// The default `.env*` READ-deny globs: any file whose BASENAME starts with `.env`
/// (`.env`, `.env.local`, `.env.production`, `.envrc`, …), at any depth. These files
/// hold the exact secrets the sandbox scrubs from the env, so reading them is denied
/// by DEFAULT on every read-granting fs policy (an unconditional floor) —
/// see [`env_deny_leaf_rules`]/[`env_deny_subtree_rules`] and the injection in
/// `fold::finalize_env_deny`. Denying reads is near-zero-breakage: legit code reads
/// secrets via the injected process env, not by `fs.read()`-ing the file.
///
/// The set splits into a LEAF band ([`ENV_DENY_LEAF_GLOBS`] — `**/.env*`, the file
/// itself) and a SUBTREE band ([`ENV_DENY_SUBTREE_GLOBS`] — `**/.env*/**`, covering a
/// `.env.d/`-style DIRECTORY of per-target secret files). Both are appended as the LAST
/// entries so the block is UNCONDITIONAL — no directory grant, glob, or exact allow can
/// reopen a `.env*` file or a `.env*/` directory's contents (sandbox.mdx "`.env` files
/// are always blocked"). See `fold::finalize_env_deny`. The rootless twins (`.env*`)
/// mirror each band for a depth-0 match; canonical candidates are absolute, so `**/…`
/// is the form that bites.
///
/// The test-only union below guards that the two bands never drift.
pub(crate) const ENV_DENY_LEAF_GLOBS: &[&str] = &["**/.env*", ".env*"];
pub(crate) const ENV_DENY_SUBTREE_GLOBS: &[&str] = &["**/.env*/**", ".env*/**"];
/// The union — the drift-guard the Linux grant derivation recognizes as builtin. Gated to
/// its consumer's cfg (linux/test) so a macOS/Windows non-test build doesn't warn unused.
#[cfg(test)]
pub(crate) const ENV_DENY_GLOBS: &[&str] = &["**/.env*", "**/.env*/**", ".env*", ".env*/**"];

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

/// The LEAF `.env*` READ-deny entries ([`ENV_DENY_LEAF_GLOBS`]) — the `.env*` file
/// itself. Depth-independent (matched by basename anywhere), NOT anchored under any
/// root. Appended as a trailing band so it beats every prior allow — a broad dir-allow,
/// a glob, OR an exact-file allow — and cannot be reopened (the unconditional floor).
pub(crate) fn env_deny_leaf_rules() -> Vec<FsRule> {
    ENV_DENY_LEAF_GLOBS
        .iter()
        .map(|g| deny(g.to_string()))
        .collect()
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

/// The policy SOURCE-FILE self-exclusion DENY — an EXACT-path deny (read AND write) on
/// the single file the sandbox rules were read from, so a sandboxed process can neither
/// read nor tamper with the policy that confines it. Canonicalized through the EXACT same
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
    }
}

/// A subtree grant expands to two globs — the node itself and everything under
/// it — so a bare path like `~/.ssh` denies both `~/.ssh` and `~/.ssh/id_rsa`.
/// A pattern already carrying a glob metachar is emitted as-is (no `/**` suffix).
pub fn subtree_globs(expanded: &str) -> Vec<String> {
    if expanded.contains(['*', '?', '[', '{']) {
        return vec![expanded.to_string()];
    }
    let trimmed = expanded.trim_end_matches('/');
    vec![trimmed.to_string(), format!("{trimmed}/**")]
}

fn deny(glob: String) -> FsRule {
    FsRule {
        matcher: CanonGlob(canonicalize_glob_prefix(&glob)),
        effect: Effect::Deny,
        access: FsAccess::DENY,
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
/// These names never appear on unix (the filter is over the ambient env, so the
/// baseline stays OS-appropriate without a `cfg`).
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

/// Whether an env key is credential-shaped and must not reach a dependency's
/// lifecycle build script. The DENYLIST twin of [`baseline_allows`]'s allowlist:
/// the build-jail keeps the whole constructed lifecycle env (a native build needs
/// `PATH`/`NODE`/`npm_package_*`/build hints, which a strip-all allowlist would drop)
/// and removes only credential-shaped keys. Two tiers, both delimiter/case-aware:
/// an `npm_config_*` key routes to the registry-credential predicate
/// ([`is_npm_config_credential`], which also scrubs `_auth*`/`email`/`key`), and any
/// other key is scrubbed when it CONTAINS an unambiguous credential word
/// (`token`/`secret`/`password`/`passwd`/`credential`/`apikey`) or carries `auth` as a
/// delimited segment (`AUTH_TOKEN`/`GITHUB_AUTH`, sparing `AUTHOR`). Best-effort by
/// NAME (a secret named nothing like a credential is not caught here — the `.env*`
/// file-read floor and aube's own constructed-env scrub are the other layers).
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

/// The build-jail ENV posture (D1): keep the constructed lifecycle env, WITHHOLD the
/// credential-shaped keys ([`is_credential_env_key`]). `ambient` is the effective
/// child env the unconfined lifecycle spawn would have had; the returned policy
/// enforces, carries the kept keys as `constructed`, and records the withheld ones
/// (sorted — `BTreeMap` iteration order — for a stable failure hint).
pub fn lifecycle_scrubbed_env(
    ambient: &std::collections::BTreeMap<String, String>,
) -> crate::policy::EnvPolicy {
    let mut constructed = std::collections::BTreeMap::new();
    let mut withheld = Vec::new();
    for (key, value) in ambient {
        if is_credential_env_key(key) {
            withheld.push(key.clone());
        } else {
            constructed.insert(key.clone(), value.clone());
        }
    }
    crate::policy::EnvPolicy {
        resolved: true,
        enforce: true,
        constructed,
        schema: Vec::new(),
        withheld,
        // Credential-shaped keys are WITHHELD entirely (above), so nothing kept in
        // `constructed` is secret — the output redactor has nothing to scrub.
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
    fn lifecycle_scrubbed_env_keeps_build_env_and_withholds_credentials() {
        // The build-jail env posture (D1): keep the whole constructed lifecycle env
        // (a native build needs PATH/NODE/npm_package_*/build hints), withhold only
        // the credential-shaped keys.
        let ambient = ambient(&[
            ("PATH", "/bin"),
            ("NODE", "/n/node"),
            ("npm_package_name", "left-pad"),
            ("npm_config_registry", "https://r/"),
            ("npm_config_target_arch", "arm64"),
            // Credentials — must be withheld.
            ("NPM_TOKEN", "t"),
            ("GITHUB_TOKEN", "t"),
            ("AWS_SECRET_ACCESS_KEY", "t"),
            ("MY_PASSWORD", "t"),
            ("AUTH_HEADER", "t"),
            ("npm_config_//registry.npmjs.org/:_authToken", "t"),
        ]);
        let p = lifecycle_scrubbed_env(&ambient);
        assert!(p.enforce && p.resolved);
        for kept in [
            "PATH",
            "NODE",
            "npm_package_name",
            "npm_config_registry",
            "npm_config_target_arch",
        ] {
            assert!(p.constructed.contains_key(kept), "must keep {kept}");
        }
        for cred in [
            "NPM_TOKEN",
            "GITHUB_TOKEN",
            "AWS_SECRET_ACCESS_KEY",
            "MY_PASSWORD",
            "AUTH_HEADER",
            "npm_config_//registry.npmjs.org/:_authToken",
        ] {
            assert!(!p.constructed.contains_key(cred), "must withhold {cred}");
            assert!(p.withheld.contains(&cred.to_string()));
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
        // The LEAF band denies the `.env*` file itself; the SUBTREE band denies a
        // `.env*/`-directory's contents. The split is what lets an exact-file allow sit
        // between them (leaf-deny → allow → subtree-deny-last).
        for g in ["**/.env*", ".env*"] {
            assert!(leaf_globs.contains(&g), "leaf band missing {g}");
            assert!(!leaf_globs.contains(&format!("{g}/**").as_str()));
        }
        for g in ["**/.env*/**", ".env*/**"] {
            assert!(subtree_globs.contains(&g), "subtree band missing {g}");
        }
        // The union still equals the drift-guarded ENV_DENY_GLOBS (is_builtin recognition).
        let mut union: Vec<&str> = leaf_globs.iter().chain(&subtree_globs).copied().collect();
        union.sort_unstable();
        let mut all = ENV_DENY_GLOBS.to_vec();
        all.sort_unstable();
        assert_eq!(union, all, "leaf+subtree must equal ENV_DENY_GLOBS");
    }
}
