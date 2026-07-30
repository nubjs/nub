//! Native addon (`.node`) embedding.
//!
//! A `.node` file is a shared library, and `dlopen` will only take a real path on
//! a real filesystem — which is why a compiler that serves its app from a virtual
//! filesystem cannot load one at all. nub extracts the app to an ordinary
//! directory before running it, so the addon is just a file at a path and the
//! platform loader needs nothing special: no chmod (dlopen wants READ, not exec)
//! and no re-signing (a Mach-O's ad-hoc signature survives a byte copy). Both
//! verified on macOS/arm64, 2026-07-30.
//!
//! So the whole feature is: notice the `.node`, put its bytes in the payload
//! beside the chunks, and make the require find it there. It rides the SAME
//! `Assets` collector as the `file` loader and `new URL(…)`, so it inherits the
//! content-hashed naming, the flat layout, the payload cache key, and the
//! drop-what-nothing-references pass for free.
//!
//! THE EMITTED MODULE IS CommonJS ON PURPOSE. Every real addon is reached by a
//! `require()` from a CJS loader, and Rolldown's ESM→CJS interop hands a CJS
//! consumer the module NAMESPACE — so an `export default binding` would arrive as
//! `{ default: binding }` and every `binding.someFn` would be `undefined`, with no
//! error to attribute. `module.exports = …` makes `require()` yield the addon's
//! own exports, exactly as it does uncompiled. `import.meta.url` stays legal
//! inside Rolldown's CJS wrapper (it is a region of the ESM chunk), which is what
//! lets the module locate itself without `__filename`.
//!
//! WHAT MAKES THE PLATFORM PACKAGES RESOLVE AT ALL is not here — it is
//! `target_defines` in [`super`]. A modern NAPI-RS loader dispatches through
//! `if (process.platform === …) { if (process.arch === …) require("@scope/pkg-…") }`
//! and `@parcel/watcher` builds its specifier as
//! `` `@parcel/watcher-${process.platform}-${process.arch}` ``; with both operands
//! defined to the TARGET's values, Rolldown folds the template, dead-code-
//! eliminates the ladder, and resolves the one surviving literal. The addon that
//! gets embedded is therefore always the target's, never the build host's — which
//! is also why [`check_target`] can be a hard error rather than a warning.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use nub_core::compile::{TargetArch, TargetOs, TargetPlatform};
use rolldown::plugin::{
    HookLoadArgs, HookLoadOutput, HookLoadReturn, HookTransformArgs, HookTransformOutput,
    HookTransformOutputMap, HookTransformReturn, HookUsage, Plugin, SharedLoadPluginContext,
    SharedTransformPluginContext,
};
use rolldown_common::ModuleType;
use rolldown_common::side_effects::HookSideEffects;
use rolldown_utils::url::clean_url;

use super::loaders::Assets;

/// The module a `.node` import evaluates to: the addon, `dlopen`ed from its
/// extracted copy.
///
/// `createRequire(import.meta.url)` rather than a baked path — the build
/// machine's directory layout has nothing to do with the deploy machine's, and
/// the app dir is content-hash-keyed, so the only base that is right on both is
/// the chunk's own URL. Every chunk and every asset land in one flat directory,
/// which is the invariant this rests on (asserted by `reject_nested_chunks`).
fn addon_module(name: &str) -> String {
    format!(
        "const {{ createRequire: __nubCreateRequire }} = require(\"node:module\");\n\
         module.exports = __nubCreateRequire(import.meta.url)({});\n",
        serde_json::to_string(&format!("./{name}")).expect("an asset name serializes")
    )
}

/// Embeds `.node` addons, and clears the one CJS idiom that stops their loaders
/// from running once bundled.
#[derive(Debug)]
pub struct NativeAddons {
    /// The platform the artifact will run on. Every embedded addon is checked
    /// against it, because an addon for the wrong platform is unloadable and
    /// nothing later in the pipeline would notice.
    target: TargetPlatform,
    collected: Arc<Assets>,
    /// Whether `--loader` claimed `.node` itself. A user who maps it wants the
    /// other behavior (a path to hand `process.dlopen`, say), and silently
    /// overriding their flag is worse than not having the default.
    user_mapped: bool,
    /// Payload name → the build-machine file name it came from, for the compile
    /// summary. Keyed by PAYLOAD name because that is what survives to the far
    /// side of tree-shaking; see [`Self::survivors`].
    embedded: Mutex<BTreeMap<String, String>>,
}

