//! Strict, bounded search-root inventory for startup-only sandbox deny masks.
//!
//! This is intentionally separate from ordinary workspace filtering: security
//! inventory treats unreadable or malformed declarations as errors and supports
//! only a fixed-depth pattern grammar whose cost depends on declared containers,
//! never on recursive project size.

use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

const MAX_PATTERNS: usize = 256;
const MAX_PATTERN_BYTES: usize = 16 * 1024;
const MAX_ENUMERATED_ENTRIES: usize = 50_000;
const MAX_MEMBER_ROOTS: usize = 4096;
const MAX_ANCESTORS: usize = 64;

pub fn discover(cwd: &Path) -> Result<Vec<PathBuf>, String> {
    let cwd = fs::canonicalize(cwd)
        .map_err(|e| format!("resolving sandbox working directory {}: {e}", cwd.display()))?;
    if !fs::metadata(&cwd)
        .map_err(|e| format!("statting sandbox working directory {}: {e}", cwd.display()))?
        .is_dir()
    {
        return Err(format!(
            "sandbox working directory is not a directory: {}",
            cwd.display()
        ));
    }

    let mut roots = BTreeSet::from([cwd.clone()]);
    let mut project_root = None;
    let mut workspace: Option<(PathBuf, Vec<String>)> = None;

    for (depth, dir) in cwd.ancestors().enumerate() {
        if depth >= MAX_ANCESTORS {
            return Err("workspace discovery exceeds the ancestor budget".to_string());
        }
        let manifest_path = dir.join("package.json");
        let manifest = read_json_if_present(&manifest_path)?;
        if let Some(manifest) = manifest.as_ref() {
            if project_root.is_none() {
                project_root = Some(dir.to_path_buf());
            }

            let neutral = neutral_workspace_patterns(manifest)?;
            let has_neutral_workspace = manifest.get("workspaces").is_some();
            if has_neutral_workspace {
                workspace = Some((dir.to_path_buf(), neutral));
                break;
            }
        }

        // The deny-mask root set must EQUAL the monorepo the PM resolves — never a
        // subset (a missed member root = a leaked `.env`) nor a superset. The gate
        // here is IDENTICAL to nub's real workspace resolution — `detect_project`
        // (detect.rs) and `discover_members` (filter.rs) both read a dir's
        // `pnpm-workspace.yaml`, and break the ancestor walk on it, ONLY when
        // `pnpm_is_incumbent` (a committed `pnpm-lock.yaml`, the on-disk incumbent
        // signal), regardless of the declared `packageManager`. Matching that
        // exactly is load-bearing in BOTH directions: a project that declares npm
        // but carries a pnpm lockfile IS a pnpm workspace to the PM, so its members
        // must be masked; and a declared-pnpm dir with no lockfile yet is NOT a
        // workspace root to the PM, so breaking the walk there would strand the
        // real (outer) workspace's other members unmasked.
        if super::filter::pnpm_is_incumbent(dir) {
            let yaml_path = dir.join("pnpm-workspace.yaml");
            if path_is_file_strict(&yaml_path)? {
                let mut patterns = manifest
                    .as_ref()
                    .map(neutral_workspace_patterns)
                    .transpose()?
                    .unwrap_or_default();
                patterns.extend(read_pnpm_workspace_patterns(&yaml_path)?);
                workspace = Some((dir.to_path_buf(), patterns));
                break;
            }
        }
    }

    if let Some(project_root) = project_root {
        roots.insert(project_root);
    }
    if let Some((workspace_root, patterns)) = workspace {
        roots.insert(
            fs::canonicalize(&workspace_root).map_err(|e| {
                format!("resolving workspace root {}: {e}", workspace_root.display())
            })?,
        );
        for member in expand_workspace_patterns(&workspace_root, &patterns)? {
            roots.insert(member);
        }
    }
    Ok(roots.into_iter().collect())
}

