//! Native source-shape detection for the JS preload.
//!
//! Two questions the transpile pipeline must answer without a JS parser, now that
//! `oxc-parser` is no longer a dependency:
//!
//!   1. **Module format** — does an ambiguous-extension file (`.ts`/`.tsx`/`.jsx`
//!      with no `package.json` `type`) carry VALUE-level ESM syntax? This mirrors
//!      Node's `--experimental-detect-module`: type-only `import`/`export` are
//!      erased by the transpiler and must NOT count; a value import/export, a bare
//!      `import "x"`, `import.meta`, or top-level `await` all force `module`.
//!   2. **Stage-3 decorators** — does the source contain `@decorator` syntax?
//!      oxc passes Stage-3 decorators through verbatim (errors: []), so the JS
//!      surfaces a clean diagnostic instead of a bare V8 `SyntaxError`. Only asked
//!      when legacy decorators are off.
//!
//! Both were previously computed in JS off `oxc-parser`'s `parseSync` AST. They
//! now ride the same `oxc` parser already compiled into this addon for `transform`,
//! so the addon is self-contained and the `oxc-parser` npm package is gone.

use napi_derive::napi;

use oxc::{
    allocator::Allocator,
    ast::ast::{ImportDeclarationSpecifier, RegExpFlags, Statement, VariableDeclaration},
    ast_visit::Visit,
    parser::Parser,
};
use oxc_napi::get_source_type;

/// What the JS preload needs to know about a source file's shape. Mirrors the
/// fields the old `oxc-parser`-based detection read off the parse result.
#[napi(object)]
pub struct ModuleInfo {
    /// True when the source carries VALUE-level ESM syntax (the module-format
    /// signal). Equivalent to the old JS `hasEsmSyntax` over the parsed module
    /// record: a non-type import/export, a bare `import "x"`, `import.meta`, or a
    /// top-level `await` (the `hasModuleSyntax`-with-no-import/export/meta case).
    pub has_value_esm_syntax: bool,

    /// True when the source contains `@decorator` syntax anywhere (class or class
    /// member). Drives the Stage-3-decorator diagnostic when legacy mode is off.
    pub has_decorators: bool,

    /// True when the source contains syntax oxc LOWERS at nub's `target: "es2022"`
    /// — i.e. running the raw source on the Node 22.15 floor would SyntaxError or
    /// misbehave. This is the skip-gate verdict for project-source plain JS
    /// (`.js`/`.mjs`/`.cjs`): when FALSE, nub returns the file verbatim (byte for
    /// byte, no codegen, no sourcemap footer) instead of running it through oxc,
    /// which reformats no-op source. When TRUE, the file must be transpiled.
    ///
    /// PROVENANCE — PINNED TO oxc =0.132.0's es2022 lowering set. The complete set
    /// of SYNTAX oxc lowers at `target:"es2022"` for plain JS is exactly:
    ///   1. `using` / `await using` declarations (ES2026 explicit resource mgmt)
    ///   2. RegExp `v`-flag literals (ES2024 unicode-sets — oxc rewrites `/…/v` to
    ///      `new RegExp(…, "v")`; the raw literal throws on the 22.15 floor's V8)
    ///   3. legacy/Stage-3 decorators (option-driven, surfaced via `has_decorators`)
    /// Everything ≤ es2022 (class fields, static blocks, logical-assignment, numeric
    /// separators, top-level await, optional chaining, import attributes, the `d`
    /// match-indices RegExp flag) is NOT lowered at es2022 and is therefore NOT a
    /// trigger. Derived from oxc's `EnvOptions::from("es2022")` (`has_feature` is
    /// false ⇒ lower) — the editions jump es2022→es2026 with no es2023/24/25 syntax
    /// transform dirs, so the only syntax lowering above es2022 is (1) and the
    /// regexp `v`-flag (2). RE-DERIVE THIS SET ON ANY oxc BUMP: a new oxc version
    /// can add a lowered syntax (or change the editions), which would silently let a
    /// floor-breaking file run verbatim. The floor tests (one per trigger, run on
    /// Node 22.15) are the CI backstop — a future oxc that lowers more must make one
    /// of them fail. `has_decorators` is folded in by the JS gate (the decorator
    /// case routes via the Stage-3 guard / legacy transform), so this field tracks
    /// the target-version-gated SYNTAX triggers (using ∪ v-flag-regexp); the JS gate
    /// ORs it with `has_decorators`.
    pub transformable_syntax: bool,
}

/// Detect a file's module-format and decorator shape. `lang` is `'ts'`, `'tsx'`,
/// or `'jsx'` (matching the JS callers); it selects the parser's `SourceType`
/// exactly as the `transform` path does via `get_source_type`.
#[allow(clippy::needless_pass_by_value, clippy::allow_attributes)]
#[napi]
pub fn detect_module_info(
    filename: String,
    source_text: String,
    lang: Option<String>,
) -> ModuleInfo {
    let source_type = get_source_type(&filename, lang.as_deref(), None);

    let allocator = Allocator::default();
    let ret = Parser::new(&allocator, &source_text, source_type).parse();

    // A parse error means we can't trust the shape. The old JS treated an
    // unparseable file as CJS for format detection (the transpile surfaces the
    // real error) and as "no decorators" for the guard (V8 surfaces the error).
    // Both fall out of an all-false return.
    // A parse error means we can't trust the verdict. Defaulting
    // `transformable_syntax: false` lets the JS gate return the raw source verbatim
    // — which is the SAFE default for plain JS: a genuinely-unparseable file would
    // SyntaxError under transpile too, so handing back the raw bytes surfaces V8's
    // own error at exactly the spot Node would, identical to a non-transpiled file.
    if ret.panicked {
        return ModuleInfo {
            has_value_esm_syntax: false,
            has_decorators: false,
            transformable_syntax: false,
        };
    }

    let has_value_esm_syntax = has_value_esm(
        &ret.program.body,
        ret.module_record.has_module_syntax,
        !ret.module_record.import_metas.is_empty(),
    );

    // ONE AST walk computes both the decorator guard and the skip-gate verdict —
    // the visitor already traverses the whole tree, so folding the using/v-flag
    // checks into the same pass is near-free (no extra parse, no second walk).
    let mut finder = SyntaxFinder {
        decorators: false,
        transformable: false,
    };
    finder.visit_program(&ret.program);

    ModuleInfo {
        has_value_esm_syntax,
        has_decorators: finder.decorators,
        transformable_syntax: finder.transformable,
    }
}

