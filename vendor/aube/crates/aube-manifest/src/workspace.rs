//! Workspace-level configuration (`pnpm-workspace.yaml` /
//! `aube-workspace.yaml`) read + write.
//!
//! Split into four submodules:
//! - [`config`] — typed `WorkspaceConfig`, parsing, in-memory caches,
//!   and the `config_write_target` rule.
//! - [`edits`] — generic yaml/json edit helpers shared by every
//!   workspace-level mutation.
//! - [`mutations`] — the domain-specific mutations: `allowBuilds`
//!   and `patchedDependencies`.
//! - [`yaml_patch`] — comment-preserving diff-and-apply on top of
//!   `yamlpatch`, with a manual injector for new sub-mappings (which
//!   `yamlpatch::Op::Add` mishandles).
//!
//! The submodules are private; the public API is re-exported from
//! this module so callers continue to use `aube_manifest::workspace::X`.

mod config;
mod edits;
mod mutations;
mod yaml_patch;

pub use config::{
    ConfigWriteTarget, JailBuildPermission, SupportedArchitectures, WorkspaceConfig,
    config_write_target, load_both, load_raw, workspace_yaml_existing, workspace_yaml_names,
    workspace_yaml_target,
};
pub use edits::{
    edit_setting_map, edit_workspace_yaml, remove_map_entry, remove_setting_entry, upsert_map_entry,
};
pub use mutations::{
    ALLOW_BUILDS_REVIEW_PLACEHOLDER, add_to_allow_builds, remove_workspace_patched_dependency,
    set_allow_builds, upsert_workspace_patched_dependency,
};

#[cfg(test)]
mod tests {
    use super::edits::workspace_yaml_submap;
    use super::*;

    #[test]
    fn test_empty_config() {
        let config: WorkspaceConfig = yaml_serde::from_str("{}").unwrap();
        assert!(config.packages.is_empty());
        assert!(config.enable_global_virtual_store.is_none());
    }

    #[test]
    fn test_packages_only() {
        let yaml = r#"
packages:
  - 'packages/*'
  - 'apps/*'
"#;
        let config: WorkspaceConfig = yaml_serde::from_str(yaml).unwrap();
        assert_eq!(config.packages, vec!["packages/*", "apps/*"]);
    }

    #[test]
    fn test_settings() {
        let yaml = r#"
packages:
  - 'packages/*'
enableGlobalVirtualStore: true
shamefullyHoist: false
packageImportMethod: hardlink
storeDir: /tmp/my-store
"#;
        let config: WorkspaceConfig = yaml_serde::from_str(yaml).unwrap();
        assert_eq!(config.packages, vec!["packages/*"]);
        assert_eq!(config.enable_global_virtual_store, Some(true));
        assert_eq!(config.shamefully_hoist, Some(false));
        assert_eq!(config.package_import_method, Some("hardlink".to_string()));
        assert_eq!(config.store_dir, Some("/tmp/my-store".to_string()));
    }

    #[test]
    fn test_link_workspace_packages_deep() {
        let yaml = r#"
packages:
  - 'packages/*'
linkWorkspacePackages: deep
"#;
        let config: WorkspaceConfig = yaml_serde::from_str(yaml).unwrap();
        assert_eq!(
            config
                .link_workspace_packages
                .as_ref()
                .and_then(yaml_serde::Value::as_str),
            Some("deep")
        );
    }

    #[test]
    fn test_catalog() {
        let yaml = r#"
catalog:
  chalk: ^4.1.2
  lodash: ^4.17.21
catalogs:
  react16:
    react: ^16.7.0
"#;
        let config: WorkspaceConfig = yaml_serde::from_str(yaml).unwrap();
        assert_eq!(config.catalog.get("chalk").unwrap(), "^4.1.2");
        assert_eq!(
            config
                .catalogs
                .get("react16")
                .unwrap()
                .get("react")
                .unwrap(),
            "^16.7.0"
        );
    }

    #[test]
    fn test_overrides() {
        let yaml = r#"
overrides:
  foo: 1.0.0
  bar: npm:baz@^2
"#;
        let config: WorkspaceConfig = yaml_serde::from_str(yaml).unwrap();
        assert_eq!(config.overrides.get("foo").unwrap(), "1.0.0");
        assert_eq!(config.overrides.get("bar").unwrap(), "npm:baz@^2");
    }

