//! Extract import/require specifier OCCURRENCES from one source file, using the
//! same oxc parser nub transpiles with.
//!
//! Every static `import`, runtime re-export, dynamic `import()`, `require()`, and
//! `require.resolve()` with a string-literal argument is recorded, tagged `soft`
//! when it appears lexically inside a runtime GUARD — a `try`/`catch`/`finally`,
//! or a conditional branch (`if`/`else`, a ternary arm, the right of `&&`/`||`).
//! Both are the "loaded only when the environment/condition permits" pattern the
//! spec classifies soft (e.g. `if (typeof phantom !== 'undefined') require('system')`,
//! or `try { require('opt') } catch {}`). A top-level, unconditional require is
//! hard. Type-only imports/exports are dropped — they are erased before runtime
//! and would false-flag a devDep-typed package as a phantom. A dynamic
//! `import(...)`/`require(...)`/`createRequire(...)(...)` is recorded only when
//! its argument is a STATIC string (a literal, or a no-substitution template);
//! a substituted-template or computed specifier cannot be attributed to a
//! package name and is skipped — guessing would risk a false positive.
//!
//! Guard-ness is LEXICAL, not control-flow: a require in a function DECLARED
//! inside an `if`/`try` is marked soft even if that function is later called
//! unconditionally. This only ever errs toward soft (a false negative on
//! hardness), never toward a false phantom — safe for the never-false-flag bar.

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Argument, ConditionalExpression, Expression, IfStatement, ImportDeclarationSpecifier,
    LogicalExpression, Statement, TryStatement,
};
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser;
use oxc_span::SourceType;

/// How a specifier was referenced. Kept for reporting; the classifier treats all
/// as runtime edges (soft-ness, not kind, gates the phantom/soft split).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    StaticImport,
    ReExport,
    DynamicImport,
    Require,
    RequireResolve,
}

/// One reference to a specifier in a file.
#[derive(Debug, Clone)]
pub struct Occurrence {
    /// The raw specifier string, exactly as written (`./x`, `lodash/fp`, `node:fs`).
    pub spec: String,
    /// Inside a `try`/`catch`/`finally` — a guarded (soft) load.
    pub soft: bool,
    pub kind: RefKind,
}

/// Parse `source` (its `SourceType` inferred from `path`) and return every
/// specifier occurrence. A parse that panics yields an empty list (the file is
/// simply not analyzed rather than aborting the package).
///
/// The production extraction path: a full, guard-aware AST visit. (A
/// static-imports-only fast-path ladder was built and benchmarked but regressed
/// the parse-dominated scan, so it was removed.)
pub fn extract(path: &str, source: &str) -> Vec<Occurrence> {
    // Framework single-file components (Astro/Vue/Svelte) keep their imports in a
    // frontmatter / `<script>` block. A backend imported there for TYPES ONLY still
    // breaks resolution under the global virtual store: the package's realpath
    // escapes into the shared store, so a type-checker's upward `node_modules` walk
    // can't reach the hoisted backend (nub#450). So for an SFC we parse only its
    // script region, as TS, and DO keep type-only imports. Plain `.js`/`.ts` are
    // unchanged — type-only imports stay dropped, so a devDep-typed package is
    // never false-flagged as a runtime phantom.
    if let Some(script) = sfc_script(path, source) {
        let ts = SourceType::from_path("_.mts").unwrap_or_else(|_| SourceType::mjs());
        return parse_and_visit(&script, ts, true);
    }
    // A `.d.ts` (`.d.mts`/`.d.cts`) is a package's TYPE surface. Its imports are
    // type-position by nature (`import * as React from 'react'`, `import type …`),
    // yet under the global virtual store they still drive TS's resolution: the
    // package's realpath escapes into the shared store, so a type import of a
    // top-level-only backend (`@types/react`, `astro/types`) fails to resolve
    // (nub#450). So a declaration file is parsed with type imports KEPT, exactly
    // like an SFC script region. Plain `.js`/`.ts` still drop type-only imports.
    if is_dts(path) {
        let ts = SourceType::from_path("_.mts").unwrap_or_else(|_| SourceType::mjs());
        return parse_and_visit(source, ts, true);
    }
    let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::mjs());
    parse_and_visit(source, source_type, false)
}