fn read_json_if_present(path: &Path) -> Result<Option<Value>, String> {
    match fs::metadata(path) {
        Ok(metadata) => {
            if !metadata.is_file() {
                return Err(format!(
                    "workspace manifest is not a file: {}",
                    path.display()
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "statting workspace manifest {}: {error}",
                path.display()
            ));
        }
    }
    let bytes = fs::read(path)
        .map_err(|e| format!("reading workspace manifest {}: {e}", path.display()))?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| format!("parsing workspace manifest {}: {e}", path.display()))
}

fn path_is_file_strict(path: &Path) -> Result<bool, String> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(true),
        Ok(_) => Err(format!(
            "workspace declaration is not a file: {}",
            path.display()
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!(
            "statting workspace declaration {}: {error}",
            path.display()
        )),
    }
}

fn neutral_workspace_patterns(manifest: &Value) -> Result<Vec<String>, String> {
    let Some(workspaces) = manifest.get("workspaces") else {
        return Ok(Vec::new());
    };
    let packages = match workspaces {
        Value::Array(packages) => packages,
        Value::Object(object) => object
            .get("packages")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                "package.json#workspaces object must contain a packages array".to_string()
            })?,
        _ => return Err("package.json#workspaces must be an array or packages object".to_string()),
    };
    string_patterns(packages, "package.json#workspaces")
}

fn read_pnpm_workspace_patterns(path: &Path) -> Result<Vec<String>, String> {
    let bytes = fs::read(path)
        .map_err(|e| format!("reading proven-pnpm workspace {}: {e}", path.display()))?;
    let yaml: serde_yaml::Value = serde_yaml::from_slice(&bytes)
        .map_err(|e| format!("parsing proven-pnpm workspace {}: {e}", path.display()))?;
    let packages = yaml
        .get("packages")
        .and_then(serde_yaml::Value::as_sequence)
        .ok_or_else(|| format!("{} must contain a packages array", path.display()))?;
    let mut out = Vec::with_capacity(packages.len());
    for package in packages {
        let Some(package) = package.as_str() else {
            return Err(format!(
                "{} packages entries must be strings",
                path.display()
            ));
        };
        out.push(package.to_string());
    }
    Ok(out)
}

fn string_patterns(values: &[Value], field: &str) -> Result<Vec<String>, String> {
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("{field} entries must be strings"))
        })
        .collect()
}

fn expand_workspace_patterns(root: &Path, patterns: &[String]) -> Result<Vec<PathBuf>, String> {
    if patterns.len() > MAX_PATTERNS
        || patterns.iter().map(String::len).sum::<usize>() > MAX_PATTERN_BYTES
    {
        return Err(
            "workspace pattern declaration exceeds the sandbox inventory budget".to_string(),
        );
    }
    let mut included = BTreeSet::new();
    let mut excluded = BTreeSet::new();
    let mut examined = 0usize;

    for pattern in patterns {
        let (exclude, pattern) = pattern
            .strip_prefix('!')
            .map_or((false, pattern.as_str()), |pattern| (true, pattern));
        let components = validate_pattern(pattern)?;
        let matches = expand_components(root, &components, &mut examined)?;
        if exclude {
            excluded.extend(matches);
        } else {
            included.extend(matches);
        }
        if included.len() > MAX_MEMBER_ROOTS {
            return Err("workspace member inventory exceeds the root budget".to_string());
        }
    }
    included.retain(|path| !excluded.contains(path));

    let mut members = Vec::new();
    for candidate in included {
        let manifest = candidate.join("package.json");
        if read_json_if_present(&manifest)?.is_some() {
            members.push(candidate);
        }
    }
    Ok(members)
}

fn validate_pattern(pattern: &str) -> Result<Vec<&str>, String> {
    if pattern.is_empty()
        || pattern.starts_with('/')
        || pattern.contains('\\')
        || pattern.contains("**")
        || pattern.contains(['?', '[', ']', '{', '}', '(', ')'])
    {
        return Err(format!("unsupported bounded workspace pattern {pattern:?}"));
    }
    let components = pattern.split('/').collect::<Vec<_>>();
    if components.iter().any(|component| {
        component.is_empty()
            || *component == "."
            || *component == ".."
            || (component.contains('*') && *component != "*")
    }) {
        return Err(format!("unsupported bounded workspace pattern {pattern:?}"));
    }
    if Path::new(pattern)
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!("unsupported bounded workspace pattern {pattern:?}"));
    }
    Ok(components)
}