    #[test]
    fn test_supported_architectures() {
        let yaml = r#"
supportedArchitectures:
  os: ["current", "linux"]
  cpu: ["current", "x64"]
  libc: ["glibc"]
"#;
        let config: WorkspaceConfig = yaml_serde::from_str(yaml).unwrap();
        let sa = config.supported_architectures.as_ref().unwrap();
        assert_eq!(sa.os, vec!["current", "linux"]);
        assert_eq!(sa.cpu, vec!["current", "x64"]);
        assert_eq!(sa.libc, vec!["glibc"]);
        assert!(!sa.is_empty());
    }

    #[test]
    fn test_ignored_optional_dependencies() {
        let yaml = r#"
ignoredOptionalDependencies:
  - fsevents
  - dtrace-provider
"#;
        let config: WorkspaceConfig = yaml_serde::from_str(yaml).unwrap();
        assert_eq!(
            config.ignored_optional_dependencies,
            vec!["fsevents", "dtrace-provider"]
        );
    }

    #[test]
    fn test_pnpmfile_path() {
        let yaml = r#"
pnpmfilePath: config/pnpmfile.cjs
"#;
        let config: WorkspaceConfig = yaml_serde::from_str(yaml).unwrap();
        assert_eq!(config.pnpmfile_path.as_deref(), Some("config/pnpmfile.cjs"));
    }

    #[test]
    fn test_patched_dependencies() {
        // pnpm v10 lets users declare patches in pnpm-workspace.yaml so
        // they can annotate each patch with YAML comments explaining
        // WHY the patch exists — something package.json's JSON syntax
        // can't host. Parse shape matches `pnpm.patchedDependencies`.
        let yaml = r#"
patchedDependencies:
  "is-positive@3.1.0": patches/is-positive@3.1.0.patch
  "@scope/pkg@1.0.0": patches/scope-pkg.patch
"#;
        let config: WorkspaceConfig = yaml_serde::from_str(yaml).unwrap();
        assert_eq!(
            config
                .patched_dependencies
                .get("is-positive@3.1.0")
                .unwrap(),
            "patches/is-positive@3.1.0.patch"
        );
        assert_eq!(
            config.patched_dependencies.get("@scope/pkg@1.0.0").unwrap(),
            "patches/scope-pkg.patch"
        );
    }

