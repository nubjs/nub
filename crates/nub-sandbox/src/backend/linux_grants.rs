//! Ordered filesystem mount-plan derivation for the stock Bubblewrap backend.
//!
//! Bubblewrap applies bind mounts in argv order. Keeping that order is what lets a
//! policy express a writable parent, a read-only child cap, and a still-narrower
//! writable reopen without recursively walking the project tree.

use crate::matcher::path::normalize_slashes;
use crate::policy::{Effect, FsAccess, FsPolicy, SandboxPolicy};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MountGrant {
    pub path: PathBuf,
    pub access: MountAccess,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MountAccess {
    ReadOnly,
    ReadWrite,
}

/// Whether the fs axis changes the host view at all.
pub(crate) fn fs_confines(fs: &FsPolicy) -> bool {
    fs.rules.default_effect != Effect::Allow || !fs.rules.entries.is_empty()
}

/// Compile literal allow entries into Bubblewrap bind operations.
///
/// Denies are installed later as masks. Every non-whole allow must be a literal
/// current-existing source; broad glob expansion and future-path creation would
/// either scan an unbounded tree or widen the authored policy.
pub(crate) fn compile_mount_plan(policy: &SandboxPolicy) -> Result<Vec<MountGrant>, String> {
    let mut grants = Vec::new();
    let mut previous_authored_grant: Option<MountGrant> = None;

    for rule in &policy.fs.rules.entries {
        let pattern = rule.matcher.as_str();
        if is_whole_root(pattern) {
            grants.clear();
            previous_authored_grant = None;
            if rule.effect == Effect::Allow && rule.access == FsAccess::ReadWrite {
                return Err("a writable whole-filesystem mount is not allowed".to_string());
            }
            continue;
        }
        if rule.effect != Effect::Allow {
            previous_authored_grant = None;
            continue;
        }

        let literal = pattern.strip_suffix("/**").unwrap_or(pattern);
        if literal.is_empty() || has_glob_meta(literal) {
            return Err(format!(
                "filesystem allow {pattern:?} cannot be represented by a bounded literal mount plan"
            ));
        }

        let path = PathBuf::from(literal);
        if is_reserved_tree(&path) {
            return Err(format!(
                "filesystem allow under reserved kernel tree {} is not permitted",
                path.display()
            ));
        }
        let access = match rule.access {
            FsAccess::Read => MountAccess::ReadOnly,
            FsAccess::ReadWrite => {
                if is_unsafe_write_root(&path) {
                    return Err(format!(
                        "writable filesystem root {} is too broad",
                        path.display()
                    ));
                }
                MountAccess::ReadWrite
            }
        };
        if !path.exists() {
            return Err(format!(
                "filesystem mount source does not exist: {}",
                path.display()
            ));
        }

        let grant = MountGrant { path, access };
        // The compiler emits a literal and an adjacent `literal/**` subtree twin.
        // They are one bind operation; no non-adjacent entry is collapsed because
        // intervening rules can deliberately change the mount layering.
        if previous_authored_grant.as_ref() != Some(&grant) {
            grants.push(grant.clone());
        }
        previous_authored_grant = Some(grant);
    }

    Ok(grants)
}

pub(crate) fn is_whole_root(pattern: &str) -> bool {
    // The glob-prefix canonicalizer represents the POSIX root literal as an empty
    // string after trimming its trailing separator.
    matches!(pattern, "" | "**" | "/" | "/**")
}

fn has_glob_meta(pattern: &str) -> bool {
    pattern.contains(['*', '?', '[', '{'])
}

fn is_reserved_tree(path: &std::path::Path) -> bool {
    ["/proc", "/sys", "/dev"]
        .iter()
        .any(|root| path == std::path::Path::new(root) || path.starts_with(root))
}

fn is_unsafe_write_root(path: &std::path::Path) -> bool {
    let Some(path) = path.to_str() else {
        return true;
    };
    let normalized = normalize_slashes(path);
    let normalized = normalized.trim_end_matches('/');
    matches!(
        normalized,
        "" | "/"
            | "/usr"
            | "/usr/bin"
            | "/usr/sbin"
            | "/usr/lib"
            | "/usr/lib32"
            | "/usr/lib64"
            | "/usr/libx32"
            | "/usr/local"
            | "/bin"
            | "/sbin"
            | "/lib"
            | "/lib32"
            | "/lib64"
            | "/libx32"
            | "/etc"
            | "/opt"
            | "/boot"
            | "/var"
            | "/home"
            | "/root"
            | "/run"
            | "/srv"
            | "/mnt"
            | "/media"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{CanonGlob, FsRule, FsRuleSet};
    use tempfile::tempdir;

    fn policy(entries: Vec<FsRule>) -> SandboxPolicy {
        let mut policy = SandboxPolicy::default();
        policy.fs.rules = FsRuleSet {
            entries,
            default_effect: Effect::Deny,
        };
        policy
    }

    fn allow(path: impl Into<String>, access: FsAccess) -> FsRule {
        FsRule {
            matcher: CanonGlob(path.into()),
            effect: Effect::Allow,
            access,
        }
    }

    fn deny(path: impl Into<String>) -> FsRule {
        FsRule {
            matcher: CanonGlob(path.into()),
            effect: Effect::Deny,
            access: FsAccess::DENY,
        }
    }

    #[test]
    fn preserves_writable_parent_readonly_child_and_writable_reopen() {
        let dir = tempdir().unwrap();
        let parent = dir.path().join("parent");
        let child = parent.join("child");
        let grandchild = child.join("grandchild");
        std::fs::create_dir_all(&grandchild).unwrap();
        let plan = compile_mount_plan(&policy(vec![
            allow(parent.to_string_lossy(), FsAccess::ReadWrite),
            allow(child.to_string_lossy(), FsAccess::Read),
            allow(grandchild.to_string_lossy(), FsAccess::ReadWrite),
        ]))
        .unwrap();
        assert_eq!(
            plan,
            vec![
                MountGrant {
                    path: parent,
                    access: MountAccess::ReadWrite
                },
                MountGrant {
                    path: child,
                    access: MountAccess::ReadOnly
                },
                MountGrant {
                    path: grandchild,
                    access: MountAccess::ReadWrite
                },
            ]
        );
    }

    #[test]
    fn collapses_only_adjacent_literal_subtree_twins() {
        let dir = tempdir().unwrap();
        let path = dir.path().to_string_lossy().into_owned();
        let plan = compile_mount_plan(&policy(vec![
            allow(&path, FsAccess::Read),
            allow(format!("{path}/**"), FsAccess::Read),
            deny(format!("{path}/secret")),
            allow(&path, FsAccess::Read),
        ]))
        .unwrap();
        assert_eq!(plan.len(), 2);
    }

    #[test]
    fn whole_root_rule_resets_earlier_literal_grants() {
        let dir = tempdir().unwrap();
        let before = dir.path().join("before");
        let after = dir.path().join("after");
        std::fs::create_dir_all(&before).unwrap();
        std::fs::create_dir_all(&after).unwrap();
        let plan = compile_mount_plan(&policy(vec![
            allow(before.to_string_lossy(), FsAccess::Read),
            allow("**", FsAccess::Read),
            allow(after.to_string_lossy(), FsAccess::ReadWrite),
        ]))
        .unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].path, after);
    }

    #[test]
    fn rejects_wildcards_reserved_trees_unsafe_writes_and_missing_sources() {
        let cases = [
            allow("/tmp/*", FsAccess::Read),
            allow("/proc/self", FsAccess::Read),
            allow("/etc", FsAccess::ReadWrite),
            allow("/definitely/not/a/current/nub/path", FsAccess::ReadWrite),
        ];
        for rule in cases {
            assert!(compile_mount_plan(&policy(vec![rule])).is_err());
        }
    }

    #[test]
    fn rejects_writable_whole_root() {
        assert!(compile_mount_plan(&policy(vec![allow("**", FsAccess::ReadWrite)])).is_err());
    }

    #[test]
    fn confinement_detects_non_relaxed_rulesets() {
        assert!(!fs_confines(&FsPolicy {
            rules: FsRuleSet {
                entries: Vec::new(),
                default_effect: Effect::Allow,
            },
            ..FsPolicy::default()
        }));
        assert!(fs_confines(&policy(Vec::new()).fs));
    }
}
