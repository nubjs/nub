//! Node builtin-module recognition.
//!
//! A `node:`-prefixed specifier is UNCONDITIONALLY a builtin (the prefix is
//! reserved). A bare specifier is a builtin only if it names one of Node's
//! built-in modules — the list below is the union across supported Node lines
//! (18.19 floor → current), so we never mistake a real dependency for a builtin
//! nor vice-versa.

/// Bare names that resolve to a Node builtin. Kept as a sorted `&[&str]` for a
/// binary-search membership test; the set changes only when Node adds a module.
const BARE_BUILTINS: &[&str] = &[
    "assert",
    "async_hooks",
    "buffer",
    "child_process",
    "cluster",
    "console",
    "constants",
    "crypto",
    "dgram",
    "diagnostics_channel",
    "dns",
    "domain",
    "events",
    "fs",
    "http",
    "http2",
    "https",
    "inspector",
    "module",
    "net",
    "os",
    "path",
    "perf_hooks",
    "process",
    "punycode",
    "querystring",
    "readline",
    "repl",
    "stream",
    "string_decoder",
    "sys",
    "timers",
    "tls",
    "trace_events",
    "tty",
    "url",
    "util",
    "v8",
    "vm",
    "wasi",
    "worker_threads",
    "zlib",
];

/// Is `spec` (the full import specifier) a Node builtin?
///
/// Handles the `node:` prefix (always builtin, incl. `node:test`, `node:sea`,
/// and any subpath like `node:stream/promises`), and bare builtins with an
/// optional subpath (`fs/promises`, `stream/web`, `dns/promises`, `util/types`,
/// `assert/strict`, `path/posix`, `timers/promises`, `inspector/promises`).
pub fn is_builtin(spec: &str) -> bool {
    if spec.strip_prefix("node:").is_some() {
        return true;
    }
    // A bare builtin may carry a first-party subpath (`fs/promises`); match the
    // head segment only.
    let head = spec.split('/').next().unwrap_or(spec);
    BARE_BUILTINS.binary_search(&head).is_ok()
}

#[cfg(test)]
mod tests {
    use super::is_builtin;

    #[test]
    fn builtin_list_is_sorted_for_binary_search() {
        assert!(super::BARE_BUILTINS.is_sorted());
    }

    #[test]
    fn recognizes_builtins_bare_prefixed_and_subpathed() {
        assert!(is_builtin("fs"));
        assert!(is_builtin("fs/promises"));
        assert!(is_builtin("node:test"));
        assert!(is_builtin("node:stream/promises"));
        assert!(is_builtin("assert/strict"));
        // A real dependency whose name merely starts like a builtin is NOT a
        // builtin (head-segment match, exact).
        assert!(!is_builtin("fs-extra"));
        assert!(!is_builtin("path-to-regexp"));
        assert!(!is_builtin("react"));
    }
}
