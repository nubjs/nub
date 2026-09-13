use std::collections::VecDeque;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use regex_automata::nfa::thompson::{State, NFA};
use regex_automata::util::primitives::StateID;

use crate::matcher::path::compile_glob;
use crate::matcher::path::PathMatcher;
use crate::policy::{Effect, FsAccess, FsRuleSet};

// Match globset's own regex-NFA cap, so traversal accepts every glob grammar
// instance accepted by the authority matcher while still bounding state memory.
const MAX_TRAVERSAL_NFA_BYTES: usize = 10 * (1 << 20);

pub(super) struct Rules {
    matcher: PathMatcher,
    traversal: Vec<TraversalMatcher>,
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
            // Validate with the authority matcher first. The traversal compiler
            // derives from that same compiled glob regex, rather than parsing
            // the source syntax a second time.
            let authority = compile_glob(pattern).map_err(io::Error::other)?;
            traversal.push(TraversalMatcher::compile(authority.glob().regex())?);
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
            || self.traversal.iter().any(|glob| glob.can_descend(path))
    }
}

/// Decides whether an existing directory can prefix a strictly deeper path
/// matched by its authority glob. This uses globset's compiled regex language,
/// not a second parser for source-glob grammar.
struct TraversalMatcher {
    nfa: NFA,
    can_complete_after_byte: Vec<bool>,
}

impl TraversalMatcher {
    fn compile(regex: &str) -> io::Result<Self> {
        let mut compiler = NFA::compiler();
        // globset parses its generated regex with these same syntax flags.
        // In particular, utf8(false) preserves raw Unix filename bytes.
        compiler.syntax(
            regex_automata::util::syntax::Config::new()
                .utf8(false)
                .dot_matches_new_line(true),
        );
        compiler.configure(
            NFA::config()
                .utf8(false)
                .nfa_size_limit(Some(MAX_TRAVERSAL_NFA_BYTES)),
        );
        let nfa = compiler.build(regex).map_err(io::Error::other)?;
        let can_complete_after_byte = descendant_states(&nfa);
        Ok(Self {
            nfa,
            can_complete_after_byte,
        })
    }

    fn can_descend(&self, path: &Path) -> bool {
        let mut input = path.as_os_str().as_bytes().to_vec();
        if input != b"/" && !input.ends_with(b"/") {
            input.push(b'/');
        }
        let mut active = vec![false; self.nfa.states().len()];
        add_closure(
            &self.nfa,
            &mut active,
            [self.nfa.start_anchored()],
            &input,
            0,
        );
        for (at, byte) in input.iter().copied().enumerate() {
            let mut next = vec![false; active.len()];
            for (index, present) in active.iter().copied().enumerate() {
                if present {
                    if let Some(next_state) =
                        byte_transition(self.nfa.state(StateID::must(index)), byte)
                    {
                        add_closure(&self.nfa, &mut next, [next_state], &input, at + 1);
                    }
                }
            }
            active = next;
        }
        active
            .iter()
            .zip(&self.can_complete_after_byte)
            .any(|(active, can_complete)| *active && *can_complete)
    }
}

fn descendant_states(nfa: &NFA) -> Vec<bool> {
    // Runtime closure checks each Look assertion against the directory bytes.
    // Treating a Look edge as possible here can only retain an extra
    // search-only directory; it cannot exclude an authority-reachable child.
    let states = nfa.states();
    let mut reverse = vec![Vec::new(); states.len()];
    for (index, state) in states.iter().enumerate() {
        for (next, consumes_byte) in edges(state) {
            reverse[next.as_usize()].push((index, consumes_byte));
        }
    }

    let mut any_completion = vec![false; states.len()];
    let mut work = VecDeque::new();
    for (index, state) in states.iter().enumerate() {
        if matches!(state, State::Match { .. }) {
            any_completion[index] = true;
            work.push_back(index);
        }
    }
    while let Some(next) = work.pop_front() {
        for &(previous, _) in &reverse[next] {
            if !any_completion[previous] {
                any_completion[previous] = true;
                work.push_back(previous);
            }
        }
    }

    let mut after_byte = vec![false; states.len()];
    for (next, predecessors) in reverse.iter().enumerate() {
        if any_completion[next] {
            for &(previous, consumes_byte) in predecessors {
                if consumes_byte && !after_byte[previous] {
                    after_byte[previous] = true;
                    work.push_back(previous);
                }
            }
        }
    }
    while let Some(next) = work.pop_front() {
        for &(previous, consumes_byte) in &reverse[next] {
            if !consumes_byte && !after_byte[previous] {
                after_byte[previous] = true;
                work.push_back(previous);
            }
        }
    }
    after_byte
}

