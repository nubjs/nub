//! Eager .env* loading with workspace walk-up and ${VAR} expansion.
//! The parser follows Node's `--env-file` grammar; expansion stays in Nub's
//! intentional post-parse layer.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

/// Max size of an env file we read into memory (16 MiB). Real env files are
/// KB-sized; this caps an absurdly large regular file.
const ENV_FILE_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// Process-controlling variables nub ignores when they come from a `.env*` /
/// `--env-file` source. A `.env` file is meant to provide configuration to the
/// user's PROGRAM, not to reconfigure the runtime nub launches — so a value it
/// sets for one of these keys is dropped rather than silently changing how Node
/// itself starts. This mirrors Deno's `ENV_FILE_DENYLIST`
/// (`.repos/deno/cli/util/env.rs`), which ignores Deno's own control vars from an
/// env file for the same reason; applied here to Node's control surface, since
/// Node is the runtime nub launches.
///
/// The set — Node's start-up / transport control knobs:
/// - `NODE_OPTIONS` — injects launch flags (`--require`/`--import`, heap sizing,
///   inspector) into the process nub spawns.
/// - `NODE_TLS_REJECT_UNAUTHORIZED` — `=0` turns off TLS certificate verification.
/// - `NODE_EXTRA_CA_CERTS` — adds a trusted CA to the process.
/// - `NODE_REPL_EXTERNAL_MODULE` — auto-loads a module at start-up.
/// - `__NUB_RUNTIME_CONFIG` — nub's own resolved `nub.jsonc` snapshot, handed to
///   the child as JSON. It carries runtime loaders and TypeScript transforms,
///   so a `.env` that sets it would rewrite the code nub transpiles. The watch path
///   in particular applies its injected `.env` values AFTER stamping this var,
///   so without the denylist a repo-supplied `.env` wins.
/// - `__NUB_COMPAT_*` / `__NUB_AUGMENTED_*` — the parent-captured ambient
///   environment and ownership metadata used when a nested Nub invocation sheds
///   its parent's augmentation. Script launchers apply `.env` values after these
///   markers, so the whole wire shape must be protected together.
///
/// Only values ORIGINATING from a `.env*` file are dropped: an ambiently-set value
/// passes through untouched (shell-wins, and the child inherits nub's env). A user
/// who legitimately wants one of these (e.g. `NODE_OPTIONS=--max-old-space-size=4096`)
/// sets it in the real shell/CI environment instead — the trade Deno also makes.
const ENV_FILE_DENYLIST: &[&str] = &[
    "NODE_OPTIONS",
    "NODE_TLS_REJECT_UNAUTHORIZED",
    "NODE_EXTRA_CA_CERTS",
    "NODE_REPL_EXTERNAL_MODULE",
    "__NUB_RUNTIME_CONFIG",
    "__NUB_COMPAT_NODE_OPTIONS",
    "__NUB_COMPAT_NODE_PATH",
    "__NUB_COMPAT_NODE",
    "__NUB_COMPAT_NODE_COMPILE_CACHE",
    "__NUB_COMPAT_PATH",
    "__NUB_COMPAT_PRESENT",
    "__NUB_AUGMENTED_NODE_OPTIONS",
    "__NUB_AUGMENTED_NODE_PATH",
    "__NUB_AUGMENTED_NODE",
    "__NUB_AUGMENTED_NODE_COMPILE_CACHE",
    "__NUB_AUGMENTED_PATH",
    "__NUB_AUGMENTED_NODE_OPTIONS_PRESENT",
    "__NUB_AUGMENTED_NODE_PATH_PRESENT",
    "__NUB_AUGMENTED_NODE_PRESENT",
    "__NUB_AUGMENTED_NODE_COMPILE_CACHE_PRESENT",
    "__NUB_AUGMENTED_PATH_PRESENT",
];

/// The runtime-control keys Nub refuses to source from env files.
///
/// The watch launcher uses this same canonical set to guard Node's raw
/// `--env-file` path at the process boundary. Keep the policy defined here so
/// parsed-map filtering and raw-file delegation cannot drift.
pub fn denied_env_file_keys() -> &'static [&'static str] {
    ENV_FILE_DENYLIST
}

/// Whether `key` is a denylisted runtime-control variable nub ignores from a
/// `.env*` / `--env-file` source. ASCII case-insensitive — environment variable
/// lookups are case-insensitive on Windows, so a lowercased spelling must not slip
/// one through (matches Deno's comparison).
pub fn is_denied_env_file_key(key: &str) -> bool {
    ENV_FILE_DENYLIST
        .iter()
        .any(|denied| key.eq_ignore_ascii_case(denied))
}

/// Remove every denylisted key from an env-file-sourced map, returning the dropped
/// keys (sorted, for a stable warning). Used by the explicit `--env-file` path,
/// which builds its map outside [`load_env_files_raw_reporting`] and so needs the
/// same strip applied for defense-in-depth (Deno denies from `--env-file` too).
pub fn strip_denied_env_file_keys(map: &mut HashMap<String, String>) -> Vec<String> {
    let mut dropped: Vec<String> = map
        .keys()
        .filter(|k| is_denied_env_file_key(k))
        .cloned()
        .collect();
    for key in &dropped {
        map.remove(key);
    }
    dropped.sort();
    dropped
}

/// Emit the "ignoring runtime-control var(s) from .env" notice at most once per
/// process. A denylisted key an env file tried to set was dropped; tell the user
/// where to set it instead so a legitimate use (e.g.
/// `NODE_OPTIONS=--max-old-space-size`) has a clear migration. `Once` guards
/// against repeats across multiple loads in one process.
pub fn warn_denied_env_file_keys(keys: &[String]) {
    if keys.is_empty() {
        return;
    }
    use std::sync::Once;
    static WARNED: Once = Once::new();
    WARNED.call_once(|| {
        eprintln!(
            "nub: ignoring {} from .env files; set them in your shell or CI environment instead",
            keys.join(", ")
        );
    });
}

/// Read an env file's contents, refusing anything that is not a regular file or
/// that exceeds the size cap, then read it. This guards against `read_to_string`
/// hanging or OOMing on a character device (`--env-file=/dev/zero`), a FIFO, or
/// a pathological file: `/dev/zero` reports size 0 yet streams forever, so the
/// `is_file` check — not the size cap — is what stops it. `metadata` follows
/// symlinks, so a `.env` symlinked to a device is rejected by its target.
/// Returns `None` on any guard failure or read error (caller treats it as an
/// absent/unreadable file).
pub fn read_env_file(path: &Path) -> Option<String> {
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > ENV_FILE_MAX_BYTES {
        return None;
    }
    fs::read_to_string(path).ok()
}

