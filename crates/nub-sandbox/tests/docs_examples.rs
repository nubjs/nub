//! Every policy example on the docs page compiles.
//!
//! The epic calls `site/content/docs/sandbox.mdx` the canonical, example-rich reference for the
//! policy grammar and the de-facto test-case set, and on any drift between the two the page wins.
//! Nothing enforced that, and the page drifted: it documented a filesystem deny form — `!path` in
//! an array, `false` in an object — that the compiler now rejects outright, and used it in the
//! last example on the page. A reader following it got a policy error naming their own path.
//!
//! This is the cheap half of keeping that promise. It does not check that an example means what
//! its prose says; it checks that every example on the page is a document this crate accepts, so a
//! grammar change cannot land while the reference still teaches the old spelling.

use nub_sandbox::{CompileCtx, Homes, ScopeCapabilities, compile};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn page() -> PathBuf {
    // Relative to the crate, not to the workspace: `cargo test` runs with the crate as its
    // working directory, and a missing page must FAIL rather than skip — a page that moved is
    // exactly when this test has something to say.
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../site/content/docs/sandbox.mdx")
}

/// The fenced ```jsonc blocks, in page order, with their 1-based opening line.
fn jsonc_blocks(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut lines = text.lines().enumerate();
    while let Some((i, line)) = lines.next() {
        if !line.trim_start().starts_with("```jsonc") {
            continue;
        }
        let mut body = String::new();
        for (_, line) in lines.by_ref() {
            if line.trim_start().starts_with("```") {
                break;
            }
            body.push_str(line);
            body.push('\n');
        }
        out.push((i + 1, body));
    }
    out
}

/// Drop `//` line comments, which the examples use and `serde_json` does not accept.
///
/// Line comments only, and that is enough because the page has no block comment and no string
/// containing `//` outside a URL — `"//registry/:_authToken"` would be mangled, so the scan
/// tracks string state rather than searching for the token.
fn strip_line_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    for line in src.lines() {
        let (mut in_string, mut escaped) = (false, false);
        let bytes: Vec<char> = line.chars().collect();
        let mut end = bytes.len();
        for i in 0..bytes.len() {
            let c = bytes[i];
            if escaped {
                escaped = false;
            } else if c == '\\' && in_string {
                escaped = true;
            } else if c == '"' {
                in_string = !in_string;
            } else if !in_string && c == '/' && bytes.get(i + 1) == Some(&'/') {
                end = i;
                break;
            }
        }
        out.extend(bytes[..end].iter());
        out.push('\n');
    }
    out
}

/// The environment the examples are compiled against.
///
/// A `secrets` entry is REQUIRED by default — compiling one whose variable is unset is an error
/// naming the variable — so an empty ambient map would make every example carrying a secret fail
/// for a reason that has nothing to do with the grammar. Listing the names the page uses keeps
/// that failure meaningful: a new secret in a new example fails here, loudly, with the variable
/// named, and the fix is to add it to this list rather than to loosen the check.
fn ambient() -> BTreeMap<String, String> {
    // The VALUES matter as much as the names: an example that pins a `format` or an `enum:` is
    // checked against the value at compile time, so a placeholder would fail validation rather
    // than exercise the grammar.
    [
        ("GITHUB_TOKEN", "ghp-docs-fixture"),
        ("DATABASE_URL", "postgres://localhost/docs"),
        ("DATABASE_HOST", "localhost"),
        ("STRIPE_KEY", "sk-docs-fixture"),
        ("PATH", "/usr/bin:/bin"),
        ("NODE_ENV", "development"),
        ("LOG_LEVEL", "info"),
        ("PORT", "8080"),
        ("REGION", "us-east-1"),
        ("DEBUG", "1"),
        ("CI", "true"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

/// Mirrors what `nub sandbox --policy` does with a file, because a test that compiled the
/// examples some OTHER way would pass while the real loader rejected them — which is exactly
/// what happened: the frontend passed the whole file as the surface and set no pointer base, so
/// every `sandbox`-wrapped policy and every `...:#/pointer` on this page failed in the CLI while
/// an earlier draft of this test was green.
fn ctx(root: &Path, document: &serde_json::Value) -> CompileCtx {
    base_ctx(root).with_document(document.clone())
}

fn base_ctx(root: &Path) -> CompileCtx {
    CompileCtx::new(
        Homes {
            home: root.join("home"),
            cache: root.join("cache"),
            tmp: root.join("tmp"),
            project: root.join("project"),
        },
        root.join("project"),
        ScopeCapabilities::approved(),
        ambient(),
    )
}

#[test]
fn every_policy_example_on_the_docs_page_compiles() {
    let path = page();
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    let blocks = jsonc_blocks(&text);
    let root = tempfile::tempdir().expect("fixture root");
    std::fs::create_dir_all(root.path().join("project")).unwrap();

    let mut compiled = 0usize;
    let mut skipped = Vec::new();
    for (line, body) in &blocks {
        // `$( … )` runs a real command while the policy compiles, so those examples need the tool
        // installed and are the one thing this cannot check hermetically. The skip is asserted
        // below rather than trusted, so it cannot quietly grow to cover a broken example.
        if body.contains("$(") {
            skipped.push((*line, body.clone()));
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(&strip_line_comments(body))
            .unwrap_or_else(|e| {
                panic!("{}:{line} is not valid JSONC: {e}\n{body}", path.display())
            });
        let surface = value.get("sandbox").unwrap_or(&value);
        if let Err(error) = compile(surface, &ctx(root.path(), &value)) {
            panic!(
                "{}:{line} does not compile: {error}\n{body}",
                path.display()
            );
        }
        compiled += 1;
    }

    // ⛔ THE INSTRUMENT'S OWN CONTROLS. A regex that matched no block, or a skip rule that
    // swallowed the page, would leave this test passing while checking nothing.
    assert!(
        blocks.len() >= 30,
        "found only {} jsonc blocks on {} — the extractor is broken, not the page",
        blocks.len(),
        path.display(),
    );
    assert!(
        compiled >= blocks.len() * 3 / 4,
        "only {compiled} of {} blocks were compiled; the rest were skipped",
        blocks.len(),
    );
    for (line, body) in &skipped {
        assert!(
            body.contains("$("),
            "{}:{line} was skipped for a reason other than command substitution",
            path.display(),
        );
    }
}
