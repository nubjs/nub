//! Node's own command-line grammar, as far as nub reads a user's Node argv: where
//! Node's options end and the entry begins, and the mode spellings nub hands on.

/// Every Node option that takes a value, under each name Node accepts for it, so a
/// separate value is never mistaken for the entry. The union of each release's own
/// option table from 18.19 through 26 (`internalBinding('options')`, every option of a
/// string, list, number or host-port type), plus the aliases that expand to one. A
/// name one release lacks makes that release refuse the command line, so listing it
/// costs nothing there. `-p` and `--print` take the code to print, as `-e` does: Node
/// rewrites `--print <arg>` to `--print --eval <arg>`. Left out on purpose:
/// `--experimental-default-config-file`, an alias of a value option that Node's own
/// filters treat as a single token. A name missing here only ends the scan early, at
/// its value, so it can never carry the scan past the entry into the program's
/// arguments; the cost is a valued mode flag after it going unrewritten, which on 26
/// and later Node contains by itself.
// @lat: [[research/node-flag-arity#Node flag arity — the exhaustive value-accepting set, for file-tier subject resolution#4. The exhaustive value-accepting set]]
const VALUE_OPTIONS: &[&str] = &[
    "-C",
    "-e",
    "-p",
    "-pe",
    "-r",
    "--allow-fs-read",
    "--allow-fs-write",
    "--build-sea",
    "--build-snapshot-config",
    "--conditions",
    "--cpu-prof-dir",
    "--cpu-prof-interval",
    "--cpu-prof-name",
    "--debug-port",
    "--diagnostic-dir",
    "--disable-proto",
    "--disable-warning",
    "--dns-result-order",
    "--env-file",
    "--env-file-if-exists",
    "--es-module-specifier-resolution",
    "--eval",
    "--experimental-config-file",
    "--experimental-default-type",
    "--experimental-loader",
    "--experimental-package-map",
    "--experimental-policy",
    "--experimental-sea-config",
    "--experimental-specifier-resolution",
    "--experimental-test-isolation",
    "--experimental-test-tag-filter",
    "--heap-prof-dir",
    "--heap-prof-interval",
    "--heap-prof-name",
    "--heapsnapshot-near-heap-limit",
    "--heapsnapshot-signal",
    "--icu-data-dir",
    "--import",
    "--input-type",
    "--inspect-port",
    "--inspect-publish-uid",
    "--loader",
    "--localstorage-file",
    "--max-http-header-size",
    "--max-old-space-size-percentage",
    "--network-family-autoselection-attempt-timeout",
    "--openssl-config",
    "--policy-integrity",
    "--print",
    "--redirect-warnings",
    "--report-dir",
    "--report-directory",
    "--report-filename",
    "--report-signal",
    "--require",
    "--run",
    "--secure-heap",
    "--secure-heap-min",
    "--security-revert",
    "--security-reverts",
    "--snapshot-blob",
    "--stack-trace-limit",
    "--test-concurrency",
    "--test-coverage-branches",
    "--test-coverage-exclude",
    "--test-coverage-functions",
    "--test-coverage-include",
    "--test-coverage-lines",
    "--test-global-setup",
    "--test-isolation",
    "--test-name-pattern",
    "--test-random-seed",
    "--test-reporter",
    "--test-reporter-destination",
    "--test-rerun-failures",
    "--test-shard",
    "--test-skip-pattern",
    "--test-timeout",
    "--title",
    "--tls-cipher-list",
    "--tls-keylog",
    "--trace-event-categories",
    "--trace-event-file-pattern",
    "--trace-require-module",
    "--unhandled-rejections",
    "--use-largepages",
    "--v8-pool-size",
    "--watch-kill-signal",
    "--watch-path",
];

/// The modes that start the program in a child of their own.
const CHILD_MODES: [&str; 2] = ["--test", "--watch"];

/// Where the entry sits in a Node command line: the first argument that is neither
/// an option nor the separate value of one, or the argument after `--`. `-` is the
/// entry (stdin). `None` when there is none.
pub fn entry_index(args: &[String]) -> Option<usize> {
    let end = options_end(args);
    match args.get(end) {
        Some(arg) if arg == "--" => (end + 1 < args.len()).then_some(end + 1),
        Some(_) => Some(end),
        None => None,
    }
}

