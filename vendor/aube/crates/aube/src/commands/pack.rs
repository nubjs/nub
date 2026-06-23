//! `aube pack` — build a publishable `.tgz` tarball from the current project.
//!
//! Mirrors `pnpm pack` / `npm pack`: the tarball entry names are rooted at
//! `package/`, every entry has a fixed mtime (1985-10-26 08:15:00 UTC) so
//! identical inputs produce byte-identical archives, and the default
//! output path is `<sanitized-name>-<version>.tgz` in the current directory.
//!
//! File selection follows npm's rules in priority order:
//!   1. `files` field in package.json — each entry is a glob relative to
//!      the project dir. Everything reachable is included.
//!   2. Otherwise: walk the project, skip the npm standard-ignore list
//!      (`.git/`, `node_modules/`, `*.tgz`, CI dotfiles, etc.), and
//!      respect a root-level `.npmignore` (or `.gitignore` if no
//!      `.npmignore` exists) with full gitignore semantics — negation
//!      (`!pattern`), anchoring (`/pattern`), and globs (`*`, `**`,
//!      `?`, `[abc]`) — via the `ignore` crate.
//!
//! In both cases `package.json`, `README*`, `LICENSE*`/`LICENCE*`, and the
//! `main` entry are always included — matching npm's "always-on" list.
//! `CHANGELOG*` is intentionally excluded (npm dropped it in
//! npm/npm-packlist#61): changelogs grow forever and few consumers read
//! them out of `node_modules`. Users who want it shipped can list it in
//! the `files` field.

use aube_manifest::PackageJson;
use clap::Args;
use flate2::Compression;
use flate2::write::GzEncoder;
use ignore::WalkBuilder;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use miette::{Context, IntoDiagnostic, miette};
use serde::Serialize;
use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Args)]
pub struct PackArgs {
    /// Don't write the tarball; print what would be packed
    #[arg(long)]
    pub dry_run: bool,
    /// Skip `prepack` / `prepare` / `postpack` lifecycle scripts.
    #[arg(long)]
    pub ignore_scripts: bool,
    /// Print the result as a JSON object
    #[arg(long)]
    pub json: bool,
    /// Directory to write the tarball into (default: current directory)
    #[arg(long, value_name = "DIR")]
    pub pack_destination: Option<PathBuf>,
    #[command(flatten)]
    pub network: crate::cli_args::NetworkArgs,
}

#[derive(Debug, Serialize)]
struct PackResult {
    name: String,
    version: String,
    filename: String,
    files: Vec<FileEntry>,
}

#[derive(Debug, Serialize)]
struct FileEntry {
    path: String,
}

pub async fn run(args: PackArgs) -> miette::Result<()> {
    args.network.install_overrides();
    let invocation_cwd = crate::dirs::cwd()?;
    let project_root = crate::dirs::project_root()?;

    run_pack_lifecycle_pre(&project_root, args.ignore_scripts).await?;
    let archive = build_archive(&project_root)?;
    run_pack_lifecycle_post(&project_root, args.ignore_scripts).await?;

    let dest_dir = args
        .pack_destination
        .as_deref()
        .map(|p| {
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                invocation_cwd.join(p)
            }
        })
        .unwrap_or_else(|| invocation_cwd.clone());
    let dest_path = dest_dir.join(&archive.filename);

    if !args.dry_run {
        std::fs::create_dir_all(&dest_dir)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to create {}", dest_dir.display()))?;
        std::fs::write(&dest_path, &archive.tarball)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to write {}", dest_path.display()))?;
    }

    let relative_filename = if dest_dir == invocation_cwd {
        archive.filename.clone()
    } else {
        dest_path.display().to_string()
    };

    let result = PackResult {
        name: archive.name.clone(),
        version: archive.version.clone(),
        filename: relative_filename,
        files: archive
            .files
            .iter()
            .map(|rel| FileEntry { path: rel.clone() })
            .collect(),
    };

    if args.json {
        // pnpm/npm return an array even for a single package; match that
        // so `jq '.[0].filename'`-style consumers keep working.
        let out = serde_json::to_string_pretty(&[&result]).into_diagnostic()?;
        println!("{out}");
    } else {
        println!("package: {}@{}", result.name, result.version);
        println!("Tarball Contents");
        for f in &result.files {
            println!("  {}", f.path);
        }
        println!("Tarball Details");
        println!("  {}", result.filename);
    }

    Ok(())
}

