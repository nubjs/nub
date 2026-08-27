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
//! resolved. Hence generated wrappers — one for the main module and one per
//! static worker realm — register/import the hook, then reach their real chunk
//! through `import()`, which resolves at call time.
//!
//! This lives in `compile`, not `bundle`, on purpose: `nub build` emits a file
//! into a real project where Node's ordinary resolution already does the right
//! thing, so it wants the bundler flags WITHOUT this shim.

use anyhow::{Result, bail};
use nub_core::compile::{AppFile, COMPILE_BOOTSTRAP_NAME};
use nub_core::node::version::NodeVersion;

use super::bundle::{ExternalImport, WorkerRoot};

/// What the artifact must resolve for itself at run time. Empty on both axes
/// means no shim at all — the bundle stays the process entry.
pub struct ShimPlan<'a> {
    /// `--external` packages, removed from the graph.
    pub external: &'a [String],
    /// Per-importer identities emitted by compile's externalization plugin.
    pub external_imports: &'a [ExternalImport],
    /// `--allow-dynamic-import`, AND at least one computed `import()` actually
    /// survived into the bundle. A build that sets the flag and has nothing to
    /// use it on ships no wrapper.
    pub dynamic: bool,
}

impl ShimPlan<'_> {
    pub fn needed(&self) -> bool {
        !self.external.is_empty() || self.dynamic
    }

    /// The flag to name in a diagnostic. `--external` first: both share the same
    /// requirement, but `--external`'s applies unconditionally, so it is the one a
    /// user can act on without first knowing whether a computed import survived.
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

/// Reject a shim-needing build against a Node the shim cannot run on, BEFORE the
/// ~100 MB runtime download. `version` is the exact embedded version, or (under
/// `--smol`) the floor the launcher refuses to start below — so in both shapes
/// it is a lower bound on what will actually run the artifact. `source` names
/// where that version came from, because a bare major pin floors at `X.0.0` and
/// the refusal is otherwise baffling on a machine running a newer X.
///
/// The generated `__nub_external.mjs` calls `module.registerHooks` unconditionally,
/// so the gate is exactly "does that API exist" — [`NodeVersion::supports_augmentation`],
/// the same predicate nub's own fast tier uses. This function used to hold a private
/// `22.15.0` floor and compare against it, which accepted Node 23.0.0–23.4.x: those
/// sort above 22.15.0 but predate `registerHooks` on the 23.x line, which got it at
/// 23.5.0. The build succeeded and the ARTIFACT died at startup on `registerHooks is
/// not a function`. A bare `--target 23` floors at 23.0.0, landing in that band.
///
/// KNOWN GAP, deliberate: a floor is a lower bound, so any pin whose FLOOR sits below
/// the band while its run-time acceptance extends past it still passes here and can be
/// run on 23.2. Two shapes do that under `--smol`: a range (`--target ">=22.15"`), and
/// a major.minor pin (`--target 22.15`) — the latter carries no range into the
/// manifest, so `SmolTarget::matches` falls back to `candidate >= floor`. Closing
/// either needs the gate to see the whole acceptance set rather than its floor, which
/// is a different change; an exact three-part target, a bare major, and a `23.x`
/// range all floor inside the band and are caught.
pub fn check_node_support(version: &NodeVersion, source: &str, plan: &ShimPlan<'_>) -> Result<()> {
    if !plan.needed() || version.supports_augmentation() {
        return Ok(());
    }
    let flag = plan.flag();
    let way_out = if plan.external.is_empty() {
        "or drop --allow-dynamic-import and make the specifier a static string"
    } else {
        "or drop --external and let the package be bundled"
    };
    // Suggest a floor on the line the user already targets. Telling someone on 23.4
    // to "pass --target 22.15 (or newer)" is both a cross-major downgrade and, taken
    // literally, wrong — 23.4 IS newer than 22.15.
    let floor = version.fast_tier_floor_for_line();
    // "or newer" is only safe from the 23.x floor up; from 22.15 it would sweep the
    // 23.0–23.4 band right back in, which is the very fallacy this gate exists to fix.
    let onward = if floor == NodeVersion::new(23, 5, 0) {
        format!("Pass --target {floor} (or newer)")
    } else {
        format!("Pass --target {floor} or newer, other than 23.0 through 23.4")
    };
    bail!(
        "{flag} needs module.registerHooks, and this build targets Node {version} \
         (from {source}).\n\
         \x20\x20The artifact installs that hook at startup to resolve what it was\n\
         \x20\x20told to leave for run time.\n\
         \x20\x20module.registerHooks reached the 22.x line at 22.15.0 and the 23.x\n\
         \x20\x20line at 23.5.0, so Node {version} does not have it.\n\
         \x20\x20{onward}, {way_out}."
    )
}

