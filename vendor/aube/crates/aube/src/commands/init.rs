//! `aube init` — create a `package.json` in the current directory.
//!
//! Mirrors `pnpm init`. Non-interactive: writes a minimal default
//! `package.json` (or a bare one with `--bare`) and exits. Fails if a
//! `package.json` already exists — like pnpm, we never clobber.

use clap::{Args, ValueEnum};
use miette::{Context, IntoDiagnostic, miette};
use std::io::Write as _;

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Create a `package.json` with only the bare minimum of required fields
    #[arg(long)]
    pub bare: bool,

    /// Pin the project to the current aube version.
    ///
    /// Adds a `packageManager` field to `package.json`.
    #[arg(long)]
    pub init_package_manager: bool,

    /// Set the module system for the package. Defaults to `commonjs`.
    #[arg(long, value_name = "commonjs|module")]
    pub init_type: Option<InitType>,
    #[command(flatten)]
    pub network: crate::cli_args::NetworkArgs,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
#[value(rename_all = "lowercase")]
pub enum InitType {
    Commonjs,
    Module,
}

pub async fn run(args: InitArgs) -> miette::Result<()> {
    args.network.install_overrides();
    let cwd = crate::dirs::cwd()?;
    let pkg_path = cwd.join("package.json");

    let name = cwd
        .file_name()
        .and_then(|s| s.to_str())
        .map(sanitize_name)
        .unwrap_or_else(|| "my-package".to_string());

    let contents = render(&name, &args);

    // O_CREAT|O_EXCL so the existence check and the write are atomic —
    // two concurrent `aube init` runs in the same dir can't both succeed.
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&pkg_path)
    {
        Ok(mut f) => f
            .write_all(contents.as_bytes())
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to write {}", pkg_path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(miette!(
                "package.json already exists at {}",
                pkg_path.display()
            ));
        }
        Err(e) => {
            return Err(miette!("failed to write {}: {e}", pkg_path.display()));
        }
    }

    println!("Wrote to {}\n\n{}", pkg_path.display(), contents);
    Ok(())
}

/// Build the JSON body by hand so field order is stable without pulling
/// `serde_json/preserve_order` into the workspace just for this command.
fn render(name: &str, args: &InitArgs) -> String {
    let mut out = String::from("{\n");
    let mut first = true;
    let push = |out: &mut String, first: &mut bool, line: &str| {
        if !*first {
            out.push_str(",\n");
        }
        *first = false;
        out.push_str("  ");
        out.push_str(line);
    };

    push(
        &mut out,
        &mut first,
        &format!("\"name\": {}", json_string(name)),
    );
    push(&mut out, &mut first, "\"version\": \"1.0.0\"");

    if args.init_package_manager {
        // packageManager pin from the active embedder (standalone aube →
        // "aube@<version>").
        let id = aube_util::embedder();
        push(
            &mut out,
            &mut first,
            &format!("\"packageManager\": \"{}@{}\"", id.name, id.version),
        );
    }

    if !args.bare {
        push(&mut out, &mut first, "\"description\": \"\"");
        push(&mut out, &mut first, "\"main\": \"index.js\"");
        push(
            &mut out,
            &mut first,
            "\"scripts\": {\n    \"test\": \"echo \\\"Error: no test specified\\\" && exit 1\"\n  }",
        );
        push(&mut out, &mut first, "\"keywords\": []");
        push(&mut out, &mut first, "\"author\": \"\"");
        push(&mut out, &mut first, "\"license\": \"ISC\"");
    }

    // `commonjs` is Node's default when `"type"` is absent — no field needed,
    // so `InitType::Commonjs` is intentionally handled as a no-op here.
    if matches!(args.init_type, Some(InitType::Module)) {
        push(&mut out, &mut first, "\"type\": \"module\"");
    }

    out.push('\n');
    out.push('}');
    out.push('\n');
    out
}

/// Lowercase + strip characters that npm rejects in `name`. Intentionally
/// lenient: if the result is empty we fall back to a placeholder so we
/// never write an invalid manifest.
fn sanitize_name(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut prev_dash = false;
    for c in raw.chars().map(|c| c.to_ascii_lowercase()) {
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
            out.push(c);
            prev_dash = c == '-';
        } else if !prev_dash {
            // Collapse runs of non-allowed chars (spaces, slashes, etc.) into a single `-`
            // so "My Cool Project" → "my-cool-project" rather than "mycoolproject".
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_matches(|c: char| matches!(c, '.' | '_' | '-'));
    if trimmed.is_empty() {
        "my-package".to_string()
    } else {
        trimmed.to_string()
    }
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> InitArgs {
        InitArgs {
            bare: false,
            init_package_manager: false,
            init_type: None,
            network: Default::default(),
        }
    }

    #[test]
    fn default_render_is_valid_json_with_expected_fields() {
        let s = render("demo", &args());
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["name"], "demo");
        assert_eq!(v["version"], "1.0.0");
        assert_eq!(v["main"], "index.js");
        assert_eq!(v["license"], "ISC");
        assert_eq!(
            v["scripts"]["test"],
            "echo \"Error: no test specified\" && exit 1"
        );
        assert!(v.get("type").is_none());
    }

    #[test]
    fn bare_render_drops_optional_fields() {
        let a = InitArgs {
            bare: true,
            ..args()
        };
        let s = render("demo", &a);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["name"], "demo");
        assert_eq!(v["version"], "1.0.0");
        assert!(v.get("main").is_none());
        assert!(v.get("scripts").is_none());
        assert!(v.get("license").is_none());
    }

    #[test]
    fn init_type_module_sets_type_field() {
        let a = InitArgs {
            init_type: Some(InitType::Module),
            ..args()
        };
        let v: serde_json::Value = serde_json::from_str(&render("demo", &a)).unwrap();
        assert_eq!(v["type"], "module");
    }

    #[test]
    fn init_package_manager_pins_aube() {
        let a = InitArgs {
            init_package_manager: true,
            ..args()
        };
        let v: serde_json::Value = serde_json::from_str(&render("demo", &a)).unwrap();
        assert_eq!(
            v["packageManager"],
            format!("aube@{}", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn sanitize_name_lowercases_and_hyphenates() {
        assert_eq!(sanitize_name("My Project"), "my-project");
        assert_eq!(sanitize_name("My Cool Project"), "my-cool-project");
        assert_eq!(sanitize_name("foo-bar"), "foo-bar");
        assert_eq!(sanitize_name("foo/bar"), "foo-bar");
        assert_eq!(sanitize_name("..."), "my-package");
    }
}
