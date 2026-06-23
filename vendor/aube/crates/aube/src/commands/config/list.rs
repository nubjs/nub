use super::{
    ListLocation, literal_aliases, read_merged, read_project_entries, read_user_entries,
    resolve_aliases, settings_meta,
};
use clap::Args;
use miette::miette;

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Also list settings that have no value set.
    ///
    /// Renders one row per setting in `settings.toml`, with the
    /// default and description shown for unset entries.
    ///
    /// Only valid with `--location merged` (the default), since a
    /// per-file view can't distinguish "not set anywhere" from "set in
    /// the other file" and would render misleading defaults.
    #[arg(long)]
    pub all: bool,

    /// Emit all entries as a JSON object keyed by setting name.
    ///
    /// Matches `pnpm config list --json`. Honors `--all` and
    /// `--location` the same way the default text output does.
    #[arg(long)]
    pub json: bool,

    /// Shortcut for `--location project`.
    ///
    /// Conflicts with `--all` since `--all` only makes sense against
    /// the merged view — see the `--all` docs for why.
    #[arg(long, conflicts_with_all = ["location", "all"])]
    pub local: bool,

    /// Which config location(s) to list.
    ///
    /// `merged` (default) walks the same file-source precedence install
    /// uses, with last-write-wins merging.
    #[arg(long, value_enum)]
    pub location: Option<ListLocation>,
}

impl ListArgs {
    fn effective_location(&self) -> ListLocation {
        if self.local {
            ListLocation::Project
        } else {
            self.location.unwrap_or(ListLocation::Merged)
        }
    }

    pub(super) fn has_parent_overrides(&self) -> bool {
        self.all || self.json || self.local || self.location.is_some()
    }

    pub(super) fn apply_parent(&mut self, parent: Self) {
        self.all |= parent.all;
        self.json |= parent.json;
        if self.location.is_none() && !self.local {
            self.local = parent.local;
        }
        if self.location.is_none() {
            self.location = parent.location;
        }
    }
}

pub fn run(args: ListArgs) -> miette::Result<()> {
    let location = args.effective_location();
    if args.all && !matches!(location, ListLocation::Merged) {
        return Err(miette!(
            "--all is only supported with --location merged (the default)"
        ));
    }
    let cwd = crate::dirs::project_root_or_cwd()?;
    let entries: Vec<(String, String)> = match location {
        ListLocation::Merged => read_merged(&cwd)?,
        ListLocation::User | ListLocation::Global => read_user_entries(&cwd)?,
        ListLocation::Project => read_project_entries(&cwd)?,
    };

    let mut seen = collect_seen(entries);

    let mut defaults: std::collections::HashSet<String> = std::collections::HashSet::new();
    if args.all {
        for meta in settings_meta::all() {
            let literals = literal_aliases(meta.npmrc_keys);
            let Some(primary) = literals.first().cloned() else {
                continue;
            };
            if !literals.iter().any(|k| seen.contains_key(k)) {
                seen.insert(primary.clone(), meta.default.to_string());
                defaults.insert(primary);
            }
        }
    }

    if args.json {
        let obj: serde_json::Map<String, serde_json::Value> = seen
            .into_iter()
            // npm's `config list --json` omits protected keys entirely;
            // mirror that so tokens never reach a JSON consumer.
            .filter(|(k, _)| !super::is_protected_key(k))
            .map(|(k, v)| {
                let value = if args.all {
                    serde_json::json!({
                        "value": v,
                        "default": defaults.contains(&k),
                    })
                } else {
                    serde_json::Value::String(v)
                };
                (k, value)
            })
            .collect();
        let out = serde_json::to_string_pretty(&serde_json::Value::Object(obj))
            .map_err(|e| miette!("failed to serialize config: {e}"))?;
        println!("{out}");
    } else {
        for (k, v) in &seen {
            if super::is_protected_key(k) {
                // Render auth-bearing keys as `(protected)` rather than
                // echoing the secret, matching `npm config list`.
                println!("{k}=(protected)");
            } else if defaults.contains(k) {
                println!("{k}={v} (default)");
            } else {
                println!("{k}={v}");
            }
        }
    }
    Ok(())
}

pub(super) fn canonical_list_key(key: &str) -> String {
    let aliases = resolve_aliases(key);
    if aliases.len() == 1 && aliases[0] == key {
        return key.to_string();
    }
    aliases.first().cloned().unwrap_or_else(|| key.to_string())
}

pub(super) fn collect_seen(
    entries: Vec<(String, String)>,
) -> std::collections::BTreeMap<String, String> {
    let mut seen = std::collections::BTreeMap::new();
    for (k, v) in entries {
        let canonical = canonical_list_key(&k);
        seen.insert(canonical, v);
    }
    seen
}