/// `args` with the test runner and watch mode spelled bare wherever Node reads them
/// as its own options.
///
/// Both modes start the program in a child, and keep themselves out of it by
/// dropping the exact token `--test` or `--watch` from the `execArgv` they pass
/// down. A valued spelling such as `--test=true` selects the same mode (Node ignores
/// a boolean's value) but survives that filter, so the child starts in the mode
/// too. On 18.x and 20.0–20.12 the test child then starts another, and so on
/// until the machine runs out of processes; `--watch=<value>` does the same on
/// 18.x and through 20.11. From 20.13 a runner started inside a test child refuses to run its files
/// (nodejs/node#52322), so there the same command skips the tests instead. Rewritten
/// here, where every Node nub launches gets its argv, the mode stays out of every
/// child on every release, and the launched process runs in the mode it would have.
pub fn child_safe_modes(args: &[String]) -> Vec<String> {
    let end = options_end(args);
    let mut out = args.to_vec();
    let mut at = 0;
    while at < end {
        let arg = &args[at];
        if let Some(mode) = CHILD_MODES.iter().find(|mode| {
            arg.strip_prefix(**mode)
                .is_some_and(|rest| rest.starts_with('='))
        }) {
            out[at] = (*mode).to_string();
        }
        at += option_width(args, at);
    }
    out
}

/// The index of the first argument that is not one of Node's options or the value
/// of one: the entry, a `--`, or the end of the line.
fn options_end(args: &[String]) -> usize {
    let mut at = 0;
    while let Some(arg) = args.get(at) {
        if arg == "--" || arg == "-" || !arg.starts_with('-') {
            return at;
        }
        at += option_width(args, at);
    }
    args.len()
}

/// How many arguments the option at `at` spans: two when Node reads the next one
/// as its value, which it does only for a value option written without `=`, and
/// only when that next argument does not start with `-` (Node refuses the line
/// otherwise, and `-p` before a flag is a bare boolean). Node names a `--` option
/// without its `=value` and reads `_` in the name as `-`.
fn option_width(args: &[String], at: usize) -> usize {
    let arg = &args[at];
    let name = if arg.starts_with("--") {
        arg.replace('_', "-")
    } else {
        arg.clone()
    };
    let takes_value = !arg.contains('=') && VALUE_OPTIONS.contains(&name.as_str());
    match args.get(at + 1) {
        Some(next) if takes_value && !next.starts_with('-') => 2,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(tokens: &[&str]) -> Vec<String> {
        tokens.iter().map(|token| token.to_string()).collect()
    }

    #[test]
    fn valued_mode_flags_among_nodes_options_are_spelled_bare() {
        let rewrites = [
            (&["--test=true", "a.js"][..], &["--test", "a.js"][..]),
            (&["--watch=false", "a.js"][..], &["--watch", "a.js"][..]),
            (&["--test="][..], &["--test"][..]),
            // Past an option whose value is a separate argument, which a scan that
            // knew only the preload options read as the entry and stopped at.
            (
                &["--test_reporter", "spec", "--test=1", "a.js"][..],
                &["--test_reporter", "spec", "--test", "a.js"][..],
            ),
        ];
        for (input, want) in rewrites {
            assert_eq!(child_safe_modes(&argv(input)), argv(want), "{input:?}");
        }
    }

    #[test]
    fn everything_that_is_not_one_of_nodes_options_is_left_alone() {
        for input in [
            // The program's own arguments, after the entry, after `--`, after `-`.
            &["a.js", "--test=true"][..],
            &["--", "--test=true"][..],
            &["-", "--watch=1"][..],
            // Other options that merely start the same way, and the negations.
            &["--test-only=1", "--watch-path=src", "--no-test=1", "a.js"][..],
        ] {
            assert_eq!(child_safe_modes(&argv(input)), argv(input), "{input:?}");
        }
    }

    #[test]
    fn nubr_reads_nodes_options_from_the_same_list() {
        let source = include_str!("../../../../runtime/nubr.mjs");
        let start = source
            .find("const VALUE_OPTIONS = new Set([")
            .expect("runtime/nubr.mjs declares VALUE_OPTIONS");
        let list = &source[start..start + source[start..].find("]);").unwrap()];
        let nubr: std::collections::BTreeSet<&str> = list.split('"').skip(1).step_by(2).collect();
        let here: std::collections::BTreeSet<&str> = VALUE_OPTIONS.iter().copied().collect();
        assert_eq!(
            nubr, here,
            "runtime/nubr.mjs must list exactly these value options"
        );
    }

    #[test]
    fn the_entry_is_found_past_every_separate_value() {
        let cases = [
            (
                &["--title", "x", "--conditions", "dev", "app.js"][..],
                Some(4),
            ),
            (&["-pe", "1", "app.js"][..], Some(2)),
            (&["--require=x", "app.js"][..], Some(1)),
            (&["-", "a"][..], Some(0)),
            (&["--test", "--", "a.js"][..], Some(2)),
            (&["--test", "--"][..], None),
            (&["--test"][..], None),
        ];
        for (input, want) in cases {
            assert_eq!(entry_index(&argv(input)), want, "{input:?}");
        }
    }
}
