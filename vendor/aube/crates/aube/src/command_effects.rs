//! What each aube command does to the world.
//!
//! aube's usage spec is derived from clap, and clap has no way to express
//! this, so the classification lives here and is applied where the spec is
//! generated.
//!
//! The three values are defined by the usage spec:
//!
//! - `read` — only inspects state; running it twice is the same as running it
//!   once, and not running it changes nothing.
//! - `write` — creates or modifies state, but removes nothing the user cannot
//!   recreate.
//! - `destructive` — removes something the user installed or configured, where
//!   getting it back means redoing work. Deserves a confirmation prompt.
//!
//! **Fetching or extracting a package is not an effect.** Almost every command
//! here may populate the store or the metadata cache on the way to doing its
//! job. That is aube doing its job: it is idempotent, aube refills it on
//! demand, and it changes nothing the user authored. Counting it would make
//! every command `write` and leave the field with no signal. The same goes for
//! `node_modules`, which is reproducible from the lockfile — `clean` and `ci`
//! delete it and are still only `write`.
//!
//! **Registry commands act on everyone, not just this machine.** `publish`,
//! `deprecate`, `dist-tag` and `access` change what every consumer of a
//! package sees. They are classified by the same rules as everything else —
//! `unpublish` removes, so it is destructive; `publish` creates, so it is
//! write — but none of them are ever `read`, and the comments say why the
//! blast radius is larger than the tier alone suggests.
//!
//! **An unlisted command means "unknown", not "safe".** Consumers treat the
//! absence of a value as "ask", so leaving one out is the conservative choice
//! and mislabeling one `read` is the dangerous one. Commands that run
//! user-supplied code have no fixed effect and are listed in [`UNCLASSIFIED`].

use std::collections::HashMap;

use clap_usage::usage;
use clap_usage::usage::SpecCommandEffect::{self, Destructive, Read, Write};

