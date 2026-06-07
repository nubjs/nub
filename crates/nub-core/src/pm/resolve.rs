//! The single PM pin reader. Every consumer that needs to know "which package
//! manager, which version" goes through here — there is no second pin parser.
//!
//! Resolution sources, in precedence order:
//!   1. `.yarnrc.yml`'s `yarnPath:` — a committed Berry release short-circuits
//!      everything (run that file directly; never provision).
//!   2. `package.json#packageManager` — the Corepack standard.
//!   3. `package.json#devEngines.packageManager` (object form only).
//!
//! Unpinned (none of the above) is a valid state: [`resolve_target`] /
//! [`resolve_pin`] return `None`, and provisioning falls back to lockfile
//! inference elsewhere.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::Pm;
use crate::workspace::detect::detect_project;

/// A resolved PM pin: which manager, and the version spec if one was stated.
///
/// `version` is `None` when the manager is known but the version is not — e.g.
/// inferred from a lockfile rather than a `packageManager` field. There is no
/// `Exact`/`Inferred` enum; a present `String` is the literal spec (Corepack
/// hash suffix kept verbatim — see [`classify_yarn`] / [`parse_spec`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PmPin {
    pub pm: Pm,
    pub version: Option<String>,
}

/// What provisioning should do with a resolved project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PmTarget {
    /// A committed Berry release at this path — exec it directly, never download.
    YarnPath(PathBuf),
    /// Provision (download/cache) the pinned manager.
    Provision(PmPin),
    /// A bare Berry pin (`yarn@>=2`) with no `yarnPath` — Nub can't synthesize a
    /// Berry release, so the engine surfaces a clear error.
    BerryNoYarnPath,
}

/// Resolve what to run for the project at `cwd`. `None` means unpinned (no
/// `.yarnrc.yml yarnPath`, no `packageManager`, no `devEngines.packageManager`).
pub fn resolve_target(cwd: &Path) -> Option<PmTarget> {
    // 1. A committed Berry release short-circuits everything.
    if let Some(path) = read_yarn_path(cwd) {
        return Some(PmTarget::YarnPath(path));
    }

    // 2. + 3. The pin from packageManager / devEngines.
    let pin = resolve_pin(cwd)?;
    if pin.pm == Pm::YarnBerry {
        // Berry pinned but no committed release to run — unresolvable.
        return Some(PmTarget::BerryNoYarnPath);
    }
    Some(PmTarget::Provision(pin))
}

/// Resolve just the pin (for `nub pm which` / `nub pm update`). `None` means no
/// `packageManager` and no `devEngines.packageManager` field.
pub fn resolve_pin(cwd: &Path) -> Option<PmPin> {
    let manifest = detect_project(cwd)?.manifest;

    // `packageManager` wins; `devEngines.packageManager` (object form) is the
    // fallback. Both are parsed by the same spec parser.
    if let Some(spec) = manifest.get("packageManager").and_then(|v| v.as_str()) {
        return parse_spec(spec).ok();
    }
    // devEngines carries name + version as separate keys; feed them straight to
    // the shared classifier. A name with no version is valid here (version stays
    // `None`), so unlike `packageManager` there's no required-version check.
    let dev = manifest.get("devEngines")?.get("packageManager")?;
    let name = dev.get("name")?.as_str()?;
    let version = dev.get("version").and_then(|v| v.as_str());
    classify(name, version).ok()
}

/// Write `packageManager` into the workspace-root `package.json` (the same
/// workspace-root rule [`crate::version_management`]'s pin uses). Preserves
/// sibling keys via a serde round-trip. Errors if no `package.json` exists at the
/// target dir — Nub never creates one (no silent scaffolding).
pub fn write_pin(pm: Pm, version: &str, cwd: &Path) -> Result<PathBuf> {
    let dir = pin_target_dir(cwd);
    let path = dir.join("package.json");
    if !path.is_file() {
        bail!(
            "no package.json at {} to write packageManager into",
            dir.display()
        );
    }

    let content =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let mut manifest: serde_json::Value =
        serde_json::from_str(&content).with_context(|| format!("parsing {}", path.display()))?;

    let obj = manifest
        .as_object_mut()
        .with_context(|| format!("{} is not a JSON object", path.display()))?;
    obj.insert(
        "packageManager".to_string(),
        serde_json::Value::String(format!("{pm}@{version}")),
    );

    let mut serialized = serde_json::to_string_pretty(&manifest)
        .with_context(|| format!("serializing {}", path.display()))?;
    serialized.push('\n');
    std::fs::write(&path, serialized).with_context(|| format!("writing {}", path.display()))?;

    Ok(path)
}

/// Resolve where a pin is written: the workspace root if one is above `cwd`,
/// else the nearest project root, else `cwd`. Mirrors `manage::pin_target_dir`'s
/// rule (a `packageManager` pin is repo-wide, like the Node pin).
fn pin_target_dir(cwd: &Path) -> PathBuf {
    if let Some(project) = detect_project(cwd) {
        return project.workspace_root.unwrap_or(project.root);
    }
    cwd.to_path_buf()
}