/// The generated entry name, plus the two files to add to the payload.
pub struct Shim {
    pub entry: String,
    pub files: Vec<AppFile<Vec<u8>>>,
}

/// Generate the public entry for each statically traceable worker. Every worker
/// realm first installs the fixed compile bootstrap, then validates its private
/// record before any generated helper can consume it. A resolver hook, when the
/// build needs one, follows the bootstrap and precedes the worker chunk. Both
/// imports are dynamic so their evaluation order is the source order here.
pub fn worker_wrappers(
    workers: &[WorkerRoot],
    install_hook: bool,
    entry_prefix: &str,
) -> Result<Vec<(String, Vec<u8>)>> {
    workers
        .iter()
        .map(|worker| {
            let chunk = serde_json::to_string(&format!("./{}", worker.chunk))?;
            let bootstrap =
                serde_json::to_string(&root_sibling(entry_prefix, COMPILE_BOOTSTRAP_NAME))?;
            let hook = install_hook
                .then(|| serde_json::to_string(&root_sibling(entry_prefix, HOOK)))
                .transpose()?
                .map(|path| format!("await import({path});\n"));
            let source = format!(
                "// Generated by `nub compile`. Worker-local bootstrap.\n\
                 await import({bootstrap});\n\
                 const record = process[Symbol.for(\"nub.compile.bootstrap\")];\n\
                 if (typeof record?.createRequire !== \"function\" ||\n\
                     typeof record?.getBuiltin !== \"function\" ||\n\
                     typeof record?.requireArg !== \"string\") {{\n\
                   throw new Error(\"nub compile: internal Worker bootstrap failed\");\n\
                 }}\n\
                 {}await import({chunk});\n",
                hook.unwrap_or_default()
            );
            Ok((worker.entry.clone(), source.into_bytes()))
        })
        .collect()
}

/// A worker wrapper shares the entry directory with every emitted chunk, while
/// the generated resolver hook stays at the extracted app root beside the main
/// wrapper. Preserve that geometry when `--include` moved the entry below the
/// app anchor.
fn root_sibling(entry_prefix: &str, name: &str) -> String {
    let depth = entry_prefix
        .split('/')
        .filter(|part| !part.is_empty())
        .count();
    if depth == 0 {
        format!("./{name}")
    } else {
        format!("{}{}", "../".repeat(depth), name)
    }
}