/// Parse `source` as `source_type` and collect specifier occurrences.
/// `capture_types` retains type-only import/re-export specifiers — set only for
/// SFC script blocks; false on the runtime path preserves the type-only drop.
fn parse_and_visit(source: &str, source_type: SourceType, capture_types: bool) -> Vec<Occurrence> {
    let allocator = Allocator::default();
    let ret = Parser::new(&allocator, source, source_type).parse();
    if ret.panicked {
        return Vec::new();
    }
    let mut v = SpecVisitor {
        guard_depth: 0,
        out: Vec::new(),
        capture_types,
    };
    v.visit_program(&ret.program);
    v.out
}

struct SpecVisitor {
    /// Lexical nesting inside a try/catch/finally OR a conditional branch. A
    /// nonzero depth means the current position is only reached under a runtime
    /// guard → any load here is soft.
    guard_depth: u32,
    out: Vec<Occurrence>,
    /// Retain type-only import/re-export specifiers. Set only for SFC script
    /// blocks, where a type-position phantom still breaks GVS resolution (nub#450).
    capture_types: bool,
}

impl SpecVisitor {
    fn record(&mut self, spec: &str, kind: RefKind) {
        self.out.push(Occurrence {
            spec: spec.to_string(),
            soft: self.guard_depth > 0,
            kind,
        });
    }
}

impl<'a> Visit<'a> for SpecVisitor {
    fn visit_try_statement(&mut self, it: &TryStatement<'a>) {
        // Everything lexically within a try — the try block, the catch handler,
        // and the finalizer — is a guarded region: the canonical optional-load
        // pattern is `try { x = require('opt') } catch { x = fallback }`.
        self.guard_depth += 1;
        walk::walk_try_statement(self, it);
        self.guard_depth -= 1;
    }

    fn visit_if_statement(&mut self, it: &IfStatement<'a>) {
        // The TEST runs unconditionally; the branches do not. Guard only the
        // branch bodies so `if (typeof x !== 'undefined') require('x')` is soft
        // while a require in the test itself stays hard.
        self.visit_expression(&it.test);
        self.guard_depth += 1;
        self.visit_statement(&it.consequent);
        if let Some(alt) = &it.alternate {
            self.visit_statement(alt);
        }
        self.guard_depth -= 1;
    }

    fn visit_conditional_expression(&mut self, it: &ConditionalExpression<'a>) {
        // Ternary: test unconditional, arms guarded.
        self.visit_expression(&it.test);
        self.guard_depth += 1;
        self.visit_expression(&it.consequent);
        self.visit_expression(&it.alternate);
        self.guard_depth -= 1;
    }

    fn visit_logical_expression(&mut self, it: &LogicalExpression<'a>) {
        // `a && require('x')` / `a || require('x')`: the left runs
        // unconditionally, the right is short-circuit-guarded.
        self.visit_expression(&it.left);
        self.guard_depth += 1;
        self.visit_expression(&it.right);
        self.guard_depth -= 1;
    }

    fn visit_statement(&mut self, it: &Statement<'a>) {
        match it {
            // Static ESM import. `import type …` (declaration-level) is erased;
            // an inline all-`type` named import is likewise erased. A bare
            // `import "x"` (side effect) and any value specifier are runtime.
            Statement::ImportDeclaration(decl)
                if self.capture_types
                    || (!decl.import_kind.is_type() && import_has_value(decl)) =>
            {
                self.record(&decl.source.value, RefKind::StaticImport);
            }
            // Runtime re-exports. `export { x } from 'y'` (value) and
            // `export * from 'y'` load the module; `export type { … }` does not
            // (unless `capture_types`, for SFC script blocks).
            Statement::ExportNamedDeclaration(decl) => {
                if let Some(src) = &decl.source
                    && (self.capture_types
                        || (!decl.export_kind.is_type() && export_named_has_value(decl)))
                {
                    self.record(&src.value, RefKind::ReExport);
                }
            }
            Statement::ExportAllDeclaration(decl)
                if self.capture_types || !decl.export_kind.is_type() =>
            {
                self.record(&decl.source.value, RefKind::ReExport);
            }
            _ => {}
        }
        walk::walk_statement(self, it);
    }