impl NativeAddons {
    pub fn new(target: TargetPlatform, collected: Arc<Assets>, user_mapped: bool) -> Self {
        Self {
            target,
            collected,
            user_mapped,
            embedded: Mutex::new(BTreeMap::new()),
        }
    }

    /// The addons that actually reached the payload, named as the user knows them.
    ///
    /// Filtered against `kept` rather than reported straight from the load hook:
    /// bytes are collected while the graph is being built, but an addon in a
    /// module nothing ends up referencing is dropped afterwards by
    /// `retain_referenced`. Reporting the load-time set would announce an addon
    /// the binary does not carry — the exact mis-signal this summary exists to
    /// prevent.
    pub fn survivors(&self, kept: &BTreeSet<&str>) -> Vec<String> {
        self.embedded
            .lock()
            .map(|e| {
                e.iter()
                    .filter(|(payload, _)| kept.contains(payload.as_str()))
                    .map(|(_, source)| source.clone())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect()
            })
            .unwrap_or_default()
    }

    fn claims(&self, id: &str) -> bool {
        !self.user_mapped && Path::new(id).extension().is_some_and(|e| e == "node")
    }
}

impl Plugin for NativeAddons {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("nub:native-addons")
    }

    fn register_hook_usage(&self) -> HookUsage {
        HookUsage::Load | HookUsage::Transform
    }

    fn load(
        &self,
        _ctx: SharedLoadPluginContext,
        args: &HookLoadArgs<'_>,
    ) -> impl std::future::Future<Output = HookLoadReturn> + Send {
        // Rolldown hands `load` the id with any `?query` still attached, and a
        // query is legal in a specifier — so both the extension test and the read
        // run against the cleaned path.
        let id = clean_url(args.id).to_string();
        let claimed = self.claims(&id);
        async move {
            if !claimed {
                return Ok(None);
            }
            let path = Path::new(&id);
            let bytes = std::fs::read(path)
                .map_err(|e| anyhow::anyhow!("reading the native addon {id}: {e}"))?;
            check_target(&bytes, path, &self.target)?;
            let payload = self.collected.add(path, bytes)?;
            if let Some(name) = path.file_name().and_then(|n| n.to_str())
                && let Ok(mut seen) = self.embedded.lock()
            {
                seen.insert(payload.clone(), name.to_string());
            }
            let code = addon_module(&payload);
            Ok(Some(HookLoadOutput {
                code: code.into(),
                module_type: Some(ModuleType::Js),
                // Asserted, not left to `Default` (which means "decide for me" and
                // consults the addon package's own `sideEffects` field). Loading an
                // addon RUNS its initializer, and plenty are imported for exactly
                // that — a package declaring `"sideEffects": false` would otherwise
                // make the addon shakeable and silently drop it from the payload.
                side_effects: Some(HookSideEffects::True),
                ..Default::default()
            }))
        }
    }

    fn transform(
        &self,
        _ctx: SharedTransformPluginContext,
        args: &HookTransformArgs<'_>,
    ) -> impl std::future::Future<Output = HookTransformReturn> + Send {
        // Real JavaScript only — a `text`/`json` module's "code" IS the file's
        // content, and blanking something inside it would corrupt user data.
        //
        // The substring test is what keeps this hook off the critical path: it is
        // a necessary condition for any match (the assignment's value must be a
        // `createRequire` call), and it holds for a handful of modules in a graph
        // of thousands — so almost nothing pays for the extra parse.
        let scannable = matches!(
            args.module_type,
            ModuleType::Js | ModuleType::Jsx | ModuleType::Ts | ModuleType::Tsx
        ) && args.code.contains("createRequire");
        let spans = if scannable {
            require_rebindings(args.id, args.code)
        } else {
            Vec::new()
        };
        let source = (!spans.is_empty()).then(|| args.code.to_string());
        async move {
            let Some(source) = source else {
                return Ok(None);
            };
            Ok(Some(HookTransformOutput {
                code: Some(blank_spans(&source, &spans)),
                // The rewrite replaces bytes with spaces and keeps every newline,
                // so no position moves — which is what makes `Null` (rather than
                // `Omitted`, which would drop the module's mapping entirely)
                // the honest answer.
                map: HookTransformOutputMap::Null,
                ..Default::default()
            }))
        }
    }
}

// ---- the `require = createRequire(…)` rebinding -------------------------------

