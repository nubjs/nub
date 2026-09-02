//! What nub's `config` surface SHOWS and ACCEPTS, driven through the real
//! binary: the DEFAULT column of `config list --all`, and which settings
//! `config set` will write.
//!
//! That column is the shared engine table's `default` field — a build-time
//! constant. A directory default written as a literal is therefore baked with
//! the ENGINE's namespace and printed verbatim by whichever host embeds the
//! engine, so nub advertised its cache default as `$XDG_CACHE_HOME/aube`, a
//! directory nub never uses. The defaults now carry namespace tokens that
//! resolve against the active embedder at display time.
//!
//! Offline: `config list` reads config files and the static table, nothing else.
//!
//! The file also holds the brand-cleanliness assertion for this command, which
//! nothing covered while `aubeNoAutoInstall` was still in the listing — the
//! engine's brand in a setting NAME, on a setting nub never reads. It reaches
//! the listing through the shared settings table rather than through any nub
//! code path, so `pm_publish_store_config`'s per-spawn `assert_brand_clean`
//! could never have caught it: that harness only ever spawned other commands.

use std::path::PathBuf;
use std::process::Command;

fn nub_binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/
    path.push("nub");
    path
}

/// Run a `config` subcommand in a throwaway project with every config root
/// pinned to the fixture, so no host `.npmrc` can supply a value where a
/// default is expected — or absorb a write that was supposed to be refused.
/// Returns stdout, stderr, the exit code, and the project dir.
fn spawn(tag: &str, args: &[&str]) -> (String, String, i32, PathBuf) {
    spawn_in(&fixture(tag), args)
}

/// A fresh throwaway project with a sibling `home` the env pinning points at.
/// Unique per call, so rows running in parallel cannot see each other's writes.
fn fixture(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "nub-cfg-defaults-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&root);
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(root.join("home")).unwrap();
    std::fs::write(
        project.join("package.json"),
        r#"{"name":"app","version":"1.0.0"}"#,
    )
    .unwrap();
    project
}

/// [`spawn`] against a project fixture that already exists, for the rows that
/// run two commands against one `.npmrc`. The home roots are derived from the
/// project path rather than passed, so both entry points pin the same set.
fn spawn_in(project: &std::path::Path, args: &[&str]) -> (String, String, i32, PathBuf) {
    let home = project
        .parent()
        .expect("fixture project has a root")
        .join("home");
    let mut cmd = Command::new(nub_binary());
    cmd.arg("config")
        .args(args)
        .current_dir(project)
        .env("NUB_SELF_SHIM", "0")
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", home.join("xdg-config"))
        .env("XDG_DATA_HOME", home.join("xdg-data"))
        .env("XDG_CACHE_HOME", home.join("xdg-cache"));
    for key in [
        "npm_config_cache_dir",
        "NPM_CONFIG_CACHE_DIR",
        "NUB_CACHE_DIR",
    ] {
        cmd.env_remove(key);
    }
    let out = cmd.output().expect("failed to spawn nub");
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.code().unwrap_or(-1),
        project.to_path_buf(),
    )
}

/// [`spawn`] for the write-path rows, which name their own fixture tag off the
/// verb rather than the assertion.
fn config(args: &[&str]) -> (String, String, i32, PathBuf) {
    spawn(args[0], args)
}

/// [`spawn`] for `config list --all`, asserting the command itself succeeded.
fn list_all(tag: &str) -> String {
    let (stdout, stderr, code, _) = spawn(tag, &["list", "--all"]);
    assert_eq!(
        code, 0,
        "`nub config list --all` failed\nstdout: {stdout}\nstderr: {stderr}"
    );
    stdout
}

fn row<'a>(listing: &'a str, key: &str) -> &'a str {
    listing
        .lines()
        .find(|line| line.starts_with(&format!("{key}=")))
        .unwrap_or_else(|| panic!("no `{key}` row in `config list --all`:\n{listing}"))
}