/// Commands whose effect is fixed, keyed by their full path under the binary.
pub const EFFECTS: &[(&str, SpecCommandEffect)] = &[
    ("__node-gyp-bootstrap", Write),
    ("access", Read),
    ("access get", Read),
    ("access get status", Read),
    // Registry-side permission change, visible to the whole team.
    ("access grant", Write),
    ("access list", Read),
    ("access list collaborators", Read),
    ("access list packages", Read),
    ("access ls", Read),
    // Removes a team's access; restoring it means re-granting.
    ("access revoke", Destructive),
    ("access set", Write),
    // Unlike most `activate` commands this does not only print shell code:
    // `ensure_shims` creates the shim dir and writes the tool shims into it.
    ("activate", Write),
    ("add", Write),
    ("approve-builds", Write),
    ("audit", Read),
    ("bin", Read),
    ("bugs", Read),
    ("cache", Read),
    // The metadata cache is refilled on demand.
    ("cache delete", Write),
    ("cache list", Read),
    ("cache list-registries", Read),
    ("cache prune", Write),
    ("cache view", Read),
    ("cat-file", Read),
    ("cat-index", Read),
    ("check", Read),
    // Deletes node_modules first, but it is reproducible from the lockfile.
    ("ci", Write),
    ("clean", Write),
    ("completion", Read),
    ("config", Read),
    // Removes a key the user wrote into .npmrc.
    ("config delete", Destructive),
    ("config explain", Read),
    ("config find", Read),
    ("config get", Read),
    ("config list", Read),
    ("config set", Write),
    ("config tui", Write),
    ("dedupe", Write),
    ("deploy", Write),
    // Registry-side and visible to every consumer, but `undeprecate` undoes it.
    ("deprecate", Write),
    ("deprecations", Read),
    ("diag", Read),
    ("diag analyze", Read),
    ("diag compare", Read),
    ("dist-tag", Read),
    // Repoints a tag every consumer resolves through, `latest` included.
    ("dist-tag add", Write),
    ("dist-tag ls", Read),
    // Removes a tag consumers may be resolving through.
    ("dist-tag rm", Destructive),
    ("doctor", Read),
    ("fetch", Write),
    ("find-hash", Read),
    ("get", Read),
    ("ignored-builds", Read),
    ("import", Write),
    ("init", Write),
    ("install", Write),
    ("la", Read),
    ("licenses", Read),
    ("link", Write),
    ("list", Read),
    ("ll", Read),
    ("login", Write),
    // Removes the stored auth token from .npmrc. Getting it back means
    // authenticating again, which is the same "redoing work" that makes
    // `config delete` destructive — this is `config delete` for the auth key.
    ("logout", Destructive),
    ("outdated", Read),
    ("pack", Write),
    ("patch", Write),
    ("patch-commit", Write),
    // Removes patch entries the user configured.
    ("patch-remove", Destructive),
    ("peers", Read),
    ("peers check", Read),
    ("prefix", Read),
    // Removes extraneous packages from node_modules; reinstall restores them.
    ("prune", Write),
    ("purge", Write),
    ("query", Read),
    ("rebuild", Write),
    // Removes a dependency the user declared in package.json.
    ("remove", Destructive),
    ("root", Read),
    ("runtime", Read),
    ("runtime list", Read),
    ("runtime set", Write),
    ("sbom", Read),
    ("sponsors", Read),
    ("store", Read),
    ("store add", Write),
    ("store path", Read),
    // The store is content-addressed and refetched on demand.
    ("store prune", Write),
    ("store status", Read),
    ("trust", Read),
    ("trust check", Read),
    ("undeprecate", Write),
    // Removes linked entries from node_modules; `link` restores them.
    ("unlink", Write),
    // Removes a package, or one of its versions, from the registry. Every
    // consumer loses it, and most registries do not allow republishing the
    // same version.
    ("unpublish", Destructive),
    ("update", Write),
    ("version", Write),
    ("view", Read),
    ("why", Read),
    // Not implemented: these print an error telling you to use npm, so they
    // change nothing.
    ("owner", Read),
    ("pkg", Read),
    ("search", Read),
    ("set-script", Read),
    ("stage", Read),
    ("token", Read),
    ("whoami", Read),
    // Hidden aliases for config commands.
    ("set", Write),
];

/// Commands that only exist behind a cargo feature, so they must only be
/// classified where they exist — otherwise the stale-entry test fails under
/// `--no-default-features`. The feature is on by default.
///
/// `config tui` is deliberately *not* here: a `cfg(not(feature = "config-tui"))`
/// stub keeps the subcommand present either way, it just errors when invoked.
pub const FEATURE_EFFECTS: &[(&str, SpecCommandEffect)] = &[
    // Publishes to the registry. Creates rather than removes, so `write` by
    // the rules above — but a published version cannot be replaced, and every
    // consumer can see it immediately. Never auto-run this.
    #[cfg(feature = "publish")]
    ("publish", Write),
];

/// Commands with no fixed effect, and why.
///
/// These run code that is not aube's: a package's lifecycle scripts, a binary
/// fetched from the registry, or a script from package.json. Their effect is
/// whatever that code does, and `read` in particular would be dangerous.
#[cfg(test)]
pub const UNCLASSIFIED: &[(&str, &str)] = &[
    (
        "create",
        "runs a create-* starter kit fetched from the registry",
    ),
    ("dlx", "fetches a package and runs its binary"),
    ("exec", "runs a locally installed binary"),
    (
        "install-test",
        "installs, then runs the package's test script",
    ),
    ("node", "runs Node.js with whatever arguments are given"),
    ("recursive", "runs another command across every workspace"),
    ("restart", "runs the package's restart script"),
    ("run", "runs a script defined in package.json"),
    ("start", "runs the package's start script"),
    ("stop", "runs the package's stop script"),
    ("test", "runs the package's test script"),
];

/// Annotate every command in the spec that has a declared effect.
pub fn apply(spec: &mut usage::Spec) {
    let effects: HashMap<&str, SpecCommandEffect> =
        EFFECTS.iter().chain(FEATURE_EFFECTS).copied().collect();
    annotate(&mut spec.cmd, &mut vec![], &effects);
}

