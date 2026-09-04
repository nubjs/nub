use super::{
    ListLocation, literal_aliases, primary_entry_key, read_merged, read_project_entries,
    read_user_entries, setting_default_value, setting_for_key, settings_meta,
};
use miette::miette;

#[derive(Debug, usage_rs::Args)]
pub struct ListArgs {
    /// Also list settings that have no value set.
    ///
    /// Renders one row per setting in `settings.toml`, with the
    /// default and description shown for unset entries.
    ///
    /// Not valid with `--local` or `--global`, since a single-file view cannot
    /// distinguish "not set anywhere" from "set in the other file".
    #[usage(long)]
    pub all: bool,

    /// List only the user configuration.
    #[usage(short = 'g', long, conflicts("--local", "--all"))]
    pub global: bool,

    /// Emit all entries as a JSON object keyed by setting name.
    ///
    /// Matches `pnpm config list --json`. Honors the selected scope.
    #[usage(long)]
    pub json: bool,

    /// List only the project configuration.
    #[usage(long, conflicts("--global", "--all"))]
    pub local: bool,
}

impl ListArgs {
    fn effective_location(&self) -> ListLocation {
        if self.global {
            ListLocation::User
        } else if self.local {
            ListLocation::Project
        } else {
            ListLocation::Merged
        }
    }

    pub(super) fn has_parent_overrides(&self) -> bool {
        self.all || self.json || self.local || self.global
    }

    pub(super) fn apply_parent(&mut self, parent: Self) {
        self.all |= parent.all;
        self.json |= parent.json;
        if !self.local && !self.global {
            self.local = parent.local;
            self.global = parent.global;
        }
    }
}

pub fn run(args: ListArgs) -> miette::Result<()> {
    let location = args.effective_location();
    if args.all && !matches!(location, ListLocation::Merged) {
        return Err(miette!("--all cannot be combined with --local or --global"));
    }
    let cwd = crate::dirs::project_root_or_cwd()?;
    let entries: Vec<(String, String)> = match location {
        ListLocation::Merged => read_merged(&cwd)?,
        ListLocation::User => read_user_entries(&cwd)?,
        ListLocation::Project => read_project_entries(&cwd)?,
    };

    let mut seen = collect_seen(entries);
    if matches!(location, ListLocation::Merged) {
        let managed_entries = super::aube_config::load_managed_entries();
        if !managed_entries.is_empty() {
            for meta in settings_meta::all() {
                if meta.managed_policy.is_empty() {
                    continue;
                }
                let primary = primary_entry_key(meta);
                let local = seen
                    .get(&primary)
                    .cloned()
                    .or_else(|| setting_default_value(meta));
                if let Some(effective) =
                    aube_settings::values::apply_managed_raw(meta.name, local, &managed_entries)
                {
                    seen.insert(primary, effective);
                }
            }
        }
    }

    let mut defaults: std::collections::HashSet<String> = std::collections::HashSet::new();
    if args.all {
        for meta in settings_meta::all() {
            let literals = literal_aliases(meta.npmrc_keys);
            let Some(primary) = literals.first().cloned() else {
                continue;
            };
            if !literals.iter().any(|k| seen.contains_key(k)) {
                seen.insert(primary.clone(), meta.rendered_default().into_owned());
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
    setting_for_key(key).map_or_else(|| key.to_string(), primary_entry_key)
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
