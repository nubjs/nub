//! `nub init` — scaffold a minimal modern-TS project (nub's own project init,
//! NOT the engine's npm-style manifest write; the verb is deliberately excluded
//! from ENGINE_VERBS). Design record: wiki/commands/init.md. The scaffold is
//! batteries-included: type devDeps + `nub install` by default, and the
//! nub-identity fields (`packageManager` + `devEngines`) from birth — the same
//! fields `nub pm pin` writes, via the same writer.

use std::io::IsTerminal;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

/// `@types/node` range written into the scaffold. Tracks the docs' latest-major
/// rule (AGENTS.md: examples always use the newest Node major) — bump on a new
/// `@types/node` major at release time. `@nubjs/types` needs no constant: it is
/// version-locked to nub itself (`make version` bumps npm/nub-types with the
/// binary), so its range derives from CARGO_PKG_VERSION.
const TYPES_NODE_RANGE: &str = "^26";

/// `typescript` range written into the scaffold. Nub transpiles TS itself, so
/// the compiler package exists for the editor's typechecking (`tsc --noEmit`,
/// tsserver pinned per-project) — bump on a new TypeScript major.
const TYPESCRIPT_RANGE: &str = "^7";

pub(crate) struct InitOptions {
    pub yes: bool,
    pub js: bool,
    pub name: Option<String>,
    pub no_git: bool,
    pub no_install: bool,
    pub force: bool,
    /// Trailing positionals — always an error (pnpm parity: `pnpm init` accepts
    /// no arguments), caught here for the `nubx create-` hint.
    pub args: Vec<String>,
}

pub(crate) fn run_init(opts: InitOptions) -> Result<i32> {
    if let Some(first) = opts.args.first() {
        bail!(
            "nub: `init` does not accept arguments (got \"{first}\")\n\
             \x20\x20(to scaffold from a template: nubx create-{first})"
        );
    }

    let cwd = std::env::current_dir().context("resolving the current directory")?;
    let interactive =
        !opts.yes && std::io::stdin().is_terminal() && std::io::stdout().is_terminal();

    // A basename that sanitizes to nothing (non-Latin script, all-symbol) must
    // not become `"name": ""` — an invalid manifest that fails silently later.
    let default_name = match sanitize_name(
        cwd.file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
            .as_str(),
    ) {
        s if s.is_empty() => "app".to_string(),
        s => s,
    };

    // ── answers: flags win, then prompts (TTY), then defaults ───────────
    let name = match (&opts.name, interactive) {
        (Some(n), _) => {
            let s = sanitize_name(n);
            if s.is_empty() {
                bail!("nub: \"{n}\" is not a valid package name");
            }
            s
        }
        (None, true) => {
            let input = demand::Input::new("Project name")
                .placeholder(&default_name)
                .validation(|s| {
                    if s.is_empty() || !sanitize_name(s).is_empty() {
                        Ok(())
                    } else {
                        Err("not a valid package name")
                    }
                })
                .run();
            match prompt_result(input)? {
                None => return Ok(130),
                Some(s) if s.trim().is_empty() => default_name,
                Some(s) => sanitize_name(&s),
            }
        }
        (None, false) => default_name,
    };
    let typescript = if opts.js {
        false
    } else if interactive {
        let pick = demand::Select::new("Language")
            .option(demand::DemandOption::new(true).label("TypeScript"))
            .option(demand::DemandOption::new(false).label("JavaScript"))
            .run();
        match prompt_result(pick)? {
            None => return Ok(130),
            Some(ts) => ts,
        }
    } else {
        true
    };
    // An existing repo always skips `git init` — no prompt for a settled fact.
    let in_repo = cwd.join(".git").exists();
    let git = if opts.no_git || in_repo {
        false
    } else if interactive {
        match prompt_result(demand::Confirm::new("Initialize a git repository?").run())? {
            None => return Ok(130),
            Some(yes) => yes,
        }
    } else {
        true
    };

    let entry = if typescript { "index.ts" } else { "index.js" };

    // ── refuse-don't-clobber: every conflicting target named, then abort ─
    let mut files: Vec<(&str, String)> =
        vec![("package.json", manifest_json(&name, entry, typescript))];
    if typescript {
        files.push(("tsconfig.json", TSCONFIG.to_string()));
    }
    files.push((entry, "console.log(\"Hello from Nub\");\n".to_string()));
    files.push((
        ".gitignore",
        // Ignore every env file, then re-admit the committed template: the
        // negation is what keeps `.env.example` tracked without un-ignoring
        // `.env.production`/`.env.development`, which routinely hold secrets.
        "node_modules\n.env*\n!.env.example\n*.log\n.DS_Store\n".to_string(),
    ));
    files.push(("README.md", format!("# {name}\n")));

    let conflicts: Vec<&str> = files
        .iter()
        .map(|(f, _)| *f)
        .filter(|f| cwd.join(f).exists())
        .collect();
    if !conflicts.is_empty() && !opts.force {
        bail!(
            "nub: refusing to overwrite existing files: {}\n\
             \x20\x20(pass --force to overwrite)",
            conflicts.join(", ")
        );
    }

    for (file, content) in &files {
        std::fs::write(cwd.join(file), content).with_context(|| format!("writing {file}"))?;
    }

    let git_ran = git && git_init(&cwd);

    println!("created {name}");
    for (file, _) in &files {
        println!("  {file}");
    }
    if git_ran {
        println!("  .git/ initialized");
    }

    // The engine's install output already ends with a blank line; only the
    // no-install path needs its own separator before the next-step line.
    if !opts.no_install {
        println!();
        let code = crate::pm_engine::run_install(crate::pm_engine::InstallFlags::default())?;
        if code != 0 {
            return Ok(code);
        }
    } else {
        println!();
    }
    println!("next: nub {entry}");
    Ok(0)
}

