use miette::{IntoDiagnostic, Result, WrapErr};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

#[derive(clap::Args)]
pub struct CompletionArgs {
    /// The shell to generate completions for (bash, zsh, fish)
    #[arg(value_name = "SHELL")]
    pub shell: String,
    /// Emit dynamic candidates for the shell completion engine.
    #[arg(long, hide = true, value_enum)]
    complete: Option<CompletionKind>,
    /// Current word being completed.
    #[arg(long, hide = true, default_value = "")]
    query: String,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum CompletionKind {
    Package,
    Bin,
    Workspace,
    Setting,
    Patch,
}

pub async fn run(args: CompletionArgs) -> Result<()> {
    if args.is_probe() {
        args.run_probe(None).await;
        return Ok(());
    }
    let shell = args.shell.as_str();
    // Command name from the active embedder (standalone aube → "aube").
    let name = aube_util::embedder().name;
    let usage_cmd = format!("{name} usage");
    let output = Command::new("usage")
        .args([
            "g",
            "completion",
            shell,
            name,
            "--usage-cmd",
            &usage_cmd,
            "--cache-key",
            env!("CARGO_PKG_VERSION"),
        ])
        .output()
        .into_diagnostic()
        .wrap_err("failed to invoke `usage`; install it from https://usage.jdx.dev")?;
    ensure_success(shell, &output)?;

    let aubr = generate_multicall_completion(shell, "run", "aubr", true)?;
    let aubx = generate_multicall_completion(shell, "dlx", "aubx", false)?;

    let mut stdout = std::io::stdout();
    stdout
        .write_all(&output.stdout)
        .into_diagnostic()
        .wrap_err("failed to write completions to stdout")?;
    stdout
        .write_all(&aubr.stdout)
        .into_diagnostic()
        .wrap_err("failed to write aubr completions to stdout")?;
    stdout
        .write_all(&aubx.stdout)
        .into_diagnostic()
        .wrap_err("failed to write aubx completions to stdout")?;
    Ok(())
}

impl CompletionArgs {
    pub(crate) fn is_probe(&self) -> bool {
        self.complete.is_some()
    }

