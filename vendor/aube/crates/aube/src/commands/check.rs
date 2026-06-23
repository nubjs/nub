//! `aube check` — verify `node_modules/` symlink tree integrity.
//!
//! Walks every package materialized under `node_modules/.aube/<cell>/node_modules/`,
//! reads its `package.json`, and confirms that every declared `dependencies`
//! entry has a corresponding sibling inside the same cell directory — the
//! shape Node's module resolver expects when walking up from a package's
//! location. Missing entries are reported as broken links.
//!
//! `peerDependencies` are out of scope — `aube peers check` validates
//! those against the lockfile.  `optionalDependencies` that the platform
//! filter legitimately skipped would look broken here, so we scope the
//! check to `dependencies` only. `devDependencies` don't ship inside
//! non-root packages' manifests, so they never appear in cell lookups.
//!
//! Exits with status 1 when at least one broken link is found, so it's
//! CI-friendly as a post-install gate.

use clap::Args;
use miette::IntoDiagnostic;
use std::collections::BTreeMap;
use std::path::Path;

pub const AFTER_LONG_HELP: &str = "\
Examples:

  $ aube check
  node_modules symlink tree is consistent (checked 248 packages).

  # With issues
  $ aube check
  2 broken dependency links found:

    vscode-languageserver@9.0.1
      ✕ cannot resolve: vscode-languageserver-protocol@3.17.5

    vscode-languageserver-protocol@3.17.5
      ✕ cannot resolve: vscode-languageserver-types@3.17.5
      ✕ cannot resolve: vscode-jsonrpc@8.2.1

  # Machine-readable
  $ aube check --json
";

#[derive(Debug, Args)]
pub struct CheckArgs {
    /// Emit a JSON report instead of the human-readable list.
    #[arg(long)]
    pub json: bool,
}

pub async fn run(args: CheckArgs) -> miette::Result<Option<i32>> {
    // `project_root_or_cwd` falls back to the current directory when
    // nothing above it has a `package.json`. `run_report` is already a
    // no-op when `node_modules/.aube/` doesn't exist, so running
    // outside a project produces the same friendly `checked 0 packages`
    // output as running in a project that hasn't been installed yet —
    // no `miette`-formatted error just for living outside a package.
    let cwd = crate::dirs::project_root_or_cwd()?;
    let report = run_report(&cwd)?;

    if args.json {
        print_json(&report);
    } else {
        print_human(&report);
    }

    if !report.issues.is_empty() {
        // Exit 1 when the store check found broken links. Return the code
        // for the binary's single `std::process::exit` rather than
        // terminating here, keeping the command embed-safe.
        return Ok(Some(1));
    }
    Ok(None)
}

/// Result of scanning the virtual store.
#[derive(Debug, Default)]
pub(crate) struct CheckReport {
    /// Number of package manifests we successfully inspected.
    pub(crate) checked: usize,
    /// Broken dependency links, stable-sorted.
    pub(crate) issues: Vec<BrokenLink>,
}

#[derive(Debug, Clone)]
pub(crate) struct BrokenLink {
    pub(crate) consumer_name: String,
    pub(crate) consumer_version: String,
    pub(crate) dep_name: String,
    pub(crate) dep_range: String,
    pub(crate) kind: BrokenKind,
}

/// Why a dependency was flagged as unresolvable. The distinction is
/// user-facing — a missing bundled dep points at a packaging/tarball
/// problem, whereas a missing sibling points at a link-tree problem.
#[derive(Debug, Clone, Copy)]
pub(crate) enum BrokenKind {
    /// No sibling entry at `<cell>/node_modules/<dep>` and the dep is
    /// not declared as bundled.
    Sibling,
    /// The consumer declares this dep via `bundledDependencies`, but
    /// the bundled copy is missing at `<pkg>/node_modules/<dep>` (the
    /// in-tarball location). Reported distinctly so a corrupted tarball
    /// or import is easier to diagnose than a generic resolve failure.
    Bundled,
}

/// Walk the virtual store under `cwd` and collect broken dependency links.
///
/// Reusable from `aube doctor` — pass `cwd` = project root. Returns an
/// empty report (0 checked, no issues) if the virtual store doesn't
/// exist yet (never installed, or hoisted layout without an isolated
/// tree); callers that want to treat that as an error do so themselves.
pub(crate) fn run_report(cwd: &Path) -> miette::Result<CheckReport> {
    let aube_dir = super::resolve_virtual_store_dir_for_cwd(cwd);
    let mut report = CheckReport::default();

    let Ok(cells) = std::fs::read_dir(&aube_dir) else {
        return Ok(report);
    };

    for entry in cells.flatten() {
        let cell_path = entry.path();
        if !cell_path.is_dir() {
            continue;
        }
        let cell_nm = cell_path.join("node_modules");
        if !cell_nm.is_dir() {
            continue;
        }
        scan_cell(&cell_nm, &mut report)?;
    }

    report.issues.sort_by(|a, b| {
        (&a.consumer_name, &a.consumer_version, &a.dep_name).cmp(&(
            &b.consumer_name,
            &b.consumer_version,
            &b.dep_name,
        ))
    });

    Ok(report)
}

