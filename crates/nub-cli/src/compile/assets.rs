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
        let found = m.walk()?;
        let mut hits = 0usize;
        for file in &found {
            if excludes.iter().any(|e| e.matches(file)) {
                continue;
            }
            let rel = relative_slash(&anchor, file)?;
            collected.insert(rel, file.clone());
            hits += 1;
        }
        // An include that ships nothing is a typo or a stale path, and staying
        // silent means discovering it when the binary fails on a user's machine.
        // (`--exclude` is deliberately the opposite — see `Matcher::matches`.)
        // The three causes need three different fixes, so name the one that applied.
        if hits == 0 {
            let hint = if !found.is_empty() {
                " (every file it matched was excluded)"
            } else if m.root.exists() {
                ""
            } else {
                " (the path does not exist)"
            };
            bail!("--include {:?} matched no files{hint}", m.token);
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
    /// The deepest directory the pattern is rooted at, with its ANCESTORS in the
    /// entry's namespace and its final component exactly as the user wrote it.
    /// This is what anchoring, naming, and walking use.
    root: PathBuf,
    /// `root` with symlinks fully resolved — a comparison key ONLY, never a name.
    /// Anchoring and matching genuinely want different namespaces: a name must
    /// mirror the tree the user described (so a symlinked `public/` stays
    /// `public/`), while an `--exclude` must still recognize the file an
    /// `--include` walked even when one of them spelled it through a link.
    canonical_root: PathBuf,
    /// The glob tail RELATIVE to `root`, when the token had any wildcards.
    /// Relative, not absolute, so the build machine's own directory names can
    /// never be read as glob syntax — a project living in `~/proj[1]` would
    /// otherwise have its every glob silently match nothing.
    pattern: Option<String>,
    /// Whether `root` is a plain file (no glob, and it exists as a file).
    is_file: bool,
}

impl Matcher {
    fn parse(cwd: &Path, token: &str) -> Result<Self> {
        if token.trim().is_empty() {
            bail!("an empty --include/--exclude path selects the entire working tree; pass a path");
        }
        let raw = Path::new(token);
        let abs = if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            cwd.join(raw)
        };

        // A path that exists under exactly this name is that path, even though
        // `[`/`{` are glob syntax AND legal filename characters everywhere —
        // otherwise a Next.js/Astro route directory (`app/[id]/data.json`) or a
        // browser-numbered download (`report[1].pdf`) could not be named at all.
        if abs.exists() {
            let root = resolve_parent(&abs);
            let is_file = root.is_file();
            return Ok(Self {
                token: token.to_string(),
                canonical_root: resolve(&abs),
                root,
                pattern: None,
                is_file,
            });
        }

        // Split at the first component carrying a wildcard: everything before it
        // is a real directory we can walk; the rest filters what we find. Scan
        // only what the USER TYPED — the cwd this gets joined onto is a machine
        // path, not pattern text, and scanning it would let a directory named
        // `my[work]` truncate the literal root and break every glob under it.
        let mut lit = PathBuf::new();
        let mut tail = PathBuf::new();
        let mut sawglob = false;
        for c in raw.components() {
            let literal = match c {
                Component::Normal(s) => !has_wildcard(&s.to_string_lossy()),
                _ => true,
            };
            if sawglob || !literal {
                sawglob = true;
                tail.push(c);
            } else {
                lit.push(c);
            }
        }

