use super::{
    ListLocation, is_protected_key, read_merged, read_project_entries, read_user_entries,
    resolve_aliases, setting_default_value, setting_for_key,
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

    /// Read only the project configuration.
    #[arg(long, conflicts_with = "global")]
    pub local: bool,

    /// Read only the user configuration.
    #[arg(long, conflicts_with = "local")]
    pub global: bool,
}

impl GetArgs {
    fn effective_location(&self) -> ListLocation {
        if self.global {
            ListLocation::User
        } else if self.local {
            ListLocation::Project
        } else {
            ListLocation::Merged
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
        ListLocation::User => read_user_entries(&cwd)?,
        ListLocation::Project => read_project_entries(&cwd)?,
    };

    let managed_entries = super::aube_config::load_managed_entries();
    if matches!(args.effective_location(), ListLocation::Merged)
        && !managed_entries.is_empty()
        && let Some(meta) = setting_for_key(&args.key)
    {
        let local = entries
            .iter()
            .rev()
            .find_map(|(k, v)| aliases.iter().any(|a| a == k).then(|| v.clone()));
        if let Some(v) = aube_settings::values::apply_managed_raw(
            meta.name,
            local.or_else(|| setting_default_value(meta)),
            &managed_entries,
        ) {
            if args.json {
                println!("{}", serde_json::Value::String(v));
            } else {
                println!("{v}");
            }
            return Ok(());
        }
    }

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
