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
    /// LIST the directory node, and open NOTHING under it — the IR's node-only read.
    ///
    /// The compiler spells a subtree as the PAIR `[P, P/**]` (`defaults::subtree_globs`),
    /// so a bare `P` with no twin names the directory NODE alone; its authors are
    /// `preset::project_cwd_node` (the build jail's unconditional project-root cwd grant,
    /// on EVERY policy) and `curated::project_cwd`. Collapsing that into `ReadOnly` handed the
    /// granted package a read of the CONSUMER'S WHOLE PROJECT, because a Landlock rule's
    /// rights are inherited by everything beneath the path it is attached to.
    ListOnly,
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
    // ⛔ THE DENY-SHADOW SCAN BELOW IS O(n) PER RULE, SO RUNNING IT UNCONDITIONALLY MAKES THIS
    // WHOLE FUNCTION O(n²) — AND THAT IS A HANG, NOT A SLOWDOWN. The `read:"disk"` walk
    // (`defaults::disk_minus_secrets_read_allows`) names every non-secret sibling of every
    // ancestor of `$HOME`, which is a few hundred rules on an ordinary layout but 36,579 on a
    // host whose home sits under a crowded tempdir — MEASURED on a macOS box, where
    // `/var/folders/*/*/T` held exactly that many entries and this loop ran for minutes without
    // finishing. Building the `PathMatcher` is itself n glob compilations on top.
    //
    // A ruleset carrying no Deny cannot shadow anything, so both are skippable OUTRIGHT rather
    // than approximated — and the build jail is exactly that ruleset by construction, since
    // `preset::enforce_pure_allowlist` strips every deny as the last step of its compile.
    let has_denies = policy
        .fs
        .rules
        .entries
        .iter()
        .any(|rule| rule.effect == Effect::Deny);
    let matcher = has_denies.then(|| PathMatcher::new(&policy.fs.rules));

    for (rule_index, rule) in policy.fs.rules.entries.iter().enumerate() {
        let pattern = rule.matcher.as_str();
        // ⛔⛔ A whole-root READ allow lands here and emits NOTHING — no `MountGrant`, and (because
        // `linux_landlock::derive_grants` builds its rule set from this same plan) no Landlock rule
        // either. Only the ReadWrite case below is a hard error; the read case is a silent drop.
        //
        // ⛔⛔⛔ DO NOT "FIX" THIS BY SYNTHESISING A `/` READ GRANT. It looks like a one-line
        // omission and it is not. Landlock rules UNION and it has no deny primitive at any ABI, so
        // a read rule on `/` cannot be narrowed by anything nested under it — VERIFIED on ABI v7:
        // under a `/` read rule a decoy at `~/.ssh` was read successfully, and adding a rule on the
        // enclosing directory granting only EXECUTE did not take that back. A `/` read grant is
        // therefore an unclawable credential leak, not an under-grant to be traded away.
        //
        // `read:"disk"` no longer reaches this branch at all: `defaults::disk_minus_secrets_read_-
        // allows` expresses the exclusion POSITIVELY, naming the disk MINUS the secret subtrees as
        // concrete allows, because an allowlist cannot subtract. The rung is live on Linux and is
        // proved end-to-end in `wiki/research/linux-full-disk-read.md`.
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
        // A subtree is the PAIR `[P, P/**]`, so a bare `P` whose own twin does not follow
        // it names the directory NODE. Match the twin on effect and access too: an
        // adjacent `P/**` that denies, or grants differently, is a different rule and
        // does not make `P` a subtree head.
        let twin_pattern = format!("{pattern}/**");
        let node_only = !pattern.ends_with("/**")
            && policy
                .fs
                .rules
                .entries
                .get(rule_index + 1)
                .is_none_or(|twin| {
                    twin.matcher.as_str() != twin_pattern.as_str()
                        || twin.effect != rule.effect
                        || twin.access != rule.access
                });
        let access = match rule.access {
            // A node-only read is exactly the path itself. For a directory that is the
            // listing right and nothing below; for a file, `ReadOnly` already IS the file,
            // since a file has no subtree to over-grant.
            FsAccess::Read if node_only && path.is_dir() => MountAccess::ListOnly,
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
        if matcher.as_ref().is_some_and(|matcher| {
            matcher.last_matching_effect_after(&path, &path, rule_index + 1) == Some(Effect::Deny)
        }) {
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
            if rule.origin.tolerates_absent() {
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

/// Reads [`crate::compiler::defaults::RESERVED_KERNEL_TREES`] rather than restating it: the set
/// the whole-disk read walk OMITS and the set this planner REFUSES have to be the same set, and
/// a hand-copied second copy would desync on the first edit.
fn is_reserved_tree(path: &std::path::Path) -> bool {
    crate::compiler::defaults::RESERVED_KERNEL_TREES
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
        // Each level is spelled as the compiler's subtree PAIR. A bare path with no twin
        // is the directory NODE, which is a different grant — see `MountAccess::ListOnly`.
        let subtree = |p: &std::path::Path, access| {
            [
                allow(p.to_string_lossy(), access),
                allow(format!("{}/**", p.to_string_lossy()), access),
            ]
        };
        let plan = compile_mount_plan(&policy(
            [
                subtree(&parent, FsAccess::ReadWrite),
                subtree(&child, FsAccess::Read),
                subtree(&grandchild, FsAccess::ReadWrite),
            ]
            .into_iter()
            .flatten()
            .collect(),
        ))
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
                    rule_index: 2,
                },
                MountGrant {
                    path: grandchild,
                    access: MountAccess::ReadWrite,
                    rule_index: 4,
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
            allow(format!("{path}/**"), FsAccess::Read),
        ]))
        .unwrap();
        assert_eq!(plan.len(), 2);
    }

    /// A read grant with no `/**` twin is the directory NODE, and must not widen into the
    /// subtree. Its authors are `preset::project_cwd_node` and `curated::project_cwd`, where
    /// the widening handed the granted package a read of the consumer's entire project — and
    /// the first of those is on every build-jail policy, so the stake is no longer a handful
    /// of curated packages. Landlock inherits a rule's
    /// rights down the whole hierarchy beneath the path it is attached to.
    ///
    /// Paired with the twin arm so the assertion cannot pass against a compiler that
    /// classifies every read as node-only, which would break every dependency-tree grant.
    #[test]
    fn a_read_with_no_subtree_twin_grants_the_node_only() {
        let dir = tempdir().unwrap();
        let node = dir.path().join("project");
        std::fs::create_dir_all(node.join("src")).unwrap();
        let node = node.to_string_lossy().into_owned();

        let plan = compile_mount_plan(&policy(vec![allow(&node, FsAccess::Read)])).unwrap();
        assert_eq!(plan[0].access, MountAccess::ListOnly);

        let with_twin = compile_mount_plan(&policy(vec![
            allow(&node, FsAccess::Read),
            allow(format!("{node}/**"), FsAccess::Read),
        ]))
        .unwrap();
        assert_eq!(with_twin[0].access, MountAccess::ReadOnly);
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
            speculative_allow(format!("{}/**", absent.to_string_lossy()), FsAccess::Read),
            speculative_allow(present.to_string_lossy(), FsAccess::Read),
            speculative_allow(format!("{}/**", present.to_string_lossy()), FsAccess::Read),
        ]))
        .unwrap_or_else(|error| {
            panic!("an absent speculated source must not abort the mount plan: {error}")
        });
        assert_eq!(
            plan,
            vec![MountGrant {
                path: present,
                access: MountAccess::ReadOnly,
                rule_index: 2,
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

    /// ⛔ THE `read:"disk"` ALLOW-SET MUST SURVIVE THIS PLANNER, AND NOTHING ELSE TESTED THAT.
    ///
    /// `defaults::disk_minus_secrets_read_allows` walks the real `/` and grants every child that
    /// does not lead to a secret — which on any real host includes the kernel-virtual trees.
    /// [`is_reserved_tree`] refuses those with a hard `Err`, and the Linux build jail is
    /// Landlock-or-nothing and FAIL-CLOSED, so one emitted `/proc/**` does not merely widen the
    /// jail: it stops `read:"disk"` launching the script at all.
    ///
    /// The gap was structural rather than an oversight in either file. The compiler's own tests
    /// (`preset::read_disk_excludes_secret_subtrees_and_emits_no_whole_disk_allow` and its
    /// siblings) assert on the ALLOW-SET and never build a mount plan; this planner's tests use
    /// hand-written fixtures and never consume the compiler's whole-disk output. Only running one
    /// into the other finds it, which is what this test does.
    #[test]
    fn the_read_disk_allow_set_compiles_to_a_mount_plan() {
        let dir = tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::create_dir_all(home.join("Documents")).unwrap();
        // A real directory name carrying a glob metacharacter. The walk emits an unescaped
        // literal, and this planner demands a bounded one — so if that is unhandled the mount
        // plan refuses for a SECOND reason, and one npm cache entry named like this would break
        // the rung on a user's machine.
        std::fs::create_dir_all(home.join("weird[1]name")).unwrap();
        let homes = crate::Homes {
            home: home.clone(),
            tmp: home.join("tmp"),
            cache: home.join("cache"),
            project: home.join("projects"),
        };

        let allows = crate::compiler::defaults::disk_minus_secrets_read_allows(&homes);
        // POSITIVE CONTROL: an emitter that returned nothing would satisfy every assertion below
        // while granting no read at all, which is the failure mode this rung already had once.
        assert!(
            allows.iter().any(|rule| {
                let pattern = rule.matcher.as_str();
                pattern.strip_suffix("/**").unwrap_or(pattern)
                    == home.join("Documents").to_str().unwrap()
            }),
            "the ordinary non-secret sibling must be granted, else this test cannot tell a working \
             exclusion from an emitter that granted nothing: {allows:?}"
        );

        let reserved: Vec<&str> = allows
            .iter()
            .map(|rule| rule.matcher.as_str())
            .filter(|pattern| {
                let literal = pattern.strip_suffix("/**").unwrap_or(pattern);
                is_reserved_tree(std::path::Path::new(literal))
            })
            .collect();
        assert!(
            reserved.is_empty(),
            "the whole-disk read walk must never name a reserved kernel tree — this planner \
             refuses one outright, so emitting it turns read:\"disk\" into a launch failure \
             rather than a broad read grant. Got: {reserved:?}"
        );

        let mut policy = SandboxPolicy::default();
        policy.fs.rules.default_effect = Effect::Deny;
        policy.fs.rules.entries.splice(0..0, allows);
        compile_mount_plan(&policy).unwrap_or_else(|error| {
            panic!("the read:\"disk\" allow-set must compile to a mount plan, got: {error}")
        });
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
            None,
            None,
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
        //
        // The plan carries the path exactly as the COMPILED rule spells it — canonicalized
        // through `canonicalize_glob_prefix`, which strips a Windows `\\?\` verbatim prefix
        // and normalizes to forward slashes. Bare `fs::canonicalize` leaves both, a form
        // the compiler never emits.
        let as_compiled = |p: &std::path::Path| {
            std::path::PathBuf::from(crate::matcher::path::normalize_slashes(
                &crate::matcher::path::canonicalize_including_nonexistent(p).to_string_lossy(),
            ))
        };
        // The jail's own HOME is materialized at compile time under the cache root this
        // fixture does create, so it is part of the plan. Read it back rather than
        // recomputing its hashed name.
        let jail_home = std::fs::read_dir(home.join(".cache/nub/jail-home"))
            .expect("the per-package jail home is materialized")
            .map(|e| e.expect("entry").path())
            .collect::<Vec<_>>();
        assert_eq!(jail_home.len(), 1, "one home per package: {jail_home:?}");
        assert_eq!(
            plan.iter()
                .map(|grant| (grant.path.clone(), grant.access))
                .collect::<Vec<_>>(),
            vec![
                // `ListOnly`, not `ReadOnly` — the cwd grant names the project root NODE, so
                // the project's own files stay out. Widening it here would silently open the
                // consumer's whole tree, which is the one way this grant can do harm.
                (as_compiled(&project), MountAccess::ListOnly),
                (
                    as_compiled(&project.join("node_modules")),
                    MountAccess::ReadOnly,
                ),
                (as_compiled(&jail_home[0]), MountAccess::ReadWrite),
                (as_compiled(&package_dir), MountAccess::ReadWrite),
            ],
            "dropping the absent cache dirs must leave the confinement intact — the project \
             root listable only, the dependency tree read-only, and the only writable \
             subtrees the package dir plus the package's own private home"
        );
        // `as_compiled` shares the normalizer with the code under test, so on Windows the
        // comparison above would stay green if BOTH sides regressed together. Pin the
        // canonical shape independently: an IR path is forward-slashed and never verbatim.
        for grant in &plan {
            let path = grant.path.to_string_lossy();
            assert!(
                !path.contains('\\') && !path.starts_with(r"\\?\"),
                "a mount-plan path must be a plain forward-slashed path: {path}"
            );
        }
    }
}
