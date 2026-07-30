//! The run-time half of `--external` and `--allow-dynamic-import`.
//!
//! Both flags leave a specifier for the artifact to resolve while it runs, and a
//! compiled artifact runs its bundle from nub's cache directory — so Node's own
//! walk starts somewhere the user has never been and finds nothing. Neither flag
//! would work on ANY machine, including one that has the file. So the artifact
//! carries one resolve hook that re-bases those specifiers onto the directory the
//! executable was started in, which is the only base with a meaning the user can
//! act on ("put it where you run it").
//!
//! The two flags differ only in WHICH specifiers the hook claims, and in the
//! order it tries the two bases:
//!
//! - `--external` re-bases its packages unconditionally. The package was removed
//!   from the graph, so the app dir provably cannot answer for it.
//! - `--allow-dynamic-import` picks the order from the specifier's SHAPE, because
//!   the two shapes fail in opposite directions. A PATH-LIKE specifier tries the
//!   artifact first: its own chunk-to-chunk `import("./chunk-abc.js")` calls are
//!   indistinguishable from a plugin load at run time, and the wrapper reaches the
//!   bundle through `import("./app.js")`, so re-basing first would let a stray
//!   file in the launch directory take over the process. A BARE specifier
//!   re-bases first: the app dir has no `node_modules`, but Node's walk does not
//!   stop there — it climbs OUT of the cache directory into whatever
//!   `node_modules` sits above it, which would silently answer ahead of the
//!   directory the user actually controls.
//!
//! The hook cannot be installed by the bundle itself: Node links a module's
//! entire static import graph before running any of its code, so a hook the
//! bundle registered would arrive after its own imports had already been
//! resolved. Hence the generated wrapper — it registers the hook, then reaches
//! the bundle through `import()`, which resolves at call time.
//!
//! This lives in `compile`, not `bundle`, on purpose: `nub build` emits a file
//! into a real project where Node's ordinary resolution already does the right
//! thing, so it wants the bundler flags WITHOUT this shim.

use anyhow::{Result, bail};
use nub_core::node::version::NodeVersion;

/// What the artifact must resolve for itself at run time. Empty on both axes
/// means no shim at all — the bundle stays the process entry.
pub struct ShimPlan<'a> {
    /// `--external` packages, removed from the graph.
    pub external: &'a [String],
    /// `--allow-dynamic-import`, AND at least one computed `import()` actually
    /// survived into the bundle. A build that sets the flag and has nothing to
    /// use it on ships no wrapper.
    pub dynamic: bool,
}

impl ShimPlan<'_> {
    pub fn needed(&self) -> bool {
        !self.external.is_empty() || self.dynamic
    }

    /// The flag to name in a diagnostic. `--external` first: both share the 22.15
    /// floor, but `--external`'s applies unconditionally, so it is the one a user
    /// can act on without first knowing whether a computed import survived.
    fn flag(&self) -> &'static str {
        if self.external.is_empty() {
            "--allow-dynamic-import"
        } else {
            "--external"
        }
    }
}

/// The generated wrapper, and the artifact's new entry.
const WRAPPER: &str = "__nub_entry.mjs";
/// The generated hook module.
const HOOK: &str = "__nub_external.mjs";

/// `module.registerHooks` — a synchronous resolve hook, no loader worker and no
/// experimental warning — landed in Node 22.15, the same floor nub's own fast
/// tier uses. Below it the shim has nothing to register with.
fn min_node() -> NodeVersion {
    NodeVersion::new(22, 15, 0)
}

/// Reject a shim-needing build against a Node the shim cannot run on, BEFORE the
/// ~100 MB runtime download. `version` is the exact embedded version, or (under
/// `--smol`) the floor the launcher refuses to start below — so in both shapes
/// it is a lower bound on what will actually run the artifact. `source` names
/// where that version came from, because a bare major pin floors at `X.0.0` and
/// the refusal is otherwise baffling on a machine running a newer X.
pub fn check_node_support(version: &NodeVersion, source: &str, plan: &ShimPlan<'_>) -> Result<()> {
    if !plan.needed() || *version >= min_node() {
        return Ok(());
    }
    let flag = plan.flag();
    let way_out = if plan.external.is_empty() {
        "or drop --allow-dynamic-import and make the specifier a static string"
    } else {
        "or drop --external and let the package be bundled"
    };
    bail!(
        "{flag} needs Node {} or newer, and this build targets Node {version} \
         (from {source}).\n\
         \x20\x20It is served by a hook the artifact installs at startup\n\
         \x20\x20(module.registerHooks), which older Node does not have.\n\
         \x20\x20Pass --target {} (or newer), {way_out}.",
        min_node(),
        min_node()
    )
}

