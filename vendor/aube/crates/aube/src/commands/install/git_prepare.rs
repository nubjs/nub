use miette::{Context, IntoDiagnostic, miette};

use super::{FrozenMode, InstallOptions, run};

/// Unique-per-call scratch directory that `rm -rf`s itself on drop.
/// Used to run a git dep's `prepare` script without mutating the
/// shared `git_shallow_clone` cache under `<cache>/git/<tool>-git-*`.
pub(super) struct ScratchDir(std::path::PathBuf);

impl ScratchDir {
    pub(super) fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Recursively copy `src` into a fresh temp directory and return it
/// wrapped in a [`ScratchDir`]. A top-level `.git` is skipped —
/// prepare scripts never need the history, and dropping it keeps the
/// copy an order of magnitude smaller on large repos.
///
/// The copy is in-process rather than a `cp -a` subprocess, which
/// stock Windows does not have. It keeps what `cp -a` kept that a
/// prepare script can observe: symlinks stay symlinks, permission
/// bits (the exec bit above all) survive through `fs::copy`, and file
/// modification times are carried over best-effort.
pub(super) fn prepare_scratch_copy(
    src: &std::path::Path,
    spec: &str,
) -> miette::Result<ScratchDir> {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    use std::hash::{Hash, Hasher};
    src.hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
        .hash(&mut hasher);
    let dst = std::env::temp_dir().join(format!(
        "{}-git-prep-{:x}",
        aube_util::prog(),
        hasher.finish()
    ));
    if dst.exists() {
        let _ = std::fs::remove_dir_all(&dst);
    }
    std::fs::create_dir_all(&dst)
        .map_err(|e| miette!("git dep {spec}: create scratch dir {}: {e}", dst.display()))?;

    // Owning the directory before the fallible copy means a failure
    // part-way through still removes the partial tree on drop instead
    // of leaking it under `<tmp>/<tool>-git-prep-*`.
    let scratch = ScratchDir(dst);
    copy_checkout(src, scratch.path(), true)
        .map_err(|e| miette!("git dep {spec}: scratch copy failed: {e}"))?;
    Ok(scratch)
}

fn copy_checkout(
    src: &std::path::Path,
    dst: &std::path::Path,
    top_level: bool,
) -> std::io::Result<()> {
    let with_path = |path: &std::path::Path, e: std::io::Error| {
        std::io::Error::new(e.kind(), format!("{}: {e}", path.display()))
    };
    for entry in std::fs::read_dir(src).map_err(|e| with_path(src, e))? {
        let entry = entry.map_err(|e| with_path(src, e))?;
        let name = entry.file_name();
        if top_level && name == ".git" {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        let file_type = entry.file_type().map_err(|e| with_path(&from, e))?;
        if file_type.is_symlink() {
            copy_symlink(&from, &to, file_type).map_err(|e| with_path(&from, e))?;
        } else if file_type.is_dir() {
            std::fs::create_dir(&to).map_err(|e| with_path(&to, e))?;
            copy_checkout(&from, &to, false)?;
        } else if file_type.is_file() {
            std::fs::copy(&from, &to).map_err(|e| with_path(&from, e))?;
            // Times are fidelity, not correctness: a build that compares
            // them still works from fresh ones, so a refusal (a read-only
            // file, a filesystem without the call) does not fail the copy.
            if let Ok(meta) = entry.metadata()
                && let Ok(file) = std::fs::OpenOptions::new().write(true).open(&to)
            {
                let mut times = std::fs::FileTimes::new();
                if let Ok(modified) = meta.modified() {
                    times = times.set_modified(modified);
                }
                if let Ok(accessed) = meta.accessed() {
                    times = times.set_accessed(accessed);
                }
                let _ = file.set_times(times);
            }
        }
        // Git stores no other file type, so a FIFO or socket here is
        // stray local state; skipping it also keeps `fs::copy` from
        // blocking on a FIFO's read end.
    }
    Ok(())
}

#[cfg(unix)]
fn copy_symlink(
    src: &std::path::Path,
    dst: &std::path::Path,
    _file_type: std::fs::FileType,
) -> std::io::Result<()> {
    std::os::unix::fs::symlink(std::fs::read_link(src)?, dst)
}

#[cfg(windows)]
fn copy_symlink(
    src: &std::path::Path,
    dst: &std::path::Path,
    file_type: std::fs::FileType,
) -> std::io::Result<()> {
    use std::os::windows::fs::FileTypeExt;
    let target = std::fs::read_link(src)?;
    if file_type.is_symlink_dir() {
        return aube_linker::create_dir_link(&target, dst);
    }
    // A file symlink needs Developer Mode or an elevated shell. Without
    // either, copy what it points at — what Git for Windows' `cp`
    // produced, so the users who had one on PATH see no change.
    match std::os::windows::fs::symlink_file(&target, dst) {
        Ok(()) => Ok(()),
        Err(e) if std::fs::metadata(src).is_ok_and(|m| m.is_file()) => {
            tracing::debug!(
                "git prepare: symlink {} unavailable ({e}); copying its target",
                dst.display()
            );
            std::fs::copy(src, dst).map(|_| ())
        }
        Err(e) => Err(e),
    }
}

/// Scripts pnpm runs after the nested install, which has already run
/// `prepare`. `prepublishOnly` is left out: neither npm nor Yarn runs it
/// for a git dependency.
const PREPUBLISH_SCRIPTS: [&str; 3] = ["prepublish", "prepack", "publish"];

fn has_script(manifest: &aube_manifest::PackageJson, name: &str) -> bool {
    manifest.scripts.get(name).is_some_and(|s| !s.is_empty())
}

/// Whether a git checkout has to be built before it is installed: it
/// declares `prepare`, or a `prepublish`/`prepack`/`publish` script with
/// its `main` file missing. pnpm's `packageShouldBeBuilt`. `has_file`
/// answers for a path relative to the package root.
pub(super) fn checkout_needs_build(
    manifest: &aube_manifest::PackageJson,
    has_file: impl Fn(&str) -> bool,
) -> bool {
    if has_script(manifest, "prepare") {
        return true;
    }
    if !PREPUBLISH_SCRIPTS.iter().any(|s| has_script(manifest, s)) {
        return false;
    }
    let main = manifest
        .extra
        .get("main")
        .and_then(|v| v.as_str())
        .unwrap_or("index.js");
    !has_file(main)
}

/// Whether the install's build policy lets a git checkout be built.
///
/// pnpm's `resolvePackageBuildPermission`. The policy is asked under the
/// checkout's own manifest name, never the alias it is installed under,
/// and under its source (`<name>@<url>#<commit>`, or the hosted archive
/// URL) in place of a version. So only an exact source key, a
/// `<name>@git+<repository>` key or allow-all approves it; a bare name
/// cannot, since it would approve whatever bytes a repository serves. A
/// deny skips the build and the checkout installs as it is. No rule at all
/// fails the install, because an unbuilt checkout is usually a package
/// with nothing in it to run.
pub(super) fn checkout_build_allowed(
    policy: Option<&aube_scripts::BuildPolicy>,
    local: &aube_lockfile::LocalSource,
    manifest: &aube_manifest::PackageJson,
) -> miette::Result<bool> {
    let deny_all = aube_scripts::BuildPolicy::deny_all();
    let policy = policy.unwrap_or(&deny_all);
    let name = manifest.name.as_deref().unwrap_or("");
    let source = local.specifier();
    let repository_key = local.git_repository_approval_key(name);
    let decisions: Vec<_> = local
        .source_approval_keys(name)
        .iter()
        .map(|key| {
            policy.decide_package_with_git_repository(
                name,
                &source,
                Some(key),
                repository_key.as_deref(),
            )
        })
        .collect();
    if decisions.contains(&aube_scripts::AllowDecision::Deny) {
        return Ok(false);
    }
    if decisions.contains(&aube_scripts::AllowDecision::Allow) {
        return Ok(true);
    }
    let version = manifest.version.as_deref().unwrap_or("");
    let field = aube_manifest::allow_scripts_field_name();
    // A source URL can carry `user:pass@` credentials; keep them out of the
    // terminal and CI logs.
    let key = format!("{name}@{}", aube_util::url::redact_url(&source));
    let example = if field == aube_manifest::ROOT_ALLOW_SCRIPTS_KEY {
        format!(
            "Add the package to \"{field}\" in your project's package.json to allow it to run scripts. For example:\n\"{field}\": {{ \"{key}\": true }}"
        )
    } else {
        format!(
            "Add the package to \"{field}\" in your project's pnpm-workspace.yaml to allow it to run scripts. For example:\n{field}:\n  {key}: true"
        )
    };
    Err(miette!(
        code = aube_codes::errors::ERR_AUBE_GIT_DEP_PREPARE_NOT_ALLOWED,
        help = example,
        "The git-hosted package \"{name}@{version}\" needs to execute build scripts but is not in the \"{field}\" allowlist."
    ))
}

/// Hard cap for nested git dep `prepare` installs. Four levels is more
/// than any real-world chain we've seen and prevents a pathological repo
/// from wedging install in an infinite clone loop.
const GIT_PREPARE_MAX_DEPTH: u32 = 4;

/// Run a nested `aube install` inside a git-dep checkout so its
/// devDependencies are linked and its root `prepare` script runs, then
/// its `prepublish`/`prepack`/`publish` scripts, before the caller
/// snapshots the tree via `aube pack`.
///
/// `ignore_scripts` is forwarded from the outer install so a user
/// who passed `--ignore-scripts` for security/reproducibility
/// reasons doesn't have the git dep's full root lifecycle sequence
/// execute regardless — the caller is expected to *skip* calling
/// this function entirely under `--ignore-scripts`, but we still
/// forward the flag as a belt-and-suspenders defense in case a
/// nested install reaches this path through some other code path.
pub(super) async fn run_git_dep_prepare(
    clone_dir: &std::path::Path,
    spec: &str,
    ignore_scripts: bool,
    depth: u32,
    inherited_build_policy: Option<std::sync::Arc<aube_scripts::BuildPolicy>>,
) -> miette::Result<()> {
    if depth >= GIT_PREPARE_MAX_DEPTH {
        return Err(miette!(
            "git dep {spec}: `prepare` nesting exceeded {GIT_PREPARE_MAX_DEPTH} levels"
        ));
    }
    let mut opts = InstallOptions::with_mode(super::super::chained_frozen_mode(FrozenMode::Prefer));
    opts.project_dir = Some(clone_dir.to_path_buf());
    opts.ignore_scripts = ignore_scripts;
    opts.git_prepare_depth = depth + 1;
    opts.inherited_build_policy = inherited_build_policy;
    // Override the chained-call default: this nested install's "root" IS
    // the git dep itself, and running its `prepare` (plus
    // pre/post-install) is the entire point of git-dep preparation.
    // Treat this as if it were an argumentless `aube install` against the
    // dep's clone directory.
    opts.skip_root_lifecycle = false;
    opts.run_dev_preinstall = true;
    // pnpm prepares a git dep with `strictDepBuilds` off (git-fetcher's
    // `prepare_env`): an unapproved build among the dep's own
    // devDependencies is skipped with a warning, not a failed prepare.
    opts.cli_flags
        .push(("strictDepBuilds".to_string(), "false".to_string()));
    let spec = spec.to_string();
    let dir = clone_dir.to_path_buf();
    tokio::task::spawn_blocking(move || -> miette::Result<()> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .into_diagnostic()
            .wrap_err("failed to build nested git prepare runtime")?;
        runtime.block_on(async move {
            run(opts).await?;
            if !ignore_scripts {
                run_prepublish_scripts(&dir).await?;
            }
            Ok::<(), miette::Report>(())
        })
    })
    .await
    .into_diagnostic()
    .wrap_err_with(|| format!("git dep {spec}: nested install task failed"))?
    .wrap_err_with(|| format!("git dep {spec}: nested install for `prepare` failed"))
}

/// Whether `path`, relative to the package root, names a file or a
/// directory in an archive's index, as `Path::exists` answers for the
/// extracted tree. The index keys files alone, by normalized relative path,
/// so a directory is a key prefix.
pub(super) fn archive_has_path<V>(
    index: &aube_util::collections::FxMap<String, V>,
    path: &str,
) -> bool {
    let mut parts = Vec::new();
    for part in path.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return false;
                }
            }
            part => parts.push(part),
        }
    }
    if parts.is_empty() {
        return true;
    }
    let key = parts.join("/");
    let dir = format!("{key}/");
    index.contains_key(&key) || index.keys().any(|k| k.starts_with(&dir))
}