    fn visit_expression(&mut self, it: &Expression<'a>) {
        match it {
            // Dynamic `import(...)` — attributable when the source is a static
            // string: a literal, or a no-substitution template (`import(`react`)`).
            // A substituted template / computed source is skipped.
            Expression::ImportExpression(imp) => {
                if let Some(spec) = static_string(&imp.source) {
                    self.record(spec, RefKind::DynamicImport);
                }
            }
            // `require("x")` / `require.resolve("x")` / `createRequire(...)("x")`.
            Expression::CallExpression(call) => {
                if let Some((spec, kind)) = require_call(call) {
                    self.record(spec, kind);
                }
            }
            _ => {}
        }
        walk::walk_expression(self, it);
    }
}

/// A named import declaration is a runtime (value) import unless every specifier
/// is inline-`type`. A bare import (no specifiers) is always runtime.
fn import_has_value(decl: &oxc_ast::ast::ImportDeclaration<'_>) -> bool {
    match &decl.specifiers {
        None => true,
        Some(specs) if specs.is_empty() => true,
        Some(specs) => specs.iter().any(|s| match s {
            ImportDeclarationSpecifier::ImportSpecifier(named) => !named.import_kind.is_type(),
            // default / namespace imports are always value bindings
            _ => true,
        }),
    }
}

/// `export { a, type B } from 'y'` is a runtime re-export unless every specifier
/// is inline-`type`.
fn export_named_has_value(decl: &oxc_ast::ast::ExportNamedDeclaration<'_>) -> bool {
    decl.specifiers.is_empty() || decl.specifiers.iter().any(|s| !s.export_kind.is_type())
}

/// A TypeScript declaration file (`.d.ts`/`.d.mts`/`.d.cts`) — a package's type
/// surface, parsed with type imports kept (see [`extract`]).
fn is_dts(path: &str) -> bool {
    let file = path.rsplit(['/', '\\']).next().unwrap_or(path);
    file.ends_with(".d.ts") || file.ends_with(".d.mts") || file.ends_with(".d.cts")
}

/// The analyzable script region of a single-file component, or `None` for a
/// non-SFC path. `.astro` → the frontmatter between the leading `---` fence and
/// its close; `.vue`/`.svelte` → the concatenated `<script>…</script>` bodies.
fn sfc_script(path: &str, source: &str) -> Option<String> {
    let file = path.rsplit(['/', '\\']).next().unwrap_or(path);
    match file.rsplit_once('.').map(|(_, e)| e) {
        Some("astro") => Some(astro_frontmatter(source)),
        Some("vue" | "svelte") => Some(script_blocks(source)),
        _ => None,
    }
}

/// Astro frontmatter: the region between a leading `---` fence (its own line) and
/// the next line that begins `---`. Empty when the file has no frontmatter.
fn astro_frontmatter(source: &str) -> String {
    let s = source.trim_start_matches('\u{feff}');
    let Some(after) = s.strip_prefix("---") else {
        return String::new();
    };
    let Some(body) = after
        .strip_prefix("\r\n")
        .or_else(|| after.strip_prefix('\n'))
    else {
        return String::new();
    };
    match body.find("\n---") {
        Some(end) => body[..end].to_string(),
        None => body.to_string(),
    }
}

/// Concatenate the bodies of every `<script …>…</script>` block (Vue/Svelte),
/// ignoring blocks inside `<!-- … -->` comments so a commented-out `<script>` is
/// never mistaken for real component code (a false phantom).
fn script_blocks(source: &str) -> String {
    let source = strip_html_comments(source);
    let mut out = String::new();
    let mut rest = source.as_str();
    while let Some(open) = rest.find("<script") {
        let after = &rest[open..];
        let Some(gt) = open_tag_end(after) else { break };
        let body_start = open + gt + 1;
        let Some(close) = rest[body_start..].find("</script>") else {
            break;
        };
        out.push_str(&rest[body_start..body_start + close]);
        out.push('\n');
        rest = &rest[body_start + close + "</script>".len()..];
    }
    out
}

