use aube_workspace::selector::EffectiveFilter;
use clap::Args;
use miette::miette;

#[derive(Debug, Args)]
pub struct RecursiveArgs {
    /// Command and arguments to run recursively across workspace packages
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

pub struct RecursiveGlobals {
    pub filters: EffectiveFilter,
    pub color: bool,
    pub no_color: bool,
}

/// Build an argv for `aube recursive <cmd>` / `aube multi <cmd>` /
/// `aube m <cmd>` without spawning a second aube process. The caller
/// parses and dispatches this in-process.
pub fn argv(args: RecursiveArgs, globals: RecursiveGlobals) -> miette::Result<Vec<String>> {
    if args.args.is_empty() {
        return Err(miette!("{}: missing command", aube_util::cmd("recursive")));
    }

    let mut argv = vec!["aube".to_string()];
    if globals.color {
        argv.push("--color".to_string());
    }
    if globals.no_color {
        argv.push("--no-color".to_string());
    }
    for filter in globals.filters.filters {
        argv.push("--filter".to_string());
        argv.push(filter);
    }
    for filter in globals.filters.filter_prods {
        argv.push("--filter-prod".to_string());
        argv.push(filter);
    }
    argv.push("-r".to_string());
    argv.extend(args.args);
    Ok(argv)
}
