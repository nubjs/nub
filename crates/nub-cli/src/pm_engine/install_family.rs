//! Install family — what is left at the nub layer once the engine claims the
//! family itself. Every verb in it (`install`/`ci`, `add`, `remove`,
//! `update`, `import`, `dedupe`, `prune`, `rebuild`, `fetch`,
//! `link`/`unlink`, `approve-builds`/`ignored-builds`, `dlx`, `create` and
//! the `patch*` trio) is in pnpm's own grammar, so `super::verb_routing`'s
//! `engine_takes` takes the command line at the CLI front door and none of
//! them reaches this module. Their nub-side runners, the per-verb parse
//! roots, and the yarn write gate that rode them are gone with them. Three
//! things remain:
//!
//! - [`run_verb`], which now settles only the **exclusions**. `recursive` is
//!   the one a user can still reach: a bare `nub recursive` gives pnpm's
//!   parser nothing to work with, so the front door declines and the command
//!   lands here, refused because nub has no meta-verb — the recursion goes on
//!   the verb, as `-r` or `--filter`. The `clean`/`purge` and `deploy` arms
//!   are dead letters kept for their tests, which call this function
//!   directly; the pm_engine module doc records what the built binary
//!   actually does with those verbs now. Anything else falls through to the
//!   shared stub error.
//! - [`run_dlx_for_nubx`], the DLX fallback behind the `nubx` entry point,
//!   and the only live engine call left here. Residual: `dlx` propagates the
//!   child's exit code via `std::process::exit` inside the engine (no return
//!   through nub's exit path), and its scratch project uses the engine's
//!   `aube-dlx-*` tempdir prefix + `aube-dlx` manifest name — on-disk temp
//!   state, never printed on success.
//! - [`stamp_virgin_dev_engines`], the virgin `devEngines.packageManager`
//!   stamp, called from `super::pnpm_engine` after a successful install.

use std::path::{Path, PathBuf};

use anyhow::Result;

use super::{VerbSpec, present, stub_error};

/// Settles the family verbs the engine deliberately does NOT take. Anything
/// else that reaches here is unwired, and falls through to the shared stub
/// error (verb + real-PM fallback).
pub(crate) fn run_verb(
    spec: &'static VerbSpec,
    typed: &str,
    args: &[String],
    pm_hint: &str,
) -> Result<i32> {
    // The verbs that used to dispatch from here are gone, and so are their
    // runners. Every one of them is in pnpm's own grammar, so `engine_takes`
    // claims the command line at the front door and this function never sees
    // it — measured verb by verb with a marker in each arm of `dispatch_verb`,
    // against a positive control that fired. `install <pkg>` went the same
    // way: the engine takes it, and a differential against pnpm 12.4.1 on
    // separate fixtures shows both writing the dependency and materializing
    // it, so nothing was lost with the host-side alias.
    match spec.canonical {
        // Exclusions. `recursive` is the only one a user can still reach —
        // a bare `nub recursive` gives pnpm's parser nothing to work with, so
        // the front door declines and the command lands here. The other two
        // are reachable only by calling this function directly, which is what
        // their tests do; the module doc says why they are kept.
        "recursive" => Err(anyhow::anyhow!(
            "nub {typed}: not supported — nub has no recursive meta-verb.\n\
             \x20\x20Use the verb's own workspace flags instead: `nub -r <verb>` /\n\
             \x20\x20`nub <verb> -r` or `--filter <pattern>` (e.g. `nub run -r build`,\n\
             \x20\x20`nub update -r`)."
        )),
        "clean" | "purge" => Err(anyhow::anyhow!(
            "nub {typed}: not supported — nub does not delete node_modules for you.\n\
             \x20\x20Remove it directly (`rm -rf node_modules`) and reinstall with\n\
             \x20\x20`nub install`; `nub ci` does the clean + frozen install in one step."
        )),
        "deploy" => Err(anyhow::anyhow!(
            "nub {typed}: not yet supported — the engine's deploy (copy a workspace\n\
             \x20\x20package + its production deps into a self-contained directory) hasn't\n\
             \x20\x20been wired. For now: pnpm deploy"
        )),
        _ => Err(stub_error(typed, args, pm_hint)),
    }
}

