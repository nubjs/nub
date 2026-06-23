use super::{
    ListLocation, is_protected_key, read_merged, read_project_entries, read_user_entries,
    resolve_aliases,
};
use clap::Args;
use miette::miette;

#[derive(Debug, Args)]
pub struct GetArgs {
    /// The setting key.
    ///
    /// Accepts either a pnpm canonical name (e.g. `autoInstallPeers`)
    /// or an `.npmrc` alias (e.g. `auto-install-peers`).
    pub key: String,

    /// Emit the value as JSON.
    ///
    /// Matches `pnpm config get --json`: a missing key renders as
    /// `undefined`, a found value is JSON-encoded.
    #[arg(long)]
    pub json: bool,

    /// Shortcut for `--location project`.
    #[arg(long, conflicts_with = "location")]
    pub local: bool,

    /// Which config location(s) to read.
    ///
    /// Defaults to `merged` — the last-write-wins view of the same
    /// file-source precedence install uses. Use `user` or `project`
    /// to restrict the lookup.
    #[arg(long, value_enum, default_value_t = ListLocation::Merged)]
    pub location: ListLocation,
}

impl GetArgs {
    fn effective_location(&self) -> ListLocation {
        if self.local {
            ListLocation::Project
        } else {
            self.location
        }
    }
}

pub fn run(args: GetArgs) -> miette::Result<()> {
    // Refuse to echo auth-bearing keys, matching `npm config get`'s
    // protected-key guard. Without this, `config get
    // //registry.npmjs.org/:_authToken` would print the registry token.
    if is_protected_key(&args.key) {
        return Err(miette!(
            "The {} option is protected, and cannot be retrieved in this way",
            args.key
        ));
    }

    let aliases = resolve_aliases(&args.key);
    let cwd = crate::dirs::project_root_or_cwd()?;
    let entries: Vec<(String, String)> = match args.effective_location() {
        ListLocation::Merged => read_merged(&cwd)?,
        ListLocation::User | ListLocation::Global => read_user_entries(&cwd)?,
        ListLocation::Project => read_project_entries(&cwd)?,
    };

    if let Some(v) = find_value(&entries, &aliases) {
        if args.json {
            println!("{}", serde_json::Value::String(v));
        } else {
            println!("{v}");
        }
        return Ok(());
    }
    println!("undefined");
    Ok(())
}

pub(super) fn find_value(entries: &[(String, String)], aliases: &[String]) -> Option<String> {
    for (k, v) in entries.iter().rev() {
        if aliases.iter().any(|a| a == k) {
            return Some(v.clone());
        }
    }
    None
}
