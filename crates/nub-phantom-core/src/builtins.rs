//! Node builtin-module recognition.
//!
//! A `node:`-prefixed specifier is UNCONDITIONALLY a builtin (the prefix is
//! reserved). A bare specifier is a builtin only if it EXACTLY names one of
//! Node's built-in modules — the lists below are the union across supported Node
//! lines (18.19 floor → current), taken from `require('node:module')
//! .builtinModules`, so we never mistake a real dependency for a builtin nor
//! vice-versa. The match is on the whole specifier, as Node's is: `buffer/`
//! (the polyfill idiom), `process/` and `fs/anything` resolve from
//! `node_modules`, and the `node:`-only modules (`node:test`, `node:sqlite`,
//! `node:sea`) are not builtins without the prefix.

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

/// The bare builtin SUBPATHS. Any other `<builtin>/<x>` is an npm request.
/// Sorted for binary search.
const BUILTIN_SUBPATHS: &[&str] = &[
    "assert/strict",
    "dns/promises",
    "fs/promises",
    "inspector/promises",
    "path/posix",
    "path/win32",
    "readline/promises",
    "stream/consumers",
    "stream/promises",
    "stream/web",
    "timers/promises",
    "util/types",
];

/// Is `spec` (the full import specifier, not a package name) a Node builtin?
///
/// Handles the `node:` prefix (always builtin, incl. `node:test`, `node:sea`,
/// and any subpath like `node:stream/promises`), a bare builtin name, and the
/// exact bare builtin subpaths in [`BUILTIN_SUBPATHS`].
pub fn is_builtin(spec: &str) -> bool {
    spec.starts_with("node:")
        || BARE_BUILTINS.binary_search(&spec).is_ok()
        || BUILTIN_SUBPATHS.binary_search(&spec).is_ok()
}

/// Bare names a HOST process injects at runtime, which no install can supply.
/// Electron short-circuits `require('electron')` (and the `electron/main`,
/// `electron/renderer`, `electron/common` subpaths) inside its own module
/// resolver before `node_modules` is consulted, and the npm package of that name
/// is only the launcher stub; VS Code's extension host injects `vscode` the same
/// way. A reference to one is therefore not a phantom any layout could repair:
/// inside the host it always resolves, outside the host the package is useless
/// either way. Ejecting `electron-log` from the global virtual store for it
/// bought nothing.
const HOST_PROVIDED: &[&str] = &["electron", "vscode"];

/// Is `spec` a module the host process provides at runtime (see
/// [`HOST_PROVIDED`])? Matches the head segment, so `electron/main` counts.
pub fn is_host_provided(spec: &str) -> bool {
    let head = spec.split('/').next().unwrap_or(spec);
    HOST_PROVIDED.contains(&head)
}

#[cfg(test)]
mod tests {
    use super::{is_builtin, is_host_provided};

    #[test]
    fn host_provided_names_match_by_head_segment() {
        assert!(is_host_provided("electron"));
        assert!(is_host_provided("electron/main"));
        assert!(is_host_provided("vscode"));
        assert!(!is_host_provided("electron-log"));
        assert!(!is_host_provided("vscode-languageserver"));
        // Host-provided is a separate class, not a builtin.
        assert!(!is_builtin("electron"));
    }

    #[test]
    fn builtin_list_is_sorted_for_binary_search() {
        assert!(super::BARE_BUILTINS.is_sorted());
        assert!(super::BUILTIN_SUBPATHS.is_sorted());
    }

    #[test]
    fn only_the_exact_specifier_node_resolves_internally_is_a_builtin() {
        // Each case checked against Node's own `require('node:module').isBuiltin`.
        // A trailing slash is the documented way to reach the npm package of a
        // builtin's name (`require('buffer/')`), and an unknown subpath is an npm
        // request too: both resolve from node_modules, so either can be a phantom.
        for spec in [
            "buffer/",
            "process/",
            "punycode/",
            "fs/",
            "fs/whatever",
            "util/x",
        ] {
            assert!(!is_builtin(spec), "{spec} resolves from node_modules");
        }
        // `node:`-only modules are builtins only WITH the prefix.
        for spec in ["test", "sqlite", "sea", "test/reporters"] {
            assert!(!is_builtin(spec), "bare {spec} is an npm package");
        }
        for spec in [
            "node:test",
            "node:sqlite",
            "node:sea",
            "node:test/reporters",
        ] {
            assert!(is_builtin(spec), "{spec} is a builtin");
        }
        for spec in [
            "path/posix",
            "path/win32",
            "util/types",
            "stream/web",
            "timers/promises",
        ] {
            assert!(is_builtin(spec), "{spec} is a builtin subpath");
        }
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
