//! `--include` / `--exclude` — verbatim asset embedding.
//!
//! Assets ride the SAME payload region as the bundle output rather than a region
//! of their own, which is what makes the runtime half free: the launcher's
//! `ensure_app` already creates parent directories, already refuses an escaping
//! name, and `app_sha256` already hashes the name, length, and bytes of every
//! entry — so a changed asset set re-keys the extraction dir with no new
//! invalidation logic and no container-format change.
//!
//! ANCHORING — the decision that makes the obvious user code work. The extracted
//! app dir mirrors the source tree, rooted at the deepest directory containing
//! both the entry and every `--include`d path. Bundle output is written under the
//! entry's own path relative to that root, so the relative geometry between the
//! entry module and its assets survives compilation exactly: `new URL("./d.json",
//! import.meta.url)` and `path.join(__dirname, "assets/x")` resolve to the same
//! files they did in the source tree. With no `--include` the root IS the entry's
//! directory, so a no-asset build lays out byte-identically to before this
//! existed.
//!
//! Assets are materialized as REAL FILES, deliberately, rather than served from a
//! virtual filesystem (VFS dismissed 2026-07-24, `wiki/commands/compile.md`): nub
//! extracts a real Node regardless of what it does with assets, so a VFS would buy
//! nothing while costing the fs-API fidelity — `createReadStream`, `fs.promises`,
//! native addons opening paths from C — that a real tree gives for free and that
//! Bun's and Deno's VFS holes are made of.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};

/// One file to embed verbatim.
#[derive(Debug)]
pub struct Asset {
    /// Where the file is on the build machine.
    pub source: PathBuf,
    /// The `/`-separated path it occupies inside the extracted app dir.
    pub rel: String,
}

/// Where the app dir is rooted and what lands in it.
#[derive(Debug)]
pub struct Layout {
    /// The entry's directory relative to the anchor, as a `/`-separated prefix
    /// (`""` when the entry sits at the anchor). Bundle output carries this
    /// prefix, so the prefix IS the observable form of where the anchor landed.
    pub entry_prefix: String,
    pub assets: Vec<Asset>,
}

impl Layout {
    /// Apply the entry prefix to a bundle-relative output name.
    pub fn bundle_path(&self, name: &str) -> String {
        if self.entry_prefix.is_empty() {
            name.to_string()
        } else {
            format!("{}/{name}", self.entry_prefix)
        }
    }
}

/// Resolve `--include` / `--exclude` into the payload layout.
///
/// `cwd` anchors relative patterns (the user typed them at a shell prompt, so
/// they mean what the shell would have meant), while `entry_dir` anchors the
/// module geometry.
pub fn plan(
    entry_dir: &Path,
    cwd: &Path,
    include: &[String],
    exclude: &[String],
) -> Result<Layout> {
    let excludes = exclude
        .iter()
        .map(|token| Matcher::parse(cwd, token))
        .collect::<Result<Vec<_>>>()?;

    // Roots first: the anchor depends on every include, so nothing can be made
    // relative until all of them are known.
    let mut roots = Vec::new();
    for token in include {
        roots.push(Matcher::parse(cwd, token)?);
    }

    let mut anchor = entry_dir.to_path_buf();
    for m in &roots {
        anchor = common_ancestor(&anchor, m.anchor_dir()).with_context(|| {
            format!(
                "--include {:?} shares no common directory with the entry — an included \
                 path must live on the same filesystem root as the entry it ships with",
                m.token
            )
        })?;
    }

    // A BTreeMap both dedups an overlapping include pair (`--include assets \
    // --include assets/x.png`) and yields a deterministic payload order, which
    // `app_sha256` hashes — so the same inputs always produce the same cache key.
    let mut collected: BTreeMap<String, PathBuf> = BTreeMap::new();
    for m in &roots {
        let mut hits = 0usize;
        for file in m.walk()? {
            if excludes.iter().any(|e| e.matches(&file)) {
                continue;
            }
            let rel = relative_slash(&anchor, &file)?;
            collected.insert(rel, file);
            hits += 1;
        }
        // An include that ships nothing is a typo or a stale path, and staying
        // silent means discovering it when the binary fails on a user's machine.
        // (`--exclude` is deliberately the opposite — see `Matcher::matches`.)
        if hits == 0 {
            bail!(
                "--include {:?} matched no files{}",
                m.token,
                if m.pattern.is_some() {
                    ""
                } else {
                    " (the path does not exist)"
                }
            );
        }
    }

    let entry_prefix = relative_slash(&anchor, entry_dir).unwrap_or_default();
    Ok(Layout {
        entry_prefix,
        assets: collected
            .into_iter()
            .map(|(rel, source)| Asset { source, rel })
            .collect(),
    })
}

