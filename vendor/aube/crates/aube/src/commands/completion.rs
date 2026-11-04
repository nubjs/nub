use miette::{Result, miette};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The programs aube answers for: itself, and the two applets its spec declares as views.
const PROGRAMS: [&str; 3] = ["aube", "aubr", "aubx"];

#[derive(usage_rs::Args)]
pub struct CompletionArgs {
    /// The shell to generate completions for (bash, zsh, fish)
    #[usage(arg, name = "SHELL")]
    pub shell: String,

    /// Replace a file at a target path that aube did not write
    #[usage(long, requires = "--install", effect = "write")]
    pub force: bool,

    /// Install the scripts where this shell looks for them, instead of printing them
    ///
    /// Writes one script per program — aube, aubr and aubx — and nothing else: no shell rc file
    /// and no PowerShell profile is edited. Where a shell needs a one-time line of its own, it is
    /// printed for you to add.
    #[usage(long, effect = "write")]
    pub install: bool,
}

pub async fn run(args: CompletionArgs) -> Result<()> {
    let shell = usage_rs::complete::Shell::from_name(&args.shell)
        .ok_or_else(|| miette!("unsupported shell {:?}", args.shell))?;
    if args.install {
        return install(shell, args.force);
    }
    for program in PROGRAMS {
        print!(
            "{}",
            crate::completion_app(program).completion_script(shell)
        );
    }
    Ok(())
}

/// Put each program's script where this shell looks for it, and say what is left to do.
///
/// One install per program rather than one for `aube`: an applet is completed under its own name,
/// so `aubr` needs its own file, exactly as it needs its own script. The location comes from
/// usage's resolver, so `aube completion zsh --install` and `usage g completion zsh aubr --install`
/// cannot disagree about where an applet's completion lives.
fn install(shell: usage_rs::complete::Shell, force: bool) -> Result<()> {
    use usage_rs::install::{self, Loading, OnForeign, Wrote};

    let on_foreign = if force {
        OnForeign::Overwrite
    } else {
        OnForeign::Refuse
    };
    // The environment is described from this process rather than reached for inside the resolver,
    // which is what lets a test point the same code path somewhere harmless.
    let env = install::Env::from_process();

    // Collected rather than printed per program: all three land in the same directory, so a shell
    // that needs a line needs it once, and saying it three times is noise a reader has to compare
    // to be sure it really is the same line.
    let mut instructions: Vec<(String, String)> = Vec::new();
    let mut notes: Vec<&'static str> = Vec::new();
    for program in PROGRAMS {
        let done = crate::completion_app(program)
            .install_completion(shell, &env, on_foreign)
            .map_err(|err| as_diagnostic(program, err))?;
        eprintln!("installing to {}", done.plan.path.display());
        if done.wrote == Wrote::Unchanged {
            eprintln!("already up to date");
        }
        if let Loading::Manual { line, file, .. } = &done.plan.loading {
            let entry = (file.clone(), line.clone());
            if !instructions.contains(&entry) {
                instructions.push(entry);
            }
        }
        if let Some(note) = done.plan.note.filter(|note| !notes.contains(note)) {
            notes.push(note);
        }
    }

    for (file, line) in instructions {
        eprintln!("\nadd this to {file}, once:\n\n{line}\n");
    }
    for note in notes {
        eprintln!("note: {note}");
    }
    Ok(())
}

/// An install failure as something aube can print, with the way out where there is one.
///
/// The chain is walked rather than formatted away: `Display` names the step and the path, and the
/// operating system's own words are on `source()`.
fn as_diagnostic(program: &str, err: usage_rs::install::Error) -> miette::Report {
    let mut message = err.to_string();
    let mut cause = std::error::Error::source(&err);
    while let Some(next) = cause {
        message.push_str(&format!(": {next}"));
        cause = next.source();
    }
    match &err {
        usage_rs::install::Error::Foreign { .. } => miette!(
            "{program}: {message}\n\nPass --force to replace it, or redirect the scripts yourself."
        ),
        _ => miette!("{program}: {message}"),
    }
}

fn finish(mut candidates: Vec<(String, String)>) -> Vec<usage_rs::spec::Candidate<'static>> {
    candidates.sort_by(|a, b| a.0.cmp(&b.0));
    candidates.dedup_by(|a, b| a.0 == b.0);
    candidates
        .into_iter()
        .map(|(value, description)| usage_rs::spec::Candidate::described(value, description))
        .collect()
}