/// The scaffolded manifest, in display order. Identity fields come from the
/// same writer `nub pm pin` uses so the two surfaces can't drift; both the pin
/// and the `@nubjs/types` range derive from the running version (nub-types is
/// release-locked to the binary).
fn manifest_json(name: &str, entry: &str, typescript: bool) -> String {
    let ver = env!("CARGO_PKG_VERSION");
    let mut root = Map::new();
    root.insert("name".into(), Value::String(name.into()));
    root.insert("version".into(), Value::String("0.0.1".into()));
    root.insert("type".into(), Value::String("module".into()));
    crate::pm_engine::use_nub::write_nub_identity_fields(&mut root, Some(ver), ver);
    root.insert("scripts".into(), json!({ "start": format!("nub {entry}") }));
    if typescript {
        root.insert(
            "devDependencies".into(),
            json!({
                "@nubjs/types": format!("^{ver}"),
                "@types/node": TYPES_NODE_RANGE,
                "typescript": TYPESCRIPT_RANGE,
            }),
        );
    }
    let mut out = serde_json::to_string_pretty(&Value::Object(root)).expect("static manifest");
    out.push('\n');
    out
}

/// Rationale for every setting: wiki/commands/init.md (2026-05-19 baseline;
/// `lib` without `"dom"` + the `types` wiring are the 2026-07-21 revision —
/// `@nubjs/types` covers the polyfilled globals, per the docs' TypesSetup
/// convention).
const TSCONFIG: &str = r#"{
  "compilerOptions": {
    "module": "nodenext",
    "moduleResolution": "nodenext",
    "target": "es2024",
    "lib": ["es2024"],
    "types": ["node", "@nubjs/types"],
    "moduleDetection": "force",
    "allowImportingTsExtensions": true,
    "verbatimModuleSyntax": true,
    "isolatedModules": true,
    "noUncheckedSideEffectImports": true,
    "resolveJsonModule": true,
    "esModuleInterop": true,
    "skipLibCheck": true,
    "strict": true,
    "noUncheckedIndexedAccess": true,
    "exactOptionalPropertyTypes": true,
    "noEmit": true
  }
}
"#;

/// Ctrl-C / Esc on a prompt is a deliberate cancel: `None` (caller exits 130,
/// the conventional SIGINT code), never an error report.
fn prompt_result<T>(r: std::io::Result<T>) -> Result<Option<T>> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => Ok(None),
        Err(e) => Err(e).context("reading prompt input"),
    }
}

/// Best-effort `git init` — a missing/failing git degrades the scaffold, it
/// doesn't abort it (the files are already valid without a repo).
fn git_init(cwd: &Path) -> bool {
    match std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(cwd)
        .status()
    {
        Ok(s) if s.success() => true,
        _ => {
            eprintln!("nub: warning: git init failed; skipping");
            false
        }
    }
}

