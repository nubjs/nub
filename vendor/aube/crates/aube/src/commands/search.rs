//! `aube search <query> [...]` — full-text package search against the
//! registry's `/-/v1/search` endpoint. Mirrors `npm search` / `pnpm search`.
//!
//! Output mirrors pnpm's human format (name, description, version line,
//! maintainers, keywords, package URL) and supports `--json` (the raw
//! package objects) and `--search-limit`.

use clap::Args;
use miette::miette;
use serde_json::Value;

use crate::commands::make_client;

#[derive(Debug, Args)]
pub struct SearchArgs {
    /// Search terms. Joined with spaces into a single query.
    #[arg(required = true)]
    pub query: Vec<String>,

    /// Print the raw package objects as JSON.
    #[arg(long)]
    pub json: bool,

    /// Maximum number of results to show (default: 20).
    #[arg(long, value_name = "N", default_value_t = 20)]
    pub search_limit: u32,

    #[command(flatten)]
    pub network: crate::cli_args::NetworkArgs,
}

pub async fn run(args: SearchArgs) -> miette::Result<()> {
    args.network.install_overrides();
    let query = args.query.join(" ");
    if query.trim().is_empty() {
        return Err(miette!("search query is required"));
    }

    let cwd = crate::dirs::project_root_or_cwd().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let client = make_client(&cwd);

    let packages = client
        .search(&query, args.search_limit)
        .await
        .map_err(|e| miette!("search failed: {e}"))?;

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&packages).unwrap_or_else(|_| "[]".to_string())
        );
        return Ok(());
    }

    if packages.is_empty() {
        println!("No packages found");
        return Ok(());
    }

    let blocks: Vec<String> = packages.iter().map(format_package).collect();
    println!("{}", blocks.join("\n\n"));
    Ok(())
}

fn format_package(pkg: &Value) -> String {
    let name = pkg.get("name").and_then(Value::as_str).unwrap_or("");
    let mut lines: Vec<String> = vec![name.to_string()];

    if let Some(desc) = pkg.get("description").and_then(Value::as_str)
        && !desc.is_empty()
    {
        lines.push(desc.to_string());
    }

    let version = pkg.get("version").and_then(Value::as_str).unwrap_or("");
    let mut version_line = vec![format!("Version {version}")];
    if let Some(date) = pkg.get("date").and_then(Value::as_str)
        && let Some(day) = date.split('T').next()
    {
        version_line.push(format!("published {day}"));
    }
    if let Some(author) = author_name(pkg) {
        version_line.push(format!("by {author}"));
    }
    lines.push(version_line.join(" "));

    if let Some(maintainers) = pkg.get("maintainers").and_then(Value::as_array) {
        let names: Vec<&str> = maintainers
            .iter()
            .filter_map(|m| m.get("username").and_then(Value::as_str))
            .collect();
        if !names.is_empty() {
            lines.push(format!("Maintainers: {}", names.join(", ")));
        }
    }

    if let Some(keywords) = pkg.get("keywords").and_then(Value::as_array) {
        let kws: Vec<&str> = keywords.iter().filter_map(Value::as_str).collect();
        if !kws.is_empty() {
            lines.push(format!("Keywords: {}", kws.join(", ")));
        }
    }

    // Neutral, registry-canonical package URL (pnpm emits a pnpm-branded
    // npmx.dev link — not appropriate here).
    if !name.is_empty() {
        lines.push(format!("https://www.npmjs.com/package/{name}"));
    }

    lines.join("\n")
}

fn author_name(pkg: &Value) -> Option<String> {
    if let Some(author) = pkg.get("author") {
        if let Some(name) = author.get("name").and_then(Value::as_str) {
            return Some(name.to_string());
        }
        if let Some(s) = author.as_str() {
            return Some(s.to_string());
        }
    }
    pkg.get("publisher")
        .and_then(|p| p.get("username"))
        .and_then(Value::as_str)
        .map(str::to_string)
}
