//! Which command lines the pnpm engine takes.
//!
//! nub keeps a handful of verbs and gives the engine every other one it
//! serves, the long tail included, so the routing table is the list of
//! what nub keeps plus a question to the engine about everything else.
//! The question is what makes a leading rc-option work: a pnpm command
//! line may be written `nub --store-dir <dir> install`, and only the
//! engine's own grammar knows which token there is the verb.

use std::ffi::OsString;

/// Verbs nub serves itself, under the engine's canonical name for each.
///
/// `run`, `exec`, `dlx`, `create` and `init` are nub's own commands. `env`
/// is a pnpm surface nub does not offer at all, and neither is `self-update`,
/// which installs a pnpm: nub updates itself with `upgrade`. The script shortcuts are
/// nub's standing refusal of an implicit `nub <script>`: the engine would
/// run the script, nub answers with `nub run <script>` instead, and
/// `install-test` is the same shortcut with an install in front of it.
///
/// `config`, and its hidden `get` / `set` shorthands, are nub's own
/// configuration surface in a nub project: project-scoped by default,
/// user scope spelled `--global`, and a `config init` that writes a
/// commented `nub.jsonc` — a file the engine knows nothing about. All
/// three names exist in the engine's grammar too, so without these
/// entries the front door would hand the surface over: `nub config init`
/// answers "unrecognized subcommand 'init'", and `nub set
/// auto-install-peers false` refuses and advises editing a
/// `pnpm-workspace.yaml`, a file nub must not write under its own
/// identity. A pnpm project is the reverse case, so there they are pnpm's
/// ([`engine_takes`]). They are the ONLY verbs where the two
/// grammars collide; every other nub verb was checked against the
/// engine's `--help` and is untouched.
///
/// nub's remaining verbs — `watch`, `compile`, `upgrade`, `node`, `pm`,
/// `agent`, `global`, `help` — are not here because nub's own table is
/// consulted first and settles them. That order is load-bearing for one
/// of them: the engine reads `upgrade` as a spelling of its `update`, and
/// nub's self-update must not become a dependency update.
const HOST_VERBS: &[&str] = &[
    "run",
    "exec",
    "dlx",
    "create",
    "init",
    "config",
    "get",
    "set",
    "env",
    "self-update",
    "test",
    "start",
    "stop",
    "restart",
    "install-test",
];

/// The command line to hand the engine, when the engine is the one to
/// take it. `None` leaves `argv` to nub's own dispatch.
///
/// The config verbs go to the engine in a pnpm project, whose settings live
/// in pnpm's files: there `nub config` is pnpm 12's `config`. What pnpm has
/// no counterpart for stays nub's in every project — `config init`, `config
/// path`, and a key naming a `nub.jsonc` field.
pub(crate) fn engine_takes(argv: Vec<OsString>) -> Option<Vec<OsString>> {
    let name = pnpm_cli::command_name(&argv)?;
    if CONFIG_VERBS.contains(&name.as_str()) {
        return (!names_nubs_own_config(&argv) && super::pnpm_engine::runs_as_pnpm(&argv))
            .then_some(argv);
    }
    (!HOST_VERBS.contains(&name.as_str())).then_some(argv)
}

/// The verbs nub keeps in a nub project only.
const CONFIG_VERBS: [&str; 3] = ["config", "get", "set"];

/// Whether a config command line names something only nub has.
///
/// Read off the words rather than a parse: the two grammars disagree on
/// the flags (`--local` against `--location`), so either parser rejects a
/// line written for the other. A `key=value` word is read as its key.
fn names_nubs_own_config(argv: &[OsString]) -> bool {
    argv.iter()
        .skip(1)
        .filter_map(|arg| arg.to_str())
        .filter(|word| !word.starts_with('-'))
        .any(|word| {
            let key = word.split_once('=').map_or(word, |(key, _)| key);
            matches!(key, "init" | "path") || super::store_config_family::is_nub_config_key(key)
        })
}

#[cfg(test)]
mod tests;
