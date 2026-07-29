use clap::Args;
use miette::{Context, IntoDiagnostic, miette};

#[derive(Debug, Args)]
pub struct UnlinkArgs {
    /// Package name to unlink (omit to unlink all linked dependencies)
    pub package: Option<String>,
    /// Operate on the global link registry instead of the current
    /// project.
    ///
    /// `aube unlink -g` removes the current package's entry from
    /// `$AUBE_HOME/global-links`; `aube unlink -g <name>` removes the
    /// named entry.
    #[arg(short = 'g', long)]
    pub global: bool,
}

/// Unlink a package: remove linked symlinks from node_modules.
///
/// Matches pnpm's semantics (https://pnpm.io/cli/unlink):
/// - `aube unlink` — remove all linked dependencies from the current project
/// - `aube unlink <pkg>` — remove a specific linked dependency from node_modules
///
/// After unlinking, run `aube install` to re-install dependencies from the registry.
pub async fn run(args: UnlinkArgs) -> miette::Result<()> {
    let package = args.package.as_deref();
    let cwd = crate::dirs::project_root()?;
    let _lock = crate::commands::take_project_lock(&cwd)?;
    let nm = super::project_modules_dir(&cwd);

    if args.global {
        return unlink_global(&cwd, package);
    }

    match package {
        Some(name) => {
            // Remove a specific linked entry from node_modules/<name>
            let link_path = nm.join(name);

            let meta = link_path
                .symlink_metadata()
                .map_err(|_| miette!("package '{name}' is not present in node_modules"))?;

            if !meta.file_type().is_symlink() {
                return Err(miette!(
                    "{} is not a symlink — not a linked package",
                    link_path.display()
                ));
            }

            // Skip symlinks pointing into .aube — those are regular install symlinks,
            // not user-created links. Remove via the shared guard so scope cleanup still runs.
            if !remove_if_external_symlink(&cwd, &link_path)? {
                return Err(miette!(
                    "{name} is not a linked package (points into .aube — run `{}` to restore)",
                    aube_util::cmd("install")
                ));
            }
            eprintln!("Unlinked {name}");

            // Clean up empty scope directory (e.g. node_modules/@scope/)
            if let Some(parent) = link_path.parent()
                && parent != nm
                && let Ok(mut entries) = std::fs::read_dir(parent)
                && entries.next().is_none()
            {
                let _ = std::fs::remove_dir(parent);
            }
        }
        None => {
            // Remove all linked (symlink) entries in node_modules that point outside the project.
            if !nm.exists() {
                eprintln!("No node_modules directory — nothing to unlink");
                return Ok(());
            }

            let mut unlinked = 0usize;
            for entry in std::fs::read_dir(&nm).into_diagnostic()? {
                let entry = entry.into_diagnostic()?;
                let path = entry.path();
                let name = entry.file_name();
                let name_str = name.to_string_lossy();

                // Skip .bin, .aube, .modules.yaml, etc.
                if name_str.starts_with('.') {
                    continue;
                }

                // Handle scoped directories: node_modules/@scope/pkg
                if name_str.starts_with('@') && path.is_dir() && !path.is_symlink() {
                    for sub in std::fs::read_dir(&path).into_diagnostic()? {
                        let sub = sub.into_diagnostic()?;
                        let sub_path = sub.path();
                        if remove_if_external_symlink(&cwd, &sub_path)? {
                            eprintln!(
                                "Unlinked {}/{}",
                                name_str,
                                sub.file_name().to_string_lossy()
                            );
                            unlinked += 1;
                        }
                    }
                    // Remove empty scope dir
                    if let Ok(mut entries) = std::fs::read_dir(&path)
                        && entries.next().is_none()
                    {
                        let _ = std::fs::remove_dir(&path);
                    }
                    continue;
                }

                if remove_if_external_symlink(&cwd, &path)? {
                    eprintln!("Unlinked {name_str}");
                    unlinked += 1;
                }
            }

            if unlinked == 0 {
                eprintln!("No linked packages found");
            } else {
                eprintln!(
                    "Unlinked {unlinked} package{}. Run `{}` to restore from registry.",
                    if unlinked == 1 { "" } else { "s" },
                    aube_util::cmd("install")
                );
            }
        }
    }

    Ok(())
}

/// `aube unlink --global [<name>]`: remove an entry from the global
/// link registry. With no name, use the current package.json's
/// `name` field. Leaves project-level `node_modules` untouched.
fn unlink_global(cwd: &std::path::Path, explicit_name: Option<&str>) -> miette::Result<()> {
    let global_links = aube_store::dirs::global_links_dir()
        .ok_or_else(|| miette!("could not determine global links directory"))?;
    let name = if let Some(n) = explicit_name {
        n.to_string()
    } else {
        let manifest = aube_manifest::PackageJson::from_path(&cwd.join("package.json"))
            .map_err(miette::Report::new)
            .wrap_err("failed to read package.json")?;
        manifest
            .name
            .clone()
            .ok_or_else(|| miette!("package.json has no \"name\" field"))?
    };
    let link_path = global_links.join(&name);
    if link_path.symlink_metadata().is_err() {
        return Err(miette!("{name} is not registered as a global link"));
    }
    std::fs::remove_file(&link_path)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to remove {}", link_path.display()))?;
    eprintln!("Unlinked global {name}");
    Ok(())
}

/// If `path` is a symlink whose target is outside `project_dir/node_modules/.aube`,
/// remove it and return `true`. Symlinks pointing into `.aube/` (regular install symlinks)
/// are left alone and return `false`.
fn remove_if_external_symlink(
    project_dir: &std::path::Path,
    path: &std::path::Path,
) -> miette::Result<bool> {
    let Ok(meta) = path.symlink_metadata() else {
        return Ok(false);
    };
    if !meta.file_type().is_symlink() {
        return Ok(false);
    }

    let target = std::fs::read_link(path).into_diagnostic()?;

    // Resolve relative targets against the link's parent
    let abs_target = if target.is_absolute() {
        target.clone()
    } else if let Some(parent) = path.parent() {
        parent.join(&target)
    } else {
        target.clone()
    };

    // Canonicalize both sides so symlinked project paths (e.g. macOS /tmp → /private/tmp)
    // compare correctly. Honors `virtualStoreDir` so a custom-path
    // `.aube/` still classifies as internal.
    let pnpm_raw = super::resolve_virtual_store_dir_for_cwd(project_dir);
    let pnpm_dir = std::fs::canonicalize(&pnpm_raw).unwrap_or_else(|_| pnpm_raw.clone());
    // Derive the virtual-store dir's leaf name from the resolved path
    // so the dangling-symlink fallback below matches regardless of
    // whether the user overrode `virtualStoreDir` to `.custom-vs`,
    // `.aube-store`, etc.
    let vstore_leaf = pnpm_raw
        .file_name()
        .map(|s| s.to_owned())
        .unwrap_or_else(|| std::ffi::OsString::from(".aube"));

    match std::fs::canonicalize(&abs_target) {
        Ok(canonical) => {
            if canonical.starts_with(&pnpm_dir) {
                return Ok(false);
            }
        }
        Err(_) => {
            // Dangling symlink — canonicalize failed. Fall back to a
            // component-wise check: if any segment of the raw target
            // matches our resolved virtual-store leaf name, treat it
            // as internal.
            if target.components().any(|c| c.as_os_str() == vstore_leaf) {
                return Ok(false);
            }
        }
    }

    std::fs::remove_file(path)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to remove {}", path.display()))?;
    Ok(true)
}