/// Walk one `<cell>/node_modules/` directory. Each first-level entry is
/// either a real package directory (`foo/`), a scope directory
/// (`@scope/`) containing scoped packages, or a sibling-dep link that
/// points into another cell. Links are skipped — we only audit the
/// manifests that actually live in this cell, so each manifest is read
/// exactly once regardless of how many cells reference it.
fn scan_cell(cell_nm: &Path, report: &mut CheckReport) -> miette::Result<()> {
    for entry in std::fs::read_dir(cell_nm).into_diagnostic()?.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        let path = entry.path();
        if is_link_or_junction(&path) {
            continue;
        }
        if !path.is_dir() {
            continue;
        }
        if let Some(scope) = name_str.strip_prefix('@') {
            let Ok(inner) = std::fs::read_dir(&path) else {
                continue;
            };
            for scoped in inner.flatten() {
                let sp = scoped.path();
                if is_link_or_junction(&sp) || !sp.is_dir() {
                    continue;
                }
                let Some(pkg) = scoped.file_name().to_str().map(|s| s.to_string()) else {
                    continue;
                };
                check_package(cell_nm, &sp, &format!("@{scope}/{pkg}"), report)?;
            }
        } else {
            check_package(cell_nm, &path, name_str, report)?;
        }
    }
    Ok(())
}

/// Is this path a link (POSIX symlink) or a Windows junction / reparse
/// point that aube's linker would write for a sibling dep? `Path::is_symlink`
/// alone is not enough on Windows — `aube_linker::create_dir_link` creates
/// NTFS junctions (tag `IO_REPARSE_TAG_MOUNT_POINT`), which Rust's
/// `FileType::is_symlink` excludes because it only matches
/// `IO_REPARSE_TAG_SYMLINK`. Without this guard, every junction-linked
/// sibling dep on Windows would be re-walked as a package owned by the
/// current cell and produce false broken-link reports.
fn is_link_or_junction(path: &Path) -> bool {
    let Ok(md) = std::fs::symlink_metadata(path) else {
        // Treat "can't read" as "skip" so we don't recurse into a
        // half-created or permission-restricted entry.
        return true;
    };
    if md.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        if md.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return true;
        }
    }
    false
}

/// Inspect one package's `package.json` and check that each declared
/// dependency has a sibling in `cell_nm/`.
fn check_package(
    cell_nm: &Path,
    pkg_dir: &Path,
    pkg_name_from_path: &str,
    report: &mut CheckReport,
) -> miette::Result<()> {
    let manifest_path = pkg_dir.join("package.json");
    let Ok(manifest) = aube_manifest::PackageJson::from_path(&manifest_path) else {
        // Packages that fail to parse their own manifest are an
        // install-layer problem, not a link-tree problem — skip and
        // let `aube install` surface the real error.
        return Ok(());
    };

    report.checked += 1;

    let consumer_name = manifest
        .name
        .clone()
        .unwrap_or_else(|| pkg_name_from_path.to_string());
    let consumer_version = manifest.version.clone().unwrap_or_default();

    let bundled = manifest
        .bundled_dependencies
        .as_ref()
        .map(|b| {
            b.names(&manifest.dependencies)
                .into_iter()
                .map(String::from)
                .collect::<std::collections::BTreeSet<_>>()
        })
        .unwrap_or_default();

    for (dep_name, dep_range) in &manifest.dependencies {
        // Entries that also appear in `optionalDependencies` are
        // legitimately absent when the platform filter dropped them
        // (e.g. `fsevents` on Linux). npm/pnpm treat the
        // `optionalDependencies` entry as authoritative when the same
        // name appears in both maps, so we mirror that here rather
        // than flagging a dep the install intentionally skipped.
        if manifest.optional_dependencies.contains_key(dep_name) {
            continue;
        }
        if bundled.contains(dep_name) {
            // Bundled deps ship inside the tarball at `<pkg>/node_modules/<dep>`;
            // they're a separate resolution path from sibling symlinks, so
            // report a distinct error when that copy is missing instead of
            // falling through to a generic "cannot resolve". A packaging
            // problem (corrupted tarball, bad publish) reads very
            // differently from a link-tree problem.
            if !pkg_dir.join("node_modules").join(dep_name).exists() {
                report.issues.push(BrokenLink {
                    consumer_name: consumer_name.clone(),
                    consumer_version: consumer_version.clone(),
                    dep_name: dep_name.clone(),
                    dep_range: dep_range.clone(),
                    kind: BrokenKind::Bundled,
                });
            }
            continue;
        }
        let sibling = cell_nm.join(dep_name);
        if sibling.exists() {
            continue;
        }
        report.issues.push(BrokenLink {
            consumer_name: consumer_name.clone(),
            consumer_version: consumer_version.clone(),
            dep_name: dep_name.clone(),
            dep_range: dep_range.clone(),
            kind: BrokenKind::Sibling,
        });
    }

    Ok(())
}

