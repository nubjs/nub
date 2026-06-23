//! `aube set-script <name> <command...>` — set an entry in the local
//! `package.json` `scripts` map. Mirrors `npm set-script` /
//! `pnpm set-script` (`@pnpm/pkg-manifest`).
//!
//! Equivalent to `aube pkg set scripts.<name>=<command>`, but with the
//! command taken as the remaining (space-joined) positional args so the
//! shell doesn't need to quote a single `key=value`. The write reuses the
//! same atomic, key-order-preserving manifest update as `pkg`.

use clap::Args;
use miette::miette;
use serde_json::Value;

use super::property_path;

#[derive(Debug, Args)]
pub struct SetScriptArgs {
    /// Script name (the key under `scripts`).
    pub name: String,

    /// The command the script runs. Remaining args are joined with spaces.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
    pub command: Vec<String>,

    /// Operate on the package.json in this directory (default: the
    /// nearest project root, or the cwd).
    #[arg(short = 'C', long, value_name = "DIR")]
    pub dir: Option<std::path::PathBuf>,
}

pub async fn run(args: SetScriptArgs) -> miette::Result<()> {
    if args.command.is_empty() {
        return Err(miette!("`set-script` requires a script name and a command"));
    }
    let dir = match &args.dir {
        Some(d) => d.clone(),
        None => {
            crate::dirs::project_root_or_cwd().unwrap_or_else(|_| std::path::PathBuf::from("."))
        }
    };
    let manifest_path = dir.join("package.json");
    let command = args.command.join(" ");

    super::update_manifest_json_object(&manifest_path, |obj| {
        let mut root = Value::Object(std::mem::take(obj));
        // Use the property-path setter so `scripts` is created if absent
        // and a non-object `scripts` is replaced (same shape as pnpm).
        let segments = vec![
            property_path::Segment::Key("scripts".to_string()),
            property_path::Segment::Key(args.name.clone()),
        ];
        property_path::set(&mut root, &segments, Value::String(command.clone()))?;
        if let Value::Object(map) = root {
            *obj = map;
        }
        Ok(())
    })
}