fn completion_dir(ctx: &usage_rs::spec::CompleteCtx<'_>) -> Option<PathBuf> {
    let mut dir = None;
    let mut words = ctx.words.iter();
    while let Some(word) = words.next() {
        if let Some(value) = word
            .strip_prefix("--dir=")
            .or_else(|| word.strip_prefix("--cd="))
            .or_else(|| word.strip_prefix("--prefix="))
        {
            dir = Some(PathBuf::from(value));
        } else if matches!(word.as_str(), "-C" | "--dir" | "--cd" | "--prefix") {
            dir = words.next().map(PathBuf::from);
        } else if let Some(value) = word.strip_prefix("-C").filter(|value| !value.is_empty()) {
            dir = Some(PathBuf::from(value));
        }
    }
    super::completion_start_dir(dir.as_deref())
}

macro_rules! sync_completer {
    ($name:ident, $body:expr) => {
        fn $name(ctx: usage_rs::spec::CompleteCtx<'_>) -> usage_rs::complete::CompletionFuture<'_> {
            Box::pin(async move {
                completion_dir(&ctx)
                    .as_deref()
                    .map($body)
                    .map(finish)
                    .unwrap_or_default()
            })
        }
    };
}

fn complete_package(
    ctx: usage_rs::spec::CompleteCtx<'_>,
) -> usage_rs::complete::CompletionFuture<'_> {
    Box::pin(async move {
        let Some(cwd) = completion_dir(&ctx) else {
            return Vec::new();
        };
        finish(package_candidates(&cwd, ctx.prefix).await)
    })
}
sync_completer!(complete_bin, bin_candidates);
sync_completer!(complete_workspace, workspace_candidates);
sync_completer!(complete_patch, patch_candidates);
fn complete_setting(
    _: usage_rs::spec::CompleteCtx<'_>,
) -> usage_rs::complete::CompletionFuture<'_> {
    Box::pin(async { finish(setting_candidates()) })
}
fn complete_script(
    ctx: usage_rs::spec::CompleteCtx<'_>,
) -> usage_rs::complete::CompletionFuture<'_> {
    Box::pin(async move {
        let Some(dir) = completion_dir(&ctx) else {
            return Vec::new();
        };
        finish(super::run::script_completion_candidates(Some(&dir)))
    })
}

pub(crate) static COMPLETIONS: [usage_rs::complete::CompletionOverlay<'static>; 9] = [
    usage_rs::complete::CompletionOverlay::async_any("package", complete_package),
    usage_rs::complete::CompletionOverlay::async_any("packages", complete_package),
    usage_rs::complete::CompletionOverlay::async_any("pkg", complete_package),
    usage_rs::complete::CompletionOverlay::async_any("params", complete_package),
    usage_rs::complete::CompletionOverlay::async_any("bin", complete_bin),
    usage_rs::complete::CompletionOverlay::async_any("workspace", complete_workspace),
    usage_rs::complete::CompletionOverlay::async_any("key", complete_setting),
    usage_rs::complete::CompletionOverlay::async_any("patch", complete_patch),
    usage_rs::complete::CompletionOverlay::async_any("script", complete_script),
];

async fn package_candidates(cwd: &Path, query: &str) -> Vec<(String, String)> {
    let mut candidates = dependency_candidates(cwd);
    candidates.extend(workspace_candidates(cwd));
    if let Ok(entries) = std::fs::read_dir(cwd) {
        candidates.extend(entries.flatten().filter_map(|entry| {
            if !entry.file_type().ok()?.is_dir() {
                return None;
            }
            let name = entry.file_name().into_string().ok()?;
            Some((format!("./{name}"), "local directory".to_string()))
        }));
    }
    if query.len() >= 2 && package_name_from_spec(query) == query {
        let client = super::make_client(cwd);
        if let Ok(results) = client
            .search_packages(query, 40, Duration::from_millis(1200))
            .await
        {
            candidates.extend(results.into_iter().map(|package| {
                let description = match package.description {
                    Some(description) if !description.is_empty() => {
                        format!("{} — {}", package.version, description)
                    }
                    _ => package.version,
                };
                (package.name, description)
            }));
        }
    }
    candidates
}

fn package_name_from_spec(spec: &str) -> &str {
    if let Some(scoped) = spec.strip_prefix('@') {
        let Some(slash) = scoped.find('/') else {
            return spec;
        };
        let after_name = &scoped[slash + 1..];
        after_name
            .find('@')
            .map(|at| &spec[..slash + 2 + at])
            .unwrap_or(spec)
    } else {
        spec.find('@').map(|at| &spec[..at]).unwrap_or(spec)
    }
}

