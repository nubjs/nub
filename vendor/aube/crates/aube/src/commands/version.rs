//! `aube version` — bump the `version` field in `package.json`, and
//! optionally create a git commit + tag.
//!
//! Mirrors `pnpm version` / `npm version`. The version bump happens as a
//! targeted text edit on the raw `package.json` so field order, indentation,
//! and surrounding formatting are preserved — we only rewrite the string
//! value that follows the `"version"` key.

use clap::Args;
use miette::{Context, IntoDiagnostic, miette};
use node_semver::{Identifier, Version};
use std::path::Path;
use std::process::Command;

#[derive(Debug, Args)]
pub struct VersionArgs {
    /// Bump keyword or an explicit version string.
    ///
    /// Accepts `major`, `minor`, `patch`, `premajor`, `preminor`,
    /// `prepatch`, `prerelease`, or an explicit version. When omitted,
    /// prints the current version.
    pub new_version: Option<String>,

    /// Allow setting the version to its current value without erroring.
    #[arg(long)]
    pub allow_same_version: bool,

    /// Skip `preversion` / `version` / `postversion` lifecycle scripts.
    #[arg(long)]
    pub ignore_scripts: bool,

    /// Emit the result as JSON instead of `v<version>` text.
    #[arg(long)]
    pub json: bool,

    /// Commit message template.
    ///
    /// `%s` is replaced with the new version. Defaults to `v%s`.
    #[arg(short = 'm', long, value_name = "MSG")]
    pub message: Option<String>,

    /// Skip git pre-commit / commit-msg hooks (passes `--no-verify`).
    #[arg(long = "no-commit-hooks", action = clap::ArgAction::SetTrue)]
    pub no_commit_hooks: bool,

    /// Don't create a git commit or tag.
    ///
    /// By default `aube version` commits the manifest change and
    /// tags it `v<version>`.
    #[arg(long = "no-git-tag-version", action = clap::ArgAction::SetTrue)]
    pub no_git_tag_version: bool,

    /// Prerelease identifier to use with the `pre*` keywords (e.g. `rc`).
    #[arg(long, value_name = "ID")]
    pub preid: Option<String>,

    /// GPG-sign the created tag (`git tag -s`).
    #[arg(long)]
    pub sign_git_tag: bool,
}