/// The cache default names nub's own namespace. This is the row that carried
/// the engine's name, so it is also the control: the assertion below fails on
/// the literal that shipped before the tokens existed.
#[test]
fn the_cache_default_names_nubs_namespace() {
    let listing = list_all("cache");
    let cache = row(&listing, "cache-dir");
    assert!(
        cache.contains("nub/pm"),
        "the cache default must name nub's cache namespace: {cache}"
    );
    assert!(
        !cache.to_lowercase().contains("aube"),
        "the cache default still names the engine: {cache}"
    );
}

/// No KNOWN token survives into the listing — the substitution ran for every
/// row, not just the one asserted above.
///
/// This deliberately proves less than it might: a MISSPELLED token is invisible
/// here, because `render_namespaces` leaves it untouched and the result looks
/// like ordinary prose. That case is caught at its source by
/// `aube_settings::meta`'s audit of the raw defaults against the substitution
/// table, which is where a typo can actually be told apart from text.
#[test]
fn no_known_token_survives_into_the_listing() {
    let listing = list_all("tokens");
    let leaked: Vec<&str> = listing
        .lines()
        .filter(|line| line.contains("{cache_namespace}") || line.contains("{data_namespace}"))
        .collect();
    assert!(
        leaked.is_empty(),
        "unsubstituted namespace tokens in the listing: {leaked:?}"
    );
}

/// Nothing in the listing names the engine.
///
/// The last leak was `aubeNoAutoInstall` — the setting behind the engine's own
/// pre-run auto-install gate, which nub never reaches because it runs scripts
/// through its own frontend. Left in the table it was pure misdirection: a
/// brand-named row a user could set and nub would never read. Nub's embedder
/// profile now declares it unsupported, which drops it from the table here.
///
/// Broad on purpose. A narrow `!listing.contains("aubeNoAutoInstall")` would
/// pass the day someone adds the next branded setting, and the whole point of
/// this row is that no such setting reached a user-visible surface.
#[test]
fn the_listing_never_names_the_engine() {
    let listing = list_all("brand");
    let leaked: Vec<&str> = listing
        .lines()
        .filter(|line| line.to_lowercase().contains("aube"))
        .collect();
    assert!(
        leaked.is_empty(),
        "engine branding in `nub config list --all`: {leaked:#?}"
    );
    // Positive control: the listing really was populated, so the emptiness
    // above is a clean sweep rather than an empty read.
    assert!(
        listing.lines().count() > 50,
        "expected a full `--all` listing, got:\n{listing}"
    );
}

/// Every setting nub's embedder profile declares it does not consume, with a
/// substring the refusal has to name so the user has somewhere to go.
///
/// One row per REASON the setting is inert under nub, not per setting name —
/// the shared machinery is the same for all of them, and a row that only
/// re-exercises it is the test bloat AGENTS.md warns about. What each row does
/// buy is the advice check: an entry whose advice pointed at something that does
/// not exist would still refuse, and refuse looking correct.
const NOT_CONSUMED: &[(&str, &str)] = &[
    // The engine's pre-run auto-install gate (nub runs scripts itself).
    ("aubeNoAutoInstall", "verify"),
    ("optimisticRepeatInstall", "verify"),
    // Its script runner (nub's own frontend always runs pre/post).
    ("enablePrePostScripts", "--ignore-scripts"),
    // Its self-update notifier (`self_update_enabled: false`).
    ("updateNotifier", "nub upgrade"),
    // Its Node provisioning (`runtime_switching: false`).
    ("runtimeInstaller", "nub node install"),
    // Its package-manager-version guard, built only by its own CLI dispatcher.
    ("managePackageManagerVersions", "nub pm pin"),
    // Its npm shell-out dispatcher (every nub verb runs in-process).
    ("npmPath", "in-process"),
    // A verb nub stubs out.
    ("deployAllFiles", "deploy"),
    // A parity no-op in the engine itself, wired to nothing.
    ("useBetaCli", "beta-gated"),
    // Read as the `CI` environment variable, never as a config key.
    ("ci", "CI"),
];

