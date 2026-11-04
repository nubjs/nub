//! `nub agent` — make AI coding agents reliably reach for nub.
//!
//! The command group has two verbs, both of which only PRINT to stdout — nub
//! never writes agent artifacts (skills, AGENTS.md, rules) into a user's
//! project. Onboarding is driven by a copyable prompt on the homepage that the
//! user pastes into their own coding agent; these verbs are the OFFLINE FALLBACK
//! for when that agent can't fetch the live docs over the web:
//!
//! - `docs` — prints usage and the page TOC. Full markdown is opt-in through
//!   `--page`; every slug and markdown link target is a valid argument. The
//!   slugs ARE the in-doc link hrefs (`/docs/runtime/decorators`, …), so an agent
//!   can take a markdown link target and plug it straight into `--page`. `--page
//!   <path>` prints one page's full markdown; `--list`/`--toc` prints just the
//!   TOC. The whole docs tree (`site/content/docs/**/*.mdx`) is baked in at build
//!   time, so an agent can pull the current docs with no network.
//! - `skill` — prints the evergreen agent skill (`site/public/skill.md`, also
//!   served at https://nubjs.com/skill.md) for the agent to install itself.
//!
//! `skill.md` is embedded via `include_str!`; the docs tree is baked by
//! `build.rs` into a `&[(slug, title, markdown)]` table — all so the verbs work
//! with no network fetch from a stale binary. This is a non-forwarding group
//! handled by a manual sub-verb match (like `nub node` / `nub pm`), so its
//! bare-usage and invalid-verb messages read consistently.

use anyhow::{Result, bail};

/// The EVERGREEN agent skill, authored once at `site/public/skill.md` (so the same
/// file also serves at https://nubjs.com/skill.md) and embedded here so `nub agent
/// skill` prints it offline with no network fetch. It's a thin, STABLE orientation
/// layer that points the agent at the always-current sources (`nub --help`,
/// https://nubjs.com/docs, https://nubjs.com/llms.txt) — self-healing even from a
/// stale binary. It deliberately omits volatile detail (exhaustive flag lists).
const SKILL_MD: &str = include_str!("../../../../site/public/skill.md");

/// The full docs tree, baked in at build time by `build.rs` as a slug-sorted
/// `&[(slug, title, markdown)]` table with each page's YAML frontmatter
/// stripped. Each slug is the page's exact `/docs/...` URL path — the same href
/// the docs link to internally — so a markdown link target is a valid `--page`
/// argument. `nub agent docs` serves the TOC and `--page <path>` content from
/// this with no network fetch.
mod baked {
    include!(concat!(env!("OUT_DIR"), "/docs_baked.rs"));
}
use baked::DOCS;

/// The canonical slug for the docs index page (`site/content/docs/index.mdx`,
/// served at `/docs`).
const INDEX_SLUG: &str = "/docs";

/// Entry point for `nub agent …`, dispatched from `dispatch_subcommand`.
pub fn run(args: &[String]) -> Result<i32> {
    // Same guard as the other two non-forwarding groups: a help flag counts after
    // the verb too, so `nub agent docs --help` prints usage instead of failing on
    // an unexpected argument, and `nub agent skill --help` stops dumping the skill.
    let verb = args.first().map(String::as_str);
    if verb.is_none() || crate::cli::group_help_requested(args) {
        print_usage();
        return Ok(0);
    }
    match verb.expect("verb present after the help/bare guard") {
        "docs" => run_docs(&args[1..]),
        "skill" => {
            print!("{SKILL_MD}");
            Ok(0)
        }
        other => bail!(
            "nub agent takes a subcommand (docs, skill). Unknown verb '{other}'. \
             See `nub agent --help`."
        ),
    }
}

/// `nub agent docs [--page <path> | --list | --toc]`.
///
/// No args → usage and the page TOC, without any page's markdown.
/// `--list` / `--toc` → just the TOC.
/// `--page <path>` → that page's full markdown (frontmatter stripped). The path
///            is the page's `/docs/...` URL — the same form the docs link to — so
///            a copied link target resolves. An unknown path errors with the
///            valid slugs and exits non-zero.
fn run_docs(args: &[String]) -> Result<i32> {
    let mut page: Option<&str> = None;
    let mut list_only = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--list" | "--toc" => list_only = true,
            "--page" => {
                let slug = iter.next().map(String::as_str).ok_or_else(|| {
                    anyhow::anyhow!(
                        "nub agent docs --page needs a path, e.g. `--page /docs/runtime/typescript`"
                    )
                })?;
                page = Some(slug);
            }
            other if other.starts_with("--page=") => {
                page = Some(&other["--page=".len()..]);
            }
            other => bail!(
                "nub agent docs: unexpected argument '{other}'. \
                 Usage: nub agent docs [--page <path> | --list | --toc]."
            ),
        }
    }

    if let Some(slug) = page {
        return print_page(slug);
    }

    if list_only {
        print_toc();
        return Ok(0);
    }

    println!(
        "nub agent docs — browse the bundled docs offline\n\n\
         Usage: nub agent docs [--page <path> | --list | --toc]\n\n\
         Options:\n\
         \x20 --page <path>  Print one page's full markdown\n\
         \x20 --list, --toc  Print only the table of contents\n\
         \x20 -h, --help     Show agent command help\n\n\
         Example:\n\
         \x20 nub agent docs --page /docs/runtime/decorators\n"
    );
    print_toc();
    Ok(0)
}

