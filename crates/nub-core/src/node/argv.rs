//! Node's own command-line grammar, as far as nub reads a user's Node argv: where
//! Node's options end and the entry begins, and the mode spellings nub hands on.

use super::spawn::{node_options_token, split_node_options};

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
/// arguments; the cost is a mode flag after it going unrewritten, which Node contains
/// by itself from 26.13.
// @lat: [[research/node-flag-arity#Node flag arity — the exhaustive value-accepting set, for file-tier subject resolution#4. The exhaustive value-accepting set]]
const VALUE_OPTIONS: &[&str] = &[
    "-C",
    "-e",
    "-p",
    "-pe",
    "-r",
    "--allow-fs-read",
    "--allow-fs-write",
    "--bench-isolation",
    "--bench-name-pattern",
    "--bench-reporter",
    "--bench-reporter-destination",
    "--bench-samples",
    "--bench-warmup",
    "--build-sea",
    "--build-snapshot-config",
    "--conditions",
    "--config-file",
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
    "--process-timeout",
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
    "--vfs-load",
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

/// `args` with the test runner and watch mode spelled the way Node's filters know
/// them, wherever Node reads them as its own options.
///
/// Both modes start the program in a child, and keep themselves out of it by
/// dropping the tokens they recognize from the `execArgv` they pass down. Those
/// filters compare text, while Node's parser ignores a boolean's value and reads `_`
/// in a name as `-`. So `--test=true`, `--watch=1` and `--watch_path=src` select the
/// mode and survive the filter, and the child starts in the mode too. It then starts
/// another, until the machine runs out of processes: `--test=<value>` on 18.x and
/// 20.0–20.12, `--watch=<value>` on 18.x and through 20.11, and `--watch_path` on
/// 18.x, 20.11, 20.12, 24, and 26 before 26.13. From 20.13 a runner started inside a
/// test child refuses to run its files (nodejs/node#52322), so there `--test=<value>`
/// skips the tests instead. Rewritten here, where every Node nub launches gets its
/// argv, the mode stays out of every child on every release, and the launched process
/// runs in the mode it would have.
pub fn child_safe_modes(args: &[String]) -> Vec<String> {
    let end = options_end(args);
    let mut out = args.to_vec();
    let mut at = 0;
    while at < end {
        if let Some(spelling) = filtered_spelling(&args[at]) {
            out[at] = spelling;
        }
        at += option_width(args, at);
    }
    out
}

/// `NODE_OPTIONS` split into what stays in it and the watch flags it carried, ready
/// for the launched process's argv. `None` when it selects no watch mode, so the value
/// goes on byte for byte.
///
/// The variable reaches every descendant, and no argv filter touches it, so a watch
/// flag there starts watch mode again in the child watch mode spawns. Node refused
/// the flag here until `--watch` with no file became legal, then recursed
/// (nodejs/node#61740), and its fix strips these from the child's `NODE_OPTIONS`.
/// Moving them to argv gives that behavior on every release: the launched process
/// runs in watch mode and nothing below it does. A `--no-watch` after the last of them
/// means no process selects the mode, and argv would override it, so that value stays
/// as written. One before them stays too, where the flags on argv override it as they
/// did in the variable.
///
/// Argv gets one `--watch`, or none beside a `--watch-path`, which implies it: on 20.13
/// through 22.13 the filter keeps whichever flag follows one it removes, so a second
/// would make the child a supervisor too.
pub fn lift_watch_flags(node_options: &str) -> Option<(String, Vec<String>)> {
    let tokens = split_node_options(node_options);
    let mut rest = Vec::new();
    let mut paths = Vec::new();
    let mut selected = false;
    let mut at = 0;
    while at < tokens.len() {
        let width = option_width(&tokens, at);
        let option = &tokens[at..at + width];
        if starts_watch_mode(&tokens[at]) {
            selected = true;
            if option_name(&tokens[at]) != "--watch" {
                paths.extend(
                    option
                        .iter()
                        .map(|token| filtered_spelling(token).unwrap_or_else(|| token.clone())),
                );
            }
        } else {
            selected &= !matches!(option_name(&tokens[at]), "--no-watch" | "--no_watch");
            rest.extend(option.iter().map(|token| node_options_token(token)));
        }
        at += width;
    }
    let flags = if paths.is_empty() {
        vec!["--watch".to_string()]
    } else {
        paths
    };
    selected.then(|| (rest.join(" "), flags))
}

/// Whether `option` selects watch mode, under any spelling Node accepts.
/// `--watch-path` implies `--watch`.
pub fn starts_watch_mode(option: &str) -> bool {
    matches!(
        option_name(option),
        "--watch" | "--watch-path" | "--watch_path"
    )
}

fn option_name(option: &str) -> &str {
    option.split_once('=').map_or(option, |(name, _)| name)
}

/// `arg` as Node's filters recognize it, when it selects a mode under another
/// spelling. `--watch-path` has only the one other spelling; `--test` and `--watch`
/// have no `_` to read.
fn filtered_spelling(arg: &str) -> Option<String> {
    if let Some(mode) = CHILD_MODES.iter().find(|mode| {
        arg.strip_prefix(**mode)
            .is_some_and(|rest| rest.starts_with('='))
    }) {
        return Some((*mode).to_string());
    }
    let rest = arg.strip_prefix("--watch_path")?;
    (rest.is_empty() || rest.starts_with('=')).then(|| format!("--watch-path{rest}"))
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
            (
                &["--watch_path=src", "a.js"][..],
                &["--watch-path=src", "a.js"][..],
            ),
            (
                &["--watch_path", "src", "a.js"][..],
                &["--watch-path", "src", "a.js"][..],
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
    fn watch_flags_leave_node_options_for_argv() {
        let cases = [
            ("--watch", "", &["--watch"][..]),
            ("--watch=1 --no-warnings", "--no-warnings", &["--watch"][..]),
            ("--watch_path=src", "", &["--watch-path=src"][..]),
            // A separate value goes with its flag, and what stays keeps its quoting.
            (
                r#"--watch-path "my src" "--title=a b""#,
                r#""--title=a b""#,
                &["--watch-path", "my src"][..],
            ),
            // Only a space separates in this variable, so a tab stays inside its token.
            (
                "--title=foo\tbar --watch",
                "\"--title=foo\tbar\"",
                &["--watch"][..],
            ),
            // A negation before the flags loses to them, as it did in the variable.
            ("--no-watch --watch", "--no-watch", &["--watch"][..]),
            // Argv gets one flag where one says it all.
            ("--watch --no-watch --watch", "--no-watch", &["--watch"][..]),
            ("--watch --watch-path=src", "", &["--watch-path=src"][..]),
        ];
        for (input, rest, flags) in cases {
            assert_eq!(
                lift_watch_flags(input),
                Some((rest.to_string(), argv(flags))),
                "{input}"
            );
        }
        // Options that merely start the same way select no mode, and stay as written.
        assert_eq!(
            lift_watch_flags("--watch-preserve-output --no-warnings"),
            None
        );
    }

    #[test]
    fn a_negation_after_the_watch_flags_leaves_node_options_as_written() {
        for input in [
            "--watch --no-watch",
            "--watch --no_watch",
            "--watch --no-watch=1",
            "--watch-path=src --no-watch",
        ] {
            assert_eq!(lift_watch_flags(input), None, "{input}");
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
