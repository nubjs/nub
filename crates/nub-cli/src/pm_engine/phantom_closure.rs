//! Which packages an install ejects from the shared store before any scan, and
//! the fingerprint token that keeps a warm tree honest about the answer.
//!
//! A package is ejected — materialized as real project-local bytes rather than a
//! symlink into the shared store — when running it from the store would break it.
//! Two sources name one ahead of time, and both live here: nub's own curated
//! lists ([`NUB_INTERNAL_DISK_MATERIALIZE_SEED`] and [`NUB_PROJECT_CONTEXT_EJECT`])
//! and the project's `install.linker.eject`, published through
//! [`set_native_config_seed`]. [`configured_eject_names`] is the union the engine
//! asks for; the per-version scan that finds the REST is
//! [`crate::dynamic_phantom`], wired to the engine in [`super::phantom_hooks`].
//!
//! [`project_context_eject_token`] is the load-bearing half (nub#457). The
//! curated seed is injected past the engine's own settings fold, so without a
//! token of its own an existing install — one from a nub predating the list, or
//! one taken after any future edit to it — keeps an identical `settings_hash`,
//! the existence-gated fast path accepts the stale symlinked tree, and the fix
//! never reaches anybody who already installed. Folding this token into the
//! install-state fingerprint forces the relink that converts the stale symlink
//! into an ejected directory.

use std::sync::{LazyLock, PoisonError, RwLock};

/// nub's own embedder-default names that may seed the eject set. Native
/// `install.linker.eject` entries are admitted separately; incumbent
/// `.npmrc`/env/workspace values remain ignored. Standalone aube installs no
/// hook and honors its full `diskMaterializePackages` knob unchanged.
///
/// SHARED with [`super::nub_setting_defaults`], which seeds exactly this name as
/// the embedder default — sourcing both from one const so a future internal
/// default can't be added in one place and silently dropped by the other.
pub(super) const NUB_INTERNAL_DISK_MATERIALIZE_SEED: &[&str] = &["vite"];

/// `install.linker.eject` from the project's `nub.jsonc`, published by
/// [`super::engine_session_inner`]. A process-global because the eject hook is a
/// bare `fn` the engine installs once and calls with only the resolved graph —
/// there is no seam to thread session state through. Poisoning is ignored
/// throughout: the guarded value is a plain name list, so a panic mid-write
/// cannot leave it inconsistent.
static NATIVE_CONFIG_SEED: LazyLock<RwLock<Vec<String>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

/// The package NAMES this project ejects before any scan: what it asked for
/// in its own configuration, plus the ones nub always ejects.
pub(super) fn configured_eject_names() -> Vec<String> {
    let configured = NATIVE_CONFIG_SEED
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    NUB_INTERNAL_DISK_MATERIALIZE_SEED
        .iter()
        .chain(NUB_PROJECT_CONTEXT_EJECT)
        .map(|name| (*name).to_owned())
        .chain(configured)
        .collect()
}

pub(super) fn set_native_config_seed(seed: Vec<String>) {
    *NATIVE_CONFIG_SEED
        .write()
        .unwrap_or_else(PoisonError::into_inner) = seed;
}

/// Curated "project-context" packages (nub#457): each one's build script READS or
/// MUTATES the CONSUMING project rather than only its own dir — git-hook installers
/// walk up from `cwd` to the project root and write `.git/hooks`; the rest generate
/// project-local files or resolve peers/bridges against the host tree. Under GVS a
/// build runs in the shared global store, DETACHED from any project, so a
/// cwd/upward-walk lands on the per-package store wrapper (which has no
/// package.json) and the script crashes — #457: `simple-git-hooks` postinstall →
/// ENOENT — or silently emits shared-mutable wrong output. Ejecting them
/// (disk-materialize project-local, via the SAME importer-closure + expand hook the
/// phantom seeds use) restores a real project above `cwd`.
///
/// DELIBERATELY curated, NOT "all script-havers": self-contained builds (node-gyp
/// native compiles, prebuilt-binary downloaders like esbuild) read only their own
/// dir, produce identical output anywhere, and share it cross-project via the
/// side-effects cache — ejecting them would erode the symlink-in-store sharing win
/// for zero correctness gain, so they STAY in GVS (built once, shared). Every name
/// here is verified category-C (build reads/mutates the host project); absent names
/// cost nothing (the seed union is presence-gated against the resolved graph).
///
/// Gate: this rides the same [`enabled`] seam as phantom eject, so disabling the
/// internal eject seam also disables curated eject (reintroducing #457) — an
/// accepted property of that internal-only escape hatch.
pub(super) const NUB_PROJECT_CONTEXT_EJECT: &[&str] = &[
    // Git-hook installers — walk up from cwd to the project root, write `.git/hooks`.
    "simple-git-hooks",
    "lefthook",
    "@evilmartians/lefthook",
    "@arkweid/lefthook",
    "pre-commit",
    "pre-push",
    "ghooks",
    "git-validate",
    "shared-git-hooks",
    "yorkie",
    "git-commit-msg-linter",
    // Other host-project reader/mutators (generate project-local files, resolve
    // peers/bridges against the consuming tree).
    "install-peers",
    "msw",
    "@bahmutov/add-typescript-to-cypress",
    "@cypress/snapshot",
    "cordova.plugins.diagnostic",
    "vue-demi",
    "vue-inbrowser-compiler-demi",
    "@intlify/vue-i18n-bridge",
    "@intlify/vue-router-bridge",
    "storage-engine",
];

/// Stable, order-independent fingerprint token for the curated eject list, folded
/// into the install-state settings hash via [`crate::dynamic_phantom::settings_token`]
/// (the `extra_settings_fingerprint` embedder hook). Load-bearing for warm/upgrade
/// trees (nub#457): the curated seed is injected INSIDE the expand hook, after
/// aube's `disk_materialize_packages` settings fold, so without this token an
/// existing install — one from a nub predating the list, or after any FUTURE list
/// edit — keeps an identical `settings_hash`, and aube's existence-gated fast path
/// accepts the stale symlinked tree and skips the relink, leaving #457 unfixed.
/// Folding this token forces the relink that converts the stale symlink to an
/// ejected dir. Hashing the SORTED names makes the token move on any add/remove and
/// hold steady on a pure reorder; FNV-1a keeps it dependency-free and stable across
/// platforms/releases (std's `DefaultHasher` is not guaranteed stable across Rust
/// versions).
pub(crate) fn project_context_eject_token() -> String {
    eject_list_token(NUB_PROJECT_CONTEXT_EJECT)
}

fn eject_list_token(names: &[&str]) -> String {
    let mut sorted: Vec<&str> = names.to_vec();
    sorted.sort_unstable();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for name in sorted {
        for byte in name.bytes().chain(std::iter::once(0x1f)) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn eject_list_token_is_order_independent_and_edit_sensitive() {
        // The token feeds the install-state fingerprint (#457 warm-tree fix): a pure
        // reorder is a no-op (it's a set), while any add/remove MUST move it so an
        // existing install relinks. And it is deterministic — a stable fingerprint,
        // not a per-run value.
        assert_eq!(
            eject_list_token(&["a", "b", "c"]),
            eject_list_token(&["c", "a", "b"]),
            "a pure reorder does not change the token"
        );
        assert_ne!(
            eject_list_token(&["a", "b"]),
            eject_list_token(&["a", "b", "c"]),
            "an added name changes the token"
        );
        assert_eq!(
            project_context_eject_token(),
            project_context_eject_token(),
            "deterministic across calls"
        );
    }
}
