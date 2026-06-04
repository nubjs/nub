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
    ast::ast::{ImportDeclarationSpecifier, Statement},
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
    if ret.panicked {
        return ModuleInfo { has_value_esm_syntax: false, has_decorators: false };
    }

    let has_value_esm_syntax = has_value_esm(
        &ret.program.body,
        ret.module_record.has_module_syntax,
        !ret.module_record.import_metas.is_empty(),
    );

    let mut decorators = DecoratorFinder { found: false };
    decorators.visit_program(&ret.program);

    ModuleInfo { has_value_esm_syntax, has_decorators: decorators.found }
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

/// Walks the AST looking for a single decorator. Stops conceptually at the first
/// (the visitor keeps going but `found` latches true).
struct DecoratorFinder {
    found: bool,
}

impl<'a> Visit<'a> for DecoratorFinder {
    fn visit_decorator(&mut self, _it: &oxc::ast::ast::Decorator<'a>) {
        self.found = true;
    }
}