/// Defines that keep the wrapper invisible to the code it runs.
///
/// The bundle stops being the process entry point once the wrapper is in front
/// of it, so Node reports `import.meta.main === false` inside it — a silent
/// wrong answer that makes an `if (import.meta.main) main();` program do
/// nothing, with no error to attribute. The bundle IS the program (the wrapper
/// imports nothing else), and an unwrapped bundle already sees `true`, so
/// pinning it restores the untouched behavior rather than inventing one.
///
/// It is a blunt instrument: a NON-entry chunk reads `true` too, where Node
/// would say `false`. That is pre-existing — `--external` has always pinned it
/// bundle-wide — and it is the right trade, since a wrong `false` on the entry
/// silently does nothing while a wrong `true` on a split chunk is visible.
///
/// Keyed on the FLAG, not on [`ShimPlan::needed`], because a define is a bundler
/// input and the plan is only knowable after the bundle. So a build that passes
/// `--allow-dynamic-import` and turns out not to need it still gets the define
/// with no wrapper to justify it — including below Node 22.15, which the shim's
/// own floor would otherwise have refused.
pub fn entry_defines(any_flag: bool) -> Vec<(String, String)> {
    if !any_flag {
        return Vec::new();
    }
    vec![("import.meta.main".into(), "true".into())]
}

/// The generated entry name, plus the two files to add to the payload.
pub struct Shim {
    pub entry: String,
    pub files: Vec<(String, Vec<u8>)>,
}