/// Run `prepack` then `prepare` against the root manifest, in npm's
/// documented order. No-op under `--ignore-scripts`, and silently
/// skips scripts that aren't defined. Shared with `aube publish` so
/// the two commands can't drift on the pack-time lifecycle. The
/// manifest is read once here so callers that chain multiple hooks
/// don't re-parse `package.json` on every call.
pub(crate) async fn run_pack_lifecycle_pre(
    project_root: &Path,
    ignore_scripts: bool,
) -> miette::Result<()> {
    if ignore_scripts {
        return Ok(());
    }
    let manifest = read_root_manifest(project_root)?;
    run_root_lifecycle_script(project_root, &manifest, "prepack").await?;
    run_root_lifecycle_script(project_root, &manifest, "prepare").await?;
    Ok(())
}

/// Run `postpack` against the root manifest. No-op under
/// `--ignore-scripts`. Symmetric with [`run_pack_lifecycle_pre`].
pub(crate) async fn run_pack_lifecycle_post(
    project_root: &Path,
    ignore_scripts: bool,
) -> miette::Result<()> {
    if ignore_scripts {
        return Ok(());
    }
    let manifest = read_root_manifest(project_root)?;
    run_root_lifecycle_script(project_root, &manifest, "postpack").await?;
    Ok(())
}

/// Read and parse the root `package.json`. Shared helper so the
/// lifecycle paths (pack/publish/version) all surface the same error
/// context when the manifest is missing or malformed.
pub(crate) fn read_root_manifest(project_root: &Path) -> miette::Result<PackageJson> {
    PackageJson::from_path(&project_root.join("package.json"))
        .map_err(miette::Report::new)
        .wrap_err("failed to read package.json")
}

/// Run a single named root-package lifecycle script. Wraps
/// `aube_scripts::run_root_script_by_name` with our standard
/// `modulesDir` + error mapping, and ensures the process-wide
/// `ScriptSettings` are populated from the project's npmrc/workspace
/// config before we spawn. The manifest is passed in so callers that
/// chain multiple hooks can share a single parse.
pub(crate) async fn run_root_lifecycle_script(
    project_root: &Path,
    manifest: &PackageJson,
    script_name: &str,
) -> miette::Result<()> {
    if !manifest.scripts.contains_key(script_name) {
        return Ok(());
    }
    super::configure_script_settings_for_cwd(project_root, Some("pack"))?;
    let modules_dir_name = super::resolve_modules_dir_name_for_cwd(project_root);
    tracing::debug!("lifecycle: {script_name}");
    aube_scripts::run_root_script_by_name(project_root, &modules_dir_name, manifest, script_name)
        .await
        .map_err(|e| miette!("root `{script_name}` script failed: {e}"))?;
    Ok(())
}

/// Sanitize a package name into the tarball filename stem.
/// `@scope/foo` -> `scope-foo`, `foo` -> `foo`.
pub(crate) fn tarball_filename(name: &str, version: &str) -> String {
    let sanitized = name.replace('@', "").replace('/', "-");
    format!("{sanitized}-{version}.tgz")
}

#[derive(Debug)]
struct PackedFile {
    /// Absolute path on disk.
    abs: PathBuf,
    /// Forward-slash path relative to the project root. Becomes the
    /// tarball entry name (prefixed with `package/` at write time).
    rel: String,
    /// File size captured while collecting the packlist.
    size: u64,
    /// Optional in-memory content used when publish normalizes package.json.
    data_override: Option<Vec<u8>>,
}

/// In-memory result of packing a project. Reused by `aube publish`,
/// which needs the tarball bytes to hash and upload.
#[derive(Debug)]
pub(crate) struct BuiltArchive {
    pub name: String,
    pub version: String,
    /// Default tarball filename (`<sanitized-name>-<version>.tgz`).
    pub filename: String,
    /// Forward-slash project-relative paths included in the tarball.
    pub files: Vec<String>,
    /// Sum of unpacked file sizes.
    pub unpacked_size: u64,
    /// Gzipped tar bytes, ready to write to disk or POST to a registry.
    pub tarball: Vec<u8>,
}

/// Build a tarball for `project_dir` entirely in memory. Shared between
/// `aube pack` (writes the bytes to disk) and the forthcoming
/// `aube publish` (hashes and uploads them).
pub(crate) fn build_archive(project_dir: &Path) -> miette::Result<BuiltArchive> {
    build_archive_with_package_json(project_dir, None)
}

