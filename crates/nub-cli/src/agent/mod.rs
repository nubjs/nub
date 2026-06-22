//! `nub agent` — make AI coding agents reliably reach for nub.
//!
//! The command group has two verbs, both of which only PRINT to stdout — nub
//! never writes agent artifacts (skills, AGENTS.md, rules) into a user's
//! project. Onboarding is driven by a copyable prompt on the homepage that the
//! user pastes into their own coding agent; these verbs are the OFFLINE FALLBACK
//! for when that agent can't fetch the live docs over the web:
//!
//! - `docs`  — MIRRORS the published docs. With no args it prints the page TOC
//!   at the top, then the `/docs` index page's markdown (the same content served
//!   at https://nubjs.com/docs), then a one-line note that every slug — and every
//!   markdown link target inside the pages — is a valid `--page` argument. The
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
/// served at `/docs`). Its body is printed verbatim by the no-args invocation.
const INDEX_SLUG: &str = "/docs";

/// Entry point for `nub agent …`, dispatched from `dispatch_subcommand`.
pub fn run(args: &[String]) -> Result<i32> {
    let verb = args.first().map(String::as_str);
    if matches!(verb, None | Some("help") | Some("--help") | Some("-h")) {
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

/// `nub agent docs [--page <path> | --list]`.
///
/// No args  → MIRRORS the docs: the page TOC at the top, then the `/docs` index
///            page's markdown, then a note that every slug (and every in-doc
///            link target) is a valid `--page` argument.
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
                 Usage: nub agent docs [--page <path> | --list]."
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

    // Mirror the docs: TOC first, then the /docs index page, then fetch note.
    print_toc();
    println!();
    if let Some((_, _, body)) = DOCS.iter().find(|(s, _, _)| *s == INDEX_SLUG) {
        print!("{body}");
        println!();
    }
    println!(
        "---\n\nFetch a page's full markdown, e.g.:\n\n    nub agent docs --page /docs/runtime/decorators"
    );
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
    println!("## Docs pages — pass any path to `nub agent docs --page <path>`\n");
    for (slug, title, _) in DOCS {
        println!("  {slug} — {title}");
    }
}

fn print_usage() {
    println!(
        "nub agent — make AI coding agents reach for nub\n\n\
         Usage: nub agent <command>\n\n\
         Commands:\n\
         \x20 docs     mirror the docs: a TOC of every page + the /docs index content\n\
         \x20          (offline fallback for https://nubjs.com/docs)\n\
         \x20          --page <path>  print one page's full markdown (e.g. /docs/runtime/jsx)\n\
         \x20          --list         print just the page TOC\n\
         \x20 skill    print nub's evergreen agent skill to stdout (install it yourself)"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bad_verb_errors() {
        assert!(run(&["bogus".into()]).is_err());
    }

    #[test]
    fn docs_verb_mirrors_the_docs_and_drops_start_md() {
        // `nub agent docs` mirrors the docs and exits 0.
        assert_eq!(run(&["docs".into()]).unwrap(), 0);

        // The /docs index page is baked and non-empty — it's what no-args prints.
        let index = DOCS
            .iter()
            .find(|(s, _, _)| *s == INDEX_SLUG)
            .expect("/docs index page baked");
        assert!(
            index.2.contains("all-in-one toolkit"),
            "index body must be the real /docs page content"
        );

        // start.md is GONE: the embedded onboarding-doc const no longer exists.
        // (Asserted structurally — the `START_MD` symbol was removed; if it were
        // reintroduced this module wouldn't compile against the old reference.)
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
    fn docs_tree_is_baked_with_url_path_slugs_matching_in_doc_links() {
        // The whole docs tree must be present, keyed by the EXACT `/docs/...` URL
        // paths the docs link to internally (so a markdown link target is a valid
        // `--page` argument). `index.mdx` collapses to its section root
        // (`runtime/index.mdx` -> `/docs/runtime`); the top-level `index.mdx` is
        // the docs root `/docs`.
        let slugs: Vec<&str> = DOCS.iter().map(|(s, _, _)| *s).collect();
        for expected in [
            "/docs",
            "/docs/runtime",
            "/docs/runtime/typescript",
            "/docs/runtime/decorators",
            "/docs/install",
            "/docs/install/pnpm",
            "/docs/pm",
            "/docs/nubx",
            "/docs/run",
        ] {
            assert!(
                slugs.contains(&expected),
                "baked docs must include slug `{expected}`; got {slugs:?}"
            );
        }
        // Every slug is a rooted `/docs` URL path, and no `*/index` leaked through.
        assert!(
            slugs.iter().all(|s| s.starts_with("/docs")),
            "every slug is a /docs URL path: {slugs:?}"
        );
        assert!(
            !slugs.iter().any(|s| s.ends_with("/index")),
            "section-root `index` slugs must collapse to the parent: {slugs:?}"
        );
        // Frontmatter is stripped: bodies don't start with the `---` fence, and
        // the title was lifted out of it.
        let ts = DOCS
            .iter()
            .find(|(s, _, _)| *s == "/docs/runtime/typescript")
            .expect("typescript page present");
        assert_eq!(ts.1, "TypeScript", "title comes from frontmatter");
        assert!(
            !ts.2.trim_start().starts_with("---"),
            "frontmatter must be stripped from the printed body"
        );
        assert!(
            ts.2.contains("oxc-based transpiler"),
            "baked body must be the real page content"
        );
    }

    #[test]
    fn docs_no_args_mirrors_toc_then_index() {
        // No args is the mirror: TOC at top + the /docs index content + fetch note.
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