/// One `--include`/`--exclude` token, split into the literal directory it walks
/// and the optional glob it filters with.
struct Matcher {
    token: String,
    /// The deepest directory the pattern is rooted at — the literal leading
    /// components, absolute and normalized.
    root: PathBuf,
    /// The absolute `/`-separated glob, when the token had any wildcards.
    pattern: Option<String>,
    /// Whether `root` is a plain file (no glob, and it exists as a file).
    is_file: bool,
}

impl Matcher {
    fn parse(cwd: &Path, token: &str) -> Result<Self> {
        let raw = Path::new(token);
        let abs = if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            cwd.join(raw)
        };

        // Split at the first component carrying a wildcard: everything before it
        // is a real directory we can walk; the whole pattern filters what we find.
        let mut root = PathBuf::new();
        let mut sawglob = false;
        for c in abs.components() {
            let literal = match c {
                Component::Normal(s) => !has_wildcard(&s.to_string_lossy()),
                _ => true,
            };
            if !literal {
                sawglob = true;
                break;
            }
            root.push(c);
        }

        let root = normalize(&root);
        if sawglob {
            return Ok(Self {
                token: token.to_string(),
                pattern: Some(to_slash(&normalize(&abs))),
                root,
                is_file: false,
            });
        }
        let is_file = root.is_file();
        Ok(Self {
            token: token.to_string(),
            root,
            pattern: None,
            is_file,
        })
    }

    /// The directory that participates in the anchor computation. A file
    /// contributes its parent — anchoring at the file itself would root the app
    /// dir inside a regular file.
    fn anchor_dir(&self) -> &Path {
        if self.is_file {
            self.root.parent().unwrap_or(&self.root)
        } else {
            &self.root
        }
    }

    /// Every file this token selects.
    fn walk(&self) -> Result<Vec<PathBuf>> {
        if self.is_file {
            return Ok(vec![self.root.clone()]);
        }
        let mut out = Vec::new();
        if self.root.is_dir() {
            walk_dir(&self.root, &mut out)?;
        }
        if let Some(pattern) = &self.pattern {
            out.retain(|p| glob_match::glob_match(pattern, &to_slash(p)));
        }
        Ok(out)
    }

    /// Whether this token prunes `path`. A literal directory prunes its whole
    /// subtree; a literal file prunes itself; a glob prunes what it matches.
    ///
    /// An `--exclude` that matches nothing is SILENT (decided 2026-07-23,
    /// matching Deno) — the residual risk being a typo'd `--exclude ./secrets`
    /// that then ships the secrets.
    fn matches(&self, path: &Path) -> bool {
        match &self.pattern {
            Some(p) => glob_match::glob_match(p, &to_slash(path)),
            None => path == self.root || path.starts_with(&self.root),
        }
    }
}

fn has_wildcard(s: &str) -> bool {
    s.contains(['*', '?', '[', '{'])
}

fn walk_dir(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
        let path = entry.path();
        // `file_type` does not follow symlinks, so a symlinked directory is never
        // descended into — that is what keeps a cyclic link out of the walk. A
        // symlinked FILE still ships: the read below follows it and embeds the
        // target's bytes, which is what "verbatim" means for a link.
        let ft = entry
            .file_type()
            .with_context(|| format!("stat {}", path.display()))?;
        if ft.is_dir() {
            walk_dir(&path, out)?;
        } else if ft.is_file() || (ft.is_symlink() && path.is_file()) {
            out.push(path);
        }
    }
    Ok(())
}

/// Lexical `..`/`.` removal. Not `fs::canonicalize`: a glob's literal prefix may
/// name a directory that does not exist, and resolving symlinks here would make
/// the anchor jump outside the project tree the user is describing.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(Component::ParentDir);
                }
            }
            other => out.push(other),
        }
    }
    out
}

fn common_ancestor(a: &Path, b: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    let mut bs = b.components();
    for ac in a.components() {
        match bs.next() {
            Some(bc) if bc == ac => out.push(ac),
            _ => break,
        }
    }
    // Component-wise equality can agree on nothing at all (two different Windows
    // drive prefixes), which would root the app dir at "".
    if out.as_os_str().is_empty() {
        None
    } else {
        Some(out)
    }
}

/// `path` expressed relative to `root`, `/`-separated — the payload's name form,
/// which must be identical whichever platform compiled it.
fn relative_slash(root: &Path, path: &Path) -> Result<String> {
    let rel = path.strip_prefix(root).with_context(|| {
        format!(
            "{} is not inside the compile root {}",
            path.display(),
            root.display()
        )
    })?;
    Ok(to_slash(rel))
}