/// `config set` refuses a setting nub does not consume, and writes nothing.
///
/// Refusing is the load-bearing half. Making the setting absent from the table
/// is not enough on its own: an unrecognized key is legal config, so the write
/// would fall through to the free-form path and land verbatim — nub reporting a
/// successful write of a value nothing will ever read back.
#[test]
fn config_set_refuses_a_setting_nub_does_not_consume() {
    // Both SCOPES for every row. `--global` matters on its own: it takes a
    // different branch that never reaches the project write router, so a guard
    // on the router alone left it writing straight to `~/.npmrc`.
    for (key, advice) in NOT_CONSUMED {
        for scope in [&[][..], &["--global"][..]] {
            let mut argv = vec!["set", key, "true"];
            argv.extend_from_slice(scope);
            let (stdout, stderr, code, project) = config(&argv);
            assert_ne!(
                code, 0,
                "`config set {key} {scope:?}` must fail: {stdout}{stderr}"
            );
            assert!(
                stderr.contains(advice),
                "the refusal for {key} must name what to use instead ({advice}): {stderr}"
            );
            // Neither scope's file may appear. The fixture pins HOME, so the
            // global target is inside it and a stray write is visible here.
            assert!(
                !project.join(".npmrc").exists(),
                "`config set {key} {scope:?}` wrote a project .npmrc it had refused"
            );
            let home = project.parent().unwrap().join("home").join(".npmrc");
            assert!(
                !home.exists(),
                "`config set {key} {scope:?}` wrote a user .npmrc it had refused"
            );
        }
    }
    // The alias surface too: the profile hangs its advice on the canonical name,
    // and the lookup has to reach it from an `.npmrc` spelling as well.
    let (_, stderr, code, _) = config(&["set", "aube-no-auto-install", "true"]);
    assert_ne!(code, 0, "the kebab alias must refuse too: {stderr}");
    assert!(stderr.contains("verify"), "{stderr}");
}

/// A setting nub does not consume is absent from `config list --all` too.
///
/// The write guard and the listing are separate code paths off one declaration,
/// and only the listing answers "what can I set here?". Advertising a row that
/// `set` then refuses is a worse surface than either failure alone.
#[test]
fn the_listing_never_offers_a_setting_nub_does_not_consume() {
    let listing = list_all("not-consumed");
    for (key, _) in NOT_CONSUMED {
        let named = format!("{key}=");
        assert!(
            !listing.lines().any(|line| line.starts_with(&named)),
            "`config list --all` still offers {key}:\n{listing}"
        );
    }
    // Positive control, same shape as the brand row above: the listing really
    // was populated, so every absence is a sweep rather than an empty read.
    assert!(
        listing.lines().count() > 50,
        "expected a full `--all` listing, got:\n{listing}"
    );
}

/// `config set` refuses a real setting that has no `.npmrc` home.
///
/// A distinct hole from the one above: these ARE consumed — just never from
/// `.npmrc`, which has no key for them. The writer's alias plan falls back to
/// the key verbatim, so the line landed, `config set` reported success, and
/// every reader looked at the command line or the workspace yaml instead.
#[test]
fn config_set_refuses_a_setting_npmrc_cannot_hold() {
    for (key, advice) in [
        ("pnpmfilePath", "--pnpmfile"),
        ("globalPnpmfile", "--global-pnpmfile"),
    ] {
        for scope in [&[][..], &["--global"][..]] {
            let mut argv = vec!["set", key, "./hooks.cjs"];
            argv.extend_from_slice(scope);
            let (stdout, stderr, code, project) = config(&argv);
            assert_ne!(
                code, 0,
                "`config set {key} {scope:?}` must fail: {stdout}{stderr}"
            );
            assert!(
                stderr.contains(advice),
                "the refusal for {key} must name a surface that is read ({advice}): {stderr}"
            );
            assert!(
                !project.join(".npmrc").exists(),
                "`config set {key} {scope:?}` wrote a project .npmrc it had refused"
            );
            let home = project.parent().unwrap().join("home").join(".npmrc");
            assert!(
                !home.exists(),
                "`config set {key} {scope:?}` wrote a user .npmrc it had refused"
            );
        }
    }
}