/// Build the wrapper + hook for `plan`, entering the real bundle at `entry`.
/// `app_files` is the payload the shim will be appended to — the launcher writes
/// those files by name, so a collision would silently overwrite a bundle chunk.
pub fn shim(app_files: &[(String, Vec<u8>)], entry: &str, plan: &ShimPlan<'_>) -> Result<Shim> {
    if let Some((name, _)) = app_files.iter().find(|(n, _)| n == WRAPPER || n == HOOK) {
        bail!(
            "the bundle already emits a file named {name}, which {} needs for its shim",
            plan.flag()
        );
    }
    let target = serde_json::to_string(&format!("./{entry}"))?;

    let external_decls = if plan.external.is_empty() {
        String::new()
    } else {
        format!(
            r#"
const PACKAGES = {};

// Node's CJS resolver ignores a substituted parentURL and re-bases on the
// requiring module's real path, so a createRequire() call inside the bundle
// would resolve against the cache dir and fail. Verified 2026-07-29 on Node
// 24.18: the ESM path honors the substitution, the CJS path does not.
const requireFromBase = createRequire(new URL("x", BASE));

// Mirrors is_package_specifier() in compile/bundle.rs: the package, or a subpath
// of it. The two rules must agree, or a specifier is dropped from the bundle and
// then not redirected.
const owner = (specifier) =>
  PACKAGES.find((p) => specifier === p || specifier.startsWith(p + "/"));
"#,
            serde_json::to_string(plan.external)?
        )
    };

    let external_branch = if plan.external.is_empty() {
        String::new()
    } else {
        r#"
    const pkg = owner(specifier);
    // An external was REMOVED from the graph, so the app dir provably cannot
    // answer for it — re-base first rather than paying a throw to learn that.
    if (pkg !== undefined) {
      try {
        return nextResolve(specifier, { ...context, parentURL: BASE });
      } catch (cause) {
        try {
          return {
            url: pathToFileURL(requireFromBase.resolve(specifier)).href,
            shortCircuit: true,
          };
        } catch {}
        // "Install it" is only the right advice for a package that is absent. An
        // installed package with an unexported subpath or a malformed specifier
        // fails here too, and mislabelling that would send the user hunting for a
        // dependency they already have.
        if (!missing(cause)) throw cause;
        const err = new Error(
          `Cannot find ${specifier}.\n\n` +
            `  This executable was compiled with --external ${pkg}, so ${pkg} is not\n` +
            `  part of it — it is resolved when the executable runs, from the directory\n` +
            `  it is started in.\n\n` +
            `  Started in: ${process.cwd()}\n` +
            `  Install ${pkg} there, or run this from a directory that already has it.`,
          { cause },
        );
        err.code = "ERR_NUB_EXTERNAL_NOT_FOUND";
        throw err;
      }
    }
"#
        .to_string()
    };

    // What an unclaimed specifier does. The dynamic branch IS this fallthrough
    // when it is present — it returns or throws for every specifier — so the two
    // are one choice, not two; emitting both would leave unreachable code in the
    // artifact.
    //
    // Deliberately no createRequire fallback here, unlike the external branch. A
    // computed `require(expr)` CAN reach this hook (registerHooks intercepts CJS
    // too, and bundle.rs only refuses a require whose specifier is static), but
    // resolving it through `requireFromBase` would give `import()` CJS extension
    // and directory resolution that plain Node does not — trading a missing
    // module for a silently different one.
    let tail = if plan.dynamic {
        r#"
    // Which base to try first is decided by the specifier's SHAPE, and both
    // orders are load-bearing — see the module docs. Path-like and `#imports`
    // specifiers belong to whoever imported them, so the artifact answers first;
    // a bare specifier can only come from a node_modules tree, and the artifact
    // has none, so the launch directory answers first.
    //
    // The two bases are captured as STRINGS, not as context objects: nextResolve
    // writes the parentURL it used back onto the context it was handed, so a
    // second entry holding the original object would silently re-try the first
    // attempt's base. Measured 2026-07-30 on Node 26.5 — both attempts reported
    // the launch directory, and the artifact fallback never ran.
    const own = context.parentURL;
    const order = /^(#|\.{0,2}\/|[a-zA-Z][a-zA-Z\d+\-.]*:)/.test(specifier)
      ? [own, BASE]
      : [BASE, own];
    let absent;
    for (const parentURL of order) {
      try {
        return nextResolve(specifier, { ...context, parentURL });
      } catch (cause) {
        if (!missing(cause)) throw cause;
        absent ??= cause;
      }
    }
    const err = new Error(
      `Cannot find ${specifier}.\n\n` +
        `  This executable was compiled with --allow-dynamic-import, so a specifier\n` +
        `  it computes at run time is resolved against the executable's own contents\n` +
        `  and the directory the executable was started in.\n\n` +
        `  Started in: ${process.cwd()}\n` +
        `  Put it there, or run this from a directory that already has it.`,
      { cause: absent },
    );
    // Node's own code, NOT a nub-specific one: the guarded optional import —
    // `catch (e) { if (e.code !== "ERR_MODULE_NOT_FOUND") throw e; return null; }`
    // — is the dominant shape in exactly the plugin loaders this flag exists for,
    // and a code it does not recognize turns "plugin absent" into a crash that
    // only happens inside the compiled binary.
    err.code = absent.code;
    throw err;
"#
    } else {
        "\n    return nextResolve(specifier, context);\n"
    };
    let module_imports = if plan.external.is_empty() {
        "registerHooks"
    } else {
        "registerHooks, createRequire"
    };

    let hook = format!(
        r#"// Generated by `nub compile`. Resolves what the artifact was told to leave for
// run time from the directory this executable was started in — the bundle itself
// runs from a cache directory, where nothing would ever be found.
import {{ {module_imports} }} from "node:module";
import {{ pathToFileURL }} from "node:url";
import {{ join }} from "node:path";

// Only the BUNDLE's imports are re-based. Once something is resolved, what IT
// imports in turn resolves normally from its own location — re-basing those too
// would let a package's internal subpath import land in a different copy.
const APP = new URL("./", import.meta.url).href;
// A directory URL, so Node's own "Cannot find package X imported from …" names
// the directory it searched rather than a placeholder file that does not exist.
const BASE = new URL("./", pathToFileURL(join(process.cwd(), "x"))).href;

const missing = (e) =>
  e?.code === "ERR_MODULE_NOT_FOUND" || e?.code === "MODULE_NOT_FOUND";
{external_decls}
registerHooks({{
  resolve(specifier, context, nextResolve) {{
    if (!context.parentURL?.startsWith(APP)) return nextResolve(specifier, context);
{external_branch}{tail}  }},
}});
"#
    );

    let wrapper = format!(
        r#"// Generated by `nub compile`. Node resolves a module's whole static import
// graph before running any of its code, so the hook has to be installed from
// OUTSIDE the bundle and the bundle reached through import().
import {{ fileURLToPath }} from "node:url";
import "./{HOOK}";

// The wrapper is an implementation detail; argv[1] stays the real entry so the
// `import.meta.url === pathToFileURL(process.argv[1]).href` main-module idiom
// keeps working.
process.argv[1] = fileURLToPath(new URL({target}, import.meta.url));
await import({target});
"#
    );

    Ok(Shim {
        entry: WRAPPER.to_string(),
        files: vec![
            (HOOK.to_string(), hook.into_bytes()),
            (WRAPPER.to_string(), wrapper.into_bytes()),
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(names: &[&str]) -> Vec<(String, Vec<u8>)> {
        names
            .iter()
            .map(|n| ((*n).to_string(), Vec::new()))
            .collect()
    }

    fn plan(external: &[String], dynamic: bool) -> ShimPlan<'_> {
        ShimPlan { external, dynamic }
    }

    fn pkgs(names: &[&str]) -> Vec<String> {
        names.iter().map(|p| (*p).to_string()).collect()
    }

    fn built(packages: &[&str], dynamic: bool) -> Shim {
        let packages = pkgs(packages);
        shim(&app(&["app.js"]), "app.js", &plan(&packages, dynamic)).expect("shim")
    }

    /// Every generated file must parse — both are assembled by `format!` with
    /// brace escaping, so a syntax error would ship as a runtime crash inside a
    /// frozen binary with no build signal at all.
    fn assert_parses(s: &Shim) {
        for (name, bytes) in &s.files {
            let source = std::str::from_utf8(bytes).expect("utf8");
            let allocator = oxc_allocator::Allocator::default();
            let parsed =
                oxc_parser::Parser::new(&allocator, source, oxc_span::SourceType::mjs()).parse();
            assert!(
                !parsed.panicked && parsed.diagnostics.is_empty(),
                "{name} must parse as ESM, got {:?}\n{source}",
                parsed.diagnostics
            );
        }
    }

    fn file<'a>(s: &'a Shim, name: &str) -> &'a str {
        let bytes = &s
            .files
            .iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("the shim must emit {name}, got {:?}", s.files))
            .1;
        std::str::from_utf8(bytes).expect("utf8")
    }

    #[test]
    fn the_node_gate_fires_for_either_flag_and_names_the_one_in_play() {
        let old = NodeVersion::new(20, 19, 0);
        let ok = NodeVersion::new(22, 15, 0);
        let none = pkgs(&[]);
        let prettier = pkgs(&["prettier"]);
        assert!(check_node_support(&old, "--target", &plan(&none, false)).is_ok());
        assert!(check_node_support(&ok, "--target", &plan(&prettier, false)).is_ok());

        let err = check_node_support(&old, ".node-version", &plan(&prettier, false))
            .expect_err("must be rejected");
        let msg = format!("{err:#}");
        assert!(msg.contains("22.15.0"), "must name the floor: {msg}");
        assert!(
            msg.contains("20.19.0"),
            "must name what was targeted: {msg}"
        );
        assert!(
            msg.contains(".node-version"),
            "must name where that version came from, or a major-pin floor looks \
             arbitrary: {msg}"
        );
        assert!(msg.contains("--target"), "must name the way out: {msg}");
        assert!(msg.contains("--external"), "must name the flag: {msg}");

        // Same floor, different flag — a dynamic-import build has no package to
        // un-externalize, so the way out has to be its own.
        let dyn_err =
            check_node_support(&old, "--target", &plan(&none, true)).expect_err("must be rejected");
        let dyn_msg = format!("{dyn_err:#}");
        assert!(
            dyn_msg.contains("--allow-dynamic-import") && !dyn_msg.contains("--external"),
            "must name only the flag in play: {dyn_msg}"
        );
    }

    // The wrapper is what puts the hook in front of the bundle's own imports, so
    // it must reach the bundle through import() and not a static import.
    #[test]
    fn the_wrapper_registers_the_hook_before_it_imports_the_bundle() {
        let s = built(&["prettier", "@scope/pkg"], false);
        assert_eq!(s.entry, "__nub_entry.mjs");
        let wrapper = file(&s, "__nub_entry.mjs");
        assert!(
            wrapper.contains(r#"import "./__nub_external.mjs""#),
            "the hook must be a static import, so it evaluates first: {wrapper}"
        );
        assert!(
            wrapper.contains(r#"await import("./app.js")"#),
            "the bundle must be reached dynamically, after the hook: {wrapper}"
        );
        assert!(
            file(&s, "__nub_external.mjs").contains(r#"["prettier","@scope/pkg"]"#),
            "the package list must be baked in as JSON"
        );
    }

    #[test]
    fn every_flag_combination_generates_valid_esm() {
        for (external, dynamic) in [
            (&["prettier", "@scope/pkg"][..], false),
            (&[][..], true),
            (&["prettier"][..], true),
        ] {
            assert_parses(&built(external, dynamic));
        }
    }

    // The two branches must not leak into each other's build: an external-only
    // artifact that also re-based unclaimed specifiers would silently prefer the
    // launch directory over its own contents, and a dynamic-only artifact has no
    // package list to consult.
    #[test]
    fn each_branch_is_emitted_only_for_the_flag_that_asked_for_it() {
        let ext = built(&["prettier"], false);
        let external_only = file(&ext, "__nub_external.mjs");
        assert!(
            external_only.contains("ERR_NUB_EXTERNAL_NOT_FOUND")
                && !external_only.contains("ERR_NUB_DYNAMIC_IMPORT_NOT_FOUND"),
            "external-only hook must carry only the external branch: {external_only}"
        );
        let dynamic = built(&[], true);
        let dynamic_only = file(&dynamic, "__nub_external.mjs");
        assert!(
            dynamic_only.contains("--allow-dynamic-import")
                && !dynamic_only.contains("PACKAGES")
                && !dynamic_only.contains("createRequire"),
            "dynamic-only hook must carry neither the package list nor the CJS \
             fallback that exists for it: {dynamic_only}"
        );
    }

    // Which base is tried first is shape-dependent, and each direction guards a
    // different silent-wrong-answer: the wrapper reaches the bundle through
    // `import("./app.js")`, so a path-like specifier must consult the artifact
    // first; a bare specifier must not let a node_modules sitting above the
    // runtime cache answer ahead of the directory the user launched from.
    #[test]
    fn the_dynamic_branch_picks_its_base_from_the_specifier_shape() {
        let shim = built(&[], true);
        let hook = file(&shim, "__nub_external.mjs");
        assert!(
            hook.contains("? [own, BASE]") && hook.contains(": [BASE, own]"),
            "path-like must try the artifact first, bare the launch directory: {hook}"
        );
        // Both bases are captured as STRINGS and each attempt builds a fresh
        // context. Handing `nextResolve` the caller's own context object instead
        // silently re-tries the first attempt's base — it writes the parentURL it
        // used back onto what it is given (measured on Node 26.5).
        assert!(
            hook.contains("const own = context.parentURL;")
                && hook.contains("nextResolve(specifier, { ...context, parentURL })"),
            "each attempt must build a fresh context from a captured URL: {hook}"
        );
        // Node's own code, not a nub one: the guarded optional-import idiom keys
        // on ERR_MODULE_NOT_FOUND, and a code it does not know turns "plugin
        // absent" into a crash that only happens inside a compiled binary.
        assert!(
            hook.contains("err.code = absent.code;"),
            "the thrown error must keep Node's own code: {hook}"
        );
    }

    #[test]
    fn a_bundle_file_named_like_the_shim_is_refused_rather_than_overwritten() {
        let p = pkgs(&["prettier"]);
        let plan = plan(&p, false);
        assert!(shim(&app(&["__nub_entry.mjs"]), "__nub_entry.mjs", &plan).is_err());
        // Not the entry, but still a collision the launcher would resolve by
        // overwriting one of the two.
        assert!(shim(&app(&["app.js", "__nub_external.mjs"]), "app.js", &plan).is_err());
    }
}