pub(crate) fn build_archive_with_package_json(
    project_dir: &Path,
    package_json: Option<Vec<u8>>,
) -> miette::Result<BuiltArchive> {
    let manifest = PackageJson::from_path(&project_dir.join("package.json"))
        .map_err(miette::Report::new)
        .wrap_err("failed to read package.json")?;
    let name = manifest
        .name
        .as_deref()
        .ok_or_else(|| miette!("pack: package.json has no `name` field"))?
        .to_string();
    let version = manifest
        .version
        .as_deref()
        .ok_or_else(|| miette!("pack: package.json has no `version` field"))?
        .to_string();

    let mut files = collect_files(project_dir, &manifest)?;
    if let Some(package_json) = package_json
        && let Some(file) = files.iter_mut().find(|f| f.rel == "package.json")
    {
        file.size = package_json.len() as u64;
        file.data_override = Some(package_json);
    }
    let filename = tarball_filename(&name, &version);
    let unpacked_size = files.iter().map(|f| f.size).sum();

    let mut buf: Vec<u8> = Vec::new();
    write_tarball(&files, &mut buf)?;

    Ok(BuiltArchive {
        name,
        version,
        filename,
        files: files.into_iter().map(|f| f.rel).collect(),
        unpacked_size,
        tarball: buf,
    })
}

/// Public-to-crate wrapper around `collect_files` that returns the
/// selected `(abs, forward-slash rel)` entries. Used by the injected
/// workspace dep materializer (`commands::inject`) so a packed snapshot
/// of a workspace package can be copied into `.aube/` without
/// round-tripping through an actual tarball.
pub(crate) fn collect_package_files(
    project_dir: &Path,
    manifest: &PackageJson,
) -> miette::Result<Vec<(PathBuf, String)>> {
    let files = collect_files(project_dir, manifest)?;
    Ok(files.into_iter().map(|f| (f.abs, f.rel)).collect())
}

fn collect_files(project_dir: &Path, manifest: &PackageJson) -> miette::Result<Vec<PackedFile>> {
    let mut keep: BTreeSet<String> = BTreeSet::new();

    if let Some(files_field) = manifest.extra.get("files")
        && let Some(arr) = files_field.as_array()
    {
        for entry in arr {
            let Some(pattern) = entry.as_str() else {
                continue;
            };
            expand_files_glob(project_dir, pattern, &mut keep)?;
        }
    } else {
        walk_with_ignores(project_dir, &mut keep)?;
    }

    // Always-on entries: npm/pnpm include these regardless of `files`
    // or ignore rules. Skip silently when missing.
    for always in always_included_files(project_dir, manifest) {
        if project_dir.join(&always).is_file() {
            keep.insert(always);
        }
    }

    // `package.json` is mandatory.
    if !keep.contains("package.json") && project_dir.join("package.json").is_file() {
        keep.insert("package.json".to_string());
    }

    let mut out: Vec<PackedFile> = Vec::with_capacity(keep.len());
    for rel in keep {
        let abs = project_dir.join(&rel);
        let size = std::fs::metadata(&abs)
            .into_diagnostic()
            .wrap_err_with(|| format!("pack: stat {}", abs.display()))?
            .len();
        out.push(PackedFile {
            abs,
            rel,
            size,
            data_override: None,
        });
    }
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok(out)
}

/// Expand a glob from the `files` field, rooted at `project_dir`. If the
/// pattern matches a directory, every non-ignored file underneath is added.
fn expand_files_glob(
    project_dir: &Path,
    pattern: &str,
    keep: &mut BTreeSet<String>,
) -> miette::Result<()> {
    let full_pattern = project_dir.join(pattern);
    let matches = glob::glob(&full_pattern.to_string_lossy())
        .map_err(|e| miette!("pack: invalid files glob {pattern:?}: {e}"))?;
    for entry in matches {
        let path = entry.map_err(|e| miette!("pack: glob walk failed: {e}"))?;
        let Ok(rel) = path.strip_prefix(project_dir) else {
            continue;
        };
        if path.is_dir() {
            let mut sub: BTreeSet<String> = BTreeSet::new();
            walk_subdir(project_dir, &path, &mut sub)?;
            keep.extend(sub);
        } else if path.is_file() {
            keep.insert(normalize_rel(rel));
        }
    }
    Ok(())
}

