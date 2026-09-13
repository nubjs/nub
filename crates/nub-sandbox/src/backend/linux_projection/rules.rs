use std::io;
use std::path::Path;

use globset::GlobMatcher;

use crate::matcher::path::PathMatcher;
use crate::matcher::path::compile_glob;
use crate::policy::{Effect, FsAccess, FsRuleSet};

pub(super) struct Rules {
    matcher: PathMatcher,
    traversal: Vec<GlobMatcher>,
}

impl Rules {
    pub(super) fn compile(set: &FsRuleSet) -> io::Result<Self> {
        let mut traversal = Vec::new();
        for rule in &set.entries {
            let pattern = rule.matcher.as_str();
            // Public filesystem compilation produces a positive union. Do not
            // reinterpret legacy deny rules, malformed globs, or unresolved roots.
            if rule.effect != Effect::Allow || !pattern.starts_with('/') {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "projection requires resolved absolute positive filesystem grants",
                ));
            }
            compile_glob(pattern).map_err(io::Error::other)?;
            for (index, _) in pattern.match_indices('/') {
                if index != 0 {
                    // A prefix is traversal, not an additional file grant. Reject
                    // unrepresentable partial brace/class expressions rather than
                    // falling back to their literal parent subtree.
                    traversal.push(compile_glob(&pattern[..index]).map_err(io::Error::other)?);
                }
            }
        }
        Ok(Self {
            matcher: PathMatcher::new(set),
            traversal,
        })
    }

    pub(super) fn access(&self, path: &Path) -> Option<FsAccess> {
        let decision = self.matcher.decide_verified_path(path);
        (decision.effect == Effect::Allow).then_some(decision.access)
    }

    pub(super) fn traversable(&self, path: &Path) -> bool {
        path == Path::new("/")
            || self.access(path).is_some()
            || self.traversal.iter().any(|glob| glob.is_match(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{CanonGlob, FsOrigin, FsRule};

    pub(super) fn set(pattern: &str, access: FsAccess) -> FsRuleSet {
        FsRuleSet {
            entries: vec![FsRule {
                matcher: CanonGlob(pattern.into()),
                effect: Effect::Allow,
                access,
                origin: FsOrigin::Authored,
            }],
            default_effect: Effect::Deny,
        }
    }

    #[test]
    fn future_embedded_patterns_have_traversal_but_not_sibling_authority() {
        let rules = Rules::compile(&set("/app/pkg-*/lib/**/*.so", FsAccess::ReadWrite)).unwrap();
        for path in [
            "/",
            "/app",
            "/app/pkg-new",
            "/app/pkg-new/lib",
            "/app/pkg-new/lib/deep",
        ] {
            assert!(rules.traversable(Path::new(path)), "{path}");
            assert_eq!(rules.access(Path::new(path)), None, "{path}");
        }
        assert_eq!(
            rules.access(Path::new("/app/pkg-new/lib/deep/future.so")),
            Some(FsAccess::ReadWrite)
        );
        assert_eq!(
            rules.access(Path::new("/app/pkg-new/lib/deep/future.so-near")),
            None
        );
        assert!(!rules.traversable(Path::new("/app/pkg-new-nearby/wrong")));
    }

    #[test]
    fn read_grants_do_not_subtract_write_and_invalid_rules_fail_acquisition() {
        let mut set = set("/app/**", FsAccess::ReadWrite);
        let mut read = set.entries[0].clone();
        read.access = FsAccess::Read;
        set.entries.push(read);
        assert_eq!(
            Rules::compile(&set).unwrap().access(Path::new("/app/new")),
            Some(FsAccess::ReadWrite)
        );
        set.entries[1].effect = Effect::Deny;
        assert!(Rules::compile(&set).is_err());
        set.entries[1].effect = Effect::Allow;
        set.entries[1].matcher = CanonGlob("/app/[".into());
        assert!(Rules::compile(&set).is_err());
    }
}