/// Whether the checkout's own package manager runs `pre<script>` and
/// `post<script>` around a script. pnpm runs a publishing script raw for a
/// pnpm checkout and as `<pm> run <script>` for any other, and of those only
/// Yarn Berry skips the hooks. The package manager is picked as pnpm's
/// `detect_wanted_pm` picks it: the first known one `devEngines` or
/// `packageManager` declares, else the one its lockfile belongs to, else
/// npm. A Yarn without a version is Berry when its `yarn.lock` is.
fn runs_pre_post_hooks(dir: &std::path::Path, manifest: &aube_manifest::PackageJson) -> bool {
    let package_manager = manifest
        .extra
        .get("packageManager")
        .and_then(|v| v.as_str())
        .map(|spec| match spec.split_once('@') {
            Some((name, version)) => (name.to_string(), Some(version.to_string())),
            None => (spec.to_string(), None),
        });
    let declared = manifest
        .dev_engines
        .iter()
        .flat_map(|d| d.package_manager.iter())
        .map(|d| (d.name.clone(), d.version.clone()))
        .chain(package_manager)
        .find(|(name, _)| ["pnpm", "npm", "yarn", "bun"].contains(&name.as_str()));
    let (name, version) =
        declared.unwrap_or_else(|| (lockfile_package_manager(dir).to_string(), None));
    match name.as_str() {
        "pnpm" => false,
        "yarn" => match version.as_deref().and_then(yarn_spec_is_berry) {
            Some(berry) => !berry,
            None => !aube_lockfile::yarn::is_berry_path(&dir.join("yarn.lock")),
        },
        _ => true,
    }
}