/// Resolve a user-supplied `--page` argument to a baked slug, tolerating minor
/// variations so a copied link target always lands: with or without the leading
/// slash, and with or without the `/docs` prefix. The canonical slug is the full
/// `/docs/...` URL path, but `runtime/decorators`, `/runtime/decorators`, and
/// `/docs/runtime/decorators` all resolve to the same page.
fn resolve_slug(arg: &str) -> Option<&'static (&'static str, &'static str, &'static str)> {
    // Strip any URL fragment (`/docs/runtime/resolution#yarn-plugnplay`).
    let arg = arg.split('#').next().unwrap_or(arg);
    let trimmed = arg.trim_matches('/');
    // Candidate canonical forms to try against the baked slugs.
    let candidates = [
        format!("/{trimmed}"),      // exact (already had a leading slash)
        format!("/docs/{trimmed}"), // bare path, prepend /docs
        format!("/docs/{}", trimmed.strip_prefix("docs/").unwrap_or(trimmed)),
    ];
    DOCS.iter().find(|(s, _, _)| {
        // The docs root: `/docs`, `docs`, `/`, or empty all mean the index.
        if trimmed.is_empty() || trimmed == "docs" {
            return *s == INDEX_SLUG;
        }
        candidates.iter().any(|c| c == s) || *s == arg
    })
}

/// Print the markdown for one page, or error (exit 1) listing valid slugs.
fn print_page(slug: &str) -> Result<i32> {
    match resolve_slug(slug) {
        Some((_, _, body)) => {
            print!("{body}");
            Ok(0)
        }
        None => {
            use std::fmt::Write as _;
            let mut msg = format!("nub agent docs: unknown page '{slug}'.\n\nAvailable pages:\n");
            for (s, title, _) in DOCS {
                let _ = writeln!(msg, "  {s} — {title}");
            }
            bail!(msg);
        }
    }
}

/// Print the table of contents: one `<slug> — <title>` line per page. The slug
/// is the page's `/docs/...` URL path — the same href the docs link to — so it
/// doubles as a `--page` argument.
fn print_toc() {
    println!("Table of contents:");
    for (slug, title, _) in DOCS {
        println!("  {slug} — {title}");
    }
}