    pub(crate) async fn run_probe(&self, cwd: Option<&Path>) {
        let Some(kind) = self.complete else {
            return;
        };
        let Some(cwd) = super::completion_start_dir(cwd) else {
            return;
        };
        print_candidates(kind, &self.query, &cwd).await;
    }
}

fn generate_multicall_completion(
    shell: &str,
    subcommand: &str,
    name: &str,
    append_shared_completers: bool,
) -> Result<std::process::Output> {
    let mut child = Command::new("usage")
        .args(["g", "completion", shell, name, "-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .into_diagnostic()
        .wrap_err("failed to invoke `usage`; install it from https://usage.jdx.dev")?;
    let spec = multicall_usage_spec(subcommand, name, append_shared_completers);
    child
        .stdin
        .take()
        .ok_or_else(|| {
            miette::miette!(
                code = aube_codes::errors::ERR_AUBE_COMPLETION_FAILED,
                "failed to open stdin for `usage g completion {shell}`"
            )
        })?
        .write_all(spec.as_bytes())
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to send the {name} usage spec to `usage`"))?;
    let output = child
        .wait_with_output()
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to wait for `usage` to generate {name} completions"))?;
    ensure_success(shell, &output)?;
    Ok(output)
}

fn ensure_success(shell: &str, output: &std::process::Output) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(miette::miette!(
        code = aube_codes::errors::ERR_AUBE_COMPLETION_FAILED,
        "`usage g completion {shell}` failed: {}",
        stderr.trim()
    ))
}

fn multicall_usage_spec(subcommand: &str, name: &str, append_shared_completers: bool) -> String {
    let mut cmd = crate::command();
    let mut spec = clap_usage::spec(&mut cmd, aube_util::prog());
    let root_flags = spec
        .cmd
        .flags
        .iter()
        .filter(|flag| flag.global)
        .cloned()
        .collect::<Vec<_>>();
    // WHY: these are compile-time members of `Cli`; if one disappeared its
    // completion generator would be invalid and its focused test would fail.
    let mut command = spec
        .cmd
        .subcommands
        .shift_remove(subcommand)
        .unwrap_or_else(|| panic!("aube command must contain {subcommand}"));
    command.flags.splice(0..0, root_flags);
    command.name = name.to_string();
    command.full_cmd = vec![name.to_string()];
    command.aliases.clear();
    command.hidden_aliases.clear();
    command.usage = command.usage.replacen(&aube_util::cmd(subcommand), name, 1);

    // Completion owns only aubx's first positional. Everything after the
    // command is passed to the fetched binary and must not be interpreted as
    // more aube arguments.
    if name == "aubx"
        && let Some(params) = command.args.first_mut()
    {
        params.var = false;
        params.var_max = Some(1);
        // WHY: `automatic` is a usage-lib enum literal; failure would mean
        // the completion protocol changed underneath clap_usage.
        params.double_dash = "automatic"
            .parse()
            .expect("automatic must remain a valid usage double-dash mode");
        params.usage = params.usage();
    }

    spec.name = name.to_string();
    spec.bin = name.to_string();
    spec.usage = command.usage.clone();
    spec.about = command.help.clone();
    spec.about_long = command.help_long.clone();
    spec.cmd = command;

    let extra = if append_shared_completers {
        include_str!("../../assets/extra.usage.kdl")
            .replace("{run}", name)
            .replace(
                "{completers}",
                &include_str!("../../assets/completion.usage.kdl").replace("{bin}", "aube"),
            )
    } else {
        format!(
            "{}\n{}",
            include_str!("../../assets/completion.usage.kdl").replace("{bin}", "aube"),
            include_str!("../../assets/aubx.usage.kdl")
        )
    };
    format!("// @generated by aube for the {name} multicall surface\n{spec}\n{extra}")
}

async fn print_candidates(kind: CompletionKind, query: &str, cwd: &Path) {
    let mut candidates = match kind {
        CompletionKind::Package => package_candidates(cwd, query).await,
        CompletionKind::Bin => bin_candidates(cwd),
        CompletionKind::Workspace => workspace_candidates(cwd),
        CompletionKind::Setting => setting_candidates(),
        CompletionKind::Patch => patch_candidates(cwd),
    };
    candidates.sort_by(|a, b| a.0.cmp(&b.0));
    candidates.dedup_by(|a, b| a.0 == b.0);
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    write_candidates(&mut stdout, candidates);
}

fn write_candidates(writer: &mut impl Write, candidates: Vec<(String, String)>) {
    for (value, description) in candidates {
        if writeln!(
            writer,
            "{}:{}",
            escape_completion_field(&value),
            escape_completion_field(&description)
        )
        .is_err()
        {
            break;
        }
    }
}

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

fn escape_completion_field(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            ':' => escaped.push_str("\\:"),
            ch if ch.is_control() => escaped.push(' '),
            ch => escaped.push(ch),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::{
        bin_candidates, dependency_candidates, escape_completion_field, multicall_usage_spec,
        package_name_from_spec, setting_candidates, write_candidates,
    };
    use std::io::{self, Write};

    #[test]
    fn aubr_spec_roots_run_arguments_and_dynamic_completion() {
        let spec = multicall_usage_spec("run", "aubr", true);
        assert!(spec.contains("name aubr\nbin aubr"));
        assert!(spec.contains("arg \"[SCRIPT]\""));
        assert!(spec.contains("flag --no-install"));
        assert!(spec.contains("flag \"-C --dir --cd --prefix\""));
        assert!(spec.contains("complete \"script\" run=\"aubr --complete\""));
        assert!(!spec.contains("\ncmd install "));
    }

    #[test]
    fn aubx_spec_completes_only_the_command_position() {
        let spec = multicall_usage_spec("dlx", "aubx", false);
        assert!(spec.contains("name aubx\nbin aubx"));
        assert!(spec.contains("arg \"[PARAMS]\""));
        assert!(!spec.contains("arg \"[PARAMS]…\""));
        assert!(spec.contains("complete \"params\""));
        assert!(spec.contains("complete \"package\""));
        assert!(spec.contains("replace(from=\"'\", to=\"'\\\\''\")"));
        assert!(!spec.contains("\ncmd install "));
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
        assert_eq!(
            escape_completion_field("1.0: useful\npackage"),
            "1.0\\: useful package"
        );
    }

    #[test]
    fn candidate_output_stops_quietly_on_broken_pipe() {
        struct BrokenPipe;

        impl Write for BrokenPipe {
            fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        write_candidates(
            &mut BrokenPipe,
            vec![("react".to_string(), "19.1.0".to_string())],
        );
    }
}