fn annotate(
    cmd: &mut usage::SpecCommand,
    path: &mut Vec<String>,
    effects: &HashMap<&str, SpecCommandEffect>,
) {
    for (name, sub) in cmd.subcommands.iter_mut() {
        path.push(name.clone());
        if let Some(effect) = effects.get(path.join(" ").as_str()) {
            sub.effect = Some(*effect);
        }
        annotate(sub, path, effects);
        path.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Every command in the tree, hidden ones included: a hidden command is
    /// still runnable.
    fn all_commands() -> Vec<String> {
        let spec: usage::Spec = crate::command().into();
        let mut out = vec![];
        collect(&spec.cmd, &mut vec![], &mut out);
        out
    }

    fn collect(cmd: &usage::SpecCommand, path: &mut Vec<String>, out: &mut Vec<String>) {
        for (name, sub) in &cmd.subcommands {
            path.push(name.clone());
            out.push(path.join(" "));
            collect(sub, path, out);
            path.pop();
        }
    }

    fn classified() -> HashSet<&'static str> {
        EFFECTS
            .iter()
            .chain(FEATURE_EFFECTS)
            .map(|(name, _)| *name)
            .chain(UNCLASSIFIED.iter().map(|(name, _)| *name))
            .collect()
    }

    /// Adding a command without deciding what it does to the world is the
    /// failure mode this table exists to prevent, so make it a test failure
    /// rather than a silently missing annotation.
    #[test]
    fn every_command_is_classified() {
        let known = classified();
        let missing: Vec<String> = all_commands()
            .into_iter()
            .filter(|cmd| !known.contains(cmd.as_str()))
            .collect();
        assert!(
            missing.is_empty(),
            "these commands have no entry in EFFECTS or UNCLASSIFIED \
             (crates/aube/src/command_effects.rs) — decide whether each is \
             read, write, destructive, or genuinely unclassifiable:\n  {}",
            missing.join("\n  ")
        );
    }

    /// Catches entries left behind by a renamed or removed command.
    #[test]
    fn no_classification_refers_to_a_missing_command() {
        let present: HashSet<String> = all_commands().into_iter().collect();
        let stale: Vec<&str> = classified()
            .into_iter()
            .filter(|name| !present.contains(*name))
            .collect();
        assert!(
            stale.is_empty(),
            "these entries no longer match a command:\n  {}",
            stale.join("\n  ")
        );
    }

    #[test]
    fn classifications_are_not_duplicated() {
        let mut seen = HashSet::new();
        for name in EFFECTS
            .iter()
            .chain(FEATURE_EFFECTS)
            .map(|(n, _)| *n)
            .chain(UNCLASSIFIED.iter().map(|(n, _)| *n))
        {
            assert!(seen.insert(name), "{name} is classified twice");
        }
    }

    /// The tables are only worth having if they reach the spec. Everything
    /// else here checks the tables against the CLI; this checks that `apply`
    /// actually transfers them.
    #[test]
    fn apply_annotates_the_spec() {
        let mut spec: usage::Spec = crate::command().into();
        apply(&mut spec);

        let cmd = |name: &str| {
            spec.cmd
                .subcommands
                .get(name)
                .unwrap_or_else(|| panic!("no `{name}`"))
        };
        assert_eq!(cmd("unpublish").effect, Some(Destructive));
        assert_eq!(cmd("remove").effect, Some(Destructive));
        assert_eq!(cmd("list").effect, Some(Read));
        assert_eq!(cmd("install").effect, Some(Write));
        // Nested commands are reached too.
        assert_eq!(cmd("dist-tag").subcommands["rm"].effect, Some(Destructive));
        // Anything in UNCLASSIFIED must be left unset, not defaulted.
        assert_eq!(cmd("run").effect, None);
        assert_eq!(cmd("dlx").effect, None);
    }
}