pub async fn run(args: VersionArgs) -> miette::Result<()> {
    let cwd = crate::dirs::project_root()?;
    let manifest_path = cwd.join("package.json");
    let raw = std::fs::read_to_string(&manifest_path)
        .into_diagnostic()
        .wrap_err("failed to read package.json")?;

    let current = extract_version(&raw)
        .ok_or_else(|| miette!("package.json has no `version` field — nothing to bump"))?;

    let Some(bump) = args.new_version.as_deref() else {
        // Bare `aube version` prints the current version and exits.
        if args.json {
            println!("{{\n  \"version\": \"{current}\"\n}}");
        } else {
            println!("{current}");
        }
        return Ok(());
    };

    let new_version = compute_new_version(&current, bump, args.preid.as_deref())?;
    if new_version == current && !args.allow_same_version {
        return Err(miette!(
            "version not changed: already at {current} (pass --allow-same-version to force)"
        ));
    }

    // npm/pnpm order: `preversion` fires BEFORE the manifest is
    // rewritten so the script sees the outgoing version and can
    // abort the bump (e.g. "refuse to bump while tests are red"). We
    // re-read the manifest between each hook so `npm_package_version`
    // reflects the right state of the world — preversion sees the
    // old version, version/postversion see the new one.
    if !args.ignore_scripts {
        let manifest = super::pack::read_root_manifest(&cwd)?;
        super::pack::run_root_lifecycle_script(&cwd, &manifest, "preversion").await?;
    }

    // Re-read the raw file *after* preversion: a common idiom is
    // `preversion: 'npm test && git add CHANGELOG.md'`, but hooks
    // can also mutate the manifest directly (formatting, touching
    // related fields, …). Using the pre-hook `raw` for
    // `replace_version` would silently discard those edits on the
    // atomic write.
    let raw = std::fs::read_to_string(&manifest_path)
        .into_diagnostic()
        .wrap_err("failed to read package.json")?;

    let updated = replace_version(&raw, &new_version)
        .ok_or_else(|| miette!("failed to locate version string in package.json"))?;
    aube_util::fs_atomic::atomic_write(&manifest_path, updated.as_bytes())
        .into_diagnostic()
        .wrap_err("failed to write package.json")?;

    // `version` fires AFTER the manifest edit but BEFORE the git
    // commit. Scripts usually regenerate derived files here (e.g.
    // `version: 'git add CHANGELOG.md'`) so they're included in the
    // version tag's tree. Matches npm docs.
    if !args.ignore_scripts {
        let manifest = super::pack::read_root_manifest(&cwd)?;
        super::pack::run_root_lifecycle_script(&cwd, &manifest, "version").await?;
    }

    // Skip git ops when the version hasn't actually changed (e.g.
    // `--allow-same-version` bumping to the current version) — otherwise
    // `git commit` would exit with "nothing to commit" and fail the whole
    // command after a successful manifest write.
    let tagged = !args.no_git_tag_version && new_version != current && is_git_repo(&cwd);
    if tagged {
        let message = args
            .message
            .as_deref()
            .unwrap_or("v%s")
            .replace("%s", &new_version);
        git_commit_and_tag(
            &cwd,
            &new_version,
            &message,
            args.no_commit_hooks,
            args.sign_git_tag,
        )?;
    }

    // `postversion` fires last — after the git commit + tag (if any;
    // `--no-git-tag-version` or a clean-same-version run skips that
    // step, in which case postversion just fires after the manifest
    // write). Common idiom when tagging is enabled is
    // `postversion: 'git push && git push --tags'`.
    if !args.ignore_scripts {
        let manifest = super::pack::read_root_manifest(&cwd)?;
        super::pack::run_root_lifecycle_script(&cwd, &manifest, "postversion").await?;
    }

    if args.json {
        println!("{{\n  \"version\": \"{new_version}\",\n  \"previous\": \"{current}\"\n}}");
    } else {
        println!("v{new_version}");
    }
    Ok(())
}

/// Pull the current `version` string out of raw JSON without round-tripping
/// through serde (which would reorder and reformat the whole manifest).
fn extract_version(raw: &str) -> Option<String> {
    let (start, end) = find_version_value(raw)?;
    Some(raw[start..end].to_string())
}

/// Replace the version string value in the raw manifest, preserving every
/// other byte of the file.
fn replace_version(raw: &str, new_version: &str) -> Option<String> {
    let (start, end) = find_version_value(raw)?;
    let mut out = String::with_capacity(raw.len() + new_version.len());
    out.push_str(&raw[..start]);
    out.push_str(new_version);
    out.push_str(&raw[end..]);
    Some(out)
}

/// Locate the byte range of the string value following the top-level
/// `"version"` key. Walks the manifest as a tiny JSON parser so that
/// nested `"version"` keys (e.g. inside `engines`) and string **values**
/// that happen to contain the word `"version"` are both skipped — only a
/// *key* at the top level counts.
fn find_version_value(raw: &str) -> Option<(usize, usize)> {
    let bytes = raw.as_bytes();
    let mut i = skip_ws(bytes, 0);
    if i >= bytes.len() || bytes[i] != b'{' {
        return None;
    }
    i += 1;

    loop {
        i = skip_ws(bytes, i);
        if i >= bytes.len() {
            return None;
        }
        if bytes[i] == b'}' {
            return None;
        }
        // Read the key string.
        if bytes[i] != b'"' {
            return None;
        }
        let key_start = i + 1;
        let key_end = scan_string_end(bytes, key_start)?;
        let key = &raw[key_start..key_end];
        i = key_end + 1;

        i = skip_ws(bytes, i);
        if i >= bytes.len() || bytes[i] != b':' {
            return None;
        }
        i += 1;
        i = skip_ws(bytes, i);

        if key == "version" {
            if i >= bytes.len() || bytes[i] != b'"' {
                return None;
            }
            let value_start = i + 1;
            let value_end = scan_string_end(bytes, value_start)?;
            return Some((value_start, value_end));
        }

        // Not the key we want: skip the whole value, then a trailing comma.
        i = skip_value(bytes, i)?;
        i = skip_ws(bytes, i);
        if i < bytes.len() && bytes[i] == b',' {
            i += 1;
        }
    }
}

