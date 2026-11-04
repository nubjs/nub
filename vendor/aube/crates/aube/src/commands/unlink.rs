use miette::{Context, IntoDiagnostic, miette};

#[derive(Debug, usage_rs::Args)]
pub struct UnlinkArgs {
    /// Package name to unlink (omit to unlink all linked dependencies)
    pub package: Option<String>,
    /// Operate on the global link registry instead of the current
    /// project.
    ///
    /// `aube unlink -g` removes the current package's entry from the
    /// `global-links` directory under the cache directory;
    /// `aube unlink -g <name>` removes the named entry.
    #[usage(short = 'g', long)]
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

            // Skip symlinks pointing into the virtual store — those are regular
            // install symlinks, not user-created links. Remove via the shared
            // guard so scope cleanup still runs.
            //
            // The message stays neutral about the store's name: it defaults to
            // `.aube`, but `virtualStoreDir` can move it (the classification
            // resolves the leaf dynamically for that reason), so naming `.aube`
            // outright would be wrong for anyone who overrode it.
            if !remove_if_external_symlink(&cwd, &link_path)? {
                return Err(miette!(
                    "{name} is not a linked package (points into the virtual store — run `{}` to restore)",
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
///
/// The "internal" test is textual first. With the global virtual store
/// enabled, `.aube/<dep_path>` is itself a symlink into
/// `<cacheDir>/virtual-store/<dep>-<hash>`, so canonicalizing the
/// target escapes the project's virtual store and every ordinary
/// registry dep would look like a user `aube link`. Comparing the
/// lex-normalized raw target against the resolved virtual-store dir
/// keeps a `.aube/<dep>` target internal regardless of where that
/// entry points. Canonicalization stays as the fallback so a symlinked
/// project path (macOS `/tmp` → `/private/tmp`) still compares
/// correctly.
///
/// Deliberately no global-virtual-store anchor here: every writer of a
/// visible `node_modules/<name>` symlink (`link.rs` steps 2/2b, both
/// hoist passes) targets `.aube/<entry>/node_modules/<name>`, a
/// workspace sibling, or a `link:` path — never the shared store
/// directly. A GVS-root check would be dead weight that could only
/// misfire, hiding a genuine link whose target happens to sit under a
/// user-configured `globalVirtualStoreDir`.
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

    // Honors `virtualStoreDir` so a custom-path `.aube/` still
    // classifies as internal.
    let pnpm_raw = super::resolve_virtual_store_dir_for_cwd(project_dir);

    // An empty anchor would make `starts_with` true for every target and
    // turn genuine user links into "internal", so never match on one.
    let contains = |target: &std::path::Path, anchor: &std::path::Path| {
        !anchor.as_os_str().is_empty() && target.starts_with(anchor)
    };

    // Textual pass: collapse `.`/`..` without following symlinks, so a
    // relative `.aube/<dep>/node_modules/<name>` target stays anchored
    // inside the project's virtual store.
    let target_lex = aube_linker::normalize_path(&abs_target);
    if contains(&target_lex, &aube_linker::normalize_path(&pnpm_raw)) {
        return Ok(false);
    }

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
            // Canonicalize the anchor too, so symlinked project paths
            // (e.g. macOS `/tmp` → `/private/tmp`) compare correctly.
            let pnpm_dir = std::fs::canonicalize(&pnpm_raw).unwrap_or(pnpm_raw);
            if contains(&canonical, &pnpm_dir) {
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
