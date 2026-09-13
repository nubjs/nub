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
/// is a pnpm surface nub does not offer at all. The script shortcuts are
/// nub's standing refusal of an implicit `nub <script>`: the engine would
/// run the script, nub answers with `nub run <script>` instead, and
/// `install-test` is the same shortcut with an install in front of it.
///
/// `config`, and its hidden `get` / `set` shorthands, are nub's own
/// configuration surface rather than the engine's: project-scoped by
/// default, user scope spelled `--global`, and a `config init` that
/// writes a commented `nub.jsonc` — a file the engine knows nothing
/// about. All three names exist in the engine's grammar too, so without
/// these entries the front door hands the surface over: `nub config
/// init` answers "unrecognized subcommand 'init'", and `nub set
/// auto-install-peers false` refuses outright and advises the user to
/// edit a `pnpm-workspace.yaml`, which is a file nub must not write
/// under its own identity. They are the ONLY verbs where the two
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
    "test",
    "start",
    "stop",
    "restart",
    "install-test",
];

/// The command line to hand the engine, when the engine is the one to
/// take it. `None` leaves `argv` to nub's own dispatch.
pub(crate) fn engine_takes(argv: Vec<OsString>) -> Option<Vec<OsString>> {
    let name = pnpm_cli::command_name(&argv)?;
    (!HOST_VERBS.contains(&name.as_str())).then_some(argv)
}

#[cfg(test)]
mod tests;