/// npm-valid name from arbitrary input. A scoped name (`@scope/pkg`) sanitizes
/// per-part, preserving `@` and `/`; a bare `@scope`, a second `/`, or a part
/// that sanitizes to nothing is invalid (empty result = the caller falls back
/// or re-prompts), rather than silently mangling to `scope`/`@a/bc`. Anything
/// else sanitizes as one token.
fn sanitize_name(raw: &str) -> String {
    let raw = raw.trim();
    if raw.starts_with('@') {
        return match raw.strip_prefix('@').and_then(|r| r.split_once('/')) {
            // An npm name carries at most one `/`. A second one is malformed
            // input, not something to splice away: `@a/b/c` must not become
            // `@a/bc`, the same silent-mangle this function exists to stop.
            Some((_, pkg)) if pkg.contains('/') => String::new(),
            Some((scope, pkg)) => {
                let (scope, mut pkg) = (sanitize_part(scope), sanitize_part(pkg));
                if scope.is_empty() || pkg.is_empty() {
                    return String::new();
                }
                // npm caps the whole name at 214, and the budget comes off the
                // package part on purpose: truncating the assembled string can
                // cut the `/` off and yield a bare `@scope` — precisely the
                // invalid shape rejected above.
                match 214usize.checked_sub(scope.len() + "@/".len()) {
                    Some(budget) if budget > 0 => {
                        pkg.truncate(budget);
                        format!("@{scope}/{pkg}")
                    }
                    _ => String::new(),
                }
            }
            None => String::new(),
        };
    }
    sanitize_part(raw)
}

/// One name segment: lowercase, whitespace → `-`, keep `[a-z0-9-_.~]`, no
/// leading `.`/`_`/`-`. Empty result = invalid input.
fn sanitize_part(raw: &str) -> String {
    let mut s: String = raw
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_whitespace() { '-' } else { c })
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~'))
        .collect();
    while s.starts_with(['.', '_', '-']) {
        s.remove(0);
    }
    s.truncate(214);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_lowercases_hyphenates_and_strips_leading_punctuation() {
        assert_eq!(sanitize_name("My App"), "my-app");
        assert_eq!(sanitize_name(".hidden"), "hidden");
        assert_eq!(sanitize_name("__weird$name!"), "weirdname");
        assert_eq!(sanitize_name("ok-name_1.2~x"), "ok-name_1.2~x");
        assert_eq!(sanitize_name("🎉🎉"), "");
    }

    #[test]
    fn sanitize_scoped_names_sanitize_per_part_and_reject_bare_scopes() {
        assert_eq!(sanitize_name("@scope/pkg"), "@scope/pkg");
        assert_eq!(sanitize_name("@My Org/My App"), "@my-org/my-app");
        // Whitespace hugging the separator belongs to neither part.
        assert_eq!(sanitize_name("@scope / pkg"), "@scope/pkg");
        // A bare `@scope` (no package part) is invalid — never mangle to
        // `scope`, and a scope that sanitizes to nothing is invalid too.
        assert_eq!(sanitize_name("@scope"), "");
        assert_eq!(sanitize_name("@/pkg"), "");
        assert_eq!(sanitize_name("@🎉/pkg"), "");
        // A second `/` is malformed input, not a mangle-to-fit case.
        assert_eq!(sanitize_name("@a/b/c"), "");
        assert_eq!(sanitize_name("@scope/pkg/"), "");
    }

    #[test]
    fn scoped_length_cap_spends_its_budget_on_the_package_part() {
        // Truncating the assembled name could cut off the `/` and leave a bare
        // `@scope`; the cap has to fall on the package part instead.
        let long = sanitize_name(&format!("@{}/{}", "a".repeat(200), "b".repeat(200)));
        assert_eq!(long.len(), 214);
        assert!(
            long.starts_with(&format!("@{}/", "a".repeat(200))),
            "{long}"
        );
        // A scope so long that no package part fits is invalid, not truncated.
        assert_eq!(sanitize_name(&format!("@{}/pkg", "a".repeat(213))), "");
    }

    #[test]
    fn manifest_carries_identity_pin_and_lockstep_types_range() {
        let v: Value = serde_json::from_str(&manifest_json("demo", "index.ts", true)).unwrap();
        let ver = env!("CARGO_PKG_VERSION");
        assert_eq!(v["packageManager"], format!("nub@{ver}"));
        assert_eq!(v["devEngines"]["packageManager"]["name"], "nub");
        assert_eq!(v["devDependencies"]["@nubjs/types"], format!("^{ver}"));
        assert_eq!(v["devDependencies"]["typescript"], TYPESCRIPT_RANGE);
        assert_eq!(v["scripts"]["start"], "nub index.ts");
    }

    #[test]
    fn js_variant_drops_type_devdeps() {
        let v: Value = serde_json::from_str(&manifest_json("demo", "index.js", false)).unwrap();
        assert!(v.get("devDependencies").is_none());
        assert_eq!(v["scripts"]["start"], "nub index.js");
    }
}