/// Build the wrapper + hook for `plan`, entering the real bundle at `entry`.
/// `app_files` is the payload the shim will be appended to — the launcher writes
/// those files by name, so a collision would silently overwrite a bundle chunk.
pub fn shim(app_files: &[AppFile<Vec<u8>>], entry: &str, plan: &ShimPlan<'_>) -> Result<Shim> {
    // Folded, not exact — the same class `reject_colliding_names` refuses in
    // mod.rs, folded the same way: the launcher writes payload entries by name, so
    // where the target's filesystem does not distinguish case, a shim and a bundle
    // file differing only in case overwrite one another with nothing failing at
    // build time. Folded on EVERY target rather than only the folding ones,
    // because these two names are nub's own — reserving them outright is one rule,
    // where narrowing it to the folding targets would mean threading a
    // TargetPlatform through for a spelling no real build wants.
    for name in app_files.iter().map(|f| &f.name) {
        let folded = name.to_lowercase();
        if folded != WRAPPER && folded != HOOK {
            continue;
        }
        let cased = if &folded == name {
            ""
        } else {
            "\n\x20\x20The names differ only in case, which a case-folding filesystem does not \
             keep apart."
        };
        bail!(
            "the bundle already emits a file named {name}, which {} needs for its shim.{cased}",
            plan.flag()
        );
    }
    let target = serde_json::to_string(&format!("./{entry}"))?;

    let external_decls = if plan.external.is_empty() {
        String::new()
    } else {
        format!(
            r#"
const EXTERNALS = Object.fromEntries({}.map((record) => [record.id, record]));

// These are source-tree-relative paths, not paths in the extracted payload. The
// latter lives in nub's cache and has no dependency tree to search.
"#,
            serde_json::to_string(plan.external_imports)?
        )
    };

    let external_branch = if plan.external.is_empty() {
        String::new()
    } else {
        r#"
    const external = EXTERNALS[specifier];
    if (external !== undefined) {
      const parentURL = external.importer === null
        ? BASE
        : pathToFileURL(join(process.cwd(), external.importer)).href;
      try {
        return nextResolve(external.specifier, { ...context, parentURL });
      } catch (cause) {
        try {
          return {
            url: pathToFileURL(createRequire(parentURL).resolve(external.specifier)).href,
            shortCircuit: true,
          };
        } catch {}
        // "Install it" is only the right advice for a package that is absent. An
        // installed package with an unexported subpath or a malformed specifier
        // fails here too, and mislabelling that would send the user hunting for a
        // dependency they already have.
        if (!missing(cause)) throw cause;
        const err = new Error(
          `Cannot find ${external.specifier}.\n\n` +
            `  This executable was compiled with --external ${external.package}, so ${external.package} is not\n` +
            `  part of it — it is resolved when the executable runs, from the directory\n` +
            `  of the module that imported it.\n\n` +
            `  Started in: ${process.cwd()}\n` +
            `  Install ${external.package} there, or run this from a directory that already has it.`,
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
    // resolving it through a synthetic CJS parent would give `import()` CJS extension
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
    let create_require = if plan.external.is_empty() {
        ""
    } else {
        ", createRequire"
    };
    let hook = format!(
        r#"// Generated by `nub compile`. Resolves what the artifact was told to leave for
// run time from the directory this executable was started in — the bundle itself
// runs from a cache directory, where nothing would ever be found.
const record = process[Symbol.for("nub.compile.bootstrap")];
const {{ registerHooks{create_require} }} = record.getBuiltin("node:module");
const {{ pathToFileURL }} = record.getBuiltin("node:url");
const {{ join }} = record.getBuiltin("node:path");

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
const record = process[Symbol.for("nub.compile.bootstrap")];
const {{ fileURLToPath }} = record.getBuiltin("node:url");
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
            AppFile::plain(HOOK, hook.into_bytes()),
            AppFile::plain(WRAPPER, wrapper.into_bytes()),
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(names: &[&str]) -> Vec<AppFile<Vec<u8>>> {
        names
            .iter()
            .map(|n| AppFile::plain(*n, Vec::new()))
            .collect()
    }

    fn plan(external: &[String], dynamic: bool) -> ShimPlan<'_> {
        ShimPlan {
            external,
            external_imports: &[],
            dynamic,
        }
    }

    fn pkgs(names: &[&str]) -> Vec<String> {
        names.iter().map(|p| (*p).to_string()).collect()
    }

    fn built(packages: &[&str], dynamic: bool) -> Shim {
        let packages = pkgs(packages);
        let imports = packages
            .iter()
            .enumerate()
            .map(|(n, package)| ExternalImport {
                id: format!("\0nub:compile-external:{n}"),
                specifier: package.clone(),
                package: package.clone(),
                importer: None,
            })
            .collect::<Vec<_>>();
        shim(
            &app(&["app.js"]),
            "app.js",
            &ShimPlan {
                external: &packages,
                external_imports: &imports,
                dynamic,
            },
        )
        .expect("shim")
    }

    fn provenance(id: &str, specifier: &str, importer: Option<&str>) -> ExternalImport {
        ExternalImport {
            id: id.into(),
            specifier: specifier.into(),
            package: "peer".into(),
            importer: importer.map(str::to_string),
        }
    }

    /// Every generated file must parse — both are assembled by `format!` with
    /// brace escaping, so a syntax error would ship as a runtime crash inside a
    /// frozen binary with no build signal at all.
    fn assert_parses(s: &Shim) {
        for file in &s.files {
            let (name, source) = (&file.name, std::str::from_utf8(&file.bytes).expect("utf8"));
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
        let found = s
            .files
            .iter()
            .find(|f| f.name == name)
            .unwrap_or_else(|| panic!("the shim must emit {name}, got {:?}", s.files));
        std::str::from_utf8(&found.bytes).expect("utf8")
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

    /// The gate is "does `module.registerHooks` exist", not "is this newer than
    /// 22.15.0". Node 23.0.0–23.4.x sorts above 22.15.0 and has no such API — the
    /// 23.x line got it at 23.5.0 — so a shim build against that band produced a
    /// binary that died at startup. A bare `--target 23` floors at 23.0.0, which is
    /// how a user reaches this without naming a patch version.
    #[test]
    fn the_node_gate_refuses_the_23_0_to_23_4_registerhooks_hole() {
        let prettier = pkgs(&["prettier"]);

        for refused in [
            NodeVersion::new(23, 0, 0),
            NodeVersion::new(23, 4, 0),
            NodeVersion::new(23, 4, 99),
        ] {
            let err = match check_node_support(&refused, "--target 23", &plan(&prettier, false)) {
                Err(err) => err,
                Ok(()) => panic!("Node {refused} has no registerHooks and must be refused"),
            };
            let msg = format!("{err:#}");
            assert!(
                msg.contains(&refused.to_string()),
                "must name what was targeted: {msg}"
            );
            // The suggestion has to stay on the line the user targets. Sending someone
            // on 23.4 to 22.15 is a cross-major downgrade, and "22.15 or newer" would
            // re-admit the very band being refused.
            assert!(
                msg.contains("--target 23.5.0"),
                "must suggest the 23.x line's own floor, not 22.15.0: {msg}"
            );
            assert!(
                !msg.contains("--target 22.15.0"),
                "must not suggest a cross-major downgrade: {msg}"
            );
        }

        // Both real floors are accepted, and so is everything above the 23.x one.
        for accepted in [
            NodeVersion::new(22, 15, 0),
            NodeVersion::new(23, 5, 0),
            NodeVersion::new(23, 11, 1),
            NodeVersion::new(24, 0, 0),
        ] {
            assert!(
                check_node_support(&accepted, "--target", &plan(&prettier, false)).is_ok(),
                "Node {accepted} has registerHooks and must be accepted"
            );
        }
    }

    /// Below the 22.x floor the suggestion cannot say a bare "or newer": that phrase
    /// sweeps 23.0–23.4 back in, which is the ordering fallacy the gate exists to fix.
    #[test]
    fn the_node_gate_excludes_the_hole_when_it_suggests_the_22_floor() {
        let err = check_node_support(
            &NodeVersion::new(20, 19, 0),
            "--target",
            &plan(&pkgs(&["prettier"]), false),
        )
        .expect_err("must be rejected");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("other than 23.0 through 23.4"),
            "a 22.15 suggestion must carve out the hole: {msg}"
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
            file(&s, "__nub_external.mjs").contains(r#""package":"prettier""#)
                && file(&s, "__nub_external.mjs").contains(r#""package":"@scope/pkg""#),
            "the external records must be baked in as JSON"
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
    fn generated_helpers_take_builtin_apis_from_the_early_bootstrap() {
        let generated = built(&["peer"], true);
        let hook = file(&generated, HOOK);
        let wrapper = file(&generated, WRAPPER);
        for source in [hook, wrapper] {
            assert!(
                source.contains(r#"process[Symbol.for("nub.compile.bootstrap")]"#),
                "the early launcher record must supply builtin APIs: {source}"
            );
            for builtin in ["node:module", "node:url", "node:path"] {
                assert!(
                    !source.contains(&format!(r#"from "{builtin}""#)),
                    "generated helpers must not statically import redirectable builtins: {source}"
                );
            }
        }
        assert!(
            hook.contains(r#"record.getBuiltin("node:module")"#)
                && hook.contains(r#"record.getBuiltin("node:url")"#)
                && hook.contains(r#"record.getBuiltin("node:path")"#),
            "the hook needs only bootstrap-captured builtins: {hook}"
        );
        assert!(
            wrapper.contains(r#"record.getBuiltin("node:url")"#),
            "the wrapper must use the bootstrap-captured node:url API: {wrapper}"
        );
    }

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
                && !dynamic_only.contains("EXTERNALS")
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
        // A case variant is the same overwrite on any target whose filesystem
        // folds, and it is invisible on a build host that folds too — which is
        // every macOS and Windows machine nub is built on.
        assert!(shim(&app(&["app.js", "__NUB_External.mjs"]), "app.js", &plan).is_err());
    }

    #[test]
    fn external_hook_resolves_each_synthetic_id_from_its_own_importer() {
        let packages = pkgs(&["peer"]);
        let imports = vec![
            provenance("\0nub:compile-external:root", "peer", None),
            provenance(
                "\0nub:compile-external:host",
                "peer",
                Some("node_modules/host/index.mjs"),
            ),
        ];
        let plan = ShimPlan {
            external: &packages,
            external_imports: &imports,
            dynamic: false,
        };
        let generated = shim(&app(&["app.js"]), "app.js", &plan).unwrap();
        let hook = file(&generated, HOOK);
        assert!(
            hook.contains("const EXTERNALS = Object.fromEntries")
                && hook.contains("node_modules/host/index.mjs"),
            "the generated table must retain both distinct importer records: {hook}"
        );
        assert!(
            hook.contains("pathToFileURL(join(process.cwd(), external.importer)).href"),
            "a physical importer must be reconstructed below the launch cwd: {hook}"
        );
        assert!(
            hook.contains("external.importer === null\n        ? BASE"),
            "a virtual importer must retain the deliberate cwd fallback: {hook}"
        );
    }

    #[test]
    fn workers_install_and_validate_the_bootstrap_before_generated_helpers() {
        let workers = vec![WorkerRoot {
            entry: "worker-a.mjs".into(),
            chunk: "worker-a-code.mjs".into(),
        }];
        let with_hook = worker_wrappers(&workers, true, "").expect("worker wrapper");
        let source = std::str::from_utf8(&with_hook[0].1).unwrap();
        let bootstrap = source
            .find(r#"await import("./__nub_compile_bootstrap.cjs")"#)
            .expect("bootstrap import");
        let record = source
            .find(r#"process[Symbol.for("nub.compile.bootstrap")]"#)
            .expect("bootstrap record validation");
        let validation = source
            .find("internal Worker bootstrap failed")
            .expect("bootstrap failure diagnostic");
        let hook = source
            .find(r#"await import("./__nub_external.mjs")"#)
            .expect("resolver hook import");
        let chunk = source
            .find(r#"await import("./worker-a-code.mjs")"#)
            .expect("worker chunk import");
        assert!(
            bootstrap < record && record < validation && validation < hook && hook < chunk,
            "bootstrap, validation, hook, and chunk must evaluate in that order: {source}"
        );
        assert!(
            source.contains(r#"typeof record?.createRequire !== "function""#)
                && source.contains(r#"typeof record?.getBuiltin !== "function""#)
                && source.contains(r#"typeof record?.requireArg !== "string""#)
                && source.contains("internal Worker bootstrap failed"),
            "the private record must be rejected before generated helpers consume it: {source}"
        );
        assert!(
            !source.contains(r#"import "./__nub_external.mjs";"#)
                && !source.contains(r#"import "./worker-a-code.mjs";"#),
            "neither dependency can be static without breaking evaluation order: {source}"
        );

        let ordinary = worker_wrappers(&workers, false, "").expect("ordinary worker wrapper");
        let source = std::str::from_utf8(&ordinary[0].1).unwrap();
        assert!(
            !source.contains(HOOK),
            "no hook file exists in this payload: {source}"
        );
        assert!(
            source.contains(r#"await import("./__nub_compile_bootstrap.cjs")"#)
                && source.contains(r#"await import("./worker-a-code.mjs")"#),
            "workers without a resolver hook still need the bootstrap: {source}"
        );

        let nested = worker_wrappers(&workers, true, "src/cli").expect("nested wrapper");
        let source = std::str::from_utf8(&nested[0].1).unwrap();
        assert!(
            source.contains(r#"await import("../../__nub_compile_bootstrap.cjs")"#)
                && source.contains(r#"await import("../../__nub_external.mjs")"#)
                && source.contains(r#"await import("./worker-a-code.mjs")"#),
            "bootstrap and hook live at the app root while the chunk stays beside its wrapper: {source}"
        );
    }

    #[test]
    fn worker_external_payload_rebases_from_a_foreign_launch_directory() {
        let packages = pkgs(&["peer"]);
        let imports = vec![provenance(
            "\0nub:compile-external:worker",
            "peer",
            Some("src/worker.ts"),
        )];
        let plan = ShimPlan {
            external: &packages,
            external_imports: &imports,
            dynamic: true,
        };
        let workers = vec![WorkerRoot {
            entry: "worker-a.mjs".into(),
            chunk: "worker-a-code.mjs".into(),
        }];
        let mut payload = app(&["main.mjs", "worker-a-code.mjs"]);
        payload.extend(
            worker_wrappers(&workers, plan.needed(), "")
                .unwrap()
                .into_iter()
                .map(|(name, bytes)| AppFile::plain(name, bytes)),
        );
        let generated = shim(&payload, "main.mjs", &plan).unwrap();
        payload.extend(generated.files);

        let worker = payload.iter().find(|f| f.name == "worker-a.mjs").unwrap();
        let worker = std::str::from_utf8(&worker.bytes).unwrap();
        assert!(
            worker.contains(HOOK) && worker.contains("await import"),
            "{worker}"
        );
        let hook = payload.iter().find(|f| f.name == HOOK).unwrap();
        let hook = std::str::from_utf8(&hook.bytes).unwrap();
        assert!(
            hook.contains("pathToFileURL(join(process.cwd(), external.importer)).href"),
            "worker-originated externals must use the launch cwd after source deletion: {hook}"
        );
        assert!(
            hook.contains("? [own, BASE]") && hook.contains(": [BASE, own]"),
            "computed worker imports retain the same foreign-cwd order: {hook}"
        );
    }
}