/// Parse a `packageManager`-style spec (`name@version`). The `version` is
/// mandatory in this strict form (Corepack requires it); a value with no `@`
/// errors naming the required `name@version` shape. The Corepack hash suffix
/// (`yarn@4.2.2+sha512.xxxx`) is kept verbatim in `version` — resolution never
/// lies about what was written; the engine strips it before download.
///
/// Public so `nub pm switch <pm>@<v>` parses through the SAME pin parser the
/// `packageManager` reader uses — there is no second spec parser.
pub fn parse_spec(spec: &str) -> Result<PmPin> {
    let spec = spec.trim();
    let (name, version) = spec.split_once('@').with_context(|| {
        format!("packageManager \"{spec}\" must be in name@version form (e.g. pnpm@9.1.0)")
    })?;
    classify(name, Some(version))
}

/// Map a `(name, version)` pair to a [`PmPin`], applying the yarn classic/berry
/// split. The version (with any hash suffix) is stored verbatim.
fn classify(name: &str, version: Option<&str>) -> Result<PmPin> {
    let pm = match name {
        "npm" => Pm::Npm,
        "pnpm" => Pm::Pnpm,
        // A pinned yarn has a version, so the yarnrc signal is irrelevant here.
        "yarn" => classify_yarn(version, false),
        other => bail!("unsupported package manager \"{other}\" — nub manages npm, pnpm, and yarn"),
    };
    Ok(PmPin {
        pm,
        version: version.map(str::to_string),
    })
}

/// The single yarn classic-vs-Berry classifier, shared by both routes:
///   - pinned: a `version` is present → major `>= 2` is Berry.
///   - lockfile-inferred (no pin): `version` is `None` → a sibling `.yarnrc.yml`
///     (`yarnrc_present`) means Berry; otherwise classic.
///
/// P1's lockfile-inference route calls this with `version = None` and the
/// `.yarnrc.yml` presence flag; the pinned route calls it with the version.
pub fn classify_yarn(version: Option<&str>, yarnrc_present: bool) -> Pm {
    match version.and_then(yarn_major) {
        Some(major) if major >= 2 => Pm::YarnBerry,
        Some(_) => Pm::Yarn,
        // No usable version: fall back to the .yarnrc.yml signal.
        None if yarnrc_present => Pm::YarnBerry,
        None => Pm::Yarn,
    }
}

/// Extract the major version from a yarn spec, tolerating the Corepack hash
/// suffix (`4.2.2+sha512.…`) and partial versions (`4`, `4.2`). The major is the
/// leading run of digits before the first `.`, `+`, or `-`.
fn yarn_major(version: &str) -> Option<u32> {
    let leading: String = version.chars().take_while(|c| c.is_ascii_digit()).collect();
    leading.parse().ok()
}