/// DLX fallback for the `nubx <tool> [args]` entry point: the bin was absent
/// from `node_modules/.bin`, so fetch it into a throwaway project and run it,
/// matching `npx` / `pnpm dlx`. Reuses the engine's `dlx` command end-to-end
/// (resolve → install into a scratch tempdir → exec the bin → drop the tempdir)
/// rather than reimplementing the fetch pipeline; the engine itself does a final
/// local-`.bin` recheck (a no-op here since the caller already missed) before
/// fetching, and resolves the project's Node pin via the user's cwd. We follow
/// `pnpm dlx` semantics deliberately: no interactive confirm-prompt — fetch+run.
///
/// `<tool>` is passed as the positional, so by default the engine derives the
/// actual bin name from the installed package's `bin` map when the command name
/// and package name differ (e.g. `@tanstack/cli` ships `tanstack`). nubx's own
/// flag handling already split off the bin; everything in `args` is forwarded to
/// the tool verbatim. The npx flags in `flags` steer the fetch: `-p`/`--package`
/// populates `DlxArgs.package` (the package(s) to fetch, with `<tool>` as the bin
/// to run from them) and `-q`/`--quiet` switches the engine's progress UI to
/// text mode.
/// Returns `(exit_code, fetched_ok)`. `fetched_ok` distinguishes "the tool was
/// resolved, installed, and executed" (engine `Ok` — whatever the tool's own exit
/// code) from "the fetch/install itself failed" (404 / resolution error /
/// binary-not-found — engine `Err`, surfaced here as a nonzero code). The consent
/// caller MUST gate its ledger write on `fetched_ok`: recording a failed fetch
/// would turn a one-time `y` on a not-yet-published spec into a permanent silent
/// run-grant that activates if the name is later (maliciously) published.
pub fn run_dlx_for_nubx(
    bin: &str,
    args: &[String],
    flags: &crate::cli::NubxDlxFlags,
    compat_mode: bool,
) -> Result<(i32, bool)> {
    if flags.quiet {
        // Same knob aube's own startup flips for `--silent`: drop the animated
        // progress UI to plain text so a `-q` fetch stays quiet.
        clx::progress::set_output(clx::progress::ProgressOutput::Text);
    }
    let verb = nubx_dlx_args(bin, args, flags);
    // Transient fetch-and-run (see `engine_session_transient`): `nubx <tool>`
    // fetches a throwaway package and runs it; the CWD project's lockfile is
    // irrelevant, so a multi-lockfile project must not raise
    // ERR_NUB_LOCKFILE_AMBIGUOUS the way npm/pnpm/bun's npx/dlx/bunx don't.
    let session = super::engine_session_transient(None)?;
    // `Ok` = fetched + ran (the tool's own code via Ok(Some(code)), success via
    // Ok(None)); `Err` = the fetch/install failed before the tool ran. We surface
    // the Err's report exactly as `finish_code` would, but also report the
    // success bit so the consent caller never records a failed fetch.
    match session.runtime.block_on(aube::commands::dlx::run_in(
        verb,
        None,
        crate::cli::dlx_child_env(compat_mode),
    )) {
        Ok(code) => Ok((code.unwrap_or(0), true)),
        Err(report) => Ok((present::emit_report(&report), false)),
    }
}

/// Build the `dlx` invocation for a `nubx <tool> [args]` fallback: `<tool>` is
/// the positional (so the engine derives the actual bin name from the package's
/// `bin` map, or runs it from `-p` packages) and `args` forward verbatim. The
/// dlx-only `-c` shell-mode and `--allow-build` are not in nubx's surface yet, so
/// they stay at their safe defaults — matching `npx <tool> [args]`.
fn nubx_dlx_args(
    bin: &str,
    args: &[String],
    flags: &crate::cli::NubxDlxFlags,
) -> aube::commands::dlx::DlxArgs {
    let mut params = Vec::with_capacity(args.len() + 1);
    params.push(bin.to_string());
    params.extend(args.iter().cloned());
    aube::commands::dlx::DlxArgs {
        params,
        shell_mode: false,
        package: flags.package.clone(),
        allow_build: Vec::new(),
        lockfile: Default::default(),
        network: Default::default(),
        virtual_store: Default::default(),
    }
}