/// Advance past ASCII whitespace (space, tab, newline, carriage return).
/// JSON allows any of these between tokens.
fn skip_ws(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    i
}

/// Given an index pointing at the first character *inside* a JSON string
/// (i.e. one past the opening `"`), return the index of the closing `"`.
/// Handles `\\` and `\"` escape sequences.
fn scan_string_end(bytes: &[u8], mut i: usize) -> Option<usize> {
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

/// Skip a single JSON value (string, number, bool, null, object, or
/// array) starting at `i`, returning the index just past the value.
/// Object/array bodies are walked with a depth counter so nested values
/// don't confuse the scan.
fn skip_value(bytes: &[u8], mut i: usize) -> Option<usize> {
    if i >= bytes.len() {
        return None;
    }
    match bytes[i] {
        b'"' => {
            let end = scan_string_end(bytes, i + 1)?;
            Some(end + 1)
        }
        b'{' | b'[' => {
            let opener = bytes[i];
            let closer = if opener == b'{' { b'}' } else { b']' };
            let mut depth = 1;
            i += 1;
            while i < bytes.len() && depth > 0 {
                match bytes[i] {
                    b'"' => {
                        let end = scan_string_end(bytes, i + 1)?;
                        i = end + 1;
                        continue;
                    }
                    c if c == opener => depth += 1,
                    c if c == closer => depth -= 1,
                    _ => {}
                }
                i += 1;
            }
            if depth != 0 {
                return None;
            }
            Some(i)
        }
        _ => {
            // number / true / false / null — read until the next
            // structural character.
            while i < bytes.len()
                && !matches!(bytes[i], b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r')
            {
                i += 1;
            }
            Some(i)
        }
    }
}

/// Apply an npm-style bump keyword, or validate and normalize an explicit
/// version string.
fn compute_new_version(current: &str, bump: &str, preid: Option<&str>) -> miette::Result<String> {
    let current_ver = Version::parse(current)
        .map_err(|e| miette!("current version {current} is not valid semver: {e}"))?;

    let bumped = match bump {
        "major" => bump_major(&current_ver),
        "minor" => bump_minor(&current_ver),
        "patch" => bump_patch(&current_ver),
        // `pre*` variants always advance the corresponding component and
        // then attach a fresh prerelease — unlike the bare `major`/`minor`/
        // `patch` helpers, which have a special case that drops a leading
        // prerelease instead of incrementing. Going through those helpers
        // would leave e.g. `premajor` of `2.0.0-rc.0` as `2.0.0-0` rather
        // than the npm-matching `3.0.0-0`.
        "premajor" => prerelease_of(Version::new(current_ver.major + 1, 0, 0), preid),
        "preminor" => prerelease_of(
            Version::new(current_ver.major, current_ver.minor + 1, 0),
            preid,
        ),
        "prepatch" => prerelease_of(
            Version::new(current_ver.major, current_ver.minor, current_ver.patch + 1),
            preid,
        ),
        "prerelease" => bump_prerelease(&current_ver, preid),
        explicit => {
            Version::parse(explicit).map_err(|e| miette!("invalid version {explicit:?}: {e}"))?
        }
    };
    Ok(bumped.to_string())
}

fn bump_major(v: &Version) -> Version {
    // If the current version has a prerelease and the core is already
    // bumped (minor=0, patch=0), the release drops the prerelease without
    // advancing the major. Mirrors npm's `inc('major')`.
    if v.minor == 0 && v.patch == 0 && v.is_prerelease() {
        Version::new(v.major, 0, 0)
    } else {
        Version::new(v.major + 1, 0, 0)
    }
}

fn bump_minor(v: &Version) -> Version {
    if v.patch == 0 && v.is_prerelease() {
        Version::new(v.major, v.minor, 0)
    } else {
        Version::new(v.major, v.minor + 1, 0)
    }
}

fn bump_patch(v: &Version) -> Version {
    if v.is_prerelease() {
        Version::new(v.major, v.minor, v.patch)
    } else {
        Version::new(v.major, v.minor, v.patch + 1)
    }
}

fn prerelease_of(core: Version, preid: Option<&str>) -> Version {
    let mut out = core;
    out.pre_release = initial_prerelease(preid);
    out
}

fn initial_prerelease(preid: Option<&str>) -> Vec<Identifier> {
    match preid {
        Some(id) if !id.is_empty() => vec![
            Identifier::AlphaNumeric(id.to_string()),
            Identifier::Numeric(0),
        ],
        _ => vec![Identifier::Numeric(0)],
    }
}

fn bump_prerelease(current: &Version, preid: Option<&str>) -> Version {
    // Already in a prerelease: increment the trailing numeric component
    // (or append `.0` if no numeric tail exists). If switching preids
    // (e.g. from `rc` to `beta`), reset to `<preid>.0`.
    if current.is_prerelease() {
        if let Some(id) = preid
            && !id.is_empty()
        {
            let current_preid = current.pre_release.first().and_then(|ident| match ident {
                Identifier::AlphaNumeric(s) => Some(s.as_str()),
                Identifier::Numeric(_) => None,
            });
            if current_preid != Some(id) {
                return Version {
                    major: current.major,
                    minor: current.minor,
                    patch: current.patch,
                    build: Vec::new(),
                    pre_release: initial_prerelease(Some(id)),
                };
            }
        }
        let mut out = Version {
            major: current.major,
            minor: current.minor,
            patch: current.patch,
            build: Vec::new(),
            pre_release: current.pre_release.clone(),
        };
        if let Some(Identifier::Numeric(n)) = out.pre_release.last().cloned() {
            *out.pre_release.last_mut().unwrap() = Identifier::Numeric(n + 1);
        } else {
            out.pre_release.push(Identifier::Numeric(0));
        }
        return out;
    }
    // Not yet a prerelease — bump patch and add `-<preid>.0` / `-0`.
    let mut out = Version::new(current.major, current.minor, current.patch + 1);
    out.pre_release = initial_prerelease(preid);
    out
}

fn is_git_repo(cwd: &Path) -> bool {
    let out = Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(cwd)
        .output();
    matches!(out, Ok(o) if o.status.success())
}

fn git_commit_and_tag(
    cwd: &Path,
    new_version: &str,
    message: &str,
    no_commit_hooks: bool,
    sign: bool,
) -> miette::Result<()> {
    run_git(cwd, &["add", "package.json"])?;

    let mut commit_args = vec!["commit", "-m", message];
    if no_commit_hooks {
        commit_args.push("--no-verify");
    }
    run_git(cwd, &commit_args)?;

    let tag = format!("v{new_version}");
    let mut tag_args = vec!["tag"];
    if sign {
        tag_args.push("-s");
    } else {
        tag_args.push("-a");
    }
    tag_args.push(&tag);
    tag_args.push("-m");
    tag_args.push(message);
    run_git(cwd, &tag_args)?;
    Ok(())
}

fn run_git(cwd: &Path, args: &[&str]) -> miette::Result<()> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to run git {}", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(miette!("git {} failed: {}", args.join(" "), stderr.trim()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_top_level_version() {
        let raw = r#"{
  "name": "demo",
  "version": "1.2.3",
  "engines": { "node": ">=18" }
}"#;
        assert_eq!(extract_version(raw).as_deref(), Some("1.2.3"));
    }

    #[test]
    fn replace_preserves_surrounding_formatting() {
        let raw = r#"{
  "name": "demo",
  "version": "1.2.3",
  "scripts": {}
}"#;
        let updated = replace_version(raw, "1.2.4").unwrap();
        assert!(updated.contains("\"version\": \"1.2.4\""));
        assert!(updated.contains("\"scripts\": {}"));
    }

    #[test]
    fn ignores_version_appearing_as_a_value() {
        // A string *value* spelled "version" must not trip the scanner.
        let raw = r#"{
  "description": "version",
  "scripts": { "build": "echo ok" },
  "version": "1.0.0"
}"#;
        assert_eq!(extract_version(raw).as_deref(), Some("1.0.0"));
        let updated = replace_version(raw, "2.0.0").unwrap();
        assert!(updated.contains("\"version\": \"2.0.0\""));
        assert!(updated.contains("\"description\": \"version\""));
    }

    #[test]
    fn tolerates_newline_between_colon_and_value() {
        let raw = "{\n  \"name\": \"demo\",\n  \"version\":\n    \"1.2.3\"\n}";
        assert_eq!(extract_version(raw).as_deref(), Some("1.2.3"));
    }

    #[test]
    fn nested_version_is_ignored() {
        // A nested `"version"` key (inside `engines`) must not be mistaken
        // for the top-level one.
        let raw = r#"{
  "engines": { "version": "18" },
  "version": "1.2.3"
}"#;
        let updated = replace_version(raw, "2.0.0").unwrap();
        assert!(updated.contains("\"version\": \"2.0.0\""));
        assert!(updated.contains("\"version\": \"18\""));
    }

    #[test]
    fn bumps_patch() {
        assert_eq!(
            compute_new_version("1.2.3", "patch", None).unwrap(),
            "1.2.4"
        );
    }

    #[test]
    fn bumps_minor_and_resets_patch() {
        assert_eq!(
            compute_new_version("1.2.3", "minor", None).unwrap(),
            "1.3.0"
        );
    }

    #[test]
    fn bumps_major_and_resets_minor_patch() {
        assert_eq!(
            compute_new_version("1.2.3", "major", None).unwrap(),
            "2.0.0"
        );
    }

    #[test]
    fn premajor_with_preid() {
        assert_eq!(
            compute_new_version("1.2.3", "premajor", Some("rc")).unwrap(),
            "2.0.0-rc.0"
        );
    }

    #[test]
    fn pre_star_on_prerelease_input_advances_component() {
        // Regression: these used to strip the prerelease without bumping
        // because they piggy-backed on `bump_major`/`bump_minor`/`bump_patch`.
        assert_eq!(
            compute_new_version("2.0.0-rc.0", "premajor", None).unwrap(),
            "3.0.0-0"
        );
        assert_eq!(
            compute_new_version("1.3.0-rc.0", "preminor", None).unwrap(),
            "1.4.0-0"
        );
        assert_eq!(
            compute_new_version("1.2.4-rc.0", "prepatch", None).unwrap(),
            "1.2.5-0"
        );
    }

    #[test]
    fn prerelease_increments_numeric_tail() {
        assert_eq!(
            compute_new_version("1.2.3-rc.0", "prerelease", None).unwrap(),
            "1.2.3-rc.1"
        );
    }

    #[test]
    fn prerelease_switches_preid() {
        assert_eq!(
            compute_new_version("1.2.3-rc.2", "prerelease", Some("beta")).unwrap(),
            "1.2.3-beta.0"
        );
    }

    #[test]
    fn explicit_version_accepted() {
        assert_eq!(
            compute_new_version("1.2.3", "9.9.9", None).unwrap(),
            "9.9.9"
        );
    }

    #[test]
    fn invalid_explicit_rejected() {
        assert!(compute_new_version("1.2.3", "not-a-version", None).is_err());
    }
}
