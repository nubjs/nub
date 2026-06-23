use super::{remove_existing, symlink_dir};
use clap::Args;
use miette::{Context, IntoDiagnostic, miette};

#[derive(Debug, Args)]
pub struct LinkArgs {
    /// Package name, or path to a local directory
    pub package: Option<String>,
    /// Register into (or resolve from) the global link registry.
    ///
    /// The registry lives at `$AUBE_HOME/global-links`. Default
    /// behavior for bare `aube link` / `aube link <name>` — the flag
    /// exists for pnpm parity and makes the intent explicit.
    #[arg(short = 'g', long)]
    pub global: bool,
}

/// Returns true if the argument looks like a filesystem path rather than a package name.
fn is_path_arg(arg: &str) -> bool {
    if arg.starts_with('@') {
        // Scoped package name (e.g. @scope/pkg) — not a filesystem path
        return false;
    }
    arg.starts_with('.')
        || arg.starts_with('/')
        || arg.contains('/')
        || arg.contains(std::path::MAIN_SEPARATOR)
}

pub async fn run(args: LinkArgs) -> miette::Result<()> {
    let _ = args.global; // accepted for pnpm parity; bare `aube link` already uses the global registry
    let package = args.package.as_deref();
    let cwd = crate::dirs::project_root()?;
    let _lock = super::take_project_lock(&cwd)?;
    let global_links = aube_store::dirs::global_links_dir()
        .ok_or_else(|| miette!("could not determine global links directory"))?;

    match package {
        None => {
            // `aube link` — register current package globally
            let manifest = super::load_manifest(&cwd.join("package.json"))?;
            let name = manifest
                .name
                .as_deref()
                .ok_or_else(|| miette!("package.json has no \"name\" field"))?;

            let link_path = global_links.join(name);
            if let Some(parent) = link_path.parent() {
                std::fs::create_dir_all(parent).into_diagnostic()?;
            }

            remove_existing(&link_path)?;
            symlink_dir(&cwd, &link_path).into_diagnostic()?;

            eprintln!("Linked {} -> {}", link_path.display(), cwd.display());
        }
        Some(arg) if is_path_arg(arg) => {
            // `aube link <dir>` — link a local directory into node_modules
            let target = std::fs::canonicalize(arg)
                .into_diagnostic()
                .wrap_err_with(|| format!("could not resolve path: {arg}"))?;

            if !target.is_dir() {
                return Err(miette!("{arg} is not a directory"));
            }

            let manifest = aube_manifest::PackageJson::from_path(&target.join("package.json"))
                .map_err(miette::Report::new)
                .wrap_err_with(|| format!("failed to read {}/package.json", target.display()))?;
            let name = manifest.name.as_deref().ok_or_else(|| {
                miette!("{}/package.json has no \"name\" field", target.display())
            })?;

            let link_path = super::project_modules_dir(&cwd).join(name);
            if let Some(parent) = link_path.parent() {
                std::fs::create_dir_all(parent).into_diagnostic()?;
            }

            remove_existing(&link_path)?;
            symlink_dir(&target, &link_path).into_diagnostic()?;

            eprintln!("Linked {} -> {}", link_path.display(), target.display());
        }
        Some(name) => {
            // `aube link <pkg>` — link a globally-registered package into node_modules
            let source = global_links.join(name);
            if source.symlink_metadata().is_err() {
                return Err(miette!(
                    "package {name} is not linked globally\n\
                     Run `{}` in the package directory first",
                    aube_util::cmd("link")
                ));
            }

            let link_path = super::project_modules_dir(&cwd).join(name);
            if let Some(parent) = link_path.parent() {
                std::fs::create_dir_all(parent).into_diagnostic()?;
            }

            // Resolve the global link to the actual target directory
            let target = std::fs::read_link(&source)
                .into_diagnostic()
                .wrap_err_with(|| format!("failed to read global link for {name}"))?;

            remove_existing(&link_path)?;
            symlink_dir(&target, &link_path).into_diagnostic()?;

            eprintln!("Linked {} -> {}", link_path.display(), target.display());
        }
    }

    Ok(())
}