fn to_slash(path: &Path) -> String {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "nub-assets-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("src")).unwrap();
        fs::create_dir_all(d.join("assets/img")).unwrap();
        fs::write(d.join("src/main.js"), "x").unwrap();
        fs::write(d.join("src/local.json"), "{}").unwrap();
        fs::write(d.join("assets/data.json"), "{}").unwrap();
        fs::write(d.join("assets/img/logo.png"), "png").unwrap();
        fs::write(d.join("assets/img/tmp.png"), "png").unwrap();
        // Normalized so `strip_prefix` lines up with the normalized include roots
        // on macOS, where temp_dir() is itself a symlink (/var → /private/var).
        fs::canonicalize(&d).unwrap()
    }

    #[test]
    fn no_includes_leaves_the_entry_at_the_root() {
        let d = fixture("noinc");
        let plan = plan(&d.join("src"), &d, &[], &[]).unwrap();
        assert_eq!(plan.entry_prefix, "");
        assert_eq!(plan.bundle_path("main.js"), "main.js");
        assert!(plan.assets.is_empty());
    }

    #[test]
    fn an_out_of_tree_include_pushes_the_entry_down_so_relative_paths_survive() {
        // Entry at <root>/src, assets at <root>/assets: the app dir has to mirror
        // BOTH, or the source's `../assets/data.json` cannot resolve once compiled.
        let d = fixture("outtree");
        let plan = plan(&d.join("src"), &d, &["assets".into()], &[]).unwrap();
        assert_eq!(plan.entry_prefix, "src");
        assert_eq!(plan.bundle_path("main.js"), "src/main.js");
        let rels: Vec<_> = plan.assets.iter().map(|a| a.rel.as_str()).collect();
        assert_eq!(
            rels,
            [
                "assets/data.json",
                "assets/img/logo.png",
                "assets/img/tmp.png"
            ]
        );
    }

    #[test]
    fn an_include_beside_the_entry_keeps_the_entry_flat() {
        let d = fixture("intree");
        let plan = plan(&d.join("src"), &d, &["src/local.json".into()], &[]).unwrap();
        assert_eq!(plan.entry_prefix, "");
        assert_eq!(plan.assets.len(), 1);
        assert_eq!(plan.assets[0].rel, "local.json");
    }

    #[test]
    fn globs_select_across_directories() {
        let d = fixture("glob");
        let plan = plan(&d.join("src"), &d, &["assets/**/*.png".into()], &[]).unwrap();
        let rels: Vec<_> = plan.assets.iter().map(|a| a.rel.as_str()).collect();
        assert_eq!(rels, ["assets/img/logo.png", "assets/img/tmp.png"]);
    }

    #[test]
    fn exclude_prunes_within_an_included_tree_by_path_and_by_glob() {
        let d = fixture("excl");
        let by_path = plan(
            &d.join("src"),
            &d,
            &["assets".into()],
            &["assets/img".into()],
        )
        .unwrap();
        assert_eq!(
            by_path.assets.iter().map(|a| &a.rel).collect::<Vec<_>>(),
            ["assets/data.json"]
        );

        let by_glob = plan(
            &d.join("src"),
            &d,
            &["assets".into()],
            &["**/tmp.png".into()],
        )
        .unwrap();
        assert_eq!(
            by_glob.assets.iter().map(|a| &a.rel).collect::<Vec<_>>(),
            ["assets/data.json", "assets/img/logo.png"]
        );
    }

    #[test]
    fn an_exclude_matching_nothing_is_silent_but_an_empty_include_is_an_error() {
        let d = fixture("unmatched");
        // Decided 2026-07-23: an unmatched --exclude is silently ignored…
        let ok = plan(
            &d.join("src"),
            &d,
            &["assets/data.json".into()],
            &["nope/absent.txt".into(), "**/*.never".into()],
        )
        .unwrap();
        assert_eq!(ok.assets.len(), 1);

        // …while an --include that ships nothing is not, in either spelling.
        for bad in ["assets/missing.json", "assets/**/*.svg"] {
            let err = plan(&d.join("src"), &d, &[bad.to_string()], &[]).unwrap_err();
            assert!(
                format!("{err:#}").contains("matched no files"),
                "{bad} should report an empty include, got: {err:#}"
            );
        }
    }

    #[test]
    fn overlapping_includes_ship_each_file_once() {
        let d = fixture("overlap");
        let plan = plan(
            &d.join("src"),
            &d,
            &["assets".into(), "assets/img/logo.png".into()],
            &[],
        )
        .unwrap();
        assert_eq!(plan.assets.len(), 3, "logo.png must not be embedded twice");
    }

    #[test]
    fn an_exclude_that_empties_an_include_still_reports_it() {
        let d = fixture("emptied");
        let err = plan(
            &d.join("src"),
            &d,
            &["assets/img".into()],
            &["assets/img".into()],
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("matched no files"), "{err:#}");
    }
}