fn print_human(report: &CheckReport) {
    if report.issues.is_empty() {
        println!(
            "node_modules symlink tree is consistent (checked {} {}).",
            report.checked,
            if report.checked == 1 {
                "package"
            } else {
                "packages"
            }
        );
        return;
    }

    let mut groups: BTreeMap<(String, String), Vec<&BrokenLink>> = BTreeMap::new();
    for i in &report.issues {
        groups
            .entry((i.consumer_name.clone(), i.consumer_version.clone()))
            .or_default()
            .push(i);
    }

    println!(
        "{} broken dependency {} found:",
        report.issues.len(),
        if report.issues.len() == 1 {
            "link"
        } else {
            "links"
        }
    );
    println!();

    for ((name, version), group) in &groups {
        if version.is_empty() {
            println!("  {name}");
        } else {
            println!("  {name}@{version}");
        }
        for link in group {
            match link.kind {
                BrokenKind::Sibling => {
                    println!("    ✕ cannot resolve: {}@{}", link.dep_name, link.dep_range)
                }
                BrokenKind::Bundled => println!(
                    "    ✕ bundled dep missing from tarball: {}@{}",
                    link.dep_name, link.dep_range
                ),
            }
        }
        println!();
    }
}

fn print_json(report: &CheckReport) {
    let mut arr = Vec::with_capacity(report.issues.len());
    for i in &report.issues {
        let mut obj = serde_json::Map::new();
        // `consumer_version` falls back to "" when the package's own
        // manifest omits the field; in that case the human output
        // drops the `@` separator and so does this.
        let consumer = if i.consumer_version.is_empty() {
            i.consumer_name.clone()
        } else {
            format!("{}@{}", i.consumer_name, i.consumer_version)
        };
        obj.insert("consumer".into(), consumer.into());
        obj.insert("name".into(), i.dep_name.clone().into());
        obj.insert("range".into(), i.dep_range.clone().into());
        let kind = match i.kind {
            BrokenKind::Sibling => "sibling",
            BrokenKind::Bundled => "bundled",
        };
        obj.insert("kind".into(), kind.into());
        arr.push(serde_json::Value::Object(obj));
    }
    let mut root = serde_json::Map::new();
    root.insert("checked".into(), report.checked.into());
    root.insert("issues".into(), serde_json::Value::Array(arr));
    let json = serde_json::to_string_pretty(&serde_json::Value::Object(root))
        .unwrap_or_else(|_| "{}".to_string());
    println!("{json}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn write_pkg(dir: &Path, name: &str, version: &str, deps: &[(&str, &str)]) {
        std::fs::create_dir_all(dir).unwrap();
        let mut deps_obj = serde_json::Map::new();
        for (n, v) in deps {
            deps_obj.insert(
                (*n).to_string(),
                serde_json::Value::String((*v).to_string()),
            );
        }
        let mut root = serde_json::Map::new();
        root.insert("name".into(), name.into());
        root.insert("version".into(), version.into());
        if !deps_obj.is_empty() {
            root.insert("dependencies".into(), serde_json::Value::Object(deps_obj));
        }
        std::fs::write(
            dir.join("package.json"),
            serde_json::to_string_pretty(&serde_json::Value::Object(root)).unwrap(),
        )
        .unwrap();
    }

    /// Build a minimal `.aube/` tree with two cells, `foo@1.0.0` and
    /// `bar@2.0.0`. `foo` declares a dep on `bar`. Caller hooks up the
    /// sibling (or deliberately omits it).
    ///
    /// `with_link` uses `aube_linker::create_dir_link` so the helper
    /// works on every host the crate itself targets — a plain
    /// `std::os::unix::fs::symlink` would silently skip Windows, and
    /// Windows needs the junction path that `create_dir_link` already
    /// picks for directory links. Kept close to what production
    /// actually writes so tests exercise the same resolution behavior.
    fn minimal_tree(with_link: bool) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_path_buf();
        std::fs::write(
            cwd.join("package.json"),
            r#"{"name":"root","version":"0.0.0"}"#,
        )
        .unwrap();

        let aube = cwd.join("node_modules").join(".aube");

        let foo_cell = aube.join("foo@1.0.0").join("node_modules");
        let foo_pkg = foo_cell.join("foo");
        write_pkg(&foo_pkg, "foo", "1.0.0", &[("bar", "^2.0.0")]);

        let bar_cell = aube.join("bar@2.0.0").join("node_modules");
        let bar_pkg = bar_cell.join("bar");
        write_pkg(&bar_pkg, "bar", "2.0.0", &[]);

        if with_link {
            aube_linker::create_dir_link(&bar_pkg, &foo_cell.join("bar")).unwrap();
        }

        (tmp, cwd)
    }

    #[test]
    fn consistent_tree_reports_zero_issues() {
        let (_tmp, cwd) = minimal_tree(true);
        let report = run_report(&cwd).unwrap();
        assert_eq!(report.checked, 2);
        assert!(report.issues.is_empty(), "{:?}", report.issues);
    }

    #[test]
    fn missing_sibling_is_reported() {
        let (_tmp, cwd) = minimal_tree(false);
        let report = run_report(&cwd).unwrap();
        assert_eq!(report.checked, 2);
        assert_eq!(report.issues.len(), 1);
        let issue = &report.issues[0];
        assert_eq!(issue.consumer_name, "foo");
        assert_eq!(issue.consumer_version, "1.0.0");
        assert_eq!(issue.dep_name, "bar");
        assert!(matches!(issue.kind, BrokenKind::Sibling));
    }

    #[test]
    fn dep_also_listed_as_optional_is_not_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_path_buf();
        std::fs::write(cwd.join("package.json"), r#"{"name":"root"}"#).unwrap();

        // foo@1.0.0 declares `fsevents` in both `dependencies` and
        // `optionalDependencies`. On a platform where the install
        // filter dropped it, there is no sibling — this should NOT
        // be flagged because optional-dep overlap wins.
        let aube = cwd.join("node_modules").join(".aube");
        let cell = aube.join("foo@1.0.0").join("node_modules");
        let pkg = cell.join("foo");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            r#"{
                "name": "foo",
                "version": "1.0.0",
                "dependencies": {"fsevents": "^2"},
                "optionalDependencies": {"fsevents": "^2"}
            }"#,
        )
        .unwrap();

        let report = run_report(&cwd).unwrap();
        assert_eq!(report.checked, 1);
        assert!(report.issues.is_empty(), "{:?}", report.issues);
    }

    #[test]
    fn missing_bundled_dep_is_reported_as_bundled() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_path_buf();
        std::fs::write(cwd.join("package.json"), r#"{"name":"root"}"#).unwrap();

        // foo@1.0.0 declares bar as both a regular dep and a
        // bundled dep, but ships no bundled copy. The sibling cell
        // is deliberately empty — this should be reported as a
        // `bundled` kind (packaging problem), not a generic sibling
        // miss.
        let aube = cwd.join("node_modules").join(".aube");
        let cell = aube.join("foo@1.0.0").join("node_modules");
        let pkg = cell.join("foo");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            r#"{
                "name": "foo",
                "version": "1.0.0",
                "dependencies": {"bar": "^1"},
                "bundledDependencies": ["bar"]
            }"#,
        )
        .unwrap();

        let report = run_report(&cwd).unwrap();
        assert_eq!(report.issues.len(), 1);
        assert!(matches!(report.issues[0].kind, BrokenKind::Bundled));
    }

    #[test]
    fn missing_virtual_store_is_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path();
        std::fs::write(cwd.join("package.json"), r#"{"name":"root"}"#).unwrap();
        let report = run_report(cwd).unwrap();
        assert_eq!(report.checked, 0);
        assert!(report.issues.is_empty());
    }

    #[test]
    fn scoped_package_is_walked() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_path_buf();
        std::fs::write(cwd.join("package.json"), r#"{"name":"root"}"#).unwrap();

        let aube = cwd.join("node_modules").join(".aube");
        let cell = aube.join("@scope+foo@1.0.0").join("node_modules");
        let pkg = cell.join("@scope").join("foo");
        write_pkg(&pkg, "@scope/foo", "1.0.0", &[("@other/missing", "^1")]);

        let report = run_report(&cwd).unwrap();
        assert_eq!(report.checked, 1);
        assert_eq!(report.issues.len(), 1);
        assert_eq!(report.issues[0].consumer_name, "@scope/foo");
        assert_eq!(report.issues[0].dep_name, "@other/missing");
    }
}