/// The canonical `NODE_ENV` values that may act as a mode fallback. Kept EXACT —
/// `development` / `production` / `test`, no short forms — for parity with Next.js
/// and Bun, which couple `.env.{mode}` to `NODE_ENV` only on these values. Any
/// other `NODE_ENV` (e.g. `staging`) is ignored for file selection, so an
/// arbitrary value can't silently flip the loaded file set (the footgun the modern
/// ecosystem decoupled away); use `APP_ENV` for arbitrary modes.
const CLAMPED_NODE_ENV_MODES: &[&str] = &["development", "production", "test"];

/// The `.env*` filenames Nub loads, in descending priority order (the file
/// listed first wins a key over later ones). Driven by the resolved *mode*. The
/// four-file taxonomy is ecosystem-wide, but the ecosystem SPLITS on where
/// `.env.local` sits: Next.js and Bun rank it above `.env.{mode}`, Vite and
/// dotenv-flow below. Nub follows Next.js / Bun, so `next build` under Nub sees
/// the environment `@next/env` would have computed. Node core has no mode cascade
/// at all (its `--env-file` loads named files only). Shared by [`load_env_files`]
/// (first-writer-wins merge) and [`discover_env_files`] (the watch path's
/// `--env-file` args), so this one function governs mode selection on both paths.
///
/// Mode precedence: `APP_ENV` (non-empty) is the primary, framework-neutral
/// selector (Vite and others) and wins even when `NODE_ENV` is also set. When
/// `APP_ENV` is unset, `NODE_ENV` is a CLAMPED fallback — it selects a mode only
/// when it is exactly `development` / `production` / `test`
/// ([`CLAMPED_NODE_ENV_MODES`], Next.js / Bun parity). Any other `NODE_ENV` value
/// yields no mode, as does both being unset — only `.env` / `.env.local` load. To
/// load a specific file directly, pass `--env-file <path>` (repeatable).
fn env_file_names() -> Vec<String> {
    env_file_names_for_mode(&resolve_env_mode())
}

/// Resolve the env-file mode from the ambient `APP_ENV` and `NODE_ENV`. Reads
/// process env once; [`resolve_mode`] holds the pure logic (hermetically testable).
fn resolve_env_mode() -> String {
    resolve_mode(
        std::env::var("APP_ENV").ok(),
        std::env::var("NODE_ENV").ok(),
    )
}

/// Pure mode resolution. `APP_ENV` (non-empty) is the primary selector and wins
/// even when `NODE_ENV` is also set. Otherwise `NODE_ENV` is a clamped fallback:
/// it yields a mode only when it is one of [`CLAMPED_NODE_ENV_MODES`]; any other
/// value (`staging`, empty) — or both being unset — resolves to no mode.
// @lat: [[research/env-file-loading#Research: eager  file loading#Synthesis]]
fn resolve_mode(app_env: Option<String>, node_env: Option<String>) -> String {
    if let Some(app_env) = app_env.filter(|v| !v.is_empty()) {
        return app_env;
    }
    node_env
        .filter(|v| CLAMPED_NODE_ENV_MODES.contains(&v.as_str()))
        .unwrap_or_default()
}

/// The `.env*` precedence list for a resolved mode. `is_test` (mode `test`) omits
/// `.env.local`, matching dotenv/Next; an empty mode yields only `.env.local` +
/// `.env`. Pure in `mode`, so callers pass any resolution and the ordering is
/// tested without touching process env.
// @lat: [[research/env-expansion-and-test-skip#Research:  expansion semantics + the -in-test convention#What Nub does# in test]]
fn env_file_names_for_mode(mode: &str) -> Vec<String> {
    // The mode is interpolated raw into `.env.{mode}` / `.env.{mode}.local` joined
    // to the project root, so a value carrying a path separator (`APP_ENV=x/../y`)
    // would escape the root; restrict the mode charset and treat anything outside
    // `[A-Za-z0-9_.-]` as no-mode (quietly loads only `.env` / `.env.local`). Real
    // modes are in-charset; `..` alone can't traverse without a `/`.
    let mode = if is_safe_mode(mode) { mode } else { "" };
    let is_test = mode == "test";

    let mut files = Vec::new();
    if !mode.is_empty() {
        files.push(format!(".env.{mode}.local"));
    }
    if !is_test {
        files.push(".env.local".to_string());
    }
    if !mode.is_empty() {
        files.push(format!(".env.{mode}"));
    }
    files.push(".env".to_string());
    files
}