/// Byte ranges of every module-level `require = createRequire(…)` statement.
///
/// WHY THIS HAS TO GO. NAPI-RS emits `require = createRequire(__filename)` at the
/// top of every generated loader so the file works when some other bundler turns
/// it into ESM. Rolldown rewrites the `require("…")` CALLS in that module anyway —
/// measured, the reassignment does not stop it — but it leaves the assignment
/// itself, and assigning to an undeclared `require` inside an ES module throws
/// `ReferenceError: require is not defined in ES module scope` before any of the
/// rewritten calls run. So the addon loader dies on line 3 of a module whose every
/// interesting line the bundler already got right.
///
/// Blanking it is not merely the fix that works, it is the only one that does.
/// Turning it into a DECLARATION (`var require = …`) also compiles — and is worse:
/// Rolldown then sees a module-level `require` binding, stops rewriting the calls,
/// and the loader fails at run time looking for `node_modules` in a cache
/// directory. Verified all three ways against Rolldown 1.2.0 on 2026-07-30.
///
/// What is left behind is `require` meaning Rolldown's `__require`, i.e.
/// `createRequire(import.meta.url)` of the chunk. That is the closest thing to
/// `createRequire(__filename)` that survives bundling, so the statement is not
/// just removable — it is redundant.
///
/// NARROW ON PURPOSE, three ways: the statement must be at module top level, the
/// target must be a bare `require` the module never declares, and the value must
/// be a `createRequire(…)` call. A module that declares its own `require` is
/// writing to a real local binding and is left alone; anything but `createRequire`
/// on the right could have side effects worth keeping.
fn require_rebindings(id: &str, source: &str) -> Vec<(usize, usize)> {
    use oxc_allocator::Allocator;
    use oxc_ast::ast::{
        AssignmentOperator, AssignmentTarget, BindingPattern, Declaration, Expression, Statement,
    };
    use oxc_parser::Parser;
    use oxc_span::{GetSpan, SourceType};

    /// Whether a top-level statement introduces a `require` binding of its own.
    fn declares_require(stmt: &Statement<'_>) -> bool {
        match stmt {
            Statement::VariableDeclaration(d) => d.declarations.iter().any(|decl| {
                matches!(&decl.id, BindingPattern::BindingIdentifier(id) if id.name == "require")
            }),
            Statement::FunctionDeclaration(f) => {
                f.id.as_ref().is_some_and(|id| id.name == "require")
            }
            Statement::ClassDeclaration(c) => c.id.as_ref().is_some_and(|id| id.name == "require"),
            Statement::ImportDeclaration(i) => i.specifiers.as_ref().is_some_and(|specs| {
                specs.iter().any(|s| s.local().name == "require")
            }),
            // An exported `const require = …` is still a declaration.
            Statement::ExportNamedDeclaration(e) => e.declaration.as_ref().is_some_and(|d| {
                matches!(d, Declaration::VariableDeclaration(v) if v.declarations.iter().any(|decl| {
                    matches!(&decl.id, BindingPattern::BindingIdentifier(id) if id.name == "require")
                }))
            }),
            _ => false,
        }
    }

    /// `createRequire(…)` in either of its two spellings — the destructured
    /// identifier, or a member call off the `module` namespace.
    fn is_create_require(expr: &Expression<'_>) -> bool {
        let Expression::CallExpression(call) = expr else {
            return false;
        };
        match &call.callee {
            Expression::Identifier(id) => id.name == "createRequire",
            Expression::StaticMemberExpression(m) => m.property.name == "createRequire",
            _ => false,
        }
    }

    let source_type = SourceType::from_path(id).unwrap_or_else(|_| SourceType::mjs());
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, source_type).parse();
    // A parse failure yields nothing: Rolldown's own error is the better
    // diagnostic, and this pass must never be the thing that fails a build.
    if parsed.panicked {
        return Vec::new();
    }
    if parsed.program.body.iter().any(declares_require) {
        return Vec::new();
    }

    parsed
        .program
        .body
        .iter()
        .filter_map(|stmt| {
            let Statement::ExpressionStatement(st) = stmt else {
                return None;
            };
            let Expression::AssignmentExpression(assign) = &st.expression else {
                return None;
            };
            let targets_require = matches!(
                &assign.left,
                AssignmentTarget::AssignmentTargetIdentifier(id) if id.name == "require"
            );
            (assign.operator == AssignmentOperator::Assign
                && targets_require
                && is_create_require(&assign.right))
            .then(|| (st.span().start as usize, st.span().end as usize))
        })
        .collect()
}