/// pnpm's `detect_preferred_pm`: the package manager a checkout's lockfile
/// or workspace file belongs to, npm when it has neither.
fn lockfile_package_manager(dir: &std::path::Path) -> &'static str {
    let has = |file: &str| dir.join(file).exists();
    if has("pnpm-lock.yaml") {
        "pnpm"
    } else if has("yarn.lock") {
        "yarn"
    } else if has("package-lock.json") {
        "npm"
    } else if has("bun.lockb") || has("bun.lock") {
        "bun"
    } else if has("pnpm-workspace.yaml") {
        "pnpm"
    } else {
        "npm"
    }
}

/// Whether a Yarn version spec selects Berry. pnpm provisions a range
/// that still admits a 1.x release as Yarn Classic (`^1 || ^4` gets
/// 1.22.22), so only a range with no 1.x left in it is Berry. `None` for
/// a spec that is not a range, which pnpm treats as no version.
fn yarn_spec_is_berry(spec: &str) -> Option<bool> {
    let spec = spec
        .split_once('+')
        .map_or(spec, |(version, _build)| version);
    let range = node_semver::Range::parse(spec).ok()?;
    Some(!range.allows_any(&node_semver::Range::parse("<2.0.0").ok()?))
}

/// Run the `prepublish`/`prepack`/`publish` scripts the checkout declares,
/// in that order, as pnpm does once its nested install is done.
async fn run_prepublish_scripts(dir: &std::path::Path) -> miette::Result<()> {
    let manifest = aube_manifest::PackageJson::from_path(&dir.join("package.json"))
        .map_err(miette::Report::new)?;
    let modules_dir_name = crate::commands::resolve_modules_dir_name_for_cwd(dir);
    let hooks = runs_pre_post_hooks(dir, &manifest);
    for script in PREPUBLISH_SCRIPTS {
        if !has_script(&manifest, script) {
            continue;
        }
        let stages = if hooks {
            vec![
                format!("pre{script}"),
                script.to_string(),
                format!("post{script}"),
            ]
        } else {
            vec![script.to_string()]
        };
        for stage in stages.iter().filter(|stage| has_script(&manifest, stage)) {
            super::lifecycle::run_root_lifecycle_script(dir, &modules_dir_name, &manifest, stage)
                .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scratch_copy_keeps_the_checkout_but_not_its_history() {
        let src = tempfile::tempdir().unwrap();
        let root = src.path();
        std::fs::create_dir_all(root.join(".git/objects")).unwrap();
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(root.join(".npmrc"), "x=1\n").unwrap();
        std::fs::create_dir_all(root.join("src/nested")).unwrap();
        std::fs::write(root.join("src/nested/a.js"), "a\n").unwrap();
        std::fs::write(root.join("build.sh"), "#!/bin/sh\n").unwrap();
        let mtime = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        std::fs::File::options()
            .write(true)
            .open(root.join("src/nested/a.js"))
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                root.join("build.sh"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
            std::os::unix::fs::symlink("build.sh", root.join("alias.sh")).unwrap();
        }
        #[cfg(windows)]
        std::os::windows::fs::symlink_file("build.sh", root.join("alias.sh")).unwrap();

        let scratch = prepare_scratch_copy(root, "test").unwrap();
        let dst = scratch.path();

        assert!(
            !dst.join(".git").exists(),
            "the clone's history is not copied"
        );
        assert_eq!(
            std::fs::read_to_string(dst.join(".npmrc")).unwrap(),
            "x=1\n"
        );
        assert_eq!(
            std::fs::read_to_string(dst.join("src/nested/a.js")).unwrap(),
            "a\n"
        );
        assert_eq!(
            std::fs::metadata(dst.join("src/nested/a.js"))
                .unwrap()
                .modified()
                .unwrap(),
            mtime,
            "modification time carries over"
        );
        assert_eq!(
            std::fs::read_link(dst.join("alias.sh")).unwrap(),
            std::path::Path::new("build.sh"),
            "a symlink is recreated as a symlink, target unchanged"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dst.join("build.sh"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o755, "the exec bit survives");
        }

        let copied = dst.to_path_buf();
        drop(scratch);
        assert!(!copied.exists(), "the scratch dir removes itself on drop");
    }

    fn manifest(json: &str) -> aube_manifest::PackageJson {
        aube_manifest::PackageJson::from_slice(json.as_bytes()).unwrap()
    }

    #[test]
    fn a_checkout_needs_building_for_prepare_or_a_missing_packed_main() {
        let none = |_: &str| false;
        let built = |p: &str| p == "./dist/x.js";
        for (json, has_file, needs) in [
            (
                r#"{"scripts":{"prepare":"tsc"}}"#,
                &none as &dyn Fn(&str) -> bool,
                true,
            ),
            (r#"{"scripts":{"prepare":""}}"#, &none, false),
            (r#"{"scripts":{"postinstall":"x"}}"#, &none, false),
            (
                r#"{"main":"./dist/x.js","scripts":{"prepack":"x"}}"#,
                &none,
                true,
            ),
            (
                r#"{"main":"./dist/x.js","scripts":{"prepack":"x"}}"#,
                &built,
                false,
            ),
        ] {
            assert_eq!(
                checkout_needs_build(&manifest(json), has_file),
                needs,
                "{json}"
            );
        }
    }

    #[test]
    fn a_git_checkout_builds_only_under_a_source_or_repository_approval() {
        const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
        let local = aube_lockfile::LocalSource::Git(aube_lockfile::GitSource {
            url: "https://github.com/acme/demo.git".to_string(),
            committish: None,
            resolved: SHA.to_string(),
            integrity: None,
            subpath: None,
        });
        let manifest = manifest(r#"{"name":"demo","version":"1.0.0","scripts":{"prepare":"x"}}"#);
        let decide = |rules: &[(&str, bool)]| {
            let map = rules
                .iter()
                .map(|(k, v)| (k.to_string(), aube_manifest::AllowBuildRaw::Bool(*v)))
                .collect();
            let (policy, warnings) = aube_scripts::BuildPolicy::from_config(&map, &[], &[], false);
            assert!(warnings.is_empty(), "{warnings:?}");
            checkout_build_allowed(Some(&policy), &local, &manifest)
        };

        let exact = format!("demo@https://github.com/acme/demo.git#{SHA}");
        let pnpm_exact = format!("demo@git+https://github.com/acme/demo.git#{SHA}");
        let repository = "demo@git+https://github.com/acme/demo.git";
        for key in [exact.as_str(), &pnpm_exact, repository] {
            assert!(decide(&[(key, true)]).unwrap(), "{key} approves the build");
        }
        for rules in [
            &[("demo", false)][..],
            &[(repository, false)][..],
            &[("demo", false), (exact.as_str(), true)][..],
        ] {
            assert!(!decide(rules).unwrap(), "{rules:?} denies the build");
        }
        assert!(
            checkout_build_allowed(
                Some(&aube_scripts::BuildPolicy::allow_all()),
                &local,
                &manifest
            )
            .unwrap()
        );

        for rules in [
            &[][..],
            &[("demo", true)][..],
            &[("demo@1.0.0", true)][..],
            &[("*", true)][..],
        ] {
            let err = decide(rules).unwrap_err();
            assert_eq!(
                err.code().map(|c| c.to_string()).as_deref(),
                Some(aube_codes::errors::ERR_AUBE_GIT_DEP_PREPARE_NOT_ALLOWED),
                "{rules:?} neither allows nor denies: {err:?}"
            );
            let help = err.help().map(|h| h.to_string()).unwrap_or_default();
            assert!(help.contains(&exact), "the remedy names the key: {help}");
        }
    }

    #[test]
    fn an_archive_path_exists_as_a_file_or_a_directory() {
        let index: aube_util::collections::FxMap<String, ()> =
            [("lib/index.js".to_string(), ())].into_iter().collect();
        for path in [
            "lib",
            "./lib",
            "lib/",
            "lib/./index.js",
            "lib/index.js",
            ".",
        ] {
            assert!(archive_has_path(&index, path), "{path} exists");
        }
        for path in ["li", "lib/other.js", "dist/index.js", "../lib/index.js"] {
            assert!(!archive_has_path(&index, path), "{path} is missing");
        }
    }

    #[test]
    fn publishing_scripts_run_with_hooks_unless_the_checkout_is_pnpm_or_yarn_berry() {
        const BERRY_LOCK: &str = "__metadata:\n  version: 8\n";
        let dir = tempfile::tempdir().unwrap();
        let hooks = |manifest_json: &str, lockfiles: &[(&str, &str)]| {
            for f in ["pnpm-lock.yaml", "yarn.lock", "package-lock.json"] {
                let _ = std::fs::remove_file(dir.path().join(f));
            }
            for (f, contents) in lockfiles {
                std::fs::write(dir.path().join(f), contents).unwrap();
            }
            runs_pre_post_hooks(dir.path(), &manifest(manifest_json))
        };
        assert!(hooks("{}", &[]), "npm by default");
        assert!(hooks(r#"{"packageManager":"npm@11.0.0"}"#, &[]));
        assert!(hooks(r#"{"packageManager":"yarn@1.22.22"}"#, &[]));
        assert!(!hooks(r#"{"packageManager":"yarn@4.5.0"}"#, &[]));
        assert!(hooks(
            r#"{"packageManager":"yarn@1.22.22+sha512.abc"}"#,
            &[]
        ));
        assert!(
            hooks(
                r#"{"devEngines":{"packageManager":{"name":"yarn","version":"<2"}}}"#,
                &[]
            ),
            "a range below 2 is Classic"
        );
        assert!(
            hooks(
                r#"{"devEngines":{"packageManager":{"name":"yarn","version":"^1 || ^4"}}}"#,
                &[]
            ),
            "a range still admitting 1.x is provisioned as Classic"
        );
        assert!(!hooks(
            r#"{"devEngines":{"packageManager":{"name":"yarn","version":">=2"}}}"#,
            &[]
        ));
        assert!(!hooks(r#"{"packageManager":"pnpm@10.0.0"}"#, &[]));
        assert!(!hooks("{}", &[("pnpm-lock.yaml", "")]));
        assert!(hooks("{}", &[("yarn.lock", "# yarn lockfile v1\n")]));
        assert!(
            !hooks("{}", &[("yarn.lock", BERRY_LOCK)]),
            "a Berry lockfile, no config"
        );
        assert!(
            !hooks(
                r#"{"packageManager":"yarn@1.22.22","devEngines":{"packageManager":{"name":"yarn","version":"4.9.2"}}}"#,
                &[("yarn.lock", BERRY_LOCK)]
            ),
            "devEngines outranks packageManager"
        );
    }
}
