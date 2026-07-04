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
//! and would false-flag a devDep-typed package as a phantom. Dynamic specifiers
//! (template/computed) are not recorded: they cannot be attributed to a package
//! name, and guessing would risk a false positive.
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
pub fn extract(path: &str, source: &str) -> Vec<Occurrence> {
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::mjs());
    let ret = Parser::new(&allocator, source, source_type).parse();
    if ret.panicked {
        return Vec::new();
    }
    let mut v = SpecVisitor {
        guard_depth: 0,
        out: Vec::new(),
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
                if !decl.import_kind.is_type() && import_has_value(decl) =>
            {
                self.record(&decl.source.value, RefKind::StaticImport);
            }
            // Runtime re-exports. `export { x } from 'y'` (value) and
            // `export * from 'y'` load the module; `export type { … }` does not.
            Statement::ExportNamedDeclaration(decl) => {
                if let Some(src) = &decl.source
                    && !decl.export_kind.is_type()
                    && export_named_has_value(decl)
                {
                    self.record(&src.value, RefKind::ReExport);
                }
            }
            Statement::ExportAllDeclaration(decl) if !decl.export_kind.is_type() => {
                self.record(&decl.source.value, RefKind::ReExport);
            }
            _ => {}
        }
        walk::walk_statement(self, it);
    }

    fn visit_expression(&mut self, it: &Expression<'a>) {
        match it {
            // Dynamic `import("x")` — only a string-literal argument is
            // attributable to a package; template/computed sources are skipped.
            Expression::ImportExpression(imp) => {
                if let Expression::StringLiteral(s) = &imp.source {
                    self.record(&s.value, RefKind::DynamicImport);
                }
            }
            // `require("x")` / `require.resolve("x")`.
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

/// If `call` is `require("lit")` or `require.resolve("lit")`, return the literal
/// specifier and which. Any non-string-literal argument yields `None`.
fn require_call<'a>(call: &'a oxc_ast::ast::CallExpression<'a>) -> Option<(&'a str, RefKind)> {
    let kind = match &call.callee {
        Expression::Identifier(id) if id.name == "require" => RefKind::Require,
        Expression::StaticMemberExpression(m) => match &m.object {
            Expression::Identifier(id) if id.name == "require" && m.property.name == "resolve" => {
                RefKind::RequireResolve
            }
            _ => return None,
        },
        _ => return None,
    };
    match call.arguments.first() {
        Some(Argument::StringLiteral(s)) => Some((&s.value, kind)),
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
}