/// Index of the `>` that closes an open tag, skipping any `>` inside a quoted
/// attribute value — Vue 3.3 `<script setup generic="T extends Record<string, X>">`
/// carries one, and a naive `find('>')` would slice the body mid-attribute.
fn open_tag_end(s: &str) -> Option<usize> {
    let mut quote: Option<u8> = None;
    for (i, &b) in s.as_bytes().iter().enumerate() {
        match quote {
            Some(q) => {
                if b == q {
                    quote = None;
                }
            }
            None => match b {
                b'"' | b'\'' => quote = Some(b),
                b'>' => return Some(i),
                _ => {}
            },
        }
    }
    None
}

/// Remove `<!-- … -->` comment spans (best-effort textual strip; a phantom scan
/// tolerates the rare `-->` inside a string literal). An unterminated comment
/// drops the remainder — commented-out code never counts.
fn strip_html_comments(source: &str) -> String {
    let mut out = String::new();
    let mut rest = source;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        match rest[start + 4..].find("-->") {
            Some(end) => rest = &rest[start + 4 + end + 3..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// If `call` is `require("lit")`, `require.resolve("lit")`, or the immediately-
/// invoked `createRequire(...)("lit")`, return the literal specifier and which.
/// Any non-string-literal argument yields `None`.
fn require_call<'a>(call: &'a oxc_ast::ast::CallExpression<'a>) -> Option<(&'a str, RefKind)> {
    let kind = match &call.callee {
        Expression::Identifier(id) if id.name == "require" => RefKind::Require,
        Expression::StaticMemberExpression(m) => match &m.object {
            Expression::Identifier(id) if id.name == "require" && m.property.name == "resolve" => {
                RefKind::RequireResolve
            }
            _ => return None,
        },
        // `createRequire(import.meta.url)("lit")` — the immediately-invoked form:
        // the callee is itself a `createRequire(...)` call whose result is a
        // require function, so a string-literal outer argument is a real edge.
        // ONLY the direct IIFE is handled; a name bound to `createRequire(...)`
        // and called later needs dataflow and stays skipped (no false positives).
        Expression::CallExpression(inner) if is_create_require(&inner.callee) => RefKind::Require,
        _ => return None,
    };
    match call.arguments.first() {
        Some(Argument::StringLiteral(s)) => Some((&s.value, kind)),
        _ => None,
    }
}

/// True if `callee` names `createRequire` — bare (`createRequire(...)`) or a
/// member (`module.createRequire(...)`). The receiver is not checked: only Node's
/// `createRequire` uses this name, and the outer call's argument must still be a
/// string literal for anything to be recorded.
fn is_create_require(callee: &Expression<'_>) -> bool {
    match callee {
        Expression::Identifier(id) => id.name == "createRequire",
        Expression::StaticMemberExpression(m) => m.property.name == "createRequire",
        _ => false,
    }
}

/// The statically-known string value of `expr`: a string literal, or a template
/// literal with no substitutions (a single quasi with a cooked value). A
/// substituted template or any computed expression has no attributable value.
fn static_string<'a>(expr: &'a Expression<'a>) -> Option<&'a str> {
    match expr {
        Expression::StringLiteral(s) => Some(&s.value),
        Expression::TemplateLiteral(t) if t.expressions.is_empty() => {
            t.quasis.first().and_then(|q| q.value.cooked.as_deref())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{RefKind, extract};

    fn specs(src: &str) -> Vec<(String, bool, RefKind)> {
        extract("mod.mjs", src)
            .into_iter()
            .map(|o| (o.spec, o.soft, o.kind))
            .collect()
    }

    #[test]
    fn captures_static_import_require_dynamic_and_reexport() {
        let got = specs(
            r#"
            import a from "aa";
            import "side-effect";
            export { z } from "zz";
            export * from "ss";
            const b = require("bb");
            const c = await import("cc");
            require.resolve("dd");
            "#,
        );
        let names: Vec<_> = got.iter().map(|(s, _, _)| s.as_str()).collect();
        assert!(names.contains(&"aa"));
        assert!(names.contains(&"side-effect"));
        assert!(names.contains(&"zz"));
        assert!(names.contains(&"ss"));
        assert!(names.contains(&"bb"));
        assert!(names.contains(&"cc"));
        assert!(names.contains(&"dd"));
    }

    #[test]
    fn try_catch_require_is_soft_toplevel_is_hard() {
        let got = specs(
            r#"
            const hard = require("hard-dep");
            let opt;
            try { opt = require("soft-dep"); } catch {}
            "#,
        );
        let hard = got.iter().find(|(s, _, _)| s == "hard-dep").unwrap();
        let soft = got.iter().find(|(s, _, _)| s == "soft-dep").unwrap();
        assert!(!hard.1, "top-level require must be hard");
        assert!(soft.1, "require inside try must be soft");
    }

    #[test]
    fn conditionally_guarded_requires_are_soft() {
        // The esprima→'system' shape: a require inside a `typeof` env-guard `if`
        // branch, plus `&&` and ternary guards. All soft; the top-level one hard.
        let got = specs(
            r#"
            const hard = require("always");
            if (typeof phantom !== "undefined") { var s = require("system"); }
            const c = cond && require("andguard");
            const t = flag ? require("ternguard") : null;
            "#,
        );
        let soft = |n: &str| got.iter().find(|(s, _, _)| s == n).unwrap().1;
        assert!(!soft("always"), "top-level require is hard");
        assert!(soft("system"), "require in if-branch is soft");
        assert!(soft("andguard"), "require in && right is soft");
        assert!(soft("ternguard"), "require in ternary arm is soft");
    }

    #[test]
    fn type_only_imports_are_dropped() {
        // Inline `type` modifiers are TS syntax — parse as .mts.
        let got: Vec<_> = extract(
            "mod.mts",
            r#"
            import type { T } from "type-pkg";
            import { type Only } from "inline-type-pkg";
            import { value, type Also } from "mixed-pkg";
            "#,
        )
        .into_iter()
        .map(|o| (o.spec, o.soft, o.kind))
        .collect();
        let names: Vec<_> = got.iter().map(|(s, _, _)| s.as_str()).collect();
        assert!(!names.contains(&"type-pkg"), "import type erased");
        assert!(
            !names.contains(&"inline-type-pkg"),
            "all-inline-type erased"
        );
        assert!(names.contains(&"mixed-pkg"), "mixed import is runtime");
    }

    #[test]
    fn astro_frontmatter_keeps_type_only_backend() {
        // astro-icon's Icon.astro imports `astro/types` for TYPES ONLY; under the
        // global virtual store that still breaks resolution (nub#450), so the SFC
        // path parses the frontmatter and keeps the type import. The HTML template
        // after the fence is not parsed.
        let src = "---\nimport type { HTMLAttributes } from \"astro/types\";\nimport { getIconData } from \"@iconify/utils\";\n---\n<svg {...rest} />\n";
        let names: Vec<_> = extract("components/Icon.astro", src)
            .into_iter()
            .map(|o| o.spec)
            .collect();
        assert!(
            names.iter().any(|s| s == "astro/types"),
            "type-only SFC import kept"
        );
        assert!(names.iter().any(|s| s == "@iconify/utils"));
    }

    #[test]
    fn vue_svelte_script_block_scanned() {
        let vue = "<template><div/></template>\n<script lang=\"ts\">\nimport type { Foo } from \"backend\";\n</script>\n";
        let names: Vec<_> = extract("Comp.vue", vue)
            .into_iter()
            .map(|o| o.spec)
            .collect();
        assert!(names.iter().any(|s| s == "backend"), "vue <script> scanned");
    }

    #[test]
    fn commented_out_script_block_is_ignored() {
        // A commented-out <script> must not be parsed as real component code, or
        // its imports would false-flag a phantom and force a needless disk eject.
        let vue = "<template><div/></template>\n<!-- <script>import 'ghost';</script> -->\n<script setup lang=\"ts\">\nimport type { Foo } from \"real-backend\";\n</script>\n";
        let names: Vec<_> = extract("Comp.vue", vue)
            .into_iter()
            .map(|o| o.spec)
            .collect();
        assert!(
            names.iter().any(|s| s == "real-backend"),
            "real <script> scanned"
        );
        assert!(
            !names.iter().any(|s| s == "ghost"),
            "commented <script> ignored: {names:?}"
        );
    }

    #[test]
    fn generic_script_setup_attribute_with_gt_is_sliced_correctly() {
        // Vue 3.3 generics: the open tag carries a `>` inside the attribute
        // value; the body must start after the real tag end or the parse is
        // garbled and the block's imports are silently missed.
        let vue = "<script setup lang=\"ts\" generic=\"T extends Record<string, unknown>\">\nimport type { Foo } from \"generic-backend\";\n</script>\n";
        let names: Vec<_> = extract("Comp.vue", vue)
            .into_iter()
            .map(|o| o.spec)
            .collect();
        assert!(
            names.iter().any(|s| s == "generic-backend"),
            "generic <script setup> body scanned: {names:?}"
        );
    }

    #[test]
    fn non_sfc_type_only_still_dropped() {
        // The runtime path is unchanged: a type-only import in a .ts stays dropped.
        let names: Vec<_> = extract("x.ts", "import type { T } from \"type-pkg\";\n")
            .into_iter()
            .map(|o| o.spec)
            .collect();
        assert!(names.is_empty(), "type-only import in .ts still dropped");
    }

    #[test]
    fn dts_keeps_type_surface_imports() {
        // A declaration file's imports are type-position but still drive TS
        // resolution under GVS (nub#450): both the value-syntax `import * as React`
        // and a `import type` are kept, and a relative type re-export.
        let dts = "import * as React from 'react';\nimport type { FormApi } from '@tanstack/form-core';\nexport type { Foo } from './internal';\n";
        for path in ["index.d.ts", "dist/index.d.mts", "types.d.cts"] {
            let names: Vec<_> = extract(path, dts).into_iter().map(|o| o.spec).collect();
            assert!(names.iter().any(|s| s == "react"), "{path}: react kept");
            assert!(
                names.iter().any(|s| s == "@tanstack/form-core"),
                "{path}: type-only import kept"
            );
            assert!(
                names.iter().any(|s| s == "./internal"),
                "{path}: relative type re-export kept (walked for .d.ts graph)"
            );
        }
    }

    #[test]
    fn computed_specifiers_are_not_recorded() {
        let got = specs(
            r#"
            const x = require("pre" + "fix");
            const y = await import(`tpl${a}`);
            require(dynamicVar);
            "#,
        );
        assert!(got.is_empty(), "no attributable string literal → skip");
    }

    #[test]
    fn createrequire_iife_and_no_substitution_template_are_caught() {
        // R3: two analyzable dynamic forms previously dropped as false negatives —
        // both carry a static string target. Guard-aware, like a bare require().
        let got = specs(
            r#"
            const a = createRequire(import.meta.url)("cr-dep");
            const b = module.createRequire(import.meta.url)("cr-member-dep");
            const c = await import(`tpl-lit`);
            let g;
            if (cond) { g = createRequire(import.meta.url)("cr-guarded"); }
            "#,
        );
        let has = |n: &str| got.iter().any(|(s, _, _)| s == n);
        assert!(has("cr-dep"), "createRequire(...)('lit') is a require edge");
        assert!(has("cr-member-dep"), "module.createRequire(...)('lit') too");
        assert!(
            has("tpl-lit"),
            "no-substitution import(`lit`) is analyzable"
        );
        let soft = |n: &str| got.iter().find(|(s, _, _)| s == n).unwrap().1;
        assert!(!soft("cr-dep"), "top-level createRequire require is hard");
        assert!(soft("cr-guarded"), "createRequire in an if-branch is soft");

        // Boundary — no new false positives: a createRequire result bound to a
        // name and called later (needs dataflow) and a SUBSTITUTED template both
        // stay skipped.
        let neg = specs(
            r#"
            const r = createRequire(import.meta.url);
            r("bound-later");
            const d = import(`pre-${x}`);
            "#,
        );
        assert!(
            neg.is_empty(),
            "bound-then-called createRequire + substituted template stay skipped"
        );
    }
}