/// A mode is safe to interpolate into a root-joined `.env.{mode}` path iff it
/// contains only `[A-Za-z0-9_.-]` — no path separator can appear, so it cannot
/// traverse out of the project root. An empty mode is trivially safe (no files).
fn is_safe_mode(mode: &str) -> bool {
    mode.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// The existing `.env*` file paths under `project_root`, in descending priority
/// order (highest-priority first — same order as [`load_env_files`]'s merge).
/// Used by `nub watch` to hand `--env-file=<path>` args to the watched Node so
/// Node watches and re-reads them across restarts, rather than freezing their
/// values at parent-spawn time. Only paths that currently exist and read as
/// regular files are returned, so a caller passing them to Node's `--env-file`
/// (which errors on a missing file) won't hit a spurious not-found.
///
/// NOTE — precedence inversion: Node's `--env-file` is *last*-writer-wins, the
/// inverse of this list's *first*-writer-wins order, so the caller must pass
/// these to Node in reverse for the priorities to line up.
pub fn discover_env_files(project_root: &Path) -> Vec<std::path::PathBuf> {
    env_file_names()
        .iter()
        .map(|name| project_root.join(name))
        .filter(|path| read_env_file(path).is_some())
        .collect()
}

/// A self-referential value grows MULTIPLICATIVELY per round, so the 10-round
/// bound alone does not bound the output: `A=${A}${A}${A}${A}${A}${A}${A}${A}`
/// is 8x per pass, i.e. 8^10 ≈ a billion characters, which leaves the process
/// thrashing swap rather than looping forever. Reachable from a cloned repo —
/// a `.env` file has always fed this, and a `nub.jsonc` `envFile` now does too.
///
/// A value that would exceed the cap keeps its PRE-expansion text rather than
/// being truncated: a truncated value is a silently wrong one, and the raw form
/// is at least visibly literal. 128 KiB is far above any real environment
/// variable and well under the point where this hurts.
const MAX_EXPANDED_VALUE: usize = 128 * 1024;

/// Expand `${VAR}` and `$VAR` references within all values of a map, in-place.
/// Multi-pass (up to 10 rounds) to resolve nested chains like `A=hello`,
/// `B=${A}_world`, `C=${B}_!`. Undefined references resolve to the empty string
/// (consistent with [`load_env_files`]). Mutates `map` in-place and returns it
/// for easy chaining.
// @lat: [[research/env-expansion-and-test-skip#Research:  expansion semantics + the -in-test convention#What Nub does#Expansion ruleset]]
pub fn expand_env_map(map: &mut HashMap<String, String>) -> &mut HashMap<String, String> {
    for _ in 0..10 {
        let snapshot = map.clone();
        let mut changed = false;
        for value in map.values_mut() {
            let Some(expanded) = expand_vars(value, &snapshot) else {
                continue;
            };
            if expanded != *value {
                *value = expanded;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    map
}

/// Load and merge `.env*` files WITHOUT `${VAR}` expansion — the *raw* values
/// Node's own `--env-file` parser would deliver. Shell env (from the parent
/// process) always wins; first-writer-wins across the `.env*` precedence.
///
/// The watch path uses this to decide which keys actually need nub's
/// pre-expansion injected: a key whose raw value equals its expanded value is
/// delivered identically by Node's `--env-file` (which re-reads on every
/// restart), so injecting it would freeze the startup value (#207). Only the
/// expansion-changed keys — which Node's `--env-file` can't reproduce — get
/// injected.
pub fn load_env_files_raw(project_root: &Path) -> HashMap<String, String> {
    load_env_files_raw_reporting(project_root).0
}

/// [`load_env_files_raw`], plus the same load-time notices [`load_env_files`]
/// emits. The watch path uses this so a dropped `NODE_ENV` or runtime-control key
/// is explained exactly once, as it is on a direct file run — without it, `nub
/// watch` silently ignores a `.env`-set `NODE_ENV` and the user has nothing to go
/// on when their mode does not apply. `Once` inside each warning keeps a
/// long-lived watcher to a single notice.
pub fn load_env_files_raw_warning(project_root: &Path) -> HashMap<String, String> {
    let (result, node_env_ignored, denied_keys) = load_env_files_raw_reporting(project_root);
    if node_env_ignored {
        warn_node_env_from_dotenv_ignored();
    }
    warn_denied_env_file_keys(&denied_keys);
    result
}

/// The raw loader, additionally reporting (1) whether a `.env` FILE declared
/// `NODE_ENV` (always dropped — dotenv/@next/env/Vite parity, #263) and (2) which
/// denylisted runtime-control keys were dropped ([`ENV_FILE_DENYLIST`]). Both
/// drops happen HERE at load, so every consumer of the returned map is covered by
/// construction. The reports are consumed by [`load_env_files`] to warn. The
/// plain [`load_env_files_raw`] wrapper discards them because watch separately
/// delegates the raw files to Node; that spawn boundary re-applies these same
/// drops — this denylist, plus `NODE_ENV` when the auto cascade is the live
/// source — before handing Node the paths.
fn load_env_files_raw_reporting(
    project_root: &Path,
) -> (HashMap<String, String>, bool, Vec<String>) {
    let files = env_file_names();

    let mut result = HashMap::new();
    let mut node_env_ignored = false;
    let mut denied_keys: Vec<String> = Vec::new();

    for filename in &files {
        let path = project_root.join(filename);
        if let Some(content) = read_env_file(&path) {
            for (key, value) in parse_env(&content) {
                // Shell env wins: don't override existing env vars. An AMBIENT
                // `NODE_ENV` (set in the real process env) therefore passes through
                // untouched and, when `APP_ENV` is unset and it is canonical
                // (development/production/test), selects `.env.<NODE_ENV>` files above
                // as the clamped fallback — only a `.env`-FILE `NODE_ENV` is dropped
                // below.
                if std::env::var_os(&key).is_some() {
                    continue;
                }
                // dotenv / @next/env / Vite parity: a `.env` FILE never sets
                // `NODE_ENV`. Injecting it broke `next build` — the prerender forks
                // inherited `NODE_ENV=development`, loading the dev React against
                // production-compiled chunks (two `ReactSharedInternals` → null hooks
                // dispatcher → `useContext` of null), #263. Reaching here means ambient
                // `NODE_ENV` is unset (shell-wins above), so this value WOULD have been
                // injected — drop it and flag it for the caller's warning.
                if key == "NODE_ENV" {
                    node_env_ignored = true;
                    continue;
                }
                // Env hygiene (Deno parity): a `.env` FILE configures the user's
                // PROGRAM, not the runtime — so a runtime-control var it sets
                // ([`ENV_FILE_DENYLIST`] — `NODE_OPTIONS` et al.) is ignored rather
                // than silently reconfiguring Node's start-up. Reaching here means it
                // is not ambiently set (shell-wins above), so it WOULD have been
                // injected → drop it and flag it for the warning.
                if is_denied_env_file_key(&key) {
                    if !denied_keys.contains(&key) {
                        denied_keys.push(key);
                    }
                    continue;
                }
                // First writer wins among .env files.
                result.entry(key).or_insert(value);
            }
        }
    }

    denied_keys.sort();
    (result, node_env_ignored, denied_keys)
}

/// Emit the "ignoring `.env` `NODE_ENV`" notice at most once per process. Called
/// only on the direct run/file injection path ([`load_env_files`]), where the
/// dropped `NODE_ENV` genuinely never reaches the child; the watch path re-applies
/// the drop at its spawn boundary and warns through
/// [`load_env_files_raw_warning`]. `Once` guards against any repeat, so a
/// long-lived watcher still emits this at most once.
fn warn_node_env_from_dotenv_ignored() {
    use std::sync::Once;
    static WARNED: Once = Once::new();
    WARNED.call_once(|| {
        eprintln!(
            "nub: ignoring NODE_ENV set in .env (dotenv, Next, and Vite do the same); set it in the environment instead"
        );
    });
}

/// Load .env* files from the project root, returning the key-value
/// pairs to inject into the child process environment. Shell env
/// (from the parent process) always wins — values already set in
/// the process environment are not overridden.
pub fn load_env_files(project_root: &Path) -> HashMap<String, String> {
    let (mut result, node_env_ignored, denied_keys) = load_env_files_raw_reporting(project_root);

    // Expand ${VAR} references within values. Multi-pass to handle
    // nested references like A=hello, B=${A}_world, C=${B}_!.
    expand_env_map(&mut result);

    // This map is injected straight into the child via `Command::env`, so a
    // dropped `.env` `NODE_ENV` / denylisted var truly never reaches it. The watch
    // path re-applies the same drops at its spawn boundary and reports them
    // through [`load_env_files_raw_warning`]; both sites share the `Once` guards
    // inside these two calls, so a long-lived watcher still emits each notice at
    // most once.
    if node_env_ignored {
        warn_node_env_from_dotenv_ignored();
    }
    warn_denied_env_file_keys(&denied_keys);

    result
}

/// Parse a .env file with Node-`--env-file`-compatible semantics.
///
/// Node only treats quotes as syntax when the trimmed value starts with `'`,
/// `"`, or `` ` ``. Regular unquoted values are otherwise copied up to the
/// newline or inline `#` comment and then trimmed, so JSON-looking values keep
/// their inner quotes and backslash escapes. Later keys override earlier ones
/// (Node's `insert_or_assign` / last-writer-wins), preserving first-seen order
/// for callers that fold these pairs into a `HashMap`.
pub fn parse_env(content: &str) -> Vec<(String, String)> {
    // Node removes carriage returns before scanning.
    let content = content.replace('\r', "");
    let content = content.strip_prefix('\u{feff}').unwrap_or(&content);
    let mut rest = trim_env_spaces(content);

    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();

    while !rest.is_empty() {
        if rest.starts_with('\n') || rest.starts_with('#') {
            rest = trim_env_spaces(skip_line(rest));
            continue;
        }

        let Some(equal_or_newline) = rest.find(['=', '\n']) else {
            break;
        };
        if rest.as_bytes()[equal_or_newline] == b'\n' {
            rest = trim_env_spaces(&rest[equal_or_newline + 1..]);
            continue;
        }

        let mut key = trim_env_spaces(&rest[..equal_or_newline]);
        rest = &rest[equal_or_newline + 1..];
        if let Some(stripped) = key.strip_prefix("export ") {
            key = trim_env_spaces(stripped);
        }
        if key.is_empty() {
            rest = trim_env_spaces(skip_line(rest));
            continue;
        }

        if rest.is_empty() || rest.starts_with('\n') {
            upsert_env_pair(&mut pairs, &mut seen, key.to_string(), String::new());
            rest = match rest.find('\n') {
                Some(newline) => trim_env_spaces(&rest[newline + 1..]),
                None => "",
            };
            continue;
        }

        rest = trim_env_spaces(rest);
        if rest.is_empty() {
            upsert_env_pair(&mut pairs, &mut seen, key.to_string(), String::new());
            break;
        }

        if rest.starts_with('"') {
            if let Some(closing_quote) = closing_quote(rest, '"') {
                let value = rest[1..closing_quote].replace("\\n", "\n");
                upsert_env_pair(&mut pairs, &mut seen, key.to_string(), value);
                rest = trim_env_spaces(after_value_line(rest, closing_quote + 1));
                continue;
            }
        }

        if let Some(quote) = leading_quote(rest) {
            if let Some(closing_quote) = closing_quote(rest, quote) {
                let value = rest[1..closing_quote].to_string();
                upsert_env_pair(&mut pairs, &mut seen, key.to_string(), value);
                rest = trim_env_spaces(after_value_line(rest, closing_quote + 1));
                continue;
            }

            let (line, next) = split_line(rest);
            upsert_env_pair(&mut pairs, &mut seen, key.to_string(), line.to_string());
            rest = trim_env_spaces(next);
            continue;
        }

        let (line, next) = split_line(rest);
        let value = line.split_once('#').map(|(value, _)| value).unwrap_or(line);
        upsert_env_pair(
            &mut pairs,
            &mut seen,
            key.to_string(),
            trim_env_spaces(value).to_string(),
        );
        rest = trim_env_spaces(next);
    }

    pairs
}

fn trim_env_spaces(input: &str) -> &str {
    input.trim_matches(|c| matches!(c, ' ' | '\t' | '\n'))
}

fn skip_line(input: &str) -> &str {
    split_line(input).1
}

fn split_line(input: &str) -> (&str, &str) {
    match input.find('\n') {
        Some(newline) => (&input[..newline], &input[newline + 1..]),
        None => (input, ""),
    }
}

fn after_value_line(input: &str, from: usize) -> &str {
    input[from..]
        .find('\n')
        .map(|newline| &input[from + newline + 1..])
        .unwrap_or("")
}

fn leading_quote(input: &str) -> Option<char> {
    match input.as_bytes().first().copied() {
        Some(b'\'') => Some('\''),
        Some(b'"') => Some('"'),
        Some(b'`') => Some('`'),
        _ => None,
    }
}

fn closing_quote(input: &str, quote: char) -> Option<usize> {
    input[1..].find(quote).map(|idx| idx + 1)
}

fn upsert_env_pair(
    pairs: &mut Vec<(String, String)>,
    seen: &mut HashMap<String, usize>,
    key: String,
    value: String,
) {
    if let Some(&idx) = seen.get(&key) {
        pairs[idx].1 = value;
    } else {
        seen.insert(key.clone(), pairs.len());
        pairs.push((key, value));
    }
}

/// Append `text`, or give up if that would grow the expansion past its cap.
fn push_bounded(result: &mut String, text: &str) -> Option<()> {
    (result.len() + text.len() <= MAX_EXPANDED_VALUE).then(|| result.push_str(text))
}

/// [`push_bounded`] for one literal character.
fn push_char_bounded(result: &mut String, ch: char) -> Option<()> {
    (result.len() + ch.len_utf8() <= MAX_EXPANDED_VALUE).then(|| result.push(ch))
}

/// Append one `$VAR` / `${VAR}` expansion. The map wins over the ambient process
/// environment, and an undefined name expands to nothing.
fn push_var_bounded(result: &mut String, name: &str, env: &HashMap<String, String>) -> Option<()> {
    match env.get(name) {
        Some(resolved) => push_bounded(result, resolved),
        None => match std::env::var(name) {
            Ok(resolved) => push_bounded(result, &resolved),
            Err(_) => Some(()),
        },
    }
}

/// Expand `${VAR}` and `$VAR` references in a value.
///
/// `None` means the expansion would exceed [`MAX_EXPANDED_VALUE`]. Callers keep
/// the raw value in that case rather than publishing a truncated replacement.
fn expand_vars(value: &str, env: &HashMap<String, String>) -> Option<String> {
    let mut result = String::new();
    let chars: Vec<char> = value.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == '\\' && i + 1 < chars.len() && chars[i + 1] == '$' {
            push_bounded(&mut result, "$")?;
            i += 2;
            continue;
        }

        if chars[i] == '$' {
            if i + 1 < chars.len() && chars[i + 1] == '{' {
                // ${VAR} form
                if let Some(close) = chars[i + 2..].iter().position(|&c| c == '}') {
                    let var_name: String = chars[i + 2..i + 2 + close].iter().collect();
                    push_var_bounded(&mut result, &var_name, env)?;
                    i += close + 3;
                    continue;
                }
            } else if i + 1 < chars.len() && chars[i + 1].is_ascii_alphabetic() {
                // $VAR form
                let start = i + 1;
                let mut end = start;
                while end < chars.len() && (chars[end].is_ascii_alphanumeric() || chars[end] == '_')
                {
                    end += 1;
                }
                let var_name: String = chars[start..end].iter().collect();
                push_var_bounded(&mut result, &var_name, env)?;
                i = end;
                continue;
            }
        }

        push_char_bounded(&mut result, chars[i])?;
        i += 1;
    }

    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A self-referential value must not be expanded into swap. The 10-round
    /// bound does not bound the OUTPUT — each pass multiplies, so eight
    /// self-references reach ~8^10 characters and the process thrashes long
    /// before it terminates. Measured before the cap: `nub` on a project whose
    /// `envFile` held this line did not finish inside 15s.
    #[test]
    fn a_self_referential_value_cannot_blow_up_the_expansion() {
        let mut map = HashMap::new();
        map.insert(
            "A".to_string(),
            "${A}${A}${A}${A}${A}${A}${A}${A}".to_string(),
        );
        map.insert("SANE".to_string(), "plain".to_string());

        expand_env_map(&mut map);

        assert!(
            map["A"].len() <= MAX_EXPANDED_VALUE,
            "runaway value grew to {} bytes",
            map["A"].len()
        );
        // The control: capping the pathological value must not stop ordinary
        // expansion happening around it.
        assert_eq!(map["SANE"], "plain");
        let mut chain = HashMap::new();
        chain.insert("A".to_string(), "hello".to_string());
        chain.insert("B".to_string(), "${A}_world".to_string());
        expand_env_map(&mut chain);
        assert_eq!(chain["B"], "hello_world", "normal chains still expand");
    }

    #[test]
    fn expansion_cap_stops_before_copying_oversized_literal_or_replacement() {
        let oversized_literal = "x".repeat(MAX_EXPANDED_VALUE + 1);
        assert_eq!(expand_vars(&oversized_literal, &HashMap::new()), None);

        let mut map = HashMap::new();
        map.insert("SOURCE".to_string(), "x".repeat(MAX_EXPANDED_VALUE + 1));
        map.insert("FROM_SOURCE".to_string(), "${SOURCE}".to_string());

        expand_env_map(&mut map);

        assert_eq!(
            map["FROM_SOURCE"], "${SOURCE}",
            "an oversized single replacement keeps the raw value"
        );
    }

    #[test]
    fn expansion_cap_allows_exact_boundary_and_ordinary_escaped_values() {
        let mut map = HashMap::new();
        map.insert("SOURCE".to_string(), "x".repeat(MAX_EXPANDED_VALUE));
        map.insert("AT_LIMIT".to_string(), "${SOURCE}".to_string());
        map.insert("SMALL".to_string(), "resolved".to_string());
        map.insert(
            "ESCAPED".to_string(),
            r"\$SMALL:${MISSING}:$SMALL".to_string(),
        );

        assert_eq!(
            expand_vars(&map["ESCAPED"], &map),
            Some("$SMALL::resolved".to_string()),
            "one pass keeps an escaped dollar literal and drops a missing variable"
        );

        expand_env_map(&mut map);

        assert_eq!(map["AT_LIMIT"].len(), MAX_EXPANDED_VALUE);
        assert_eq!(map["ESCAPED"], "resolved::resolved");
    }

    #[test]
    fn parse_simple_env() {
        let pairs = parse_env("FOO=bar\nBAZ=qux\n");
        assert_eq!(
            pairs,
            vec![
                ("FOO".to_string(), "bar".to_string()),
                ("BAZ".to_string(), "qux".to_string()),
            ]
        );
    }

    /// Mode precedence: `APP_ENV` (non-empty) is primary and wins even when
    /// `NODE_ENV` is also set; otherwise `NODE_ENV` is a CLAMPED fallback that
    /// selects only on the canonical `development`/`production`/`test` values. A
    /// non-canonical `NODE_ENV` (`staging`) or both-unset yields no mode.
    #[test]
    fn resolve_mode_app_env_primary_node_env_clamped_fallback() {
        let s = |v: &str| Some(v.to_string());

        // APP_ENV set → that mode, even an arbitrary one (APP_ENV isn't clamped).
        assert_eq!(resolve_mode(s("production"), None), "production");
        assert_eq!(resolve_mode(s("staging"), None), "staging");
        // APP_ENV wins over NODE_ENV when both are set.
        assert_eq!(resolve_mode(s("staging"), s("production")), "staging");
        assert_eq!(resolve_mode(s("production"), s("test")), "production");

        // APP_ENV unset → NODE_ENV as a clamped fallback, canonical values only.
        assert_eq!(resolve_mode(None, s("production")), "production");
        assert_eq!(resolve_mode(None, s("development")), "development");
        assert_eq!(resolve_mode(None, s("test")), "test");
        // Empty APP_ENV is treated as unset, so the fallback still applies.
        assert_eq!(resolve_mode(s(""), s("production")), "production");

        // Non-canonical NODE_ENV is ignored for file selection (no arbitrary-value
        // footgun) — and short forms are NOT accepted, matching Next.js/Bun.
        assert_eq!(resolve_mode(None, s("staging")), "");
        assert_eq!(resolve_mode(None, s("prod")), "");
        assert_eq!(resolve_mode(None, s("dev")), "");
        assert_eq!(resolve_mode(None, s("")), "");

        // Both unset → no mode.
        assert_eq!(resolve_mode(None, None), "");
    }

    /// A mode carrying a path separator (a hostile `APP_ENV`/`NODE_ENV`)
    /// must not build `.env.{mode}` filenames — it would traverse out of the
    /// project root when joined. Such a value is treated as no-mode (only `.env` /
    /// `.env.local` load); real modes stay in-charset and are unaffected.
    #[test]
    fn out_of_charset_mode_is_treated_as_no_mode() {
        // Separator-bearing values collapse to the base cascade — no `.env.<x>`.
        for hostile in ["x/../../etc", "../secrets", "a\\b", "a b", "$(x)"] {
            assert_eq!(
                env_file_names_for_mode(hostile),
                vec![".env.local", ".env"],
                "hostile mode {hostile:?} must not build a `.env.{{mode}}` name"
            );
        }
        // In-charset values (incl. dots and dashes) remain valid modes.
        assert!(is_safe_mode("production"));
        assert!(is_safe_mode("test.ci-1"));
        assert!(!is_safe_mode("x/../y"));
    }

    /// The resolved mode drives which `.env.{mode}` files are selected, so the
    /// precedence table's outcome (which value wins `WHICH`) is `.env.{mode}`
    /// ranking above `.env`. Highest-priority-first; `.env.{mode}.local` and
    /// `.env.{mode}` bracket `.env.local`, and `test` mode omits `.env.local`.
    #[test]
    fn env_file_names_for_mode_orders_mode_files_above_base() {
        assert_eq!(
            env_file_names_for_mode("production"),
            vec![
                ".env.production.local",
                ".env.local",
                ".env.production",
                ".env",
            ]
        );
        // Empty mode (neither var set) → only `.env.local` + `.env`, so `.env` wins.
        assert_eq!(env_file_names_for_mode(""), vec![".env.local", ".env"]);
        // `test` mode drops `.env.local` (dotenv/Next parity), keyed off the
        // resolved mode — so `APP_ENV=test` skips it.
        assert_eq!(
            env_file_names_for_mode("test"),
            vec![".env.test.local", ".env.test", ".env"]
        );
    }

    /// End-to-end file-set selection through the clamped `NODE_ENV` fallback,
    /// composing `resolve_mode` with `env_file_names_for_mode` (hermetic — no
    /// process env). `NODE_ENV=production` (APP_ENV unset) selects the production
    /// cascade; `NODE_ENV=test` inherits the `.env.local` skip, consistent with
    /// `APP_ENV=test`; a non-canonical `NODE_ENV=staging` selects no mode.
    #[test]
    fn clamped_node_env_fallback_selects_the_expected_file_set() {
        let names = |app: Option<&str>, node: Option<&str>| {
            env_file_names_for_mode(&resolve_mode(
                app.map(str::to_string),
                node.map(str::to_string),
            ))
        };

        assert_eq!(
            names(None, Some("production")),
            vec![
                ".env.production.local",
                ".env.local",
                ".env.production",
                ".env"
            ],
        );
        // NODE_ENV=test → the same `.env.local` skip as APP_ENV=test.
        assert_eq!(
            names(None, Some("test")),
            vec![".env.test.local", ".env.test", ".env"],
        );
        // Non-canonical NODE_ENV → no mode: only the base cascade.
        assert_eq!(names(None, Some("staging")), vec![".env.local", ".env"]);
        // Both unset → no mode.
        assert_eq!(names(None, None), vec![".env.local", ".env"]);
        // APP_ENV wins even when NODE_ENV is also a canonical value.
        assert_eq!(
            names(Some("staging"), Some("production")),
            vec![".env.staging.local", ".env.local", ".env.staging", ".env"],
        );
    }

    #[test]
    fn parse_quoted_values() {
        let pairs = parse_env("A=\"hello world\"\nB='single'\n");
        assert_eq!(pairs[0].1, "hello world");
        assert_eq!(pairs[1].1, "single");
    }

    #[test]
    fn unquoted_json_value_is_verbatim() {
        let pairs = parse_env("FOO={\"field\":\"line1\\nline2\"}\n");
        let value = pairs
            .iter()
            .find(|(key, _)| key == "FOO")
            .map(|(_, value)| value.as_str());
        assert_eq!(value, Some("{\"field\":\"line1\\nline2\"}"));
    }

    /// Node's `--env-file` treats backticks as a third quote style alongside
    /// `'` and `"` (`src/node_dotenv.cc`): the surrounding backticks are
    /// stripped and the content is taken verbatim. dotenvy alone leaves the
    /// backticks in the value, so [`parse_env`] must close the gap. Covers all
    /// three quote styles plus the empty-backtick case the regression flagged
    /// (`parallel/test-dotenv.js` BACKTICKS / EMPTY_BACKTICKS). Reference
    /// values were captured from node-v25.8.1's `--env-file` on this fixture.
    #[test]
    fn strips_surrounding_quotes_for_single_double_and_backtick() {
        let pairs = parse_env(concat!(
            "SQ='hi'\n",
            "DQ=\"hi\"\n",
            "BT=`hi`\n",
            "EMPTY_BT=``\n",
            "SPACED_BT=`    pad    `\n",
        ));
        let get = |k: &str| pairs.iter().find(|(p, _)| p == k).map(|(_, v)| v.as_str());
        assert_eq!(get("SQ"), Some("hi"));
        assert_eq!(get("DQ"), Some("hi"));
        assert_eq!(get("BT"), Some("hi"), "backtick value must be unwrapped");
        assert_eq!(
            get("EMPTY_BT"),
            Some(""),
            "empty backticks must yield an empty string, not ``"
        );
        assert_eq!(
            get("SPACED_BT"),
            Some("    pad    "),
            "interior whitespace inside backticks is preserved verbatim"
        );
    }

    /// Backtick content is verbatim the way Node's parser is: no `$`
    /// substitution, no `\n` unescaping, inner quotes retained, a trailing
    /// inline comment after the closing backtick stripped, and the value may
    /// span newlines until the closing backtick. These are the exact cases in
    /// `test/fixtures/dotenv/valid.env`; values match node-v25.8.1.
    #[test]
    fn backtick_values_are_verbatim_and_may_span_lines() {
        let pairs = parse_env(concat!(
            "INNER=`{\"foo\": \"bar's\"}`\n",
            "NOEXPAND=`he$X llo`\n",
            "NOESCAPE=`a\\nb`\n",
            "COMMENT=`outside #hash` # work\n",
            "MULTI=`THIS\nIS\n\"MULTI'S\"\nSTRING`\n",
            "AFTER=plain\n",
        ));
        let get = |k: &str| pairs.iter().find(|(p, _)| p == k).map(|(_, v)| v.as_str());
        assert_eq!(get("INNER"), Some("{\"foo\": \"bar's\"}"));
        assert_eq!(
            get("NOEXPAND"),
            Some("he$X llo"),
            "no $-substitution in backticks"
        );
        assert_eq!(
            get("NOESCAPE"),
            Some("a\\nb"),
            "no escape processing in backticks"
        );
        assert_eq!(get("COMMENT"), Some("outside #hash"));
        assert_eq!(get("MULTI"), Some("THIS\nIS\n\"MULTI'S\"\nSTRING"));
        // A line following a multi-line backtick value must still parse.
        assert_eq!(
            get("AFTER"),
            Some("plain"),
            "parsing resumes after the closing backtick line"
        );
    }

    #[test]
    fn parse_comments_and_blanks() {
        let pairs = parse_env("# comment\n\nFOO=bar\n");
        assert_eq!(pairs.len(), 1);
    }

    #[test]
    fn parse_export_prefix() {
        let pairs = parse_env("export FOO=bar\n");
        assert_eq!(pairs, vec![("FOO".to_string(), "bar".to_string())]);
    }

    #[test]
    fn read_env_file_reads_a_regular_file() {
        let p = std::env::temp_dir().join(format!("nub-a41-{}.env", std::process::id()));
        std::fs::write(&p, "FOO=bar\n").unwrap();
        assert_eq!(read_env_file(&p).as_deref(), Some("FOO=bar\n"));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn read_env_file_rejects_unbounded_and_missing_sources() {
        // The guard refuses anything that isn't a regular file, so a hostile
        // --env-file can't stream forever or OOM (A41).
        assert_eq!(
            read_env_file(&std::env::temp_dir()),
            None,
            "directory rejected"
        );
        assert_eq!(
            read_env_file(Path::new("/nonexistent-nub-a41")),
            None,
            "missing rejected"
        );
        #[cfg(unix)]
        assert_eq!(
            read_env_file(Path::new("/dev/zero")),
            None,
            "char device rejected — would otherwise read forever"
        );
    }

    #[test]
    fn parse_multiline_double_quoted() {
        let pairs = parse_env("KEY=\"line1\nline2\"\n");
        assert_eq!(pairs[0].1, "line1\nline2");
    }

    #[test]
    fn parse_escape_sequences() {
        let pairs = parse_env("KEY=\"hello\\nworld\"\n");
        assert_eq!(pairs[0].1, "hello\nworld");
    }

    #[test]
    fn parse_inline_comments() {
        let pairs = parse_env("FOO=bar # this is a comment\n");
        assert_eq!(pairs[0].1, "bar");
    }

    #[test]
    fn expand_dollar_brace() {
        let mut env = HashMap::new();
        env.insert("HOST".to_string(), "localhost".to_string());
        assert_eq!(
            expand_vars("http://${HOST}:3000", &env),
            Some("http://localhost:3000".to_string())
        );
    }

    #[test]
    fn expand_dollar_bare() {
        let mut env = HashMap::new();
        env.insert("PORT".to_string(), "8080".to_string());
        assert_eq!(
            expand_vars("port=$PORT", &env),
            Some("port=8080".to_string())
        );
    }

    #[test]
    fn expand_escaped_dollar() {
        let env = HashMap::new();
        assert_eq!(
            expand_vars("price=\\$5", &env),
            Some("price=$5".to_string())
        );
    }

    // `discover_env_files` underpins `nub watch`'s `--env-file` precedence: it
    // must return only files that exist, highest-priority first, so the watch
    // path can reverse them into Node's last-writer-wins order. Locking the
    // ordering + existence-filtering here guards that translation. (The reload
    // behavior itself — Node re-reading `--env-file` on `--watch` restart — is
    // timing-dependent and verified ad hoc, not unit-tested; see `run_watch`.)
    #[test]
    fn discover_env_files_returns_existing_files_highest_priority_first() {
        let dir = std::env::temp_dir().join(format!("nub-discover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Create `.env` and `.env.local` but deliberately omit `.env.production`,
        // so the absent priority slot must be skipped.
        std::fs::write(dir.join(".env"), "X=1\n").unwrap();
        std::fs::write(dir.join(".env.local"), "X=2\n").unwrap();

        let found = discover_env_files(&dir);

        assert!(
            found.iter().all(|p| p.is_file()),
            "every returned path must exist (no `.env.production` slot for an absent file): {found:?}"
        );
        // `.env` is the lowest-priority slot, so it is always last when present.
        assert_eq!(
            found.last(),
            Some(&dir.join(".env")),
            "`.env` must sort last (lowest priority): {found:?}"
        );
        // `.env.local` outranks `.env` (except under NODE_ENV=test, which omits
        // it); when both are returned, `.env.local` must precede `.env`.
        if found.contains(&dir.join(".env.local")) {
            let local = found
                .iter()
                .position(|p| p == &dir.join(".env.local"))
                .unwrap();
            let base = found.iter().position(|p| p == &dir.join(".env")).unwrap();
            assert!(local < base, "`.env.local` must precede `.env`: {found:?}");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `load_env_files` must expand `${VAR}` cross-references, matching the
    /// behavior the direct `nub <file>` path delivers. This is the regression
    /// guard for the bug where `nub watch` / `--env-file` left `${VAR}` literal.
    #[test]
    fn load_env_files_expands_var_references() {
        let dir = std::env::temp_dir().join(format!("nub-expand-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(".env"),
            "DB_HOST=localhost\nDATABASE_URL=postgres://${DB_HOST}:5432/db\n",
        )
        .unwrap();

        let vars = load_env_files(&dir);

        assert_eq!(
            vars.get("DATABASE_URL").map(String::as_str),
            Some("postgres://localhost:5432/db"),
            "`${{DB_HOST}}` must be expanded to its value; got {vars:?}"
        );
        assert_eq!(vars.get("DB_HOST").map(String::as_str), Some("localhost"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `load_env_files_raw` returns the UNEXPANDED file values — the raw bytes
    /// Node's own `--env-file` parser delivers — while `load_env_files` expands
    /// `${VAR}`. The watch path diffs the two to decide which vars to inject vs
    /// leave to Node's live-reloading `--env-file` (#207); this guards that
    /// distinction (a plain var is identical in both; an expanded var differs).
    #[test]
    fn load_env_files_raw_leaves_var_references_literal() {
        let dir = std::env::temp_dir().join(format!("nub-raw-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(".env"),
            "DB_HOST=localhost\nDATABASE_URL=postgres://${DB_HOST}:5432/db\nPLAIN=p1\n",
        )
        .unwrap();

        let raw = load_env_files_raw(&dir);
        let expanded = load_env_files(&dir);

        // Plain var: identical in both → the watch path leaves it to `--env-file`.
        assert_eq!(raw.get("PLAIN"), expanded.get("PLAIN"));
        assert_eq!(raw.get("PLAIN").map(String::as_str), Some("p1"));
        // Expanded var: raw keeps `${DB_HOST}` literal; expanded resolves it →
        // the watch path injects this one.
        assert_eq!(
            raw.get("DATABASE_URL").map(String::as_str),
            Some("postgres://${DB_HOST}:5432/db"),
            "raw load must NOT expand ${{DB_HOST}}; got {raw:?}"
        );
        assert_eq!(
            expanded.get("DATABASE_URL").map(String::as_str),
            Some("postgres://localhost:5432/db")
        );
        assert_ne!(raw.get("DATABASE_URL"), expanded.get("DATABASE_URL"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #263: a `.env` FILE must never inject `NODE_ENV` into the child env
    /// (dotenv/@next/env/Vite parity). Leaking `NODE_ENV=development` into
    /// `next build` loaded the dev React against production-compiled chunks → a
    /// null hooks dispatcher. Sibling keys still load; `.env.<NODE_ENV>` file
    /// SELECTION is untouched — it reads AMBIENT `NODE_ENV`, never this map — so
    /// the assertion holds whether or not the test process itself has `NODE_ENV`
    /// set (an ambient value is dropped by shell-wins; a file value by the strip).
    #[test]
    fn dotenv_node_env_is_never_injected() {
        let dir = std::env::temp_dir().join(format!("nub-nodeenv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".env"), "NODE_ENV=development\nAPP_KEY=secret\n").unwrap();

        let vars = load_env_files(&dir);

        assert!(
            !vars.contains_key("NODE_ENV"),
            "a `.env` file must not inject NODE_ENV (#263); got {vars:?}"
        );
        assert_eq!(
            vars.get("APP_KEY").map(String::as_str),
            Some("secret"),
            "sibling keys must still load after NODE_ENV is stripped; got {vars:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every denylisted key, canonically spelled except `NODE_OPTIONS`, which is
    /// lower-cased so the ASCII-case-insensitive match is exercised rather than
    /// assumed.
    fn denied_keys_under_test() -> Vec<String> {
        denied_env_file_keys()
            .iter()
            .map(|key| {
                if *key == "NODE_OPTIONS" {
                    key.to_ascii_lowercase()
                } else {
                    (*key).to_string()
                }
            })
            .collect()
    }

    /// Env hygiene (Deno parity): a `.env` FILE must never inject a runtime-control
    /// var ([`ENV_FILE_DENYLIST`]) — those configure Node's own start-up, not the
    /// user's program. Benign siblings must still load. The strip runs in the
    /// shared per-file loop, so `.env` coverage exercises it for every `.env*`
    /// file — mode-file SELECTION is a separate concern (tested by
    /// `env_file_names*`) and needs no ambient env here.
    #[test]
    fn denylisted_runtime_control_vars_never_injected() {
        let dir = std::env::temp_dir().join(format!("nub-deny-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut source = String::new();
        for key in denied_keys_under_test() {
            source.push_str(&format!("{key}=blocked\n"));
        }
        source.push_str("APP_KEY=safe\n");
        std::fs::write(dir.join(".env"), source).unwrap();

        let base = load_env_files(&dir);
        for denied in denied_env_file_keys() {
            assert!(
                !base.keys().any(|key| key.eq_ignore_ascii_case(denied)),
                "a `.env` {denied} must not reach the child; got {base:?}"
            );
        }
        assert_eq!(
            base.get("APP_KEY").map(String::as_str),
            Some("safe"),
            "benign siblings must still load after the denylist strip; got {base:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `strip_denied_env_file_keys` (the explicit `--env-file` path's guard) removes
    /// every denylisted key case-insensitively, leaves benign keys, and returns the
    /// dropped keys sorted.
    #[test]
    fn strip_denied_env_file_keys_removes_control_vars() {
        let mut map: HashMap<String, String> = denied_keys_under_test()
            .into_iter()
            .map(|key| (key, "blocked".to_string()))
            .collect();
        map.insert("PORT".to_string(), "3000".to_string());

        let dropped = strip_denied_env_file_keys(&mut map);

        assert_eq!(dropped.len(), denied_env_file_keys().len());
        assert!(dropped.is_sorted(), "the warning needs a stable order");
        for denied in denied_env_file_keys() {
            assert!(
                !map.keys().any(|key| key.eq_ignore_ascii_case(denied)),
                "explicit env map retained {denied}: {map:?}"
            );
        }
        assert_eq!(map.get("PORT").map(String::as_str), Some("3000"));
        assert!(is_denied_env_file_key("node_options"));
        assert!(!is_denied_env_file_key("PATH"));
    }

    /// The wire shape of the fresh-invocation protocol, spelled out on purpose:
    /// every other denylist test iterates [`denied_env_file_keys`] and so would
    /// still pass if a key were dropped from the list. This one pins the names.
    #[test]
    fn fresh_invocation_wire_vars_are_denied_from_env_files() {
        for key in [
            "__NUB_COMPAT_NODE_OPTIONS",
            "__NUB_COMPAT_NODE_PATH",
            "__NUB_COMPAT_NODE",
            "__NUB_COMPAT_NODE_COMPILE_CACHE",
            "__NUB_COMPAT_PATH",
            "__NUB_COMPAT_PRESENT",
            "__NUB_AUGMENTED_NODE_OPTIONS",
            "__NUB_AUGMENTED_NODE_PATH",
            "__NUB_AUGMENTED_NODE",
            "__NUB_AUGMENTED_NODE_COMPILE_CACHE",
            "__NUB_AUGMENTED_PATH",
            "__NUB_AUGMENTED_NODE_OPTIONS_PRESENT",
            "__NUB_AUGMENTED_NODE_PATH_PRESENT",
            "__NUB_AUGMENTED_NODE_PRESENT",
            "__NUB_AUGMENTED_NODE_COMPILE_CACHE_PRESENT",
            "__NUB_AUGMENTED_PATH_PRESENT",
        ] {
            assert!(
                is_denied_env_file_key(key),
                "{key} is parent-owned invocation metadata, not project env"
            );
        }
    }

    /// `expand_env_map` (used by the `nub.jsonc` `envFile` source path) must
    /// apply the same multi-pass expansion as `load_env_files`. It is NOT used
    /// by the `--env-file` flag, which delivers values verbatim like Node.
    #[test]
    fn expand_env_map_expands_var_references() {
        let mut map = HashMap::new();
        map.insert("DB_HOST".to_string(), "localhost".to_string());
        map.insert(
            "DATABASE_URL".to_string(),
            "postgres://${DB_HOST}:5432/db".to_string(),
        );

        expand_env_map(&mut map);

        assert_eq!(
            map.get("DATABASE_URL").map(String::as_str),
            Some("postgres://localhost:5432/db"),
            "`${{DB_HOST}}` must be expanded; got {map:?}"
        );
    }
}