/// The positive control for the row above: the same harness, a real setting,
/// and the write lands. Without it the refusal test would pass just as well
/// against a `config set` that was broken for every key.
#[test]
fn config_set_still_writes_a_setting_nub_does_consume() {
    let (stdout, stderr, code, project) = config(&["set", "auto-install-peers", "false"]);
    assert_eq!(code, 0, "`config set auto-install-peers` failed: {stderr}");
    let npmrc = std::fs::read_to_string(project.join(".npmrc")).unwrap_or_default();
    assert!(
        npmrc.contains("auto-install-peers=false"),
        "expected the write in .npmrc, got {npmrc:?} (stdout: {stdout})"
    );
}

/// A user who set the key under an older nub can still see it and REMOVE it.
///
/// Refusing the write is not the same as pretending the line isn't there. Their
/// `.npmrc` is their file, so `config list` echoes it verbatim — and `delete`
/// has to keep working, or the guard on `set` would leave a dead key in their
/// config with no supported way to take it out. Easy to break by copying the
/// `set` refusal onto `delete`, and silent when broken.
#[test]
fn a_stale_key_from_an_older_nub_can_still_be_deleted() {
    let project = fixture("stale");
    let npmrc = project.join(".npmrc");
    std::fs::write(&npmrc, "aubeNoAutoInstall=true\nauto-install-peers=false\n").unwrap();

    let (listing, _, code, _) = spawn_in(&project, &["list"]);
    assert_eq!(code, 0);
    assert!(
        listing.contains("aubeNoAutoInstall=true"),
        "the user's own .npmrc line must be echoed, not hidden: {listing}"
    );

    let (_, stderr, code, _) = spawn_in(&project, &["delete", "aubeNoAutoInstall"]);
    assert_eq!(code, 0, "`config delete` must still work: {stderr}");
    let after = std::fs::read_to_string(&npmrc).unwrap();
    assert!(
        !after.contains("aubeNoAutoInstall"),
        "the stale key survived the delete: {after:?}"
    );
    // Control: the delete was surgical, not a truncation of the whole file.
    assert!(
        after.contains("auto-install-peers=false"),
        "the delete took the neighbouring setting with it: {after:?}"
    );
}

/// A setting the project's own `nub.jsonc` supplies is refused on the `.npmrc`
/// route — and the SAME key is written normally when it does not.
///
/// The control is the whole point. `nub.jsonc` outranks `.npmrc`, so once
/// `install.linker` is set an `.npmrc` `nodeLinker` line is read by nothing and
/// writing it is the silent no-op this suite exists to catch. But a project
/// with no `install` block reads that line exactly as before, so a refusal
/// keyed on the KEY rather than on the project would break a configuration that
/// is correct today. Both halves are asserted against one key so the difference
/// can only be the project.
#[test]
fn config_set_refuses_a_setting_nub_jsonc_already_supplies() {
    let shadowed = fixture("shadowed");
    std::fs::write(
        shadowed.join("nub.jsonc"),
        r#"{ "install": { "linker": "hoisted" } }"#,
    )
    .unwrap();
    let (_out, err, code, _) = spawn_in(&shadowed, &["set", "nodeLinker", "isolated"]);
    assert_ne!(code, 0, "`config set nodeLinker` must fail: {err}");
    assert!(
        err.contains("install.linker"),
        "the refusal must name the field that wins: {err}"
    );
    assert!(
        !shadowed.join(".npmrc").exists(),
        "a refused write must leave no .npmrc behind"
    );

    // Control: same key, same command, no `install` block. The `.npmrc` value
    // is what gets read here, so the write has to land.
    let plain = fixture("unshadowed");
    let (_out, err, code, _) = spawn_in(&plain, &["set", "nodeLinker", "isolated"]);
    assert_eq!(code, 0, "`config set nodeLinker` must succeed here: {err}");
    let written = std::fs::read_to_string(plain.join(".npmrc")).expect("the write must land");
    assert!(
        written.contains("isolated"),
        "the .npmrc must carry the value: {written:?}"
    );
}

