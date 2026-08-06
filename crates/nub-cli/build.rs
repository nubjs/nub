//! Bake the user-facing docs tree (`site/content/docs/**/*.mdx`) into the binary
//! so `nub agent docs` can serve the current docs offline.
//!
//! We generate a `&[(slug, title, markdown)]` table at build time instead of
//! pulling in an embed crate: it keeps the dependency surface flat, lets us
//! derive the site's routing slugs + read the frontmatter `title` exactly the
//! way we want, and strips the YAML frontmatter from the printed markdown.
//!
//! Slugs are the EXACT URL-path form the docs link to internally (`grep` the
//! MDX: internal links are `](/docs/runtime/decorators)` etc.), so a markdown
//! link target pastes straight into `nub agent docs --page <target>`:
//!   - `runtime/typescript.mdx` -> `/docs/runtime/typescript`
//!   - `index.mdx`              -> `/docs`                 (docs root)
//!   - `runtime/index.mdx`      -> `/docs/runtime`         (section root)
//!   - `install/index.mdx`      -> `/docs/install`         (section root)
//!
//! The docs live in-repo, so the directory is always present; if it ever goes
//! missing the build fails loudly rather than shipping an empty TOC.

// `collapsible_if` fires on nested `if let { if let }` now that the workspace
// MSRV supports let chains; collapsing it is cosmetic, so allow it.
#![allow(clippy::collapsible_if)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Cargo's `CARGO_MANIFEST_DIR`, read at RUN time rather than baked in by `env!`.
///
/// `env!` freezes the compiling worktree's path into the build-script binary.
/// Cargo caches those binaries per target dir and this repo shares ONE target
/// dir across worktrees, so the path outlives the tree that produced it: a
/// sibling tree reads that tree's files, and once it is deleted the build fails
/// on a path absent from the current checkout.
///
/// Measured on cargo 1.95: the run-time lookup alone is sufficient, because the
/// `rerun-if-changed` path this script emits then differs per tree and cargo
/// invalidates on that. A review on cargo 1.97 measured the opposite, so the
/// paired `cargo:rerun-if-env-changed=CARGO_MANIFEST_DIR` below is kept as
/// version-insurance; it costs nothing in a single tree, where the value never
/// changes. It is independently load-bearing in `nub-native`, which emits no
/// path-dependent instruction when there is no git checkout.
///
/// `cargo publish --verify` is unaffected: cargo sets the variable from the
/// package being built, so it still resolves inside `target/package/`.
fn manifest_dir() -> PathBuf {
    PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR")
            .expect("cargo always sets CARGO_MANIFEST_DIR for build scripts"),
    )
}

/// Give `nub.exe` the 8 MiB main-thread stack Unix gives it. Windows reserves 1 MiB, and
/// nub's entry path does not fit in it unoptimized: clap-derive's generated
/// `<Command as Subcommand>::augment_subcommands` materializes every verb's every `Arg` in
/// ONE frame, so a debug build aborts `STATUS_STACK_OVERFLOW` (0xC00000FD) inside
/// `Cli::parse_from`, before it reads an argument. Measured on one binary under a shrinking
/// rlimit: 1024 KiB overflows, 1200 KiB does not — a ~150 KiB margin every added `#[arg]`
/// eats into, which is why this surfaces and re-hides as the CLI grows.
///
/// Emitted from the build script rather than as `RUSTFLAGS` or `.cargo/config.toml`
/// rustflags because those are single-valued and mutually exclusive: seven Windows
/// workflows carry `-C link-arg=/STACK:8388608` by hand, and the one that instead needed
/// `RUSTFLAGS` for `-C target-feature=+crt-static` (sandbox-conformance) silently lost the
/// stack size and crashed on every `nub run`. A build-script link arg composes with any
/// RUSTFLAGS instead of racing it.
fn emit_windows_stack_reserve() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    // Reserve is address space, not commit — Windows pages it in on demand.
    const RESERVE: usize = 8 * 1024 * 1024;
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bins=/STACK:{RESERVE}");
    } else {
        println!("cargo:rustc-link-arg-bins=-Wl,--stack,{RESERVE}");
    }
}