fn add_closure(
    nfa: &NFA,
    active: &mut [bool],
    starts: impl IntoIterator<Item = StateID>,
    input: &[u8],
    at: usize,
) {
    let mut stack = starts.into_iter().collect::<Vec<_>>();
    while let Some(state) = stack.pop() {
        let index = state.as_usize();
        if active[index] {
            continue;
        }
        active[index] = true;
        match nfa.state(state) {
            State::Look { look, next } if nfa.look_matcher().matches(*look, input, at) => {
                stack.push(*next);
            }
            State::Union { alternates } => stack.extend(alternates.iter().copied()),
            State::BinaryUnion { alt1, alt2 } => stack.extend([*alt1, *alt2]),
            State::Capture { next, .. } => stack.push(*next),
            _ => {}
        }
    }
}

fn byte_transition(state: &State, byte: u8) -> Option<StateID> {
    match state {
        State::ByteRange { trans } if trans.matches_byte(byte) => Some(trans.next),
        State::Sparse(transitions) => transitions.matches_byte(byte),
        State::Dense(transitions) => transitions.matches_byte(byte),
        _ => None,
    }
}

fn edges(state: &State) -> Vec<(StateID, bool)> {
    match state {
        State::ByteRange { trans } => vec![(trans.next, true)],
        State::Sparse(transitions) => transitions
            .transitions
            .iter()
            .map(|transition| (transition.next, true))
            .collect(),
        State::Dense(transitions) => transitions
            .transitions
            .iter()
            .copied()
            .filter(|state| *state != StateID::ZERO)
            .map(|state| (state, true))
            .collect(),
        State::Look { next, .. } | State::Capture { next, .. } => vec![(*next, false)],
        State::Union { alternates } => alternates
            .iter()
            .copied()
            .map(|state| (state, false))
            .collect(),
        State::BinaryUnion { alt1, alt2 } => vec![(*alt1, false), (*alt2, false)],
        State::Fail | State::Match { .. } => Vec::new(),
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
    fn brace_alternatives_crossing_directories_only_grant_search() {
        let rules = Rules::compile(&set("/app/{a/{b,c},d/e}/file", FsAccess::ReadWrite)).unwrap();
        for path in [
            "/", "/app", "/app/a", "/app/a/b", "/app/a/c", "/app/d", "/app/d/e",
        ] {
            assert!(rules.traversable(Path::new(path)), "{path}");
            assert_eq!(rules.access(Path::new(path)), None, "{path}");
        }
        assert_eq!(
            rules.access(Path::new("/app/a/b/file")),
            Some(FsAccess::ReadWrite)
        );
        assert_eq!(
            rules.access(Path::new("/app/d/e/file")),
            Some(FsAccess::ReadWrite)
        );
        for path in ["/app/x", "/app/a/no", "/app/a/b/sibling"] {
            assert!(!rules.traversable(Path::new(path)), "{path}");
            assert_eq!(rules.access(Path::new(path)), None, "{path}");
        }
    }

    #[test]
    fn class_slashes_do_not_create_invalid_prefix_globs() {
        let rules = Rules::compile(&set("/app/[a/b]/file", FsAccess::Read)).unwrap();
        assert!(rules.traversable(Path::new("/app")));
        assert!(rules.traversable(Path::new("/app/a")));
        assert_eq!(rules.access(Path::new("/app/a")), None);
        assert!(!rules.traversable(Path::new("/app/z")));
        assert_eq!(rules.access(Path::new("/app/a/file")), Some(FsAccess::Read));
    }

    #[test]
    fn negated_class_initial_bracket_keeps_braces_literal() {
        // In globset, the first `]` after `!` is a class member, not its
        // closing delimiter. The braces are therefore members too, rather
        // than a directory-alternative expression.
        let rules = Rules::compile(&set("/app/[!]{a,b}]/file", FsAccess::Read)).unwrap();
        for path in ["/app", "/app/x"] {
            assert!(rules.traversable(Path::new(path)), "{path}");
            assert_eq!(rules.access(Path::new(path)), None, "{path}");
        }
        assert_eq!(rules.access(Path::new("/app/x/file")), Some(FsAccess::Read));
        assert!(!rules.traversable(Path::new("/app/a")));
    }

    #[test]
    fn class_negation_consumes_only_one_marker_before_braces() {
        // `[!!]` has one negation marker and `!` as its first member. The
        // following braces are a real alternative, not class contents.
        let rules = Rules::compile(&set("/app/[!!]{a/x,b/y}]/file", FsAccess::Read)).unwrap();
        for path in ["/app", "/app/za", "/app/za/x]", "/app/zb", "/app/zb/y]"] {
            assert!(rules.traversable(Path::new(path)), "{path}");
            assert_eq!(rules.access(Path::new(path)), None, "{path}");
        }
        assert_eq!(
            rules.access(Path::new("/app/za/x]/file")),
            Some(FsAccess::Read)
        );
        assert_eq!(rules.access(Path::new("/app/za/x]/sibling")), None);
    }

    #[test]
    fn brace_recursive_suffixes_keep_future_leaf_ancestors_search_only() {
        // globset parses the `**` before the brace close as RecursiveSuffix.
        // Traversal is deliberately a search-only superset; the exact matcher
        // below remains the sole authority for the future leaf and siblings.
        let rules = Rules::compile(&set("/app/{lib/**,plain}/file", FsAccess::Read)).unwrap();
        for path in ["/app", "/app/lib", "/app/lib/deep", "/app/plain"] {
            assert!(rules.traversable(Path::new(path)), "{path}");
            assert_eq!(rules.access(Path::new(path)), None, "{path}");
        }
        assert_eq!(
            rules.access(Path::new("/app/lib/deep/file")),
            Some(FsAccess::Read)
        );
        assert_eq!(rules.access(Path::new("/app/lib/deep/sibling")), None);
    }

    #[test]
    fn compiled_regex_reaches_recursive_prefix_inside_braces() {
        let rules = Rules::compile(&set("/app/pre{**/deep,x}/file", FsAccess::Read)).unwrap();
        for path in [
            "/app",
            "/app/preone",
            "/app/preone/two",
            "/app/preone/two/deep",
        ] {
            assert!(rules.traversable(Path::new(path)), "{path}");
            assert_eq!(rules.access(Path::new(path)), None, "{path}");
        }
        assert_eq!(
            rules.access(Path::new("/app/preone/two/deep/file")),
            Some(FsAccess::Read)
        );
        assert_eq!(
            rules.access(Path::new("/app/preone/two/deep/sibling")),
            None
        );
    }

    #[test]
    fn long_valid_glob_uses_the_authority_nfa_cap() {
        assert_eq!(MAX_TRAVERSAL_NFA_BYTES, 10 * (1 << 20));
        let pattern = format!("/app/{}file", "?".repeat(32 * 1024));
        assert!(Rules::compile(&set(&pattern, FsAccess::Read)).is_ok());
    }

    #[test]
    fn escaped_literals_and_unix_bytes_keep_compile_glob_semantics() {
        let literal = Rules::compile(&set(r"/app/\{literal\}/file", FsAccess::Read)).unwrap();
        assert!(literal.traversable(Path::new("/app/{literal}")));
        assert_eq!(literal.access(Path::new("/app/{literal}")), None);
        assert_eq!(
            literal.access(Path::new("/app/{literal}/file")),
            Some(FsAccess::Read)
        );

        let escaped_separator =
            Rules::compile(&set(r"/app/foo\/bar/file", FsAccess::Read)).unwrap();
        assert!(escaped_separator.traversable(Path::new("/app/foo")));
        assert!(escaped_separator.traversable(Path::new("/app/foo/bar")));
        assert_eq!(escaped_separator.access(Path::new("/app/foo/bar")), None);

        #[cfg(target_os = "linux")]
        {
            use std::ffi::OsString;
            use std::os::unix::ffi::OsStringExt;

            let wildcard = Rules::compile(&set("/app/*/file", FsAccess::Read)).unwrap();
            let bad_name = Path::new("/app").join(OsString::from_vec(vec![0xFF]));
            assert!(wildcard.traversable(&bad_name));
            assert_eq!(wildcard.access(&bad_name), None);
        }
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

    #[test]
    fn explicitly_unconstrained_filesystem_preserves_write_authority() {
        let rules = Rules::compile(&FsRuleSet {
            entries: Vec::new(),
            default_effect: Effect::Allow,
        })
        .unwrap();
        assert_eq!(
            rules.access(Path::new("/future/nested/file")),
            Some(FsAccess::ReadWrite)
        );
        assert!(rules.traversable(Path::new("/future/nested")));
    }
}