/// A transient overlay is not a `nub.jsonc` field, so it must not trigger the
/// shadow refusal.
///
/// `effective_config().sources` records CLI and environment overlays alongside
/// the two config files, so testing merely "not defaulted" classifies
/// `NUB_VERIFY_DEPS` as a field this project sets. The write would then be
/// refused — even though it is the persistent setting for every run WITHOUT
/// that variable — and the advice would name a field that still loses to the
/// environment. The refusal has to follow the FILES.
#[test]
fn an_env_overlay_does_not_count_as_a_nub_jsonc_field() {
    let project = fixture("env-overlay");
    let home = project.parent().unwrap().join("home");
    let out = Command::new(nub_binary())
        .args(["config", "set", "verifyDepsBeforeRun", "error"])
        .current_dir(&project)
        .env("NUB_SELF_SHIM", "0")
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", home.join("xdg-config"))
        .env("XDG_DATA_HOME", home.join("xdg-data"))
        .env("XDG_CACHE_HOME", home.join("xdg-cache"))
        .env("NUB_VERIFY_DEPS", "error")
        .output()
        .expect("nub config set must run");
    assert!(
        out.status.success(),
        "an env overlay must not be mistaken for a nub.jsonc field: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        project.join(".npmrc").exists(),
        "the persistent write must still land"
    );
}

/// The shadow set is computed with the REAL embedder defaults, so an injected
/// dependency changes which settings count as supplied.
///
/// Under `linker: global-virtual-store` the lowering pushes
/// `enableGlobalVirtualStore` only when the defaults do NOT already carry
/// `hoist=true`. An injected dependency puts it there, the push is suppressed,
/// and the engine takes that setting from `.npmrc` after all — so refusing the
/// write would reject configuration the install honors. Computing the set with
/// empty defaults inverts exactly this one case, which is why the two halves
/// differ only by `dependenciesMeta`.
#[test]
fn an_injected_dependency_changes_what_counts_as_supplied() {
    let gvs = r#"{ "install": { "linker": "global-virtual-store" } }"#;

    let injected = fixture("injected");
    std::fs::write(
        injected.join("package.json"),
        r#"{"name":"app","version":"1.0.0","dependenciesMeta":{"dep":{"injected":true}}}"#,
    )
    .unwrap();
    std::fs::write(injected.join("nub.jsonc"), gvs).unwrap();
    let (_out, err, code, _) = spawn_in(&injected, &["set", "enableGlobalVirtualStore", "false"]);
    assert_eq!(
        code, 0,
        "an injected dep suppresses the lowering's push, so .npmrc still answers: {err}"
    );

    // Control: identical but for `dependenciesMeta`. Here the lowering DOES
    // push the setting, so the same write is genuinely unreadable.
    let plain = fixture("not-injected");
    std::fs::write(plain.join("nub.jsonc"), gvs).unwrap();
    let (_out, err, code, _) = spawn_in(&plain, &["set", "enableGlobalVirtualStore", "false"]);
    assert_ne!(
        code, 0,
        "without an injected dep this must be refused: {err}"
    );
    assert!(
        err.contains("install.linker"),
        "the refusal must name the field that wins: {err}"
    );
}