fn expand_components(
    root: &Path,
    components: &[&str],
    examined: &mut usize,
) -> Result<Vec<PathBuf>, String> {
    let mut candidates = vec![root.to_path_buf()];
    for component in components {
        let mut next = Vec::new();
        for base in candidates {
            if *component == "*" {
                let entries = match fs::read_dir(&base) {
                    Ok(entries) => entries,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => {
                        return Err(format!(
                            "enumerating workspace container {}: {error}",
                            base.display()
                        ));
                    }
                };
                let mut children = Vec::new();
                for entry in entries {
                    let entry = entry.map_err(|e| {
                        format!("enumerating workspace container {}: {e}", base.display())
                    })?;
                    *examined += 1;
                    if *examined > MAX_ENUMERATED_ENTRIES {
                        return Err("workspace enumeration exceeds the entry budget".to_string());
                    }
                    let path = entry.path();
                    let metadata = match fs::metadata(&path) {
                        Ok(metadata) => metadata,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(error) => {
                            return Err(format!(
                                "statting workspace member {}: {error}",
                                path.display()
                            ));
                        }
                    };
                    if metadata.is_dir() {
                        children.push(fs::canonicalize(&path).map_err(|e| {
                            format!("resolving workspace member {}: {e}", path.display())
                        })?);
                    }
                }
                children.sort();
                next.extend(children);
            } else {
                let path = base.join(component);
                match fs::metadata(&path) {
                    Ok(metadata) if metadata.is_dir() => {
                        next.push(fs::canonicalize(&path).map_err(|e| {
                            format!("resolving workspace member {}: {e}", path.display())
                        })?);
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(format!(
                            "statting workspace member {}: {error}",
                            path.display()
                        ));
                    }
                }
            }
        }
        next.sort();
        next.dedup();
        candidates = next;
    }
    Ok(candidates)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write(path: &Path, body: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    #[test]
    fn inventories_cwd_project_workspace_members_and_symlink_aliases() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        write(
            &root.join("package.json"),
            r#"{"workspaces":["packages/*","examples/*/demo"]}"#,
        );
        write(&root.join("packages/a/package.json"), "{}");
        write(&root.join("examples/one/demo/package.json"), "{}");
        let external = tempdir().unwrap();
        write(&external.path().join("package.json"), "{}");
        #[cfg(unix)]
        std::os::unix::fs::symlink(external.path(), root.join("packages/link")).unwrap();

        let cwd = root.join("packages/a/src");
        fs::create_dir_all(&cwd).unwrap();
        let roots = discover(&cwd).unwrap();
        assert!(roots.contains(&fs::canonicalize(&cwd).unwrap()));
        assert!(roots.contains(&fs::canonicalize(root).unwrap()));
        assert!(roots.contains(&fs::canonicalize(root.join("packages/a")).unwrap()));
        assert!(roots.contains(&fs::canonicalize(root.join("examples/one/demo")).unwrap()));
        #[cfg(unix)]
        assert!(roots.contains(&fs::canonicalize(external.path()).unwrap()));
    }

    #[test]
    fn exclusions_and_missing_literals_are_zero_match() {
        let temp = tempdir().unwrap();
        write(
            &temp.path().join("package.json"),
            r#"{"workspaces":["packages/*","!packages/private","missing/pkg"]}"#,
        );
        write(&temp.path().join("packages/public/package.json"), "{}");
        write(&temp.path().join("packages/private/package.json"), "{}");
        let roots = discover(temp.path()).unwrap();
        assert!(roots.contains(&fs::canonicalize(temp.path().join("packages/public")).unwrap()));
        assert!(!roots.contains(&fs::canonicalize(temp.path().join("packages/private")).unwrap()));
    }

    #[test]
    fn rejects_unbounded_workspace_grammar_and_malformed_manifests() {
        for pattern in [
            "packages/**",
            "packages/a*",
            "../packages/*",
            "packages/{a,b}",
            "",
        ] {
            let temp = tempdir().unwrap();
            write(
                &temp.path().join("package.json"),
                &format!(r#"{{"workspaces":[{pattern:?}]}}"#),
            );
            assert!(discover(temp.path()).is_err(), "pattern {pattern:?}");
        }
        let temp = tempdir().unwrap();
        write(&temp.path().join("package.json"), "{");
        assert!(discover(temp.path()).is_err());
    }

    #[test]
    fn reads_pnpm_workspace_only_when_a_lockfile_marks_pnpm_incumbent() {
        // The gate is EXACTLY the PM's (`detect_project`/`discover_members` →
        // `pnpm_is_incumbent`): a committed `pnpm-lock.yaml` — NOT the declared
        // `packageManager`. A contradictory manifest that declares npm but carries
        // a pnpm lockfile is resolved as a pnpm workspace by the PM, so the mask
        // must read the pnpm-workspace here too (else its members' `.env` would
        // leak) — proven by the strict parser rejecting the malformed file.
        let npm_but_pnpm_lock = tempdir().unwrap();
        write(
            &npm_but_pnpm_lock.path().join("package.json"),
            r#"{"packageManager":"npm@11.0.0"}"#,
        );
        write(
            &npm_but_pnpm_lock.path().join("pnpm-workspace.yaml"),
            "not: [valid",
        );
        write(
            &npm_but_pnpm_lock.path().join("pnpm-lock.yaml"),
            "lockfileVersion: '9.0'",
        );
        assert!(discover(npm_but_pnpm_lock.path()).is_err());

        // Declared pnpm but NO lockfile yet: NOT read — the PM does not treat this
        // dir as a workspace root, so neither does the mask. (Reading it here would
        // break the ancestor walk early and strand an outer workspace's members.)
        let pnpm_no_lock = tempdir().unwrap();
        write(
            &pnpm_no_lock.path().join("package.json"),
            r#"{"packageManager":"pnpm@10.0.0"}"#,
        );
        write(
            &pnpm_no_lock.path().join("pnpm-workspace.yaml"),
            "not: [valid",
        );
        assert!(discover(pnpm_no_lock.path()).is_ok());

        // No pnpm signal at all: a stray pnpm-workspace.yaml is not read.
        let no_signal = tempdir().unwrap();
        write(&no_signal.path().join("package.json"), r#"{}"#);
        write(&no_signal.path().join("pnpm-workspace.yaml"), "not: [valid");
        assert!(discover(no_signal.path()).is_ok());

        let fallback = tempdir().unwrap();
        write(&fallback.path().join("package.json"), "{}");
        write(
            &fallback.path().join("pnpm-lock.yaml"),
            "lockfileVersion: '9.0'",
        );
        write(
            &fallback.path().join("pnpm-workspace.yaml"),
            "packages:\n  - packages/*\n",
        );
        write(&fallback.path().join("packages/a/package.json"), "{}");
        let roots = discover(fallback.path()).unwrap();
        assert!(roots.contains(&fs::canonicalize(fallback.path().join("packages/a")).unwrap()));
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_existing_manifest_fails_but_unrelated_tree_is_not_scanned() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempdir().unwrap();
        write(
            &temp.path().join("package.json"),
            r#"{"workspaces":["packages/*"]}"#,
        );
        write(&temp.path().join("packages/a/package.json"), "{}");
        fs::create_dir_all(temp.path().join("unrelated/deep/tree")).unwrap();
        fs::set_permissions(
            temp.path().join("unrelated"),
            fs::Permissions::from_mode(0o000),
        )
        .unwrap();
        assert!(
            discover(temp.path()).is_ok(),
            "unrelated tree is never scanned"
        );

        let manifest = temp.path().join("packages/a/package.json");
        fs::set_permissions(&manifest, fs::Permissions::from_mode(0o000)).unwrap();
        let result = discover(temp.path());
        fs::set_permissions(&manifest, fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(
            temp.path().join("unrelated"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        assert!(result.is_err(), "existing unreadable declaration must fail");
    }

    #[test]
    fn pattern_budget_is_enforced_before_enumeration() {
        let temp = tempdir().unwrap();
        let patterns = (0..=MAX_PATTERNS)
            .map(|index| format!("packages/p{index}"))
            .collect::<Vec<_>>();
        write(
            &temp.path().join("package.json"),
            &serde_json::json!({ "workspaces": patterns }).to_string(),
        );
        assert!(discover(temp.path()).is_err());
    }

    #[test]
    fn ancestor_budget_exhaustion_fails_closed() {
        let temp = tempdir().unwrap();
        write(&temp.path().join("package.json"), r#"{"workspaces":[]}"#);
        let mut cwd = temp.path().to_path_buf();
        for _ in 0..MAX_ANCESTORS {
            cwd.push("d");
        }
        fs::create_dir_all(&cwd).unwrap();
        assert!(discover(&cwd).is_err());
    }

    // The maintainer-decided scope for the `.env*` secret floor: the deny mask
    // covers env files at the workspace root and EVERY sub-project (member) root
    // — "exactly equivalent to resolving the monorepo" — and does NOT reach into
    // arbitrary non-project nested directories (intentionally out of scope). The
    // three tests below encode that contract so a future reader does not "fix" it
    // by adding a recursive walk (which would over-mask non-members) or by
    // dropping the pnpm gate (which would under-mask and leak a member's `.env`).

    /// The mask root set for a bounded monorepo is EXACTLY the PM's resolution:
    /// `{workspace root} ∪ {member roots discover_members() returns}`. Asserted
    /// for npm (`workspaces`), pnpm (`pnpm-workspace.yaml` + lockfile), and yarn
    /// (`workspaces`), each with a nested member and a mid-glob member.
    #[test]
    fn mask_root_set_equals_the_pm_monorepo_resolution() {
        use crate::workspace::filter::discover_members;

        // Expected = {root} ∪ canonicalized member dirs the PM resolves; with
        // cwd == root, discover() returns exactly that (cwd/project == root).
        let expect_equivalence = |root: &Path| {
            let mut expected = BTreeSet::from([fs::canonicalize(root).unwrap()]);
            for member in discover_members(root) {
                expected.insert(fs::canonicalize(&member.dir).unwrap());
            }
            let got: BTreeSet<_> = discover(root).unwrap().into_iter().collect();
            assert_eq!(got, expected, "sandbox mask set must equal PM resolution");
            assert!(!expected.is_empty());
        };

        let npm = tempdir().unwrap();
        write(
            &npm.path().join("package.json"),
            r#"{"workspaces":["packages/*","examples/*/demo"]}"#,
        );
        write(
            &npm.path().join("packages/a/package.json"),
            r#"{"name":"a"}"#,
        );
        write(
            &npm.path().join("packages/b/package.json"),
            r#"{"name":"b"}"#,
        );
        write(
            &npm.path().join("examples/one/demo/package.json"),
            r#"{"name":"demo"}"#,
        );
        expect_equivalence(npm.path());

        let pnpm = tempdir().unwrap();
        write(&pnpm.path().join("package.json"), r#"{"name":"root"}"#);
        write(
            &pnpm.path().join("pnpm-lock.yaml"),
            "lockfileVersion: '9.0'\n",
        );
        write(
            &pnpm.path().join("pnpm-workspace.yaml"),
            "packages:\n  - packages/*\n  - examples/*/demo\n",
        );
        write(
            &pnpm.path().join("packages/a/package.json"),
            r#"{"name":"a"}"#,
        );
        write(
            &pnpm.path().join("examples/one/demo/package.json"),
            r#"{"name":"demo"}"#,
        );
        expect_equivalence(pnpm.path());

        let yarn = tempdir().unwrap();
        write(
            &yarn.path().join("package.json"),
            r#"{"packageManager":"yarn@4.0.0","workspaces":["packages/*"]}"#,
        );
        write(&yarn.path().join("yarn.lock"), "");
        write(
            &yarn.path().join("packages/a/package.json"),
            r#"{"name":"a"}"#,
        );
        write(
            &yarn.path().join("packages/b/package.json"),
            r#"{"name":"b"}"#,
        );
        expect_equivalence(yarn.path());
    }

    /// A `.env`-bearing directory that is NOT a workspace member (an arbitrary
    /// nested non-project subdir) is intentionally OUT of the mask root set — the
    /// maintainer-decided boundary against a recursive walk.
    #[test]
    fn non_project_nested_dir_is_not_a_mask_root() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        write(
            &root.join("package.json"),
            r#"{"workspaces":["packages/*"]}"#,
        );
        write(&root.join("packages/a/package.json"), r#"{"name":"a"}"#);
        // A deep non-member dir holding a .env — not a package, not a member.
        fs::create_dir_all(root.join("packages/a/config/secrets")).unwrap();
        let roots: BTreeSet<_> = discover(root).unwrap().into_iter().collect();
        assert!(roots.contains(&fs::canonicalize(root.join("packages/a")).unwrap()));
        assert!(
            !roots.contains(&fs::canonicalize(root.join("packages/a/config/secrets")).unwrap()),
            "non-project nested dir must not be a mask root (monorepo scope, not recursive)"
        );
    }

    /// A declared-pnpm subdir with NO lockfile, nested inside an outer npm
    /// workspace, must NOT break the ancestor walk early: the walk reaches the
    /// real (npm) workspace root and masks ALL its members — not just the subdir.
    /// (The gate keys on `pnpm_is_incumbent`, not the declaration, so the subdir
    /// is not mistaken for a workspace root and no sibling member's `.env` leaks.)
    #[test]
    fn declared_pnpm_subdir_without_lockfile_does_not_strand_outer_members() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        write(
            &root.join("package.json"),
            r#"{"workspaces":["packages/*"]}"#,
        );
        write(&root.join("packages/b/package.json"), r#"{"name":"b"}"#);
        // The nested member declares pnpm and carries a pnpm-workspace.yaml but no
        // pnpm-lock.yaml — pnpm is not incumbent here, so it is a plain member.
        write(
            &root.join("packages/a/package.json"),
            r#"{"name":"a","packageManager":"pnpm@10.0.0"}"#,
        );
        write(
            &root.join("packages/a/pnpm-workspace.yaml"),
            "packages:\n  - inner/*\n",
        );
        let roots: BTreeSet<_> = discover(&root.join("packages/a"))
            .unwrap()
            .into_iter()
            .collect();
        assert!(
            roots.contains(&fs::canonicalize(root).unwrap()),
            "outer root"
        );
        assert!(
            roots.contains(&fs::canonicalize(root.join("packages/b")).unwrap()),
            "sibling member packages/b must be masked, not stranded by an early break"
        );
    }

    /// A workspace declared with an unbounded pattern the bounded grammar rejects
    /// (`**`, a partial-segment glob, a brace group) fails CLOSED — discover()
    /// errors so `run_sandboxed` aborts, never running with a silent under-mask.
    #[test]
    fn unbounded_member_patterns_fail_closed_not_silently_undermasked() {
        for pattern in ["packages/**", "pkg-*", "{apps,libs}/*"] {
            let temp = tempdir().unwrap();
            write(
                &temp.path().join("package.json"),
                &format!(r#"{{"workspaces":[{pattern:?}]}}"#),
            );
            write(&temp.path().join("pkg-a/package.json"), r#"{"name":"a"}"#);
            assert!(discover(temp.path()).is_err(), "pattern {pattern:?}");
        }
    }
}