/// Nearest ancestor (inclusive) carrying a `package.json`, bounded like
/// `super::detect_lockfile_walk_up`. Approximation of aube's
/// `dirs::project_root` (which is crate-private); the home-dir boundary is
/// not enforced here.
fn find_manifest_root(cwd: &Path) -> Option<PathBuf> {
    let mut dir = cwd.to_path_buf();
    for _ in 0..16 {
        if dir.join("package.json").is_file() {
            return Some(dir);
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

/// Write `devEngines.packageManager = {name:"nub", version:"^<x.y.z>",
/// onFail:"ignore"}` into the project manifest of a VIRGIN install — the
/// non-locking cross-tool PM signal (see the call-site comment). Best-effort: a
/// successful install must not fail on a manifest the stamp can't reach (no
/// `package.json` — nub never scaffolds one — or an unwritable file).
/// Format-preserving + atomic via the shared manifest editor; it ranks the new
/// key by insertion order at the manifest's tail, leaving the user's existing
/// keys untouched. The caller has already proven virginity (no prior
/// `devEngines`), so the object is created wholesale.
pub(super) fn stamp_virgin_dev_engines(cwd: &Path) {
    // Skip silently when the install ran without a `package.json` (the editor
    // refuses to scaffold one); the install already succeeded, so this is a
    // no-op, not an error.
    let Some(root) = find_manifest_root(cwd) else {
        return;
    };
    // Defensive symmetric-brand-boundary guard. The `truly_fresh` gate derives
    // virginity from aube's lockfile detection, which does NOT recognize two
    // foreign PM signals: bun's pre-1.2 BINARY lockfile `bun.lockb` (only the
    // text `bun.lock` is a detection candidate) and a lone yarn-berry
    // `.yarnrc.yml` config with no `yarn.lock` yet. Without this re-check a nub
    // `devEngines.packageManager` stamp could land in a bun/yarn-owned project —
    // exactly the brand imposition the virgin predicate forbids. Walk up like
    // `is_truly_fresh_project` does for pnpm-named files, and bail on a hit.
    if dir_walk_up_has_any(cwd, &["bun.lockb", ".yarnrc.yml"]) {
        return;
    }
    // Stamp ONLY when this op wrote nub's OWN canonical (neutral) lockfile. The
    // stamp's whole purpose is to supply the PM signal that nub's UNBRANDED
    // lockfile otherwise withholds; when a virgin project resolves to a FOREIGN
    // lockfile format instead (e.g. `default_lockfile_format=pnpm`), that
    // lockfile IS the signal — so the stamp is unneeded, AND a nub claim beside a
    // pnpm/npm-format lock misrepresents the project. Keyed off the
    // canonical-lockfile NAME accessor (rename-safe; resolves the embedder
    // profile + git-branch variant).
    if !root.join(aube_lockfile::aube_lock_filename(&root)).exists() {
        return;
    }
    let range = format!("^{}", env!("CARGO_PKG_VERSION"));
    let _ = nub_core::pm::resolve::edit_root_manifest(&root, |obj| {
        let dev = obj
            .entry("devEngines")
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
        if let Some(dev) = dev.as_object_mut() {
            // Never overwrite an existing `devEngines.packageManager`. The
            // `truly_fresh` gate keys on lockfiles + pnpm-named files, NOT on the
            // manifest's declaration fields, so a hand-written foreign
            // `devEngines.packageManager` (e.g. `{name:"pnpm"}`) can coexist with
            // a virgin lockfile state — and clobbering it would impose nub's
            // brand over another PM's declaration (the symmetric brand boundary).
            dev.entry("packageManager").or_insert_with(
                || serde_json::json!({ "name": "nub", "version": range, "onFail": "ignore" }),
            );
        }
    });
}

/// Bounded ancestor walk (inclusive, 16 levels like the identity/manifest
/// walk-ups) testing whether any directory carries one of `names`.
fn dir_walk_up_has_any(cwd: &Path, names: &[&str]) -> bool {
    let mut dir = cwd.to_path_buf();
    for _ in 0..16 {
        if names.iter().any(|n| dir.join(n).exists()) {
            return true;
        }
        if !dir.pop() {
            break;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bare `nubx <tool>` DLX fallback (run when the bin is absent from
    /// `node_modules/.bin`) hands the tool to the engine's `dlx` as a plain
    /// positional with args forwarded verbatim — `npx`/`pnpm dlx` semantics:
    /// the tool name doubles as the package (no `-p`, so the engine resolves the
    /// real bin name from the package's `bin` map), nothing is run through `sh -c`
    /// (no `-c`), and no lifecycle scripts are auto-approved (no `--allow-build`).
    #[test]
    fn nubx_dlx_fallback_forwards_tool_and_args_with_no_dlx_flags() {
        let flags = crate::cli::NubxDlxFlags::default();
        let verb = nubx_dlx_args(
            "cowsay",
            &["-f".into(), "tux".into(), "hi there".into()],
            &flags,
        );
        // Tool is the positional; args ride after it untouched (a tool flag like
        // `-f` is the tool's, never consumed by nubx/dlx).
        assert_eq!(verb.params, ["cowsay", "-f", "tux", "hi there"]);
        // With no `-p`, the tool name is the package — the engine derives the bin.
        assert!(verb.package.is_empty(), "no -p: tool name is the package");
        assert!(
            !verb.shell_mode,
            "no -c: tool argv must round-trip, not sh -c"
        );
        assert!(verb.allow_build.is_empty(), "no scripts auto-approved");

        // A tool with no args still produces a single-positional invocation.
        let bare = nubx_dlx_args("serve", &[], &flags);
        assert_eq!(bare.params, ["serve"]);
    }

    /// `nubx -p <spec> <bin> [args]` populates `DlxArgs.package` with the spec(s)
    /// and keeps `<bin>` as the positional, so the engine fetches the package and
    /// runs the named bin from it (npx's package≠bin decoupling).
    #[test]
    fn nubx_dlx_package_flag_drives_the_fetch_set() {
        let flags = crate::cli::NubxDlxFlags {
            package: vec!["@tanstack/cli".into()],
            ..Default::default()
        };
        let verb = nubx_dlx_args("tanstack", &["--help".into()], &flags);
        assert_eq!(verb.package, ["@tanstack/cli"], "-p spec drives the fetch");
        assert_eq!(
            verb.params,
            ["tanstack", "--help"],
            "the positional bin + its args still ride params[0..]"
        );
    }
    /// fd capture round-trips engine prints so the rewrite can reach raw
    /// println/eprintln sites (unix; the non-unix fallback is a documented
    /// pass-through). Writes at the fd level — libtest's output capture
    /// hooks Rust's `print!` machinery thread-locally, so a `println!` here
    /// would be swallowed before it ever reached fd 1 (the production
    /// engine prints run uncaptured and do reach the fd).
    #[cfg(unix)]
    #[test]
    fn fd_capture_round_trips_raw_prints() {
        let (value, captured) = crate::pm_engine::with_fd_captured(1, || {
            let line = b"Run `aube install` to execute their scripts.\n";
            // SAFETY: plain write(2) on fd 1, which the helper owns here.
            let wrote = unsafe { libc::write(1, line.as_ptr().cast(), line.len()) };
            assert_eq!(wrote, line.len() as isize, "raw write must not short");
            42
        });
        assert_eq!(value, 42);
        // `contains`, not `ends_with`/equality: fd 1 redirection is
        // process-global, so the libtest harness's own progress lines
        // ("test … ok") from parallel tests can land anywhere in the capture
        // window — before OR after our write — so neither a prefix nor a
        // suffix check is stable. The contract under test is narrow and fully
        // pinned by presence: the raw write survives the capture, the rewrite
        // reaches it (the rewritten `nub install` line is present), and the
        // rewrite neutralized the engine's `aube` brand (the un-rewritten
        // `aube install` form is absent).
        let rewritten = present::rewrite(&captured);
        assert!(
            rewritten.contains("Run `nub install` to execute their scripts.\n"),
            "captured+rewritten stream must contain the rewritten engine line, got: {rewritten:?}"
        );
        assert!(
            !rewritten.contains("Run `aube install`"),
            "rewrite must neutralize the engine's aube brand, got: {rewritten:?}"
        );
    }

    /// The virgin stamp writes the `devEngines.packageManager` caret range
    /// object, appended at the manifest tail (the `preserve_order` editor never
    /// reflows the user's existing keys), and NEVER the hard `packageManager`
    /// pin. Gating on virginity is the caller's job; this asserts the write
    /// itself. Seed nub's OWN canonical lockfile in `dir` so the stamp's
    /// "nub wrote its neutral format" gate passes deterministically, whichever
    /// embedder profile the test binary happens to have registered.
    fn seed_nub_lockfile(dir: &Path) {
        std::fs::write(
            dir.join(aube_lockfile::aube_lock_filename(dir)),
            "lockfileVersion: '9.0'\n",
        )
        .unwrap();
    }

    #[test]
    fn virgin_stamp_writes_dev_engines_range_at_tail() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            "{\n  \"name\": \"app\",\n  \"version\": \"1.0.0\"\n}\n",
        )
        .unwrap();
        seed_nub_lockfile(dir.path());

        stamp_virgin_dev_engines(dir.path());

        let written = std::fs::read_to_string(dir.path().join("package.json")).unwrap();
        let manifest: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert_eq!(
            manifest.pointer("/devEngines/packageManager"),
            Some(&serde_json::json!({
                "name": "nub",
                "version": format!("^{}", env!("CARGO_PKG_VERSION")),
                "onFail": "ignore"
            })),
            "value must be the non-locking caret range on the running nub version"
        );
        assert!(
            manifest.get("packageManager").is_none(),
            "the virgin stamp must NOT write the hard packageManager pin: {written:?}"
        );
        // Tail-append + format preservation: the pre-existing keys keep their
        // order and the new key lands last, so the diff is one added block.
        assert!(
            written.contains("\"version\": \"1.0.0\",\n  \"devEngines\""),
            "stamp must append after the user's keys, not reflow them: {written:?}"
        );
    }

    /// The stamp NEVER overwrites an existing `devEngines.packageManager`. The
    /// `truly_fresh` gate keys on lockfiles + pnpm-named files, not on manifest
    /// declarations, so a hand-written foreign `devEngines.packageManager` can
    /// reach this path — and imposing nub's brand over it would break the
    /// symmetric brand boundary. A sibling `devEngines` entry is left intact.
    #[test]
    fn virgin_stamp_never_overwrites_an_existing_dev_engines_pin() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name":"app","devEngines":{"packageManager":{"name":"pnpm","version":"^10"},"runtime":{"name":"node"}}}"#,
        )
        .unwrap();
        seed_nub_lockfile(dir.path());

        stamp_virgin_dev_engines(dir.path());

        let manifest: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("package.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            manifest.pointer("/devEngines/packageManager/name"),
            Some(&serde_json::json!("pnpm")),
            "a foreign devEngines.packageManager must not be clobbered by the nub stamp"
        );
        assert_eq!(
            manifest.pointer("/devEngines/runtime/name"),
            Some(&serde_json::json!("node")),
            "a sibling devEngines entry survives"
        );
    }

    /// A successful install in a directory with NO `package.json` must not fail
    /// or scaffold one — the stamp is best-effort and silently no-ops (nub never
    /// creates a manifest).
    #[test]
    fn virgin_stamp_is_silent_noop_without_a_manifest() {
        let dir = tempfile::tempdir().unwrap();
        // Hermeticity guard: the stamp walks UP for a manifest root, so if the
        // tempdir sits under a checkout (TMPDIR inside a repo) it could reach an
        // ancestor `package.json` and the no-op intent wouldn't hold. Assert the
        // precondition and skip rather than risk touching an unrelated manifest.
        if find_manifest_root(dir.path()).is_some() {
            return;
        }
        stamp_virgin_dev_engines(dir.path());
        assert!(
            !dir.path().join("package.json").exists(),
            "stamp must never scaffold a missing package.json"
        );
    }
    /// Stamp ONLY when nub wrote its own neutral lockfile. A virgin project that
    /// resolved to a FOREIGN lockfile format (e.g. `default_lockfile_format=pnpm`
    /// writes `pnpm-lock.yaml`, not nub's lockfile) is NOT stamped — that
    /// lockfile is already the PM signal, and a nub claim beside it would
    /// misrepresent the project.
    #[test]
    fn virgin_stamp_skips_when_nub_wrote_no_neutral_lockfile() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            "{\n  \"name\": \"app\"\n}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("pnpm-lock.yaml"),
            "lockfileVersion: '9.0'\n",
        )
        .unwrap();

        stamp_virgin_dev_engines(dir.path());

        let written = std::fs::read_to_string(dir.path().join("package.json")).unwrap();
        assert!(
            !written.contains("devEngines"),
            "a foreign-format lockfile must not get a nub stamp: {written:?}"
        );
    }

    /// Symmetric brand boundary: a foreign PM signal aube's detection misses
    /// (`bun.lockb`, pre-1.2 bun) still blocks the stamp, so nub never imposes
    /// its `devEngines` marker on a bun-owned project.
    #[test]
    fn virgin_stamp_skips_an_undetected_foreign_lockfile() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            "{\n  \"name\": \"app\"\n}\n",
        )
        .unwrap();
        seed_nub_lockfile(dir.path()); // isolate the bun.lockb guard, not the no-lockfile path
        std::fs::write(dir.path().join("bun.lockb"), b"\0bun").unwrap();

        stamp_virgin_dev_engines(dir.path());

        let written = std::fs::read_to_string(dir.path().join("package.json")).unwrap();
        assert!(
            !written.contains("devEngines"),
            "a bun.lockb project must not be stamped: {written:?}"
        );
    }
}