/// Does the statement list carry value-level ESM syntax? Reproduces the JS
/// `hasEsmSyntax` decision over oxc's parse result:
///   * a value (non-`type`) `import`/`export` declaration, or a bare `import "x"`
///     (no specifiers), or `import.meta`, → true;
///   * otherwise, `has_module_syntax` set with NO import/export/meta is the
///     top-level-await case → true.
fn has_value_esm(body: &[Statement<'_>], has_module_syntax: bool, has_import_meta: bool) -> bool {
    // `import.meta` anywhere forces module format (the JS `mod.importMetas.length
    // > 0` rule), regardless of imports/exports.
    if has_import_meta {
        return true;
    }

    let mut saw_import_export = false;

    for stmt in body {
        match stmt {
            Statement::ImportDeclaration(decl) => {
                saw_import_export = true;
                // `import type ...` is erased; it does not force module format.
                if decl.import_kind.is_type() {
                    continue;
                }
                // A bare `import "x"` (no specifiers) is a value import. Otherwise
                // it's a value import iff at least one specifier is non-type.
                match &decl.specifiers {
                    None => return true,
                    Some(specs) => {
                        if specs.iter().any(|s| !specifier_is_type(s)) {
                            return true;
                        }
                    }
                }
            }
            Statement::ExportNamedDeclaration(decl) => {
                saw_import_export = true;
                if decl.export_kind.is_type() {
                    continue;
                }
                // `export const x = ...` (a declaration) or any non-type specifier
                // is a value export. `export {}` (the empty marker) carries module
                // syntax but no value binding — matched by the has_module_syntax
                // top-level-await fallthrough below, exactly like the old JS
                // (`se.entries.length === 0` counted as a value export there, but
                // the empty-export marker is stripped post-transpile, so treating
                // a lone `export {}` as the module-syntax/TLA case is equivalent —
                // both yield `module`).
                if decl.declaration.is_some()
                    || decl.specifiers.iter().any(|s| !s.export_kind.is_type())
                {
                    return true;
                }
                // A lone bare `export {}` (no declaration, no specifiers): value
                // export per the old JS `entries.length === 0` rule.
                if decl.declaration.is_none() && decl.specifiers.is_empty() {
                    return true;
                }
            }
            Statement::ExportDefaultDeclaration(_) => return true,
            Statement::ExportAllDeclaration(decl) => {
                saw_import_export = true;
                if !decl.export_kind.is_type() {
                    return true;
                }
            }
            _ => {}
        }
    }

    // Top-level await: `has_module_syntax` is set with no static import/export/meta
    // (import.meta already returned above). This is the JS TLA branch.
    if has_module_syntax && !saw_import_export {
        return true;
    }

    false
}

fn specifier_is_type(spec: &ImportDeclarationSpecifier<'_>) -> bool {
    use ImportDeclarationSpecifier as S;
    match spec {
        S::ImportSpecifier(s) => s.import_kind.is_type(),
        // default and namespace specifiers are always value bindings
        S::ImportDefaultSpecifier(_) | S::ImportNamespaceSpecifier(_) => false,
    }
}

/// Walks the AST once, latching two independent verdicts:
///   * `decorators` — a `@decorator` appears anywhere (drives the Stage-3 guard).
///   * `transformable` — the source contains target-version-gated SYNTAX oxc lowers
///     at `target:"es2022"`: a `using`/`await using` declaration, or a `v`-flag
///     RegExp literal. See `ModuleInfo::transformable_syntax` for the pinned
///     provenance of this set (oxc =0.132.0). Both fields keep going once latched
///     (the visit completes) — correctness only needs "did we ever see one".
struct SyntaxFinder {
    decorators: bool,
    transformable: bool,
}

impl<'a> Visit<'a> for SyntaxFinder {
    fn visit_decorator(&mut self, _it: &oxc::ast::ast::Decorator<'a>) {
        self.decorators = true;
    }

    fn visit_variable_declaration(&mut self, it: &VariableDeclaration<'a>) {
        // `using x = …` / `await using x = …` — ES2026 explicit resource management,
        // lowered at es2022 to the `usingCtx` helper shape. Unparseable on the floor.
        if it.kind.is_using() {
            self.transformable = true;
        }
        oxc::ast_visit::walk::walk_variable_declaration(self, it);
    }

    fn visit_reg_exp_literal(&mut self, it: &oxc::ast::ast::RegExpLiteral<'a>) {
        // `/…/v` — ES2024 unicode-sets RegExp, lowered at es2022 to `new RegExp(…)`.
        // The raw `v`-flag literal throws a SyntaxError on the 22.15 floor's V8.
        if it.regex.flags.contains(RegExpFlags::V) {
            self.transformable = true;
        }
    }
}
