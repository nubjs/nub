//! Conservative main-isolate tuning for measured, memory-constrained Node builds.

use super::version::NodeVersion;

pub const SEMI_SPACE_FLAG: &str = "--max-semi-space-size=16";
/// Consumed by the first CJS preload before any application code can create a Worker.
pub const STARTUP_ENV: &str = "__NUB_GC_STARTUP";
const MIB: u64 = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryBudget {
    // Node sizes its nursery from the leaf limit, not our ancestor minimum.
    node_limit: u64,
    effective_limit: u64,
}

/// A closed version list: changing a process-global V8 flag after main-isolate
/// initialization requires checking that release's flag readers and startup order.
/// Explicit Node options also exclude preloads, snapshots, and GC experiments.
pub fn eligible(
    version: &NodeVersion,
    user_args: &[String],
    node_options: Option<&str>,
    memory: impl FnOnce() -> Option<MemoryBudget>,
) -> bool {
    // Measured ranges, not a universal floor: Node 22 above 1 GiB and Node 26
    // regressed production SSR with 16 MiB despite gains in retained JSON.
    // Node 24 already chooses 16 MiB immediately above 512 MiB.
    let ceiling = match version.0.to_string().as_str() {
        "22.23.2" => 1024 * MIB,
        "24.20.0" => 512 * MIB,
        _ => return false,
    };
    node_options.is_none_or(|options| options.trim().is_empty())
        // Everything after an entry path is application argv, not Node options.
        && !user_args.first().is_some_and(|arg| arg.starts_with('-'))
        && memory().is_some_and(|budget| {
            budget.effective_limit >= 512 * MIB && budget.node_limit <= ceiling
        })
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
pub fn constrained_memory() -> Option<MemoryBudget> {
    None
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub fn constrained_memory() -> Option<MemoryBudget> {
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let mounts = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    let limit = read_constraint(&cgroup, &mounts, |path| std::fs::read_to_string(path).ok())?;
    // Node takes the minimum of physical and constrained memory as well.
    // SAFETY: sysconf has no pointer arguments or side effects on process state.
    let pages = unsafe { libc::sysconf(libc::_SC_PHYS_PAGES) };
    // SAFETY: as above; a nonpositive result is rejected below.
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let physical = u64::try_from(pages)
        .ok()?
        .checked_mul(u64::try_from(page_size).ok()?)?;
    (physical > 0).then_some(MemoryBudget {
        node_limit: limit.node_limit,
        effective_limit: limit.effective_limit.min(physical),
    })
}

#[cfg(all(target_os = "linux", any(target_arch = "x86_64", test)))]
fn read_constraint(
    cgroup: &str,
    mounts: &str,
    read: impl Fn(&std::path::Path) -> Option<String>,
) -> Option<MemoryBudget> {
    use std::path::{Component, Path, PathBuf};

    fn mount_path(raw: &str) -> Option<PathBuf> {
        // mountinfo escapes whitespace and backslashes as octal byte sequences.
        let mut decoded = String::new();
        let mut rest = raw;
        while let Some(index) = rest.find('\\') {
            decoded.push_str(&rest[..index]);
            let escape = rest.get(index..index + 4)?;
            decoded.push(match escape {
                "\\040" => ' ',
                "\\011" => '\t',
                "\\012" => '\n',
                "\\134" => '\\',
                _ => return None,
            });
            rest = &rest[index + 4..];
        }
        decoded.push_str(rest);
        let path = PathBuf::from(decoded);
        (path.is_absolute() && !path.components().any(|c| c == Component::ParentDir))
            .then_some(path)
    }

    fn value(raw: &str) -> Option<u64> {
        match raw.trim() {
            "max" => Some(u64::MAX),
            n => n.parse().ok(),
        }
    }

    let mut candidates = Vec::new();
    for line in cgroup.lines() {
        let mut fields = line.splitn(3, ':');
        let id = fields.next()?;
        let controllers = fields.next()?;
        let membership = Path::new(fields.next()?);
        if !membership.is_absolute() || membership.components().any(|c| c == Component::ParentDir) {
            return None;
        }
        let unified = id == "0" && controllers.is_empty();
        if !unified && !controllers.split(',').any(|c| c == "memory") {
            continue;
        }
        for mount in mounts.lines() {
            let Some((before, after)) = mount.split_once(" - ") else {
                continue;
            };
            let mut after = after.split_whitespace();
            let kind = after.next()?;
            let _source = after.next()?;
            let options = after.next()?;
            if !(unified && kind == "cgroup2"
                || !unified && kind == "cgroup" && options.split(',').any(|o| o == "memory"))
            {
                continue;
            }
            let mut before = before.split_whitespace().skip(3);
            let root = mount_path(before.next()?)?;
            let mountpoint = mount_path(before.next()?)?;
            if let Ok(relative) = membership.strip_prefix(&root) {
                candidates.push((
                    root.components().count(),
                    mountpoint.join(relative),
                    mountpoint,
                    unified,
                ));
            }
        }
    }
    candidates.sort_by_key(|(depth, ..)| std::cmp::Reverse(*depth));
    let (_, leaf, mountpoint, unified) = candidates.first()?;
    let (hard, soft) = if *unified {
        ("memory.max", "memory.high")
    } else {
        ("memory.limit_in_bytes", "memory.soft_limit_in_bytes")
    };
    // Require a leaf limit Node itself can see. A parent-only limit may constrain
    // the process without reducing Node's automatic nursery; overriding that
    // already-larger nursery would not be a floor.
    let node_limit = value(&read(&leaf.join(hard))?)?.min(value(&read(&leaf.join(soft))?)?);
    if node_limit == 0 || node_limit > 1024 * MIB {
        return None;
    }
    let mut limit = node_limit;
    for parent in leaf.ancestors().skip(1) {
        if !parent.starts_with(mountpoint) {
            break;
        }
        for name in [hard, soft] {
            match read(&parent.join(name)) {
                Some(raw) => limit = limit.min(value(&raw)?),
                // The unified hierarchy root has no memory controller files.
                None if parent == mountpoint => {}
                None => return None,
            }
        }
    }
    Some(MemoryBudget {
        node_limit,
        effective_limit: limit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    use std::collections::HashMap;

    fn eligible(
        version: &NodeVersion,
        args: &[String],
        options: Option<&str>,
        memory: Option<u64>,
    ) -> bool {
        super::eligible(version, args, options, || {
            memory.map(|limit| MemoryBudget {
                node_limit: limit,
                effective_limit: limit,
            })
        })
    }

    #[test]
    fn ineligible_launches_do_not_read_memory_constraints() {
        for (version, args, options) in [
            ("26.8.1", vec!["app.js".into()], None),
            ("24.20.1", vec!["app.js".into()], None),
            ("24.20.0", vec!["--inspect".into()], None),
            (
                "24.20.0",
                vec!["app.js".into()],
                Some("--require before.cjs"),
            ),
        ] {
            assert!(!super::eligible(
                &version.parse().unwrap(),
                &args,
                options,
                || panic!("ineligible launch read memory constraints")
            ));
        }
    }

    #[test]
    fn policy_is_closed_and_explicit_options_win() {
        for version in ["22.23.2", "24.20.0"] {
            let version = version.parse().unwrap();
            assert!(eligible(
                &version,
                &["app.js".into(), "--port=3000".into()],
                None,
                Some(512 * MIB)
            ));
            for limit in [None, Some(256 * MIB), Some(384 * MIB), Some(512 * MIB - 1)] {
                assert!(!eligible(&version, &[], None, limit));
            }
            for options in [
                "--max_semi_space_size=4",
                "--require before.cjs",
                "--snapshot-blob=app.blob",
            ] {
                assert!(!eligible(&version, &[], Some(options), Some(512 * MIB)));
                assert!(!eligible(
                    &version,
                    &[options.into()],
                    None,
                    Some(512 * MIB)
                ));
            }
        }
        for version in [
            "22.23.1",
            "24.20.1",
            "26.8.1",
            "26.8.2",
            "27.0.0",
            "24.20.0-custom",
        ] {
            assert!(!eligible(
                &version.parse().unwrap(),
                &[],
                None,
                Some(512 * MIB)
            ));
        }
    }

    #[test]
    fn budget_range_stops_at_the_measured_ceiling() {
        for (version, ceiling) in [("22.23.2", 1024), ("24.20.0", 512)] {
            let version = version.parse().unwrap();
            for memory in [512 * MIB, (512 + ceiling) * MIB / 2, ceiling * MIB] {
                assert!(eligible(&version, &[], None, Some(memory)));
            }
            assert!(!eligible(&version, &[], None, Some(ceiling * MIB + 1)));
            assert!(!super::eligible(&version, &[], None, || Some(
                MemoryBudget {
                    node_limit: ceiling * MIB + 1,
                    effective_limit: 512 * MIB,
                }
            )));
            assert!(!super::eligible(&version, &[], None, || Some(
                MemoryBudget {
                    node_limit: ceiling * MIB,
                    effective_limit: 512 * MIB - 1,
                }
            )));
        }
    }

    #[cfg(target_os = "linux")]
    fn detect(cgroup: &str, mounts: &str, files: &[(&str, &str)]) -> Option<MemoryBudget> {
        let files: HashMap<_, _> = files.iter().copied().collect();
        read_constraint(cgroup, mounts, |path| {
            files.get(path.to_str()?).map(|s| s.to_string())
        })
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn v2_resolves_mount_roots_and_hierarchical_limits() {
        let mount = "1 0 0:1 /host/group /cg rw - cgroup2 cgroup rw";
        let files = [
            ("/cg/job/memory.max", "536870912"),
            ("/cg/job/memory.high", "max"),
            ("/cg/memory.max", "268435456"),
            ("/cg/memory.high", "max"),
        ];
        assert_eq!(
            detect("0::/host/group/job", mount, &files),
            Some(MemoryBudget {
                node_limit: 512 * MIB,
                effective_limit: 256 * MIB,
            })
        );
        assert_eq!(detect("0::/host/groupish/job", mount, &files), None);
        assert_eq!(detect("0::/host/group/../job", mount, &files), None);
        assert_eq!(detect("0::/host/group/job", mount, &files[..1]), None);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn ancestor_limits_restrict_headroom_without_hiding_nodes_leaf_budget() {
        let mount = "1 0 0:1 / /cg rw - cgroup2 cgroup rw";
        for (ancestor, expected) in [("805306368", true), ("535822336", false)] {
            let budget = detect(
                "0::/parent/job",
                mount,
                &[
                    ("/cg/parent/job/memory.max", "1073741824"),
                    ("/cg/parent/job/memory.high", "max"),
                    ("/cg/parent/memory.max", "max"),
                    ("/cg/parent/memory.high", ancestor),
                ],
            );
            assert_eq!(budget.map(|b| b.node_limit), Some(1024 * MIB));
            assert_eq!(
                super::eligible(&"22.23.2".parse().unwrap(), &[], None, || budget),
                expected
            );
            assert!(!super::eligible(
                &"24.20.0".parse().unwrap(),
                &[],
                None,
                || budget
            ));
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn v1_memory_controller_and_escaped_mountpoint() {
        let mount = "1 0 0:1 /host /memory\\040group rw - cgroup cgroup rw,memory";
        let files = [
            ("/memory group/job/memory.limit_in_bytes", "536870912"),
            (
                "/memory group/job/memory.soft_limit_in_bytes",
                "9223372036854771712",
            ),
        ];
        assert_eq!(
            detect("4:cpu:/ignored\n5:memory:/host/job", mount, &files),
            Some(MemoryBudget {
                node_limit: 512 * MIB,
                effective_limit: 512 * MIB
            })
        );
        assert_eq!(detect("4:cpu:/host/job", mount, &files), None);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn unknown_unlimited_and_parent_only_limits_do_not_tune() {
        let mount = "1 0 0:1 / /cg rw - cgroup2 cgroup rw";
        for raw in ["max", "0", "garbage", "4294967296"] {
            assert_eq!(
                detect(
                    "0::/job",
                    mount,
                    &[
                        ("/cg/job/memory.max", raw),
                        ("/cg/job/memory.high", "max"),
                        ("/cg/memory.max", "536870912")
                    ]
                ),
                None
            );
        }
        assert_eq!(detect("0::/job", "", &[]), None);
    }
}
