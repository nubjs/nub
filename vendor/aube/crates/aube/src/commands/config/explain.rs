use super::setting_for_key;
use clap::Args;
use miette::miette;

#[derive(Debug, Args)]
pub struct ExplainArgs {
    /// Setting key, `.npmrc` alias, env var, workspace YAML key, or CLI flag.
    pub key: String,
}

pub fn run(args: ExplainArgs) -> miette::Result<()> {
    let Some(meta) = setting_for_key(&args.key) else {
        return Err(miette!(
            "unknown setting `{}`; try `{} <words>`",
            args.key,
            aube_util::cmd("config find")
        ));
    };

    println!("{}", meta.name);
    println!("  Type: {}", meta.type_);
    println!("  Default: {}", meta.default);
    println!("  Description: {}", meta.description);
    print_source_line("CLI flags", meta.cli_flags);
    print_source_line("Environment", meta.env_vars);
    print_source_line(".npmrc keys", meta.npmrc_keys);
    print_source_line("Workspace YAML keys", meta.workspace_yaml_keys);

    let docs = meta.docs.trim();
    if !docs.is_empty() {
        println!();
        println!("{docs}");
    }

    if !meta.examples.is_empty() {
        println!();
        println!("Examples:");
        for example in meta.examples {
            println!("  {example}");
        }
    }

    Ok(())
}

fn print_source_line(label: &str, values: &[&str]) {
    if !values.is_empty() {
        println!("  {label}: {}", values.join(", "));
    }
}