/// Replace each range with whitespace, keeping every newline where it was.
///
/// Line-preserving, which is what lets the transform report an unchanged source
/// map. Ranges arrive in source order (they come from a single forward walk), so
/// one pass suffices.
fn blank_spans(source: &str, spans: &[(usize, usize)]) -> String {
    let mut out = String::with_capacity(source.len());
    let mut last = 0usize;
    for (start, end) in spans {
        out.push_str(&source[last..*start]);
        out.extend(
            source[*start..*end]
                .chars()
                .map(|c| if c == '\n' { '\n' } else { ' ' }),
        );
        last = *end;
    }
    out.push_str(&source[last..]);
    out
}

// ---- target matching ----------------------------------------------------------

/// What a `.node` file's object header says it is. `arch: None` is a Mach-O
/// universal binary, which carries several and satisfies any of them.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Object {
    os: TargetOs,
    arch: Option<TargetArch>,
}

/// Refuse an addon the target cannot load.
///
/// This is a HARD ERROR, not a warning, and it is the whole reason a cross-compile
/// is trustworthy. `--platform linux-x64` on a macOS host resolves whatever
/// `node_modules` happens to hold; without this check a Mach-O addon would be
/// embedded silently and the binary would die on a Linux machine — the failure
/// furthest in time and space from the mistake. It can be strict precisely because
/// the platform defines make resolution pick the TARGET's platform package
/// whenever one is installed, so a mismatch means it genuinely is not there.
fn check_target(bytes: &[u8], path: &Path, target: &TargetPlatform) -> Result<()> {
    let advice = "\x20\x20A native addon is machine code for one platform, and a compiled binary \
                  loads it\n\x20\x20from a real file at run time — there is no later step that \
                  could translate it.\n\x20\x20Install this dependency for the target (its \
                  platform package) and compile again,\n\x20\x20or drop --platform to build for \
                  this machine.";
    let Some(found) = classify(bytes) else {
        bail!(
            "this file is not a native addon any supported platform can load: {}\n\
             \x20\x20Expected a Mach-O, ELF, or PE shared library; its header is none of those.",
            path.display()
        );
    };
    if found.os != target.os {
        bail!(
            "this native addon is built for {}, but the binary targets {}: {}\n{advice}",
            os_name(found.os),
            target.triple(),
            path.display()
        );
    }
    if found.arch.is_some_and(|a| a != target.arch) {
        bail!(
            "this native addon is built for {} {}, but the binary targets {}: {}\n{advice}",
            os_name(found.os),
            arch_name(found.arch.expect("checked")),
            target.triple(),
            path.display()
        );
    }
    Ok(())
}

fn os_name(os: TargetOs) -> &'static str {
    match os {
        TargetOs::Darwin => "macOS",
        TargetOs::Linux => "Linux",
        TargetOs::Win32 => "Windows",
    }
}

fn arch_name(arch: TargetArch) -> &'static str {
    match arch {
        TargetArch::X64 => "x64",
        TargetArch::Arm64 => "arm64",
    }
}