fn print_usage() {
    println!(
        "nub agent — make AI coding agents reach for nub\n\n\
         Usage: nub agent <command>\n\n\
         Commands:\n\
         \x20 docs     show docs usage and a table of contents\n\
         \x20          (offline fallback for https://nubjs.com/docs)\n\
         \x20          --page <path>  print one page's full markdown (e.g. /docs/runtime/jsx)\n\
         \x20          --list, --toc  print just the page TOC\n\
         \x20 skill    print nub's evergreen agent skill to stdout (install it yourself)"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    #[test]
    fn bad_verb_errors() {
        assert!(run(&["bogus".into()]).is_err());
    }

    #[test]
    fn docs_verb_lists_the_baked_docs() {
        assert_eq!(run(&["docs".into()]).unwrap(), 0);

        // The overview remains available through --page /docs.
        let index = DOCS
            .iter()
            .find(|(s, _, _)| *s == INDEX_SLUG)
            .expect("/docs index page baked");
        assert!(
            index.2.contains("all-in-one toolkit"),
            "index body must be the real /docs page content"
        );

        let toc_out = DOCS.iter().map(|(s, _, _)| *s).collect::<Vec<_>>();
        assert!(
            toc_out.contains(&INDEX_SLUG),
            "the TOC carries the /docs index slug, not a start.md entry"
        );
    }

    #[test]
    fn skill_verb_prints_evergreen_skill_with_the_key_pointers() {
        // `nub agent skill` prints the embedded skill and exits 0.
        assert_eq!(run(&["skill".into()]).unwrap(), 0);

        // The skill is the EVERGREEN orientation layer: it must be non-empty and
        // carry the self-healing pointers at the always-current sources, plus the
        // `--node` escape hatch. (We assert against the embedded const directly —
        // it's what `run` prints verbatim.)
        let body = SKILL_MD;
        assert!(!body.trim().is_empty(), "skill must not be empty");
        // First line is the YAML front-matter fence. Compare line-ending-agnostically:
        // a Windows checkout embeds the file with CRLF, so `body` may start with "---\r\n".
        assert_eq!(
            body.lines().next(),
            Some("---"),
            "skill needs YAML front matter"
        );
        for pointer in [
            "nubjs.com/docs",
            "nubjs.com/llms.txt",
            "nub --help",
            "--node",
        ] {
            assert!(
                body.contains(pointer),
                "evergreen skill must point the agent at `{pointer}`"
            );
        }
        // Brand boundary: the agent-facing skill is PUBLIC copy a user's coding
        // agent reads. The embedded PM engine's brand ("aube") is internal
        // mechanism and must never surface here (the engine is an invisible
        // implementation detail under nub).
        assert!(
            !body.to_lowercase().contains("aube"),
            "agent skill copy must not leak the embedded engine brand 'aube'"
        );
    }

    #[test]
    fn bare_and_help_print_usage_ok() {
        assert_eq!(run(&[]).unwrap(), 0);
        assert_eq!(run(&["help".into()]).unwrap(), 0);
        assert_eq!(run(&["--help".into()]).unwrap(), 0);
    }

    #[test]
    fn a_help_flag_after_the_verb_is_not_an_unexpected_argument() {
        // The #653 shape in the third non-forwarding group: `nub agent docs --help`
        // failed with "unexpected argument '--help'" before the shared guard, so the
        // exit code alone catches that regression.
        //
        // `skill` is deliberately not asserted here. It returned 0 either way — it
        // dumped the whole skill markdown and ignored the flag — so an exit-code
        // assertion on it could never fail. The flag-anywhere semantics it now
        // depends on are asserted directly in
        // `cli::tests::group_help_is_recognized_after_the_verb`.
        for flag in ["--help", "-h"] {
            assert_eq!(
                run(&["docs".into(), flag.into()]).unwrap(),
                0,
                "`nub agent docs {flag}` must print usage, not error"
            );
        }
    }

    #[test]
    fn docs_tree_is_baked_with_url_path_slugs_matching_in_doc_links() {
        // The baked table must be exactly the docs tree on disk — every page,
        // keyed by the `/docs/...` URL path it is linked by internally (so a
        // markdown link target is a valid `--page` argument), with the title
        // lifted out of the frontmatter and the frontmatter stripped from the
        // body. The expectation is DERIVED from `site/content/docs` rather than
        // pinned here, so a docs move is a docs-only change and never a Rust one
        // — a pinned list once dragged the full Rust matrix onto every docs
        // restructure. Comparing whole tuples, not just slugs, also catches a
        // stale bake: the shared target dir can hand a worktree a sibling's
        // baked tree, which a slug list from a page that still exists could not
        // tell apart from a fresh one.
        //
        // The docs root is resolved at RUN time: `env!` would freeze the
        // compiling worktree's path into a test binary that the shared target
        // dir then hands to sibling worktrees.
        let manifest_dir = std::env::var_os("CARGO_MANIFEST_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
        let docs_dir = manifest_dir.join("../../site/content/docs");
        let mut expected = Vec::new();
        collect_pages(&docs_dir, &docs_dir, &mut expected);
        expected.sort();
        assert!(
            !expected.is_empty(),
            "no .mdx pages under {}",
            docs_dir.display()
        );

        let mut baked: Vec<(String, String, String)> = DOCS
            .iter()
            .map(|(s, t, b)| ((*s).to_string(), (*t).to_string(), (*b).to_string()))
            .collect();
        baked.sort();
        let baked_slugs: Vec<&str> = baked.iter().map(|(s, _, _)| s.as_str()).collect();
        let expected_slugs: Vec<&str> = expected.iter().map(|(s, _, _)| s.as_str()).collect();
        assert_eq!(
            baked_slugs, expected_slugs,
            "baked slugs must match the tree under site/content/docs (stale bake or slug rule drift)"
        );
        for ((slug, title, body), (_, want_title, want_body)) in baked.iter().zip(&expected) {
            assert_eq!(
                title, want_title,
                "{slug}: title must come from the page's frontmatter"
            );
            assert_eq!(
                body, want_body,
                "{slug}: body must be the page with its frontmatter stripped"
            );
        }
        assert!(
            baked_slugs.contains(&"/docs"),
            "top-level index.mdx collapses to /docs"
        );
        assert!(
            !baked_slugs.iter().any(|s| s.ends_with("/index")),
            "section-root `index` slugs must collapse to the parent: {baked_slugs:?}"
        );
    }

    /// The build script's slug and frontmatter rules, restated: every `.mdx`
    /// under the docs root, `index` collapsing to its parent (`runtime/index` ->
    /// `/docs/runtime`, the root `index` -> `/docs`); the title is the
    /// frontmatter `title:` (quotes stripped, the slug when absent) and the body
    /// is everything past the closing fence. Kept in the test rather than shared
    /// with `build.rs` on purpose — a shared helper would make the test agree
    /// with the bake by construction.
    fn collect_pages(root: &Path, dir: &Path, out: &mut Vec<(String, String, String)>) {
        for entry in std::fs::read_dir(dir).expect("readable docs dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                collect_pages(root, &path, out);
            } else if path.extension().is_some_and(|e| e == "mdx") {
                let rel = path
                    .strip_prefix(root)
                    .expect("under docs root")
                    .with_extension("");
                let parts: Vec<String> = rel
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect();
                let tail = match parts.split_last() {
                    Some((last, head)) if last == "index" => head.join("/"),
                    _ => parts.join("/"),
                };
                let slug = if tail.is_empty() {
                    "/docs".to_string()
                } else {
                    format!("/docs/{tail}")
                };
                let raw = std::fs::read_to_string(&path)
                    .expect("readable page")
                    .replace("\r\n", "\n");
                let (title, body) = match raw.strip_prefix("---\n").and_then(|rest| {
                    let end = rest.find("\n---")?;
                    Some((&rest[..end], &rest[end + 4..]))
                }) {
                    Some((front, after)) => {
                        let title = front
                            .lines()
                            .find_map(|l| l.trim().strip_prefix("title:"))
                            .map(|t| t.trim().trim_matches(['"', '\'']).to_string())
                            .filter(|t| !t.is_empty())
                            .unwrap_or_else(|| slug.clone());
                        (title, after.strip_prefix('\n').unwrap_or(after).to_string())
                    }
                    None => (slug.clone(), raw.clone()),
                };
                out.push((slug, title, body));
            }
        }
    }

    #[test]
    fn docs_help_and_list_variants_succeed() {
        assert_eq!(run_docs(&[]).unwrap(), 0);
        // `--list`/`--toc` is the TOC-only variant.
        assert_eq!(run_docs(&["--list".into()]).unwrap(), 0);
        assert_eq!(run_docs(&["--toc".into()]).unwrap(), 0);
    }

    #[test]
    fn docs_page_double_duty_link_target_round_trips() {
        // THE acceptance test: a verbatim in-doc markdown link target resolves.
        // The docs link as `](/docs/runtime/decorators)` — so that exact path,
        // pasted straight into `--page`, must serve the page.
        let canonical =
            resolve_slug("/docs/runtime/decorators").expect("canonical /docs link target resolves");
        assert_eq!(canonical.0, "/docs/runtime/decorators");

        // the maintainer's example spelling (`/runtime/decorators`, no `/docs`) resolves to
        // the same page via tolerance.
        assert_eq!(
            resolve_slug("/runtime/decorators").map(|p| p.0),
            Some("/docs/runtime/decorators")
        );
        // Bare (no leading slash) and a fragment also resolve.
        assert_eq!(
            resolve_slug("runtime/decorators").map(|p| p.0),
            Some("/docs/runtime/decorators")
        );
        assert_eq!(
            resolve_slug("/docs/runtime/resolution#yarn-plugnplay").map(|p| p.0),
            Some("/docs/runtime/resolution")
        );
        // The docs root resolves from any of its spellings.
        for root in ["/docs", "docs", "/", ""] {
            assert_eq!(
                resolve_slug(root).map(|p| p.0),
                Some(INDEX_SLUG),
                "`{root}` resolves to the /docs index"
            );
        }

        // `--page` end-to-end, both `--page <path>` and `--page=<path>`.
        assert_eq!(
            run_docs(&["--page".into(), "/docs/runtime/typescript".into()]).unwrap(),
            0
        );
        assert_eq!(run_docs(&["--page=/docs/pm".into()]).unwrap(), 0);

        // Unknown slug is an error (non-zero exit via the bubbled-up anyhow),
        // and the error lists the valid `/docs/...` slugs.
        let err = run_docs(&["--page".into(), "nonexistent".into()]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown page 'nonexistent'"), "{msg}");
        assert!(
            msg.contains("/docs/runtime/typescript"),
            "error lists valid slugs: {msg}"
        );

        // `--page` with no path is a usage error.
        assert!(run_docs(&["--page".into()]).is_err());
    }
}