    #[test]
    fn test_load_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let config = WorkspaceConfig::load(dir.path()).unwrap();
        assert!(config.packages.is_empty());
    }

    #[test]
    fn test_load_from_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'src/*'\nenableGlobalVirtualStore: false\n",
        )
        .unwrap();
        let config = WorkspaceConfig::load(dir.path()).unwrap();
        assert_eq!(config.packages, vec!["src/*"]);
        assert_eq!(config.enable_global_virtual_store, Some(false));
    }

    #[test]
    fn aube_workspace_preferred_over_pnpm_workspace() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("aube-workspace.yaml"),
            "packages:\n  - 'aube/*'\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'pnpm/*'\n",
        )
        .unwrap();
        let config = WorkspaceConfig::load(dir.path()).unwrap();
        assert_eq!(config.packages, vec!["aube/*"]);
    }

    #[test]
    fn add_to_allow_builds_writes_to_package_json_when_no_yaml() {
        // No yaml on disk, no `pnpm` namespace in package.json: the
        // setting lands under `aube.allowBuilds` per the shared
        // `config_write_target` rule. Tests for the existing-yaml
        // branch live in `add_to_allow_builds_writes_to_existing_pnpm_workspace`
        // and `add_to_allow_builds_writes_to_aube_file_when_present`.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name":"solo","version":"0.0.0"}"#,
        )
        .unwrap();
        let path =
            add_to_allow_builds(dir.path(), &["esbuild".to_string(), "sharp".to_string()]).unwrap();
        assert_eq!(path, dir.path().join("package.json"));
        let raw = std::fs::read_to_string(dir.path().join("package.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["aube"]["allowBuilds"]["esbuild"], true);
        assert_eq!(parsed["aube"]["allowBuilds"]["sharp"], true);
        // Existing manifest keys are preserved.
        assert_eq!(parsed["name"], "solo");
        // No yaml file should have been created.
        assert!(!dir.path().join("aube-workspace.yaml").exists());
        assert!(!dir.path().join("pnpm-workspace.yaml").exists());
    }

    #[test]
    fn add_to_allow_builds_writes_to_existing_pnpm_workspace() {
        // Pin the backward-compat behavior: a project that
        // already ships `pnpm-workspace.yaml` (e.g. migrated
        // from pnpm) keeps mutating the existing file in
        // place rather than spawning a parallel
        // `aube-workspace.yaml`. Without this, an `aube
        // approve-builds` run would silently fork the config
        // into two files.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name":"solo","version":"0.0.0"}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'pnpm/*'\n",
        )
        .unwrap();
        let path = add_to_allow_builds(dir.path(), &["esbuild".to_string()]).unwrap();
        assert_eq!(path, dir.path().join("pnpm-workspace.yaml"));
        assert!(!dir.path().join("aube-workspace.yaml").exists());
    }

    #[test]
    fn add_to_allow_builds_flips_existing_workspace_entries() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "allowBuilds:\n  esbuild: false\n",
        )
        .unwrap();
        add_to_allow_builds(dir.path(), &["sharp".to_string(), "esbuild".to_string()]).unwrap();
        let config = WorkspaceConfig::load(dir.path()).unwrap();
        assert!(matches!(
            config.allow_builds.get("esbuild"),
            Some(yaml_serde::Value::Bool(true))
        ));
        assert!(matches!(
            config.allow_builds.get("sharp"),
            Some(yaml_serde::Value::Bool(true))
        ));
    }

    #[test]
    fn allow_builds_raw_round_trips_review_placeholder_from_yaml() {
        // Regression: `yaml_serde::to_string(Value::String(...))` wraps
        // the payload (quotes if needed, trailing newline, etc.). If
        // `allow_builds_raw` re-rendered yaml strings, the round-trip
        // would mutate the canonical placeholder string and the read
        // side wouldn't recognize it — every install would emit a
        // spurious `UnsupportedValue` warning.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            format!("allowBuilds:\n  esbuild: \"{ALLOW_BUILDS_REVIEW_PLACEHOLDER}\"\n"),
        )
        .unwrap();
        let config = WorkspaceConfig::load(dir.path()).unwrap();
        let raw = config.allow_builds_raw();
        assert_eq!(
            raw.get("esbuild"),
            Some(&crate::AllowBuildRaw::Other(
                ALLOW_BUILDS_REVIEW_PLACEHOLDER.to_string()
            ))
        );
    }

    #[test]
    fn add_to_allow_builds_appends_and_dedupes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'packages/*'\nallowBuilds:\n  esbuild: true\n",
        )
        .unwrap();
        add_to_allow_builds(dir.path(), &["sharp".to_string(), "esbuild".to_string()]).unwrap();
        let config = WorkspaceConfig::load(dir.path()).unwrap();
        assert_eq!(config.packages, vec!["packages/*"]);
        assert!(matches!(
            config.allow_builds.get("esbuild"),
            Some(yaml_serde::Value::Bool(true))
        ));
        assert!(matches!(
            config.allow_builds.get("sharp"),
            Some(yaml_serde::Value::Bool(true))
        ));
        let on_disk = std::fs::read_to_string(dir.path().join("pnpm-workspace.yaml")).unwrap();
        assert!(on_disk.contains("\n  esbuild: true"), "got:\n{on_disk}");
        assert!(on_disk.contains("\n  sharp: true"), "got:\n{on_disk}");
    }

    #[test]
    fn add_to_allow_builds_writes_to_aube_file_when_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("aube-workspace.yaml"),
            "packages:\n  - 'a/*'\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'p/*'\n",
        )
        .unwrap();
        let path = add_to_allow_builds(dir.path(), &["esbuild".to_string()]).unwrap();
        assert_eq!(path, dir.path().join("aube-workspace.yaml"));
        let pnpm = std::fs::read_to_string(dir.path().join("pnpm-workspace.yaml")).unwrap();
        assert!(!pnpm.contains("allowBuilds"));
    }

    #[test]
    fn pnpm_workspace_used_when_aube_missing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'pnpm/*'\n",
        )
        .unwrap();
        let config = WorkspaceConfig::load(dir.path()).unwrap();
        assert_eq!(config.packages, vec!["pnpm/*"]);
    }

    #[test]
    fn test_unknown_fields_captured() {
        let yaml = r#"
someNewField: true
anotherSetting: value
"#;
        let config: WorkspaceConfig = yaml_serde::from_str(yaml).unwrap();
        assert!(config.extra.contains_key("someNewField"));
    }

    #[test]
    fn update_config_deserializes_ignore_dependencies() {
        let yaml = r#"
updateConfig:
  ignoreDependencies:
    - is-odd
"#;
        let config: WorkspaceConfig = yaml_serde::from_str(yaml).unwrap();
        assert_eq!(
            config
                .update_config
                .as_ref()
                .map(|u| u.ignore_dependencies.as_slice()),
            Some(["is-odd".to_string()].as_slice())
        );
    }

    #[test]
    fn upsert_workspace_patched_dependency_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pnpm-workspace.yaml");
        upsert_workspace_patched_dependency(
            &path,
            "is-positive@3.1.0",
            "patches/is-positive@3.1.0.patch",
        )
        .unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("patchedDependencies:"));
        assert!(written.contains("is-positive@3.1.0"));
        assert!(written.contains("patches/is-positive@3.1.0.patch"));
    }

    #[test]
    fn upsert_workspace_patched_dependency_preserves_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pnpm-workspace.yaml");
        std::fs::write(&path, "packages:\n  - 'pkgs/*'\noverrides:\n  foo: 1.0.0\n").unwrap();
        upsert_workspace_patched_dependency(&path, "bar@2.0.0", "patches/bar@2.0.0.patch").unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("packages:"));
        assert!(written.contains("- 'pkgs/*'") || written.contains("- pkgs/*"));
        assert!(written.contains("overrides:"));
        assert!(written.contains("foo:"));
        assert!(written.contains("patchedDependencies:"));
        assert!(written.contains("bar@2.0.0"));
    }

    #[test]
    fn remove_workspace_patched_dependency_drops_empty_map() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pnpm-workspace.yaml");
        std::fs::write(
            &path,
            "patchedDependencies:\n  \"a@1.0.0\": patches/a@1.0.0.patch\n",
        )
        .unwrap();
        let removed = remove_workspace_patched_dependency(&path, "a@1.0.0").unwrap();
        assert!(removed);
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(!written.contains("patchedDependencies"));
    }

    #[test]
    fn remove_workspace_patched_dependency_missing_returns_false() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pnpm-workspace.yaml");
        std::fs::write(
            &path,
            "patchedDependencies:\n  \"a@1.0.0\": patches/a@1.0.0.patch\n",
        )
        .unwrap();
        let removed = remove_workspace_patched_dependency(&path, "missing@9.9.9").unwrap();
        assert!(!removed);
    }

    #[test]
    fn remove_workspace_patched_dependency_does_not_rewrite_when_key_absent() {
        // yaml_serde's round-trip drops comments. `aube patch-remove`
        // calls remove on both the workspace yaml and package.json
        // regardless of where the patch lives, so a no-op remove must
        // not touch the file (and lose the user's comments).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pnpm-workspace.yaml");
        let original = "# top-level comment\npatchedDependencies:\n  # patch annotation\n  \"a@1.0.0\": patches/a@1.0.0.patch\n";
        std::fs::write(&path, original).unwrap();
        let removed = remove_workspace_patched_dependency(&path, "missing@9.9.9").unwrap();
        assert!(!removed);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn upsert_workspace_patched_dependency_does_not_rewrite_when_value_unchanged() {
        // Same comment-preservation argument as the remove case: an
        // idempotent re-record after editing the patch file should not
        // strip yaml comments.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pnpm-workspace.yaml");
        let original = "# top-level comment\npatchedDependencies:\n  # patch annotation\n  \"a@1.0.0\": patches/a@1.0.0.patch\n";
        std::fs::write(&path, original).unwrap();
        upsert_workspace_patched_dependency(&path, "a@1.0.0", "patches/a@1.0.0.patch").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn add_to_allow_builds_does_not_rewrite_when_already_approved() {
        // Re-approving an already-approved name must leave the file
        // (and its yaml comments) untouched. `aube approve-builds` calls
        // into this path on every invocation, so steady-state runs must
        // not strip comments.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pnpm-workspace.yaml");
        let original = "# why we trust this build\nallowBuilds:\n  # esbuild ships native bindings\n  esbuild: true\n";
        std::fs::write(&path, original).unwrap();
        let written = add_to_allow_builds(dir.path(), &["esbuild".to_string()]).unwrap();
        assert_eq!(written, path);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn edit_workspace_yaml_preserves_comments_on_no_op() {
        // Direct test of the shared helper: a closure that doesn't
        // mutate the parsed structure must leave the file byte-equal,
        // including comments yaml_serde would otherwise strip.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pnpm-workspace.yaml");
        let original = "# header comment\npackages:\n  # workspace globs\n  - 'pkgs/*'\n";
        std::fs::write(&path, original).unwrap();
        edit_workspace_yaml(&path, |_map| Ok(())).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn edit_workspace_yaml_writes_when_structure_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pnpm-workspace.yaml");
        std::fs::write(&path, "packages:\n  - 'pkgs/*'\n").unwrap();
        edit_workspace_yaml(&path, |map| {
            map.insert(
                yaml_serde::Value::String("foo".to_string()),
                yaml_serde::Value::String("bar".to_string()),
            );
            Ok(())
        })
        .unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("foo: bar"));
    }

    #[test]
    fn edit_workspace_yaml_preserves_comments_around_unchanged_keys() {
        // The whole point of going through yamlpatch: a structural
        // change to one key must not strip comments attached to keys
        // the closure didn't touch. Without a comment-preserving
        // backend, the previous yaml_serde round-trip would erase
        // every `# ...` line on any non-no-op edit.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pnpm-workspace.yaml");
        let original = "\
# header explaining the workspace
packages:
  # globs we ship
  - 'pkgs/*'
allowBuilds:
  # esbuild ships native bindings
  esbuild: true
";
        std::fs::write(&path, original).unwrap();
        edit_workspace_yaml(&path, |map| {
            let allow_builds = workspace_yaml_submap(map, "allowBuilds", &path)?;
            allow_builds.insert(
                yaml_serde::Value::String("sharp".to_string()),
                yaml_serde::Value::Bool(true),
            );
            Ok(())
        })
        .unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(
            written.contains("# header explaining the workspace"),
            "header comment lost:\n{written}"
        );
        assert!(
            written.contains("# globs we ship"),
            "sequence comment lost:\n{written}"
        );
        assert!(
            written.contains("# esbuild ships native bindings"),
            "annotation comment lost:\n{written}"
        );
        assert!(
            written.contains("sharp: true"),
            "new entry not added:\n{written}"
        );
    }

    #[test]
    fn upsert_workspace_patched_dependency_preserves_comments_on_real_change() {
        // patch-commit on a workspace yaml that already documents
        // existing patches with `# ...` annotations: the new entry
        // lands at the end and the original annotations stay put.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pnpm-workspace.yaml");
        let original = "\
patchedDependencies:
  # a is patched because of upstream bug #123
  \"a@1.0.0\": patches/a@1.0.0.patch
";
        std::fs::write(&path, original).unwrap();
        upsert_workspace_patched_dependency(&path, "b@2.0.0", "patches/b@2.0.0.patch").unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(
            written.contains("# a is patched because of upstream bug #123"),
            "annotation comment lost:\n{written}"
        );
        assert!(written.contains("b@2.0.0"), "new entry missing:\n{written}");
    }

    #[test]
    fn add_to_allow_builds_merges_with_quoted_existing_key() {
        // Repro for a bats failure: the workspace yaml's existing
        // entry uses a quoted key (`"@pnpm.e2e/install-script-example"`).
        // Adding a new entry must produce a parse-able file regardless
        // of how the existing key was quoted.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "allowBuilds:\n  \"@pnpm.e2e/install-script-example\": true\n",
        )
        .unwrap();
        add_to_allow_builds(
            dir.path(),
            &["@pnpm.e2e/pre-and-postinstall-scripts-example".to_string()],
        )
        .unwrap();
        let written = std::fs::read_to_string(dir.path().join("pnpm-workspace.yaml")).unwrap();
        let _config: WorkspaceConfig = yaml_serde::from_str(&written)
            .unwrap_or_else(|e| panic!("written yaml fails to parse: {e}\n{written}"));
        assert!(
            written.contains("@pnpm.e2e/install-script-example"),
            "existing entry lost:\n{written}"
        );
        assert!(
            written.contains("@pnpm.e2e/pre-and-postinstall-scripts-example"),
            "new entry missing:\n{written}"
        );
    }

    #[test]
    fn upsert_workspace_patched_dependency_does_not_quote_unreserved_at_keys() {
        // Cursor bot follow-up: `b@2.0.0` and `is-positive@3.1.0` are
        // valid YAML plain scalars (the `@` is reserved only when it
        // *starts* a scalar). Earlier revisions of `scalar_key_str`
        // quoted them anyway, producing `"b@2.0.0": ...` style entries
        // that drifted from the rest of the file. Guard the unquoted
        // form on the wire.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pnpm-workspace.yaml");
        upsert_workspace_patched_dependency(&path, "b@2.0.0", "patches/b@2.0.0.patch").unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(
            written.contains("\n  b@2.0.0: patches/b@2.0.0.patch"),
            "expected unquoted plain-scalar key:\n{written}"
        );
    }

    #[test]
    fn upsert_workspace_patched_dependency_quotes_leading_at_keys() {
        // The complement of the above: a key that *starts* with `@`
        // (scoped npm package) must be quoted — leading `@` is a YAML
        // reserved indicator and would otherwise produce a parse
        // error on read.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pnpm-workspace.yaml");
        upsert_workspace_patched_dependency(&path, "@scope/pkg@1.0.0", "patches/scope-pkg.patch")
            .unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        // Round-trip through the typed parser as the soundness check.
        let parsed: WorkspaceConfig = yaml_serde::from_str(&written)
            .unwrap_or_else(|e| panic!("written yaml fails to parse: {e}\n{written}"));
        assert_eq!(
            parsed
                .patched_dependencies
                .get("@scope/pkg@1.0.0")
                .map(String::as_str),
            Some("patches/scope-pkg.patch"),
            "scoped key did not round-trip:\n{written}"
        );
    }

    #[test]
    fn edit_workspace_yaml_adds_nested_mapping_under_existing_parent() {
        // Same shape as the top-level case below, but the new
        // sub-mapping (`my-catalog`) lands under an *existing*
        // `catalogs:` block. yamlpatch's Op::Add mishandles this by
        // collapsing nested indentation; the helper has to fall
        // through to direct injection to keep the YAML structurally
        // valid.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pnpm-workspace.yaml");
        std::fs::write(&path, "catalogs:\n  evens:\n    is-even: ^1.0.0\n").unwrap();
        edit_workspace_yaml(&path, |map| {
            let catalogs = workspace_yaml_submap(map, "catalogs", &path)?;
            let named = workspace_yaml_submap(catalogs, "my-catalog", &path)?;
            named.insert(
                yaml_serde::Value::String("is-even".to_string()),
                yaml_serde::Value::String("^1.0.0".to_string()),
            );
            Ok(())
        })
        .unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        let parsed: WorkspaceConfig = yaml_serde::from_str(&written).unwrap_or_else(|e| {
            panic!("written yaml fails to parse as WorkspaceConfig: {e}\n{written}")
        });
        assert_eq!(
            parsed
                .catalogs
                .get("evens")
                .and_then(|m| m.get("is-even"))
                .unwrap(),
            "^1.0.0"
        );
        assert_eq!(
            parsed
                .catalogs
                .get("my-catalog")
                .and_then(|m| m.get("is-even"))
                .unwrap(),
            "^1.0.0"
        );
    }

    #[test]
    fn edit_workspace_yaml_adds_nested_mapping_and_round_trips() {
        // Repro for a bats failure: `aube add --save-catalog-name=my-catalog`
        // against a workspace yaml that already declares the default
        // `catalog:` map should append a *new* `catalogs:` block whose
        // value is a nested mapping (catalogs.my-catalog.<pkg>: <range>).
        // The write must produce yaml that parses back as
        // `catalogs: { <name>: { <pkg>: <range> } }`, not as a string.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pnpm-workspace.yaml");
        std::fs::write(&path, "catalog:\n  is-odd: ^3.0.1\n").unwrap();
        edit_workspace_yaml(&path, |map| {
            let catalogs = workspace_yaml_submap(map, "catalogs", &path)?;
            let named = workspace_yaml_submap(catalogs, "my-catalog", &path)?;
            named.insert(
                yaml_serde::Value::String("is-even".to_string()),
                yaml_serde::Value::String("^1.0.0".to_string()),
            );
            Ok(())
        })
        .unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        let parsed: WorkspaceConfig = yaml_serde::from_str(&written).unwrap_or_else(|e| {
            panic!("written yaml fails to parse as WorkspaceConfig: {e}\n{written}")
        });
        assert_eq!(parsed.catalog.get("is-odd").unwrap(), "^3.0.1");
        assert_eq!(
            parsed
                .catalogs
                .get("my-catalog")
                .and_then(|m| m.get("is-even"))
                .unwrap(),
            "^1.0.0"
        );
    }

    #[test]
    fn edit_workspace_yaml_adds_sequence_value_as_block_style() {
        // Greptile/cursor follow-up: `render_entry`'s catch-all arm
        // would inline a sequence value as `key: - a\n- b\n` when
        // run through the default scalar path. The new entry must
        // emit block-style so a re-parse round-trips through the
        // typed `packages` field on `WorkspaceConfig`.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pnpm-workspace.yaml");
        std::fs::write(&path, "shamefullyHoist: true\n").unwrap();
        edit_workspace_yaml(&path, |map| {
            let packages = vec![
                yaml_serde::Value::String("pkgs/*".to_string()),
                yaml_serde::Value::String("apps/*".to_string()),
            ];
            map.insert(
                yaml_serde::Value::String("packages".to_string()),
                yaml_serde::Value::Sequence(packages),
            );
            Ok(())
        })
        .unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        let parsed: WorkspaceConfig = yaml_serde::from_str(&written).unwrap_or_else(|e| {
            panic!("written yaml fails to parse as WorkspaceConfig: {e}\n{written}")
        });
        assert_eq!(parsed.packages, vec!["pkgs/*", "apps/*"]);
        assert_eq!(parsed.shamefully_hoist, Some(true));
    }

    #[test]
    fn edit_workspace_yaml_replaces_scalar_with_nested_mapping() {
        // Greptile follow-up: when a key changes from a scalar value
        // (or any non-mapping shape) to a non-empty sub-mapping, the
        // raw `Op::Replace` path through yamlpatch strips nested
        // indentation. The diff plumbing has to split the change into
        // a Remove + manual injection so the new sub-mapping's
        // children land at the canonical column rather than aliased
        // to the parent's.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pnpm-workspace.yaml");
        std::fs::write(&path, "shamefullyHoist: true\nplaceholder: legacy\n").unwrap();
        edit_workspace_yaml(&path, |map| {
            let mut nested = yaml_serde::Mapping::new();
            nested.insert(
                yaml_serde::Value::String("react".to_string()),
                yaml_serde::Value::String("^18".to_string()),
            );
            map.insert(
                yaml_serde::Value::String("placeholder".to_string()),
                yaml_serde::Value::Mapping(nested),
            );
            Ok(())
        })
        .unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        let doc: yaml_serde::Value = yaml_serde::from_str(&written)
            .unwrap_or_else(|e| panic!("written yaml fails to parse: {e}\n{written}"));
        let placeholder = doc
            .as_mapping()
            .and_then(|m| m.get("placeholder"))
            .and_then(|v| v.as_mapping())
            .unwrap_or_else(|| panic!("placeholder did not round-trip as a mapping:\n{written}"));
        assert_eq!(
            placeholder.get("react").and_then(|v| v.as_str()),
            Some("^18"),
            "scalar -> mapping replacement lost child:\n{written}"
        );
    }

    #[test]
    fn remove_workspace_patched_dependency_preserves_comments_on_real_remove() {
        // Removing one patch entry from a multi-entry list must keep
        // the surviving entries' annotation comments intact.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pnpm-workspace.yaml");
        let original = "\
patchedDependencies:
  # a is patched because of upstream bug #123
  \"a@1.0.0\": patches/a@1.0.0.patch
  # b is patched for a build issue
  \"b@2.0.0\": patches/b@2.0.0.patch
";
        std::fs::write(&path, original).unwrap();
        let removed = remove_workspace_patched_dependency(&path, "a@1.0.0").unwrap();
        assert!(removed);
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(
            written.contains("# b is patched for a build issue"),
            "surviving annotation lost:\n{written}"
        );
        assert!(
            !written.contains("a@1.0.0"),
            "removed entry still present:\n{written}"
        );
    }
}
