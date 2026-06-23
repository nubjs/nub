use std::ffi::OsString;

/// Strip pnpm-style generic `--config.<key>[=<value>]` flags out of the
/// argv before clap sees them. Returns the parsed `(key, value)` pairs
/// in the order they appeared so the last one wins on duplicates. The
/// supported forms are:
///
///   --config.<key>            -> ("<key>", "true")
///   --config.<key>=<value>    -> ("<key>", "<value>")
///
/// `--config.<key> <value>` (space-separated) is NOT consumed: a stray
/// positional after a bool-form switch could shadow a real argument
/// (e.g. `aube add --config.foo lodash`), and the `=` form is what
/// pnpm's docs use anyway. Anything after a bare `--` separator is
/// left untouched so user-supplied positional args containing the
/// literal `--config.` prefix still pass through.
pub(crate) fn extract_config_overrides(args: &mut Vec<OsString>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut i = 1;
    while i < args.len() {
        let Some(s) = args[i].to_str() else {
            i += 1;
            continue;
        };
        if s == "--" {
            break;
        }
        if let Some(rest) = s.strip_prefix("--config.") {
            let (key, value) = match rest.split_once('=') {
                Some((k, v)) => (k.to_string(), v.to_string()),
                None => (rest.to_string(), "true".to_string()),
            };
            if !key.is_empty() {
                out.push((key, value));
                args.remove(i);
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Inspect `argv[0]` and, when invoked as a multicall shim (`aubr`, `aubx`),
/// rewrite the argv so clap sees the equivalent `aube run …` / `aube dlx …`.
pub(crate) fn rewrite_multicall_argv(mut args: Vec<OsString>) -> Vec<OsString> {
    normalize_npm_interpreter_shim_argv(&mut args);
    let Some(argv0) = args.first() else {
        return args;
    };
    let stem = std::path::Path::new(argv0)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("aube")
        .to_ascii_lowercase();
    let subcommand = match stem.as_str() {
        "aubr" => "run",
        "aubx" => "dlx",
        _ => return args,
    };
    args[0] = OsString::from("aube");
    // `--version` / `-V` belong to the top-level `aube` command; `run` and
    // `dlx` don't accept them, and for `dlx` the bare word would be parsed
    // as a package name and trigger a registry lookup.
    if matches!(
        args.get(1).and_then(|s| s.to_str()),
        Some("--version") | Some("-V")
    ) {
        return args;
    }
    args.insert(1, OsString::from(subcommand));
    args
}

/// npm's Windows `.cmd` shim can only execute extensioned native binaries.
/// When npm invokes `aube.exe bin/aube ...`, drop the shebang file and use
/// it as argv[0] so multicall dispatch still sees `aubr` / `aubx`.
fn normalize_npm_interpreter_shim_argv(args: &mut Vec<OsString>) {
    let Some(shim) = args.get(1).cloned() else {
        return;
    };
    let shim_path = std::path::Path::new(&shim);
    let Some(stem) = shim_path.file_stem().and_then(|s| s.to_str()) else {
        return;
    };
    if !matches!(stem, "aube" | "aubr" | "aubx") {
        return;
    }
    let Ok(bytes) = std::fs::read(shim_path) else {
        return;
    };
    if !bytes.starts_with(b"#!") {
        return;
    }
    args[0] = shim;
    args.remove(1);
}

/// pnpm-compat: shift flag tokens that used to be `global = true` on
/// `Cli` past the subcommand so `aube --frozen-lockfile install`,
/// `aube --registry=URL install`, etc. keep parsing after those flags
/// moved into per-command Args groups.
pub(crate) fn lift_per_subcommand_flags(mut args: Vec<OsString>) -> Vec<OsString> {
    // (long_name_without_dashes, takes_value)
    const LIFTED_LONGS: &[(&str, bool)] = &[
        ("frozen-lockfile", false),
        ("no-frozen-lockfile", false),
        ("prefer-frozen-lockfile", false),
        ("registry", true),
        ("fetch-retries", true),
        ("fetch-retry-factor", true),
        ("fetch-retry-maxtimeout", true),
        ("fetch-retry-mintimeout", true),
        ("fetch-timeout", true),
        ("disable-global-virtual-store", false),
        ("disable-gvs", false),
        ("enable-global-virtual-store", false),
        ("enable-gvs", false),
    ];
    // Long-form Cli flags that still live on `Cli` *and* take a value.
    // We must skip past `flag value` pairs so the value isn't mistaken
    // for the subcommand. Bool flags need no entry here.
    const KEPT_LONGS_WITH_VALUE: &[&str] = &[
        "dir",
        "cd",
        "prefix",
        "loglevel",
        "reporter",
        "filter",
        "filter-prod",
    ];
    const KEPT_SHORTS_WITH_VALUE: &[&str] = &["-C", "-F"];

    // True when the token at `args[idx]` looks like another flag rather
    // than a free-form value. Used to avoid eating the next flag as the
    // current flag's value when the user wrote `--dir --frozen-lockfile
    // install` (omitting the `--dir` value); without this guard we'd
    // silently consume `--frozen-lockfile` as a directory name and
    // `--frozen-lockfile` would never get lifted past the subcommand.
    let token_looks_like_flag = |args: &[OsString], idx: usize| -> bool {
        args.get(idx)
            .and_then(|t| t.to_str())
            .is_some_and(|s| s.starts_with('-') && s != "-")
    };

    let mut lifted: Vec<OsString> = Vec::new();
    let mut subcommand_idx: Option<usize> = None;
    let mut i = 1;
    while i < args.len() {
        let Some(s) = args[i].to_str() else { break };
        if s == "--" {
            break;
        }
        if let Some(rest) = s.strip_prefix("--") {
            let (bare, has_inline_value) = match rest.split_once('=') {
                Some((bare, _)) => (bare, true),
                None => (rest, false),
            };
            if let Some((_, takes_value)) =
                LIFTED_LONGS.iter().copied().find(|(name, _)| *name == bare)
            {
                lifted.push(args.remove(i));
                if takes_value
                    && !has_inline_value
                    && i < args.len()
                    && !token_looks_like_flag(&args, i)
                {
                    lifted.push(args.remove(i));
                }
                continue;
            }
            if KEPT_LONGS_WITH_VALUE.contains(&bare) {
                i += 1;
                if !has_inline_value && i < args.len() && !token_looks_like_flag(&args, i) {
                    i += 1;
                }
                continue;
            }
            i += 1;
            continue;
        }
        if s == "-" {
            subcommand_idx = Some(i);
            break;
        }
        if s.strip_prefix('-').is_some() {
            if s == "-F" {
                i += 1;
                if i < args.len() && !token_looks_like_flag(&args, i) {
                    i += 1;
                }
                continue;
            }
            if let Some(rest) = s.strip_prefix("-F")
                && !rest.is_empty()
            {
                i += 1;
                continue;
            }
            if KEPT_SHORTS_WITH_VALUE.contains(&s) {
                i += 1;
                if i < args.len() && !token_looks_like_flag(&args, i) {
                    i += 1;
                }
                continue;
            }
            i += 1;
            continue;
        }
        subcommand_idx = Some(i);
        break;
    }
    if let Some(idx) = subcommand_idx {
        let insert_at = idx + 1;
        for (j, tok) in lifted.into_iter().enumerate() {
            args.insert(insert_at + j, tok);
        }
    } else {
        // No subcommand found — restore the lifted tokens at their
        // original front position so clap's error message still
        // mentions them in argv order.
        for tok in lifted.into_iter().rev() {
            args.insert(1, tok);
        }
    }
    args
}