fn main() {
    emit_windows_stack_reserve();

    let manifest_dir = manifest_dir();
    let docs_dir = manifest_dir.join("../../site/content/docs");
    let docs_dir = docs_dir
        .canonicalize()
        .unwrap_or_else(|e| panic!("docs dir {} not found: {e}", docs_dir.display()));

    if !docs_dir.is_dir() {
        panic!(
            "nub agent docs: expected docs tree at {} (is the site/ checkout present?)",
            docs_dir.display()
        );
    }

    // Collect every .mdx page, keyed by slug, in a BTreeMap for a stable,
    // sorted TOC.
    let mut pages: BTreeMap<String, (String, String)> = BTreeMap::new();
    collect(&docs_dir, &docs_dir, &mut pages);

    if pages.is_empty() {
        panic!(
            "nub agent docs: no .mdx pages found under {}",
            docs_dir.display()
        );
    }

    // Re-run if any docs file changes.
    println!("cargo:rerun-if-changed={}", docs_dir.display());
    // Paired with the run-time lookup in `manifest_dir`: the recorded
    // rerun-if-changed path above is absolute, so without this cargo keeps a
    // sibling worktree's generated DOCS and never reruns the script.
    println!("cargo:rerun-if-env-changed=CARGO_MANIFEST_DIR");

    let mut out = String::new();
    out.push_str("// @generated by build.rs — the baked docs tree. Do not edit.\n");
    out.push_str("pub static DOCS: &[(&str, &str, &str)] = &[\n");
    for (slug, (title, body)) in &pages {
        out.push_str(&format!(
            "    ({}, {}, {}),\n",
            quote(slug),
            quote(title),
            quote(body),
        ));
    }
    out.push_str("];\n");

    // Write into OUT_DIR so it's never checked in.
    let dest = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("docs_baked.rs");
    fs::write(&dest, out).expect("write generated docs table");
}

fn collect(root: &Path, dir: &Path, pages: &mut BTreeMap<String, (String, String)>) {
    let entries = fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
    for entry in entries {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if path.is_dir() {
            collect(root, &path, pages);
        } else if path.extension().and_then(|s| s.to_str()) == Some("mdx") {
            let rel = path
                .strip_prefix(root)
                .expect("page under docs root")
                .with_extension("");
            let slug = slug_for(&rel);
            let raw = fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            let (title, body) = parse(&raw, &slug);
            pages.insert(slug, (title, body));
        }
    }
}

/// Derive the URL-path slug from a path relative to the docs root (no
/// extension). The slug is the exact `/docs/...` href the docs link to, so a
/// markdown link target is a valid `--page` argument. `index` segments collapse
/// to their section root, matching how fumadocs routes `index.mdx`.
fn slug_for(rel: &Path) -> String {
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().replace('\\', "/"))
        .collect();
    let tail = match parts.split_last() {
        // `runtime/index` -> `runtime`, `install/index` -> `install`.
        Some((last, head)) if last == "index" && !head.is_empty() => head.join("/"),
        // Top-level `index` -> "" so the docs root is just `/docs`.
        Some((last, head)) if last == "index" && head.is_empty() => String::new(),
        _ => parts.join("/"),
    };
    if tail.is_empty() {
        "/docs".to_string()
    } else {
        format!("/docs/{tail}")
    }
}

/// Split YAML frontmatter from the body, returning `(title, body)`. The title
/// comes from the frontmatter `title:` field; it falls back to the slug. The
/// returned body has the frontmatter stripped so an agent reads only content.
fn parse(raw: &str, slug: &str) -> (String, String) {
    let normalized = raw.replace("\r\n", "\n");
    if let Some(rest) = normalized.strip_prefix("---\n") {
        if let Some(end) = rest.find("\n---") {
            let front = &rest[..end];
            let after = &rest[end + 4..]; // past "\n---"
            let body = after.strip_prefix('\n').unwrap_or(after);
            let title = front
                .lines()
                .find_map(|l| l.trim().strip_prefix("title:"))
                .map(|t| t.trim().trim_matches(['"', '\'']).to_string())
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| slug.to_string());
            return (title, body.to_string());
        }
    }
    (slug.to_string(), normalized)
}

/// Emit a Rust raw string literal that can hold arbitrary markdown verbatim,
/// picking a `#` count larger than any `"#…` run in the content so the literal
/// can't be closed early.
fn quote(s: &str) -> String {
    // Longest run of `#` that immediately follows a `"` anywhere in `s`.
    let mut max_run = 0usize;
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            let mut run = 0;
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] == b'#' {
                run += 1;
                j += 1;
            }
            max_run = max_run.max(run);
        }
        i += 1;
    }
    let pad = "#".repeat(max_run + 1);
    format!("r{pad}\"{s}\"{pad}")
}