        let literal = if lit.is_absolute() {
            lit
        } else {
            cwd.join(lit)
        };
        let root = resolve_parent(&literal);
        let canonical_root = resolve(&literal);
        if sawglob {
            return Ok(Self {
                token: token.to_string(),
                pattern: Some(to_slash(&tail)),
                root,
                canonical_root,
                is_file: false,
            });
        }
        let is_file = root.is_file();
        Ok(Self {
            token: token.to_string(),
            root,
            canonical_root,
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
        if self.pattern.is_some() {
            out.retain(|p| self.glob_hits(p));
        }
        Ok(out)
    }

    /// Whether this matcher's glob selects `path`, compared on the portion below
    /// `root` so only the user's own pattern text is ever glob syntax.
    fn glob_hits(&self, path: &Path) -> bool {
        let Some(pattern) = &self.pattern else {
            return false;
        };
        path.strip_prefix(&self.root).is_ok_and(|rel| {
            // An empty remainder means `path` IS the root, which a tail pattern
            // describes something below — `*` must not prune the root itself.
            !rel.as_os_str().is_empty() && glob_match::glob_match(pattern, &to_slash(rel))
        })
    }

    /// Whether this token prunes `path`. A literal directory prunes its whole
    /// subtree; a literal file prunes itself; a glob prunes what it matches.
    ///
    /// An `--exclude` that matches nothing is SILENT (decided 2026-07-23,
    /// matching Deno) — the residual risk being a typo'd `--exclude ./secrets`
    /// that then ships the secrets.
    fn matches(&self, path: &Path) -> bool {
        match self.pattern {
            Some(_) => self.glob_hits(path),
            // Either spelling counts. Comparing only the as-written form lets an
            // `--exclude` naming a symlink miss the file its `--include` walked,
            // and an unmatched exclude is silent — so the miss ships the very
            // file the user asked to leave out.
            None => path.starts_with(&self.root) || resolve(path).starts_with(&self.canonical_root),
        }
    }
}

fn has_wildcard(s: &str) -> bool {
    s.contains(['*', '?', '[', '{'])
}

/// Reconcile `path`'s ANCESTORS with the canonicalized entry while leaving the
/// final component exactly as written.
///
/// The leaf is what the user is naming, and naming is what the extracted layout
/// mirrors: fully resolving `--include public` when `public` is a symlink into a
/// sibling package drags the anchor out of the project and the app's own
/// `../public/...` stops resolving. Resolving the ancestors is still required,
/// because a token spelled `/tmp/...` against an entry canonicalized to
/// `/private/tmp/...` otherwise shares no common directory at all.
fn resolve_parent(path: &Path) -> PathBuf {
    let normalized = normalize(path);
    match (normalized.parent(), normalized.file_name()) {
        (Some(parent), Some(name)) => {
            let mut out = resolve(parent);
            out.push(name);
            out
        }
        _ => normalized,
    }
}

/// Fully resolve `path` — every symlink followed — by canonicalizing its deepest
/// EXISTING ancestor and re-appending the rest. Used as a comparison key, where
/// two spellings of one file must compare equal; see [`resolve_parent`] for why
/// names are not built this way.
fn resolve(path: &Path) -> PathBuf {
    let normalized = normalize(path);
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut probe = normalized.as_path();
    loop {
        if let Ok(real) = probe.canonicalize() {
            let mut out = real;
            out.extend(tail.iter().rev());
            return out;
        }
        match (probe.parent(), probe.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name.to_os_string());
                probe = parent;
            }
            // A path with no existing ancestor at all (or a bare root): the
            // lexical form is the best available answer.
            _ => return normalized,
        }
    }
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
    #[cfg(unix)]
    fn an_exclude_spelled_through_a_symlink_still_prunes() {
        // The entry arrives canonicalized, so a token left in its symlinked
        // spelling used to prefix-match nothing — and an unmatched --exclude is
        // silent, so `--include . --exclude "$PWD/.env"` shipped the secret
        // ($PWD is logical, current_dir() is physical).
        let d = fixture("symlink");
        let link = d.parent().unwrap().join(format!(
            "nub-assets-link-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_file(&link);
        std::os::unix::fs::symlink(&d, &link).unwrap();

        let plan = plan(
            &d.join("src"),
            &d,
            &["assets".into()],
            // Spelled through the link; names the same physical file.
            &[link.join("assets/img").to_string_lossy().into_owned()],
        )
        .unwrap();
        assert_eq!(
            plan.assets.iter().map(|a| &a.rel).collect::<Vec<_>>(),
            ["assets/data.json"],
            "an exclude spelled through a symlink must prune the same files as one spelled directly"
        );
        let _ = fs::remove_file(&link);
    }

    #[test]
    #[cfg(unix)]
    fn an_exclude_naming_a_symlinked_file_prunes_the_file_the_include_walked() {
        // The include walks `assets/` and finds the link by its own path; the
        // exclude names the same link. Comparing only fully-resolved forms (or
        // only as-written ones) makes one side miss — and a missed exclude is
        // silent, so the file it named ships anyway.
        let d = fixture("symlinked-file");
        fs::create_dir_all(d.join("secret")).unwrap();
        fs::write(d.join("secret/prod.json"), "SECRET").unwrap();
        std::os::unix::fs::symlink("../secret/prod.json", d.join("assets/config.json")).unwrap();

        let plan = plan(
            &d.join("src"),
            &d,
            &["assets".into()],
            &["assets/config.json".into()],
        )
        .unwrap();
        assert!(
            !plan.assets.iter().any(|a| a.rel.contains("config.json")),
            "a symlinked file named by --exclude must not ship: {:?}",
            plan.assets.iter().map(|a| &a.rel).collect::<Vec<_>>()
        );
    }

    #[test]
    #[cfg(unix)]
    fn an_include_naming_a_symlinked_directory_keeps_the_name_the_user_wrote() {
        // The monorepo shape: `public` is a link into a sibling package. Naming
        // the link's TARGET would move the asset outside the entry's tree, and
        // the app's own `../public/...` would stop resolving once compiled.
        let d = fixture("symlinked-dir");
        fs::create_dir_all(d.join("shared/public")).unwrap();
        fs::write(d.join("shared/public/data.json"), "{}").unwrap();
        std::os::unix::fs::symlink("shared/public", d.join("public")).unwrap();

        let plan = plan(&d.join("src"), &d, &["public".into()], &[]).unwrap();
        assert_eq!(plan.entry_prefix, "src");
        assert_eq!(
            plan.assets.iter().map(|a| &a.rel).collect::<Vec<_>>(),
            ["public/data.json"]
        );
    }

    #[test]
    fn a_glob_metacharacter_in_the_project_path_is_not_glob_syntax() {
        // The user's pattern is matched BELOW the include root, so a build
        // machine directory literally named `my[work]` — part of the path, not
        // of what they typed — can never be read as a character class.
        let d = fixture("brackets");
        let odd = d.join("my[work]");
        fs::create_dir_all(odd.join("assets/img")).unwrap();
        fs::create_dir_all(odd.join("src")).unwrap();
        fs::write(odd.join("assets/img/a.png"), "png").unwrap();
        let plan = plan(&odd.join("src"), &odd, &["assets/**/*.png".into()], &[]).unwrap();
        assert_eq!(
            plan.assets.iter().map(|a| &a.rel).collect::<Vec<_>>(),
            ["assets/img/a.png"]
        );
    }

    #[test]
    fn a_path_that_exists_verbatim_wins_over_reading_it_as_a_glob() {
        // `[id]` route directories and `report[1].pdf` downloads are ordinary
        // names that happen to contain glob syntax.
        let d = fixture("literal-brackets");
        fs::create_dir_all(d.join("app/[id]")).unwrap();
        fs::write(d.join("app/[id]/data.json"), "{}").unwrap();
        fs::write(d.join("report[1].pdf"), "pdf").unwrap();
        let plan = plan(
            &d.join("src"),
            &d,
            &["app/[id]".into(), "report[1].pdf".into()],
            &[],
        )
        .unwrap();
        assert_eq!(
            plan.assets.iter().map(|a| &a.rel).collect::<Vec<_>>(),
            ["app/[id]/data.json", "report[1].pdf"]
        );
    }

    #[test]
    fn an_empty_include_token_is_rejected_rather_than_taking_the_whole_tree() {
        let d = fixture("empty");
        // `--include "$ASSET_DIR"` with the variable unset would otherwise embed
        // the entire working tree, node_modules and dotfiles included.
        for token in ["", "   "] {
            let err = plan(&d.join("src"), &d, &[token.to_string()], &[]).unwrap_err();
            assert!(format!("{err:#}").contains("empty"), "{err:#}");
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
        let msg = format!("{err:#}");
        assert!(msg.contains("matched no files"), "{msg}");
        // The three ways an include can come up empty need three different
        // fixes, so a path emptied by --exclude must not be reported as missing.
        assert!(msg.contains("every file it matched was excluded"), "{msg}");
        assert!(!msg.contains("does not exist"), "{msg}");
    }
}
