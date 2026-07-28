//! Ordered filesystem mount-plan derivation for the stock Bubblewrap backend.
//!
//! Bubblewrap applies bind mounts in argv order. Keeping that order is what lets a
//! policy express a writable parent, a read-only child cap, and a still-narrower
//! writable reopen without recursively walking the project tree.

use crate::matcher::path::{PathMatcher, normalize_slashes};
use crate::policy::{Effect, FsAccess, FsOrigin, FsPolicy, SandboxPolicy};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MountGrant {
    pub path: PathBuf,
    pub access: MountAccess,
    /// Position of the allow rule in `FsRuleSet::entries` that produced this bind.
    /// The emitter interleaves binds and deny masks by this key, so a bind lands where
    /// the policy authored it rather than ahead of every mask.
    pub rule_index: usize,
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
/// Denies are installed later as masks. Every non-whole allow must be a literal source;
/// broad glob expansion and future-path creation would either scan an unbounded tree or
/// widen the authored policy. Whether that source has to exist depends on who named it —
/// see the [`FsOrigin`] arm below.
pub(crate) fn compile_mount_plan(policy: &SandboxPolicy) -> Result<Vec<MountGrant>, String> {
    let mut grants = Vec::new();
    let mut previous_grant: Option<MountGrant> = None;
    let matcher = PathMatcher::new(&policy.fs.rules);

    for (rule_index, rule) in policy.fs.rules.entries.iter().enumerate() {
        let pattern = rule.matcher.as_str();
        if is_whole_root(pattern) {
            grants.clear();
            previous_grant = None;
            if rule.effect == Effect::Allow && rule.access == FsAccess::ReadWrite {
                return Err("a writable whole-filesystem mount is not allowed".to_string());
            }
            continue;
        }
        if rule.effect != Effect::Allow {
            previous_grant = None;
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
        // A bind must never re-expose a path the policy goes on to DENY. An allow that a
        // later rule shadows is dead policy — the surface compiler already warns that it
        // "can never take effect" — and the old emitter got away with compiling it anyway,
        // because writing every mask after every bind buried the dead bind under the mask.
        // Ordered emission puts that bind AFTER the mask instead, which hands the denied
        // file straight back; the `.env`/`.npmrc` floor is appended after every user entry,
        // so this is the common shape, not an exotic one. Dropping the grant fixes it where
        // it originates rather than reintroducing an ordering rule that would also break
        // the legitimate reopen.
        //
        // Placed ahead of the existence check on purpose: a grant that can never take
        // effect has no business aborting the launch over a source it will not mount. The
        // representability and safety refusals above still apply to it.
        if matcher.last_matching_effect_after(&path, &path, rule_index + 1) == Some(Effect::Deny) {
            continue;
        }

        if !path.exists() {
            // A speculated grant covers every ecosystem it knows of, so most are absent
            // on any given machine — refusing there would abort every confined run on a
            // clean host over caches the policy never depended on. Absence is not a hole
            // either: the bind is src==dest, so a missing source has an equally missing
            // destination under any ancestor bind, and no existing content becomes
            // reachable. An AUTHORED path is the opposite — the author named a specific
            // location, so a missing one is a mistake worth refusing rather than silently
            // downgrading to a grant that is not there. Skipping leaves `previous_grant`
            // alone deliberately: a rule that emitted no bind cannot have changed which
            // operation the next twin would duplicate.
            if rule.origin == FsOrigin::Speculative {
                continue;
            }
            return Err(format!(
                "filesystem mount source does not exist: {}",
                path.display()
            ));
        }

        let grant = MountGrant {
            path,
            access,
            rule_index,
        };
        // The compiler emits a literal and an adjacent `literal/**` subtree twin.
        // They are one bind operation; no non-adjacent entry is collapsed because
        // intervening rules can deliberately change the mount layering. The twins
        // differ only in `rule_index`, so the duplicate test compares what makes them
        // the same OPERATION — the collapsed grant keeps the EARLIER position, which is
        // where the pair starts and therefore where the bind belongs in the stream.
        if previous_grant
            .as_ref()
            .is_none_or(|previous| (&previous.path, previous.access) != (&grant.path, grant.access))
        {
            grants.push(grant.clone());
        }
        previous_grant = Some(grant);
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
    use std::collections::BTreeMap;
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
            origin: FsOrigin::Authored,
        }
    }

    fn speculative_allow(path: impl Into<String>, access: FsAccess) -> FsRule {
        FsRule {
            origin: FsOrigin::Speculative,
            ..allow(path, access)
        }
    }

    fn deny(path: impl Into<String>) -> FsRule {
        FsRule {
            matcher: CanonGlob(path.into()),
            effect: Effect::Deny,
            access: FsAccess::DENY,
            origin: FsOrigin::Authored,
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
                    access: MountAccess::ReadWrite,
                    rule_index: 0,
                },
                MountGrant {
                    path: child,
                    access: MountAccess::ReadOnly,
                    rule_index: 1,
                },
                MountGrant {
                    path: grandchild,
                    access: MountAccess::ReadWrite,
                    rule_index: 2,
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
            // Speculation excuses an ABSENT source, never an unsafe one.
            speculative_allow("/proc/self", FsAccess::Read),
        ];
        for rule in cases {
            let matcher = rule.matcher.as_str().to_string();
            assert!(
                compile_mount_plan(&policy(vec![rule])).is_err(),
                "allow {matcher:?} must not compile to a mount"
            );
        }
    }

    /// `$tooldirs` names every ecosystem's cache dir at once, so a clean machine carries
    /// almost none of them. Those members drop out of the plan instead of aborting the
    /// run; a member that is present still binds, and an authored path — one the policy
    /// author named specifically — gets no such tolerance.
    #[test]
    fn absent_speculative_sources_drop_out_while_authored_ones_still_refuse() {
        let dir = tempdir().unwrap();
        let present = dir.path().join("pnpm-store");
        let absent = dir.path().join("gradle-caches");
        std::fs::create_dir_all(&present).unwrap();

        let plan = compile_mount_plan(&policy(vec![
            speculative_allow(absent.to_string_lossy(), FsAccess::Read),
            speculative_allow(present.to_string_lossy(), FsAccess::Read),
        ]))
        .unwrap_or_else(|error| {
            panic!("an absent speculated source must not abort the mount plan: {error}")
        });
        assert_eq!(
            plan,
            vec![MountGrant {
                path: present,
                access: MountAccess::ReadOnly,
                rule_index: 1,
            }],
            "only the present speculated path binds; {} is not on this machine",
            absent.display()
        );

        let authored = compile_mount_plan(&policy(vec![allow(
            absent.to_string_lossy(),
            FsAccess::Read,
        )]));
        assert!(
            authored.is_err(),
            "an absent path the author named must still refuse, got {authored:?}"
        );
    }

    /// The dotenv/npmrc floor is appended after every user entry, so an explicit allow of
    /// a secret inside a denied directory is dead policy the compiler already warns about.
    /// It must not reach the mount plan: ordered emission would write that bind after the
    /// mask and hand the file back, which is exactly what it hid before.
    #[test]
    fn an_allow_a_later_deny_shadows_never_becomes_a_bind() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let private = root.join("private");
        let secret = private.join(".env");
        std::fs::create_dir_all(&private).unwrap();
        std::fs::write(&secret, "SECRET").unwrap();

        let plan = compile_mount_plan(&policy(vec![
            allow(root.to_string_lossy(), FsAccess::ReadWrite),
            deny(private.to_string_lossy()),
            allow(secret.to_string_lossy(), FsAccess::Read),
            deny("**/.env*"),
        ]))
        .unwrap();

        assert_eq!(
            plan.iter()
                .map(|grant| grant.path.clone())
                .collect::<Vec<_>>(),
            vec![root.to_path_buf()],
            "{} is denied last, so binding it would reopen what the mask hides",
            secret.display()
        );
    }

    /// The control for the test above: with no later deny, the same nested allow DOES
    /// bind. Without this, the shadow filter could drop every nested grant and still pass.
    #[test]
    fn an_unshadowed_nested_allow_still_becomes_a_bind() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let reopened = root.join("private/reopened");
        std::fs::create_dir_all(&reopened).unwrap();

        let plan = compile_mount_plan(&policy(vec![
            allow(root.to_string_lossy(), FsAccess::ReadWrite),
            deny(root.join("private").to_string_lossy()),
            allow(reopened.to_string_lossy(), FsAccess::ReadWrite),
            deny("**/.env*"),
        ]))
        .unwrap();

        assert_eq!(
            plan.iter()
                .map(|grant| grant.path.clone())
                .collect::<Vec<_>>(),
            vec![root.to_path_buf(), reopened],
            "nothing after it denies the reopen, so it must still bind"
        );
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

    /// The build jail speculates on every read it takes: the dependency tree, nub's PM
    /// cache (where it bootstraps node-gyp), and a per-spawn toolchain layout
    /// (`<node-root>/include/node`) derived without ever looking it up. A clean machine
    /// (fresh container, CI runner, a distro Node whose headers ship separately) has almost
    /// none of them, and before this was origin-aware the FIRST absent one aborted every
    /// confined lifecycle script. The jail must still compile there, and still compile to
    /// the same confinement.
    ///
    /// It also pins the SHAPE the read-set measurement settled on: the mount plan reaches
    /// `<project>/node_modules`, NOT the project root. The consuming project's source,
    /// config, `.git/hooks/` and `.github/workflows/` are outside the jail's read set, and
    /// a plan naming the project root would silently put them all back.
    #[test]
    fn build_jail_survives_a_machine_with_no_tool_caches() {
        let dir = tempdir().unwrap();
        let home = dir.path().join("home");
        let project = dir.path().join("proj");
        let package_dir = project.join("node_modules/native");
        std::fs::create_dir_all(&package_dir).unwrap();
        std::fs::create_dir_all(home.join(".cache")).unwrap();
        let policy = crate::compile_build_jail(
            crate::Homes {
                home: home.join(".cache").parent().unwrap().to_path_buf(),
                tmp: std::env::temp_dir(),
                cache: home.join(".cache"),
                project: project.clone(),
            },
            &package_dir,
            Vec::new(),
            // The header dir a distro Node without its `-dev` package does not ship.
            vec![dir.path().join("node-root/include/node")],
            BTreeMap::new(),
        )
        .expect("the build-jail preset compiles");

        let plan = compile_mount_plan(&policy).unwrap_or_else(|error| {
            panic!("the build jail must compile where no tool cache exists: {error}")
        });
        // Compared as (path, access): the preset's absolute rule positions are its own
        // business, and pinning them here would make this confinement assertion fail on
        // any unrelated reordering of the jail's speculated reads.
        assert_eq!(
            plan.iter()
                .map(|grant| (grant.path.clone(), grant.access))
                .collect::<Vec<_>>(),
            vec![
                (
                    std::fs::canonicalize(project.join("node_modules")).unwrap(),
                    MountAccess::ReadOnly,
                ),
                (
                    std::fs::canonicalize(&package_dir).unwrap(),
                    MountAccess::ReadWrite,
                ),
            ],
            "dropping the absent cache dirs must leave the confinement intact — the \
             dependency tree read-only and the package dir the only writable subtree"
        );
    }
}
