//! The closed preset table. A `"sandbox": "<preset>"` string opts into a
//! nub-implemented named policy set. The resolver is a CLOSED table — an unknown
//! preset is a hard error naming the supported set (same discipline as the env
//! type grammar), so adding a preset later is non-breaking.
//!
//! A preset expands to the equivalent granular surface `Value`, which the pipeline
//! then folds — one code path, no separate preset→IR translator to keep in sync.

use super::{CompileCtx, CompileError, defaults};
use crate::policy::SandboxPolicy;
use serde_json::{Value, json};

/// Resolve a preset name to its granular surface object. `"build-jail"` is the
/// only preset today (the lifecycle-script baseline).
pub fn resolve(name: &str) -> Result<Value, CompileError> {
    match name {
        "build-jail" => Ok(build_jail()),
        other => Err(CompileError::unknown_preset(other, &["build-jail"])),
    }
}

/// Re-assert a preset's built-in secret floor AFTER its surface object has folded,
/// closing the last-match-wins hole a broad subtree grant opens.
///
/// build-jail's fs (`{ "/": "r", "./": "rw" }`) folds to `[whole-fs read, <project
/// rw>, <.env floor>]`; the `"./"` grant re-allows EVERY path under the project —
/// `<proj>/.env` included — because it is the last matching entry, silently
/// re-exposing the project's own secrets to the untrusted lifecycle script this jail
/// exists to confine. Re-appending the built-in secret denies makes them the last
/// match again, so the floor holds. Uses the [`defaults::secret_read_denies`] set
/// directly (identical matchers + Deny/Read access), so the floor is byte-consistent
/// across the policy rather than a project-anchored re-derivation — surface `!`-entries
/// would anchor the depth-independent `**/.env` globs to the project and carry
/// ReadWrite access, a divergence.
pub fn reassert_secret_floor(name: &str, policy: &mut SandboxPolicy, ctx: &CompileCtx) {
    if name == "build-jail" {
        policy
            .fs
            .rules
            .entries
            .extend(defaults::secret_read_denies(&ctx.homes));
    }
}

/// The build-jail baseline: read everything but the secret set, write only the
/// project subtree, deny all egress, strip the ambient env. Expressed in surface
/// syntax so it folds through the same pipeline as a hand-written policy.
///
/// This is a Stage-1 approximation of the lifecycle-script posture — the
/// production build-jail (own-package-dir confinement, prefetch, interactive net
/// grants, curated env baseline) is the later build-jail thread's job; it refines
/// this via `install.sandbox` + `dependenciesMeta` grants. Kept deliberately
/// simple + project-relative so it is meaningful in the frontend-less engine.
fn build_jail() -> Value {
    json!({
        // Whole-filesystem READ (`"/": "r"` → an `is_whole_root` read → the backends'
        // read-only root view) plus the project subtree rw — a build script must reach
        // its interpreter, system libs/headers, and out-of-project caches. `"/"` is the
        // whole-fs read form; a bare `"**"` would anchor to the project (`<proj>/**`) and
        // silently narrow to project-only read. The `"./"` grant re-opens the secret
        // floor for the project subtree under last-match-wins; `reassert_secret_floor`
        // re-appends the home-secret denies post-fold to close it (they cannot be
        // expressed faithfully in surface form). `"/": "r"` is byte-equivalent to the
        // removed naked-`...` splice's whole-fs read; the `.env*` floor still applies.
        "fs": { "/": "r", "./": "rw" },
        // deny all egress (the build-jail thread adds prefetch + grants).
        "net": false,
        // strip the ambient env (the embedder injects the curated baseline). `vars:
        // []` is the strip-all posture (no entries → OS essentials only), matching
        // the old `env: false`.
        "vars": []
    })
}