fn dependency_candidates(cwd: &Path) -> Vec<(String, String)> {
    let Some(root) = crate::dirs::find_project_root(cwd) else {
        return Vec::new();
    };
    let Ok(manifest) = aube_manifest::PackageJson::from_path(&root.join("package.json")) else {
        return Vec::new();
    };
    let mut dependencies = BTreeMap::new();
    for (kind, entries) in [
        ("dependency", &manifest.dependencies),
        ("dev dependency", &manifest.dev_dependencies),
        ("optional dependency", &manifest.optional_dependencies),
        ("peer dependency", &manifest.peer_dependencies),
    ] {
        for (name, version) in entries {
            dependencies
                .entry(name.clone())
                .or_insert_with(|| format!("{version} ({kind})"));
        }
    }
    dependencies.into_iter().collect()
}

fn bin_candidates(cwd: &Path) -> Vec<(String, String)> {
    let Some(root) = crate::dirs::find_project_root(cwd) else {
        return Vec::new();
    };
    let bin_dir = super::project_modules_dir(&root).join(".bin");
    let Ok(entries) = std::fs::read_dir(bin_dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| !name.ends_with(".cmd") && !name.ends_with(".ps1"))
        .map(|name| (name, "local executable".to_string()))
        .collect()
}

fn workspace_candidates(cwd: &Path) -> Vec<(String, String)> {
    let root = crate::dirs::find_workspace_root(cwd)
        .or_else(|| crate::dirs::find_project_root(cwd))
        .unwrap_or_else(|| cwd.to_path_buf());
    aube_workspace::find_workspace_packages(&root)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|path| {
            let manifest =
                aube_manifest::PackageJson::from_path(&path.join("package.json")).ok()?;
            let name = manifest.name?;
            let rel = path.strip_prefix(&root).unwrap_or(&path);
            Some((name, format!("workspace {}", rel.display())))
        })
        .collect()
}

fn setting_candidates() -> Vec<(String, String)> {
    aube_settings::all()
        .flat_map(|setting| {
            std::iter::once((setting.name.to_string(), setting.description.to_string())).chain(
                setting
                    .npmrc_keys
                    .iter()
                    .map(|key| ((*key).to_string(), setting.description.to_string())),
            )
        })
        .collect()
}

fn patch_candidates(cwd: &Path) -> Vec<(String, String)> {
    let Some(root) = crate::dirs::find_project_root(cwd) else {
        return Vec::new();
    };
    crate::patches::read_patched_dependencies(&root)
        .unwrap_or_default()
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        bin_candidates, dependency_candidates, package_name_from_spec, setting_candidates,
    };

    #[test]
    fn built_in_scripts_target_each_binary() {
        let shell = usage_rs::complete::Shell::Bash;
        assert!(
            crate::completion_app("aube")
                .completion_script(shell)
                .contains("aube")
        );
        assert!(
            crate::completion_app("aubr")
                .completion_script(shell)
                .contains("aubr")
        );
        assert!(
            crate::completion_app("aubx")
                .completion_script(shell)
                .contains("aubx")
        );
    }

    #[test]
    fn local_candidates_include_dependencies_bins_and_setting_aliases() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{
                "dependencies": {"react": "^19"},
                "devDependencies": {"vitest": "^3"}
            }"#,
        )
        .unwrap();
        let bin_dir = dir.path().join("node_modules/.bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::write(bin_dir.join("vite"), "").unwrap();

        let dependencies = dependency_candidates(dir.path());
        assert!(dependencies.iter().any(|(name, _)| name == "react"));
        assert!(dependencies.iter().any(|(name, _)| name == "vitest"));
        assert!(
            bin_candidates(dir.path())
                .iter()
                .any(|(name, _)| name == "vite")
        );
        assert!(
            setting_candidates()
                .iter()
                .any(|(name, _)| name == "auto-install-peers")
        );
    }

    #[test]
    fn package_specs_and_descriptions_are_completion_safe() {
        assert_eq!(package_name_from_spec("react@next"), "react");
        assert_eq!(package_name_from_spec("@scope/pkg@^2"), "@scope/pkg");
        assert_eq!(package_name_from_spec("@scope/pkg"), "@scope/pkg");
    }
}