/// Recursive walk of `dir` collecting relative paths, respecting the
/// standard npm ignore list. Used both for the no-`files` default and
/// when a `files` entry points to a directory.
fn walk_subdir(project_dir: &Path, dir: &Path, out: &mut BTreeSet<String>) -> miette::Result<()> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let entries = match std::fs::read_dir(&current) {
            Ok(it) => it,
            Err(e) => {
                return Err(miette!("pack: read_dir({}) failed: {e}", current.display()));
            }
        };
        for entry in entries {
            let entry = entry
                .into_diagnostic()
                .wrap_err_with(|| format!("failed to read {}", current.display()))?;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if is_npm_ignored(&name) {
                continue;
            }
            // `file_type()` doesn't follow symlinks, so symlinked
            // directories are treated as leaves — avoids infinite loops
            // on circular links like `src/self -> src/`.
            let ft = entry
                .file_type()
                .into_diagnostic()
                .wrap_err_with(|| format!("failed to stat {}", path.display()))?;
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file()
                && let Ok(rel) = path.strip_prefix(project_dir)
            {
                out.insert(normalize_rel(rel));
            }
        }
    }
    Ok(())
}

/// Top-level walker used when `files` is absent. Combines the secret/junk
/// blocklist (`is_npm_ignored`) and the root-level `.npmignore` /
/// `.gitignore` matcher into one `filter_entry` so `WalkBuilder` prunes
/// fully-ignored subtrees during traversal — a checked-in `dist/` with
/// thousands of files is skipped without stat'ing them.
///
/// `WalkBuilder::add_ignore` is intentionally NOT used: it builds an
/// internal matcher with an empty root, which mishandles anchored
/// (`/dist/`) and prefix-pathed (`build/*.js`) patterns. Driving our own
/// `Gitignore` rooted at `project_dir` keeps full gitignore semantics.
fn walk_with_ignores(project_dir: &Path, out: &mut BTreeSet<String>) -> miette::Result<()> {
    let matcher = build_root_ignore(project_dir);
    let mut builder = WalkBuilder::new(project_dir);
    // Disable every ambient source (parent .gitignore, global excludes,
    // hidden-file skipping). npm includes dotfiles by default; the
    // secret/junk blocklist below is the actual gatekeeper for those.
    builder
        .hidden(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .ignore(false)
        .parents(false)
        .require_git(false)
        .follow_links(false);

    builder.filter_entry(move |entry| {
        // Don't filter the project root itself — its file_name is the
        // user's CWD basename and could spuriously match the blocklist
        // (e.g. a project literally named `node_modules`).
        if entry.depth() == 0 {
            return true;
        }
        if is_npm_ignored(&entry.file_name().to_string_lossy()) {
            return false;
        }
        let is_dir = entry.file_type().is_some_and(|ft| ft.is_dir());
        !matcher.matched(entry.path(), is_dir).is_ignore()
    });

    for result in builder.build() {
        let entry = result.map_err(|e| miette!("pack: walk failed: {e}"))?;
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(project_dir) else {
            continue;
        };
        out.insert(normalize_rel(rel));
    }
    Ok(())
}

/// Build a gitignore matcher rooted at `project_dir` from `.npmignore`
/// (preferred) or `.gitignore`. Returns an empty matcher when neither
/// file exists or the chosen file fails to parse.
fn build_root_ignore(project_dir: &Path) -> Gitignore {
    let npmignore = project_dir.join(".npmignore");
    let chosen = if npmignore.is_file() {
        Some(npmignore)
    } else {
        let gitignore = project_dir.join(".gitignore");
        gitignore.is_file().then_some(gitignore)
    };
    let mut builder = GitignoreBuilder::new(project_dir);
    if let Some(path) = chosen
        && let Some(err) = builder.add(&path)
    {
        // npm/pnpm tolerate malformed lines silently; do the same.
        tracing::debug!("pack: {} parse error: {err}", path.display());
    }
    builder.build().unwrap_or_else(|err| {
        tracing::debug!("pack: ignore matcher build failed: {err}");
        Gitignore::empty()
    })
}

fn is_npm_ignored(name: &str) -> bool {
    // Secret-file blocklist. User runs `aube publish` in a dir with
    // `.env`, SSH keys, AWS creds. No `files` allowlist, no
    // `.npmignore`. Without this list, those files ship to the
    // registry. Real footgun, real incidents, npm/pnpm both ship a
    // similar list. Users can still override via `files` field if
    // they really want to publish one of these (nobody should).
    if name == aube_util::embedder().lockfile_basename {
        return true;
    }
    if matches!(
        name,
        ".git"
            | ".svn"
            | ".hg"
            | "CVS"
            | ".DS_Store"
            | "node_modules"
            | "npm-debug.log"
            | ".npmrc"
            | ".npmignore"
            | ".gitignore"
            | "package-lock.json"
            | "yarn.lock"
            | "pnpm-lock.yaml"
            | "bun.lock"
            | ".env"
            | ".envrc"
            | ".ssh"
            | ".aws"
            | ".gnupg"
            | "id_rsa"
            | "id_dsa"
            | "id_ecdsa"
            | "id_ed25519"
    ) {
        return true;
    }
    if name.ends_with(".tgz") || name.ends_with(".swp") {
        return true;
    }
    // `.env.local`, `.env.production`, etc.
    if name.starts_with(".env.") {
        return true;
    }
    // Private keys / certs users routinely keep alongside source.
    if name.ends_with(".pem") || name.ends_with(".key") || name.ends_with(".p12") {
        return true;
    }
    false
}

fn always_included_files(project_dir: &Path, manifest: &PackageJson) -> Vec<String> {
    let mut out = vec!["package.json".to_string()];

    // README, LICENSE, LICENCE — any extension. CHANGELOG is intentionally
    // omitted to match npm's behavior (npm/npm-packlist#61).
    let Ok(entries) = std::fs::read_dir(project_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let upper = name.to_uppercase();
        let stem = upper
            .rsplit_once('.')
            .map(|(s, _)| s.to_string())
            .unwrap_or(upper);
        if matches!(stem.as_str(), "README" | "LICENSE" | "LICENCE") {
            out.push(name);
        }
    }

    // `main` field entry. Strip any leading `./` so the path matches
    // the walker's normalized form — otherwise `"./index.js"` becomes a
    // second BTreeSet key alongside `"index.js"` and the file is packed
    // twice (as `package/./index.js` and `package/index.js`).
    if let Some(main) = manifest.extra.get("main").and_then(|v| v.as_str()) {
        let cleaned = main.trim_start_matches("./");
        if !cleaned.is_empty() {
            out.push(cleaned.to_string());
        }
    }

    out
}

fn normalize_rel(rel: &Path) -> String {
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join("/")
}

/// Write every packed file into `writer` as a gzipped tar with
/// `package/<rel>` entries. All mtimes are pinned for reproducibility,
/// matching pnpm/npm.
fn write_tarball<W: Write>(files: &[PackedFile], writer: W) -> miette::Result<()> {
    const REPRODUCIBLE_MTIME: u64 = 499_162_500; // 1985-10-26 08:15:00 UTC

    let gz = GzEncoder::new(writer, Compression::default());
    let mut builder = tar::Builder::new(gz);

    for packed in files {
        let data = match &packed.data_override {
            Some(data) => data.clone(),
            None => std::fs::read(&packed.abs)
                .into_diagnostic()
                .wrap_err_with(|| format!("failed to read {}", packed.abs.display()))?,
        };
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(file_mode(&packed.abs));
        header.set_mtime(REPRODUCIBLE_MTIME);
        header.set_uid(0);
        header.set_gid(0);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_cksum();
        let entry_name = format!("package/{}", packed.rel);
        builder
            .append_data(&mut header, &entry_name, data.as_slice())
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to add {entry_name} to tarball"))?;
    }

    let gz = builder
        .into_inner()
        .into_diagnostic()
        .wrap_err("failed to finalize tar builder")?;
    gz.finish()
        .into_diagnostic()
        .wrap_err("failed to finalize gzip stream")?;
    Ok(())
}

/// Preserve the source file's executable bit so packages with `bin`
/// scripts stay runnable after install: the store reads exec-ness from
/// the tarball header (`mode & 0o111`), and the linker only chmods the
/// symlink target when that bit is set.
fn file_mode(path: &Path) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::metadata(path)
            .map(|m| m.permissions().mode())
            .unwrap_or(0o644);
        if perms & 0o111 != 0 { 0o755 } else { 0o644 }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        0o644
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tarball_filename_unscoped() {
        assert_eq!(tarball_filename("lodash", "4.17.21"), "lodash-4.17.21.tgz");
    }

    #[test]
    fn tarball_filename_scoped() {
        assert_eq!(
            tarball_filename("@babel/core", "7.24.0"),
            "babel-core-7.24.0.tgz"
        );
    }

    #[test]
    fn ignores_standard_dotfiles_and_lockfiles() {
        assert!(is_npm_ignored("node_modules"));
        assert!(is_npm_ignored(".git"));
        assert!(is_npm_ignored("pnpm-lock.yaml"));
        assert!(is_npm_ignored("aube-lock.yaml"));
        assert!(is_npm_ignored("some-package-1.0.0.tgz"));
        assert!(!is_npm_ignored("src"));
    }

    fn write_tree(root: &Path, files: &[&str]) {
        for rel in files {
            let p = root.join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&p, b"x").unwrap();
        }
    }

    fn collected(root: &Path) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        walk_with_ignores(root, &mut out).unwrap();
        out
    }

    #[test]
    fn npmignore_directory_pattern_excludes_subtree() {
        let dir = tempfile::tempdir().unwrap();
        write_tree(
            dir.path(),
            &[
                "src/main.js",
                "dist/bundle.js",
                "dist/nested/x.js",
                "keep.js",
            ],
        );
        std::fs::write(dir.path().join(".npmignore"), "dist/\n").unwrap();
        let got = collected(dir.path());
        assert!(got.contains("src/main.js"));
        assert!(got.contains("keep.js"));
        assert!(!got.contains("dist/bundle.js"));
        assert!(!got.contains("dist/nested/x.js"));
    }

    #[test]
    fn npmignore_glob_pattern() {
        let dir = tempfile::tempdir().unwrap();
        write_tree(dir.path(), &["a.log", "nested/b.log", "keep.txt"]);
        std::fs::write(dir.path().join(".npmignore"), "*.log\n").unwrap();
        let got = collected(dir.path());
        assert!(got.contains("keep.txt"));
        assert!(!got.contains("a.log"));
        assert!(!got.contains("nested/b.log"));
    }

    #[test]
    fn npmignore_negation_re_includes() {
        let dir = tempfile::tempdir().unwrap();
        write_tree(dir.path(), &["build/a.js", "build/keep.js", "build/c.js"]);
        std::fs::write(
            dir.path().join(".npmignore"),
            "build/*.js\n!build/keep.js\n",
        )
        .unwrap();
        let got = collected(dir.path());
        assert!(got.contains("build/keep.js"));
        assert!(!got.contains("build/a.js"));
        assert!(!got.contains("build/c.js"));
    }

    #[test]
    fn npmignore_anchored_pattern_only_matches_root() {
        let dir = tempfile::tempdir().unwrap();
        write_tree(dir.path(), &["dist/a.js", "vendor/dist/b.js"]);
        std::fs::write(dir.path().join(".npmignore"), "/dist/\n").unwrap();
        let got = collected(dir.path());
        assert!(!got.contains("dist/a.js"));
        assert!(got.contains("vendor/dist/b.js"));
    }

    #[test]
    fn npmignore_double_star_matches_any_depth() {
        let dir = tempfile::tempdir().unwrap();
        write_tree(
            dir.path(),
            &["a/__tests__/x.js", "a/b/__tests__/y.js", "a/keep.js"],
        );
        std::fs::write(dir.path().join(".npmignore"), "**/__tests__/\n").unwrap();
        let got = collected(dir.path());
        assert!(got.contains("a/keep.js"));
        assert!(!got.contains("a/__tests__/x.js"));
        assert!(!got.contains("a/b/__tests__/y.js"));
    }

    #[test]
    fn gitignore_used_when_no_npmignore() {
        let dir = tempfile::tempdir().unwrap();
        write_tree(dir.path(), &["a.tmp", "keep.js"]);
        std::fs::write(dir.path().join(".gitignore"), "*.tmp\n").unwrap();
        let got = collected(dir.path());
        assert!(got.contains("keep.js"));
        assert!(!got.contains("a.tmp"));
    }

    #[test]
    fn npmignore_takes_precedence_over_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        write_tree(dir.path(), &["a.tmp", "b.bak"]);
        // .gitignore would block *.bak; .npmignore should win and only block *.tmp.
        std::fs::write(dir.path().join(".gitignore"), "*.bak\n").unwrap();
        std::fs::write(dir.path().join(".npmignore"), "*.tmp\n").unwrap();
        let got = collected(dir.path());
        assert!(got.contains("b.bak"));
        assert!(!got.contains("a.tmp"));
    }
}