/// Read the object header. Covers exactly the three formats nub targets, so a
/// file matching none of them cannot load anywhere and is reported as such.
///
/// glibc-vs-musl is deliberately NOT distinguished: both are ordinary ELF and the
/// difference lives in the dynamic-link requirements, not the header. A musl/glibc
/// mismatch still fails at run time, as it does uncompiled.
fn classify(bytes: &[u8]) -> Option<Object> {
    let magic: [u8; 4] = bytes.get(..4)?.try_into().ok()?;
    let le32 = |at: usize| -> Option<u32> {
        bytes
            .get(at..at + 4)
            .and_then(|s| s.try_into().ok())
            .map(u32::from_le_bytes)
    };
    let le16 = |at: usize| -> Option<u16> {
        bytes
            .get(at..at + 2)
            .and_then(|s| s.try_into().ok())
            .map(u16::from_le_bytes)
    };
    match magic {
        // Mach-O, 64-bit, either byte order.
        [0xcf, 0xfa, 0xed, 0xfe] | [0xfe, 0xed, 0xfa, 0xcf] => {
            let cpu = if magic[0] == 0xcf {
                le32(4)?
            } else {
                u32::from_be_bytes(bytes.get(4..8)?.try_into().ok()?)
            };
            Some(Object {
                os: TargetOs::Darwin,
                arch: match cpu {
                    // CPU_TYPE_ARM64 / CPU_TYPE_X86_64 (`mach/machine.h`).
                    0x0100_000C => Some(TargetArch::Arm64),
                    0x0100_0007 => Some(TargetArch::X64),
                    _ => None,
                },
            })
        }
        // Mach-O universal ("fat"), 32- and 64-bit headers, either byte order.
        // The slice list is not decoded: a universal binary is accepted for any
        // darwin arch, which can only ever be too permissive — and a false ACCEPT
        // degrades to the run-time failure the addon would have had anyway, while
        // a false REJECT would break a build that works.
        [0xca, 0xfe, 0xba, 0xbe]
        | [0xbe, 0xba, 0xfe, 0xca]
        | [0xca, 0xfe, 0xba, 0xbf]
        | [0xbf, 0xba, 0xfe, 0xca] => Some(Object {
            os: TargetOs::Darwin,
            arch: None,
        }),
        [0x7f, b'E', b'L', b'F'] => Some(Object {
            os: TargetOs::Linux,
            arch: match le16(18)? {
                // EM_X86_64 / EM_AARCH64.
                0x3E => Some(TargetArch::X64),
                0xB7 => Some(TargetArch::Arm64),
                _ => None,
            },
        }),
        // PE: the DOS stub points at the real header, whose Machine field is the
        // arch. A truncated or non-PE `MZ` file falls through to `None`.
        [b'M', b'Z', ..] => {
            let pe = le32(0x3C)? as usize;
            let sig = pe.checked_add(4)?;
            (bytes.get(pe..sig)? == b"PE\0\0".as_slice()).then_some(())?;
            Some(Object {
                os: TargetOs::Win32,
                arch: match le16(sig)? {
                    // IMAGE_FILE_MACHINE_AMD64 / _ARM64.
                    0x8664 => Some(TargetArch::X64),
                    0xAA64 => Some(TargetArch::Arm64),
                    _ => None,
                },
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(triple: &str) -> TargetPlatform {
        TargetPlatform::parse(triple).expect("a supported triple")
    }

    fn macho(cpu: u32) -> Vec<u8> {
        let mut v = vec![0xcf, 0xfa, 0xed, 0xfe];
        v.extend_from_slice(&cpu.to_le_bytes());
        v.resize(64, 0);
        v
    }

    fn elf(machine: u16) -> Vec<u8> {
        let mut v = vec![0u8; 64];
        v[..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
        v[18..20].copy_from_slice(&machine.to_le_bytes());
        v
    }

    fn pe(machine: u16) -> Vec<u8> {
        let mut v = vec![0u8; 256];
        v[..2].copy_from_slice(b"MZ");
        v[0x3C..0x40].copy_from_slice(&128u32.to_le_bytes());
        v[128..132].copy_from_slice(b"PE\0\0");
        v[132..134].copy_from_slice(&machine.to_le_bytes());
        v
    }

    #[test]
    fn every_targetable_object_format_is_recognized() {
        assert_eq!(
            classify(&macho(0x0100_000C)),
            Some(Object {
                os: TargetOs::Darwin,
                arch: Some(TargetArch::Arm64)
            })
        );
        assert_eq!(
            classify(&elf(0x3E)),
            Some(Object {
                os: TargetOs::Linux,
                arch: Some(TargetArch::X64)
            })
        );
        assert_eq!(
            classify(&pe(0xAA64)),
            Some(Object {
                os: TargetOs::Win32,
                arch: Some(TargetArch::Arm64)
            })
        );
        // A universal binary carries every slice, so it pins no single arch.
        assert_eq!(
            classify(&[0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 2]),
            Some(Object {
                os: TargetOs::Darwin,
                arch: None
            })
        );
        assert_eq!(classify(b"#!/bin/sh\n"), None);
        assert_eq!(classify(b""), None);
    }

    // The cross-compile guarantee: a host-built addon must not ride into a
    // foreign-platform binary, because nothing downstream would notice.
    #[test]
    fn a_foreign_addon_is_refused_and_the_message_names_both_sides() {
        let err = check_target(
            &macho(0x0100_000C),
            Path::new("/p/node_modules/x/x.node"),
            &target("linux-x64"),
        )
        .expect_err("a Mach-O cannot ship in a linux binary");
        let msg = format!("{err:#}");
        assert!(msg.contains("macOS"), "must name what the addon is: {msg}");
        assert!(
            msg.contains("linux-x64"),
            "must name what was targeted: {msg}"
        );
        assert!(msg.contains("x.node"), "must name the file: {msg}");

        // Right OS, wrong arch is the same class of unloadable.
        assert!(
            check_target(&elf(0x3E), Path::new("x.node"), &target("linux-arm64")).is_err(),
            "an x64 ELF cannot ship in an arm64 binary"
        );
        // …and the matching cases pass, including a universal binary against
        // either arch.
        check_target(&macho(0x0100_0007), Path::new("x"), &target("darwin-x64")).expect("match");
        check_target(&pe(0x8664), Path::new("x"), &target("win32-x64")).expect("match");
        for triple in ["darwin-x64", "darwin-arm64"] {
            check_target(
                &[0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 2],
                Path::new("x"),
                &target(triple),
            )
            .expect("a universal binary satisfies either arch");
        }
    }

    #[test]
    fn a_file_that_is_not_an_addon_at_all_says_so() {
        let err = check_target(
            b"not an object file",
            Path::new("x.node"),
            &target("darwin-arm64"),
        )
        .expect_err("unrecognized headers are refused");
        assert!(
            format!("{err:#}").contains("Mach-O, ELF, or PE"),
            "must say what was expected: {err:#}"
        );
    }

    // The NAPI-RS preamble, verbatim. Left in place it throws
    // `ReferenceError: require is not defined in ES module scope` before any
    // rewritten call runs.
    #[test]
    fn the_napi_rs_require_rebinding_is_blanked_and_nothing_else_moves() {
        let source = "const { createRequire } = require('node:module')\n\
                      require = createRequire(__filename)\n\
                      module.exports = require('@scope/pkg-darwin-arm64')\n";
        let spans = require_rebindings("index.js", source);
        assert_eq!(spans.len(), 1, "exactly the rebinding: {spans:?}");
        let out = blank_spans(source, &spans);
        assert!(
            !out.contains("require = createRequire"),
            "the rebinding must be gone: {out}"
        );
        assert!(
            out.contains("const { createRequire } = require('node:module')")
                && out.contains("module.exports = require('@scope/pkg-darwin-arm64')"),
            "everything else must survive byte-for-byte: {out}"
        );
        assert_eq!(
            out.lines().count(),
            source.lines().count(),
            "line geometry must be preserved, or the Null source map lies"
        );
        assert_eq!(out.len(), source.len(), "only bytes inside the span change");
    }

    #[test]
    fn only_a_bare_require_assigned_a_create_require_call_is_touched() {
        // A module that DECLARES require owns that binding; rewriting there would
        // also stop Rolldown rewriting its calls, which is strictly worse.
        for declared in [
            "let require = createRequire(import.meta.url)\nrequire = createRequire(import.meta.url)\n",
            "import { createRequire } from 'node:module'\nfunction require() {}\nrequire = createRequire(1)\n",
        ] {
            assert!(
                require_rebindings("index.js", declared).is_empty(),
                "a declared require must be left alone: {declared}"
            );
        }
        // Anything other than createRequire on the right could have side effects.
        assert!(require_rebindings("i.js", "require = makeRequire(__filename)\n").is_empty());
        // A nested assignment is out of scope — see `require_rebindings`.
        assert!(
            require_rebindings("i.js", "function f() { require = createRequire(1) }\n").is_empty()
        );
        // The member spelling is the same idiom.
        assert_eq!(
            require_rebindings("i.js", "require = Module.createRequire(__filename)\n").len(),
            1
        );
    }

    // The interop contract: a CJS consumer must receive the addon's own exports,
    // never a `{ default: … }` namespace.
    #[test]
    fn the_emitted_module_is_commonjs_and_locates_itself_from_the_chunk() {
        let code = addon_module("watcher-a1b2c3d4.node");
        assert!(
            code.contains("module.exports ="),
            "must be CJS-shaped or Rolldown's interop wraps it: {code}"
        );
        assert!(
            code.contains("import.meta.url"),
            "must locate itself from the chunk, not a build-machine path: {code}"
        );
        assert!(
            code.contains(r#""./watcher-a1b2c3d4.node""#),
            "must name the payload entry: {code}"
        );
        let allocator = oxc_allocator::Allocator::default();
        let parsed =
            oxc_parser::Parser::new(&allocator, &code, oxc_span::SourceType::mjs()).parse();
        assert!(
            !parsed.panicked && parsed.diagnostics.is_empty(),
            "the generated module must parse: {:?}",
            parsed.diagnostics
        );
    }
}