/// Read the single `yarnPath:` key from `.yarnrc.yml` at the project root,
/// resolved relative to that root. A committed Berry release lives there
/// (`.yarn/releases/yarn-4.2.2.cjs`).
///
/// This is a hand line-scan for one flat top-level `yarnPath:` key, mirroring
/// `workspace::filter::read_pnpm_workspace`'s idiom — nub-core takes no YAML
/// dependency. LIMITATION: only a single, top-level, unindented `yarnPath:`
/// entry is recognized; a nested or multi-document form is not (no real-world
/// `.yarnrc.yml` nests `yarnPath`).
fn read_yarn_path(cwd: &Path) -> Option<PathBuf> {
    let root = detect_project(cwd)?.root;
    let path = root.join(".yarnrc.yml");
    let content = std::fs::read_to_string(&path).ok()?;
    for line in content.lines() {
        // Top-level keys are unindented; a leading space means nested config.
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("yarnPath:") {
            let value = rest.trim().trim_matches('"').trim_matches('\'');
            if !value.is_empty() {
                return Some(root.join(value));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A unique temp dir under the system temp root (NOT under $HOME, so the
    /// detect walk-up can't escape into a stray ancestor package.json). Mirrors
    /// `manage.rs`'s `tmpdir`.
    fn tmpdir(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "nub-pm-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_pkg(dir: &Path, json: &str) {
        std::fs::write(dir.join("package.json"), json).unwrap();
    }

    #[test]
    fn resolve_pin_reads_package_manager_then_dev_engines_then_none() {
        // 1. packageManager field is the primary source.
        let dir = tmpdir("pin-pkgmgr");
        write_pkg(&dir, r#"{"packageManager":"pnpm@9.1.0"}"#);
        assert_eq!(
            resolve_pin(&dir),
            Some(PmPin {
                pm: Pm::Pnpm,
                version: Some("9.1.0".to_string())
            })
        );

        // 2. No packageManager → devEngines.packageManager (object form).
        let dir = tmpdir("pin-devengines");
        write_pkg(
            &dir,
            r#"{"devEngines":{"packageManager":{"name":"pnpm","version":"9.1.0"}}}"#,
        );
        assert_eq!(
            resolve_pin(&dir),
            Some(PmPin {
                pm: Pm::Pnpm,
                version: Some("9.1.0".to_string())
            })
        );

        // 3. Neither field → unpinned.
        let dir = tmpdir("pin-none");
        write_pkg(&dir, r#"{"name":"app"}"#);
        assert_eq!(resolve_pin(&dir), None);
    }

    #[test]
    fn yarn_classic_vs_berry_split_by_major_and_keeps_hash_suffix() {
        let dir = tmpdir("yarn-classic");
        write_pkg(&dir, r#"{"packageManager":"yarn@1.22.19"}"#);
        assert_eq!(resolve_pin(&dir).unwrap().pm, Pm::Yarn);

        let dir = tmpdir("yarn-berry");
        write_pkg(&dir, r#"{"packageManager":"yarn@3.0.0"}"#);
        assert_eq!(resolve_pin(&dir).unwrap().pm, Pm::YarnBerry);

        // The Corepack hash suffix is preserved byte-for-byte in `version`.
        let dir = tmpdir("yarn-hash");
        write_pkg(&dir, r#"{"packageManager":"yarn@4.2.2+sha512.abc"}"#);
        let pin = resolve_pin(&dir).unwrap();
        assert_eq!(pin.pm, Pm::YarnBerry);
        assert_eq!(pin.version.as_deref(), Some("4.2.2+sha512.abc"));
    }

    #[test]
    fn yarn_disambiguated_by_yarnrc_when_only_lockfile_present() {
        // The lockfile-inference route has no version, so `.yarnrc.yml` presence
        // (the same helper, second route) decides classic vs Berry.
        assert_eq!(
            classify_yarn(None, false),
            Pm::Yarn,
            "yarn.lock alone (no .yarnrc.yml) is classic"
        );
        assert_eq!(
            classify_yarn(None, true),
            Pm::YarnBerry,
            "a sibling .yarnrc.yml flips lockfile-only yarn to Berry"
        );
    }

    #[test]
    fn resolve_target_yarn_path_short_circuits_to_yarn_path() {
        let dir = tmpdir("target-yarnpath");
        // A committed Berry release + a Berry pin: yarnPath must win, never
        // Provision/BerryNoYarnPath.
        write_pkg(&dir, r#"{"packageManager":"yarn@4.2.2"}"#);
        let release = dir.join(".yarn/releases");
        std::fs::create_dir_all(&release).unwrap();
        let release_file = release.join("yarn-4.2.2.cjs");
        std::fs::write(&release_file, "// yarn\n").unwrap();
        std::fs::write(
            dir.join(".yarnrc.yml"),
            "yarnPath: .yarn/releases/yarn-4.2.2.cjs\n",
        )
        .unwrap();

        assert_eq!(resolve_target(&dir), Some(PmTarget::YarnPath(release_file)));
    }

    #[test]
    fn resolve_target_bare_berry_without_yarn_path_is_unresolvable() {
        let dir = tmpdir("target-berry-bare");
        write_pkg(&dir, r#"{"packageManager":"yarn@4.2.2"}"#);
        assert_eq!(resolve_target(&dir), Some(PmTarget::BerryNoYarnPath));
    }

    #[test]
    fn unsupported_manager_and_missing_version_are_named_errors() {
        // bun is out of scope → error names the supported set.
        let dir = tmpdir("err-bun");
        write_pkg(&dir, r#"{"packageManager":"bun@1.1.0"}"#);
        // resolve_pin swallows the parse error into None (it's a "no usable pin"
        // query); the underlying parser carries the message.
        let err = parse_spec("bun@1.1.0").unwrap_err().to_string();
        assert!(
            err.contains("npm, pnpm, and yarn"),
            "bun error must name the supported set, got: {err}"
        );
        assert_eq!(resolve_pin(&dir), None);

        // packageManager with no @version → error names the required form.
        let err = parse_spec("pnpm").unwrap_err().to_string();
        assert!(
            err.contains("name@version"),
            "missing-version error must name the form, got: {err}"
        );
    }

    #[test]
    fn write_pin_preserves_siblings_and_errors_without_package_json() {
        let dir = tmpdir("write-pin");
        write_pkg(
            &dir,
            "{\n  \"name\": \"app\",\n  \"scripts\": {\n    \"build\": \"tsc\"\n  }\n}\n",
        );
        let written = write_pin(Pm::Pnpm, "9.1.0", &dir).unwrap();
        assert_eq!(written, dir.join("package.json"));

        let manifest: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&written).unwrap()).unwrap();
        assert_eq!(
            manifest["packageManager"].as_str(),
            Some("pnpm@9.1.0"),
            "the pin is written"
        );
        assert_eq!(
            manifest["name"].as_str(),
            Some("app"),
            "sibling keys survive the round-trip"
        );
        assert_eq!(
            manifest["scripts"]["build"].as_str(),
            Some("tsc"),
            "nested sibling keys survive the round-trip"
        );

        // No package.json at the target dir → error, never create one.
        let empty = tmpdir("write-pin-empty");
        let err = write_pin(Pm::Npm, "10.0.0", &empty)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("no package.json"),
            "missing-manifest error must say so, got: {err}"
        );
        assert!(
            !empty.join("package.json").exists(),
            "write_pin must not scaffold a package.json"
        );
    }
}
