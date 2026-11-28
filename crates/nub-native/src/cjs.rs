//! `import`/`export` → CommonJS, as `tsc` emits it under `"module": "commonjs"`.
//!
//! oxc has no module transform (oxc-project/oxc#4050 is still open), and its
//! `Module::CommonJS` option only adds `"use strict"`. This pass runs after oxc's own
//! transforms, on a freshly built `Scoping`, so it sees plain JavaScript in which the
//! TypeScript pass has already dropped type-only and unused imports, exactly the
//! imports `tsc` elides.
//!
//! The emit mirrors `tsc` 5.9–7.0, verified against each, and
//! tests/ts-commonjs/corpus.json holds what an importer observes under `tsc` 5.9.3
//! for every form below:
//! - an import binds `const <module>_1 = require(…)`, wrapped in `__importDefault`
//!   or `__importStar` by the names it imports, as `tsc` picks them, when
//!   `esModuleInterop` is on; every reference becomes `<module>_1.name`, and a call
//!   `(0, <module>_1.name)(…)` so the callee is not bound to the module;
//! - `export const x = …` drops the local: every read and write of `x` becomes
//!   `exports.x`, so a test stubbing `exports.x` reaches the module's own calls;
//! - other exported locals (`export { x }`, functions, classes) keep the local and
//!   copy it to `exports` after its declaration, hoisted for a function;
//! - every write to an exported binding reaches every name it is exported as:
//!   `exports.alias = exports.x = x = v`, and after a destructuring assignment or a
//!   `for … in`/`of` head, a copy of each binding it wrote;
//! - re-exports from a module are getters, `export *` is `__exportStar`, and an
//!   imported binding re-exported with `export { x }` is exported straight after its
//!   `require()`;
//! - the export names are declared up front (`exports.a = exports.b = void 0`), so a
//!   local name shadows an `export *` of the same name, as in ESM.
//!
//! Where it departs from `tsc`:
//! - `tsc` leaves an export stale after a `for … in`/`of` head writes it; this pass
//!   copies it, as ESM's live binding would.
//! - A dynamic `import()` stays a real `import()`, so an ES-module-only package
//!   still loads below Node's `require(esm)`. `__importDynamic` hands back a module
//!   that ran as CommonJS (Node's facade for the `require.cache` entry), a builtin or
//!   JSON the way `tsc`'s `require()` would, and an ES module as its namespace.
//! - Top-level `await` and `import.meta`, which `tsc` emits verbatim into a file
//!   Node then rejects, are errors that name the file.
//! - A helper whose name the file already uses gets a fresh one (`tsc` rejects the
//!   file). Helpers are always inlined (`importHelpers` would make the file depend on
//!   `tslib` being installed).

use oxc::{
    allocator::{Allocator, ArenaVec, TakeIn},
    ast::{AstBuilder, NONE, ast::*},
    ast_visit::VisitMut,
    diagnostics::OxcDiagnostic,
    parser::Parser,
    semantic::{Scoping, SemanticBuilder, SymbolId},
    span::{GetSpan, GetSpanMut, SPAN, SourceType, Span},
    syntax::identifier::is_identifier_name,
};
use oxc_traverse::{Ancestor, Traverse, TraverseCtx, traverse_mut};
use rustc_hash::{FxHashMap, FxHashSet};

type Ctx<'a> = TraverseCtx<'a, ()>;

/// What `tsc` reads to shape the emit. `es_module_interop` is resolved in JS, where
/// the project's TypeScript version decides the default.
#[derive(Clone, Copy)]
pub struct CjsOptions {
    pub es_module_interop: bool,
}

/// Facts the TypeScript pass erases, read off the parsed program before it runs: a
/// file with only type imports is still a module to `tsc` (`"use strict"`,
/// `__esModule`), `export =` suppresses the `__esModule` marker, an
/// `export import x = require(…)` is not among the names declared up front, and
/// `import {} from "x"`, which the TypeScript pass turns into `import "x"`, is
/// dropped as `tsc` drops it.
#[derive(Clone, Default)]
pub struct ModuleShape {
    is_module: bool,
    has_export_assignment: bool,
    export_import_equals: Vec<String>,
    empty_imports: Vec<u32>,
}

pub fn module_shape(program: &Program<'_>) -> ModuleShape {
    let mut shape = ModuleShape::default();
    for stmt in &program.body {
        match stmt {
            Statement::ImportDeclaration(decl) => {
                shape.is_module = true;
                if decl.specifiers.as_ref().is_some_and(|s| s.is_empty()) {
                    shape.empty_imports.push(decl.span.start);
                }
            }
            Statement::ExportAllDeclaration(_) | Statement::ExportDefaultDeclaration(_) => {
                shape.is_module = true;
            }
            Statement::ExportNamedDeclaration(export) => {
                shape.is_module = true;
                if let Some(Declaration::TSImportEqualsDeclaration(decl)) = &export.declaration
                    && matches!(
                        decl.module_reference,
                        TSModuleReference::ExternalModuleReference(_)
                    )
                {
                    shape.export_import_equals.push(decl.id.name.to_string());
                }
            }
            Statement::TSExportAssignment(_) => {
                shape.is_module = true;
                shape.has_export_assignment = true;
            }
            Statement::TSImportEqualsDeclaration(decl)
                if matches!(
                    decl.module_reference,
                    TSModuleReference::ExternalModuleReference(_)
                ) =>
            {
                shape.is_module = true;
            }
            _ => {}
        }
    }
    shape
}

/// Rewrite `program` to CommonJS. Returns the scoping to hand on and any errors.
pub fn to_commonjs<'a>(
    allocator: &'a Allocator,
    program: &mut Program<'a>,
    shape: ModuleShape,
    options: CjsOptions,
) -> (Scoping, Vec<OxcDiagnostic>) {
    let scoping = SemanticBuilder::new()
        .build(program)
        .semantic
        .into_scoping();
    let mut pass = CjsTransform::new(shape, options);
    let scoping = traverse_mut(&mut pass, allocator, program, scoping, ());
    (scoping, pass.errors)
}

const CREATE_BINDING: &str = r#"var __createBinding = (this && this.__createBinding) || (Object.create ? (function(o, m, k, k2) {
    if (k2 === undefined) k2 = k;
    var desc = Object.getOwnPropertyDescriptor(m, k);
    if (!desc || ("get" in desc ? !m.__esModule : desc.writable || desc.configurable)) {
      desc = { enumerable: true, get: function() { return m[k]; } };
    }
    Object.defineProperty(o, k2, desc);
}) : (function(o, m, k, k2) {
    if (k2 === undefined) k2 = k;
    o[k2] = m[k];
}));"#;

const SET_MODULE_DEFAULT: &str = r#"var __setModuleDefault = (this && this.__setModuleDefault) || (Object.create ? (function(o, v) {
    Object.defineProperty(o, "default", { enumerable: true, value: v });
}) : function(o, v) {
    o["default"] = v;
});"#;

const IMPORT_STAR: &str = r#"var __importStar = (this && this.__importStar) || (function () {
    var ownKeys = function(o) {
        ownKeys = Object.getOwnPropertyNames || function (o) {
            var ar = [];
            for (var k in o) if (Object.prototype.hasOwnProperty.call(o, k)) ar[ar.length] = k;
            return ar;
        };
        return ownKeys(o);
    };
    return function (mod) {
        if (mod && mod.__esModule) return mod;
        var result = {};
        if (mod != null) for (var k = ownKeys(mod), i = 0; i < k.length; i++) if (k[i] !== "default") __createBinding(result, mod, k[i]);
        __setModuleDefault(result, mod);
        return result;
    };
})();"#;

const EXPORT_STAR: &str = r#"var __exportStar = (this && this.__exportStar) || function(m, exports) {
    for (var p in m) if (p !== "default" && !Object.prototype.hasOwnProperty.call(exports, p)) __createBinding(exports, m, p);
};"#;

const IMPORT_DEFAULT: &str = r#"var __importDefault = (this && this.__importDefault) || function (mod) {
    return (mod && mod.__esModule) ? mod : { "default": mod };
};"#;

/// `import(specifier)`, observed as `tsc`'s `require()` where the two load the same
/// file. With import attributes it is the real `import()`, untouched. Without, a
/// builtin is required. Otherwise the helper first asks the loader which URL the
/// import loads and in which format, with a request both hook tiers answer
/// (transform-core `importTargetQuestion`) and that without them is a module whose
/// default is null. When that URL is the file `require.resolve` names, a JSON file is
/// required, as `import()` would refuse it without an attribute, and a CommonJS module
/// comes back as its live `module.exports`. Anything else comes back as the import's
/// namespace, an ES module that `require()` cached under its `module.exports` export
/// included. The cache is read off `node:module`, since the `require` Node hands an
/// entry loaded with a hook's source has no `cache`. `WRAP` is `__importStar` under
/// `esModuleInterop`, else nothing.
const IMPORT_DYNAMIC: &str = r#"var __importDynamic = function (specifier, options) {
    if (arguments.length > 1) return import(specifier, options);
    var id;
    try { id = require.resolve(specifier); } catch (e) {}
    if (id === undefined) return import(specifier);
    var mod = require("node:module"), url = require("node:url");
    if (mod.isBuiltin(id)) return Promise.resolve().then(function () { return WRAP(require(id)); });
    var question = JSON.stringify([specifier, url.pathToFileURL(__filename).href]);
    return import("data:text/javascript,export default null//import-target:" + encodeURIComponent(question)).then(function (ns) {
        return ns.default;
    }, function () {
        return null;
    }).then(function (target) {
        var format = target && target.url === url.pathToFileURL(id).href ? target.format : null;
        if (format === "json") return WRAP(require(id));
        return import(specifier).then(function (ns) {
            var m = format === "commonjs" && mod._cache[id];
            return m && m.loaded ? WRAP(m.exports) : ns;
        });
    });
};"#;

/// The helpers the emit can inline, in the order `tsc` prints them.
#[derive(Clone, Copy)]
enum Helper {
    CreateBinding,
    SetModuleDefault,
    ImportStar,
    ExportStar,
    ImportDefault,
    ImportDynamic,
}

const HELPERS: [(&str, &str); 6] = [
    ("__createBinding", CREATE_BINDING),
    ("__setModuleDefault", SET_MODULE_DEFAULT),
    ("__importStar", IMPORT_STAR),
    ("__exportStar", EXPORT_STAR),
    ("__importDefault", IMPORT_DEFAULT),
    ("__importDynamic", IMPORT_DYNAMIC),
];

/// Where a write to an exported binding lands: the kept local, or `exports.x` for
/// an `export let x`, whose local is gone.
#[derive(Clone, Copy)]
enum Target<'a> {
    Local(&'a str),
    Exports(&'a str),
}

/// A binding a write touched, and the export names to copy it to afterwards.
struct ExportCopy<'a> {
    symbol: SymbolId,
    target: Target<'a>,
    names: Vec<&'a str>,
}

/// One exported local that keeps its binding: the names it is exported as.
struct LiveLocal<'a> {
    symbol: SymbolId,
    names: Vec<&'a str>,
}

struct CjsTransform<'a> {
    shape: ModuleShape,
    interop: bool,
    /// Every name the file already uses, plus the ones this pass generated.
    taken: FxHashSet<String>,
    counters: FxHashMap<String, u32>,
    /// Import specifier → (module binding, imported name).
    imports: FxHashMap<SymbolId, (&'a str, &'a str)>,
    namespace_imports: FxHashSet<SymbolId>,
    /// The module binding each `import` statement gets, by statement index.
    import_bindings: Vec<Option<&'a str>>,
    /// `export const x` locals, which become `exports.x` everywhere.
    exported_vars: FxHashMap<SymbolId, &'a str>,
    /// The other names an `export let x` is exported as (`export { x as alias }`).
    var_aliases: FxHashMap<SymbolId, Vec<&'a str>>,
    live: Vec<LiveLocal<'a>>,
    live_index: FxHashMap<SymbolId, usize>,
    /// The import statement that binds each imported local, by statement index.
    import_statement: FxHashMap<SymbolId, usize>,
    /// `export { x }` of an imported `x`, emitted after that import's `require()`.
    after_import: FxHashMap<usize, Vec<String>>,
    function_depth: u32,
    /// Temporaries a postfix update needs, declared as one `var` at the top.
    temps: Vec<&'a str>,
    /// Each helper's name in this file, once something uses it.
    helpers: [Option<&'a str>; HELPERS.len()],
    errors: Vec<OxcDiagnostic>,
}

impl<'a> CjsTransform<'a> {
    fn new(shape: ModuleShape, options: CjsOptions) -> Self {
        Self {
            shape,
            interop: options.es_module_interop,
            taken: FxHashSet::default(),
            counters: FxHashMap::default(),
            imports: FxHashMap::default(),
            namespace_imports: FxHashSet::default(),
            import_bindings: Vec::new(),
            exported_vars: FxHashMap::default(),
            var_aliases: FxHashMap::default(),
            live: Vec::new(),
            live_index: FxHashMap::default(),
            import_statement: FxHashMap::default(),
            after_import: FxHashMap::default(),
            function_depth: 0,
            temps: Vec::new(),
            helpers: [None; HELPERS.len()],
            errors: Vec::new(),
        }
    }

    /// `tsc`'s generated name for a module binding: the specifier's last path
    /// segment with every non-word character replaced (`node:path` → `node_path`),
    /// then `_1`, `_2`, … until it collides with nothing in the file.
    fn unique(&mut self, base: &str, ast: AstBuilder<'a>) -> &'a str {
        let base = if base.is_empty() { "module" } else { base };
        let mut n = self.counters.get(base).copied().unwrap_or(0);
        loop {
            n += 1;
            let candidate = format!("{base}_{n}");
            if self.taken.insert(candidate.clone()) {
                self.counters.insert(base.to_string(), n);
                return ast.allocator.alloc_str(&candidate);
            }
        }
    }

    /// `_a`, `_b`, … as `tsc` names its temporaries, skipping names the file uses.
    fn temp(&mut self, ast: AstBuilder<'a>) -> &'a str {
        let name = (b'a'..=b'z')
            .map(|c| format!("_{}", c as char))
            .find(|name| !self.taken.contains(name));
        let name = match name {
            Some(name) => {
                self.taken.insert(name.clone());
                ast.allocator.alloc_str(&name)
            }
            None => self.unique("_tmp", ast),
        };
        self.temps.push(name);
        name
    }

    /// The helper's name, `__importStar` unless the file already uses it, and the
    /// helpers it calls.
    fn helper(&mut self, helper: Helper, ast: AstBuilder<'a>) -> &'a str {
        if let Some(name) = self.helpers[helper as usize] {
            return name;
        }
        let canonical = HELPERS[helper as usize].0;
        let name = if self.taken.insert(canonical.to_string()) {
            ast.allocator.alloc_str(canonical)
        } else {
            self.unique(canonical, ast)
        };
        self.helpers[helper as usize] = Some(name);
        match helper {
            Helper::ImportStar => {
                self.helper(Helper::CreateBinding, ast);
                self.helper(Helper::SetModuleDefault, ast);
            }
            Helper::ExportStar => {
                self.helper(Helper::CreateBinding, ast);
            }
            Helper::ImportDynamic if self.interop => {
                self.helper(Helper::ImportStar, ast);
            }
            _ => {}
        }
        name
    }

    /// The used helpers' source, each under the name `helper` gave it.
    fn helper_source(&self) -> String {
        let mut out = String::new();
        for (i, &(_, text)) in HELPERS.iter().enumerate() {
            if self.helpers[i].is_none() {
                continue;
            }
            let mut text = text.to_string();
            if i == Helper::ImportDynamic as usize {
                // The canonical name, which the renaming below then maps.
                let wrap = if self.interop {
                    HELPERS[Helper::ImportStar as usize].0
                } else {
                    ""
                };
                text = text.replace("WRAP", wrap);
            }
            for (j, &(canonical, _)) in HELPERS.iter().enumerate() {
                if let Some(name) = self.helpers[j]
                    && name != canonical
                {
                    text = text.replace(canonical, name);
                }
            }
            out.push_str(&text);
            out.push('\n');
        }
        out
    }

    fn add_live(&mut self, symbol: SymbolId, name: &'a str) {
        if let Some(&i) = self.live_index.get(&symbol) {
            self.live[i].names.push(name);
        } else {
            self.live_index.insert(symbol, self.live.len());
            self.live.push(LiveLocal {
                symbol,
                names: vec![name],
            });
        }
    }

    fn error(&mut self, message: &str, span: Span) {
        self.errors
            .push(OxcDiagnostic::error(message.to_string()).with_label(span));
    }
}

fn module_base_name(specifier: &str) -> String {
    let normalized = specifier.replace('\\', "/");
    let trimmed = normalized.trim_end_matches('/');
    let base = trimmed.rsplit('/').next().unwrap_or(trimmed);
    let mut out = String::with_capacity(base.len() + 1);
    for (i, c) in base.chars().enumerate() {
        if i == 0 && c.is_ascii_digit() {
            out.push('_');
        }
        out.push(if c.is_ascii_alphanumeric() || c == '_' {
            c
        } else {
            '_'
        });
    }
    out
}

fn reference_symbol(ident: &IdentifierReference<'_>, ctx: &Ctx<'_>) -> Option<SymbolId> {
    ident
        .reference_id
        .get()
        .and_then(|id| ctx.scoping().get_reference(id).symbol_id())
}

fn binding_symbol(ident: &BindingIdentifier<'_>) -> Option<SymbolId> {
    ident.symbol_id.get()
}

fn collect_bound<'p, 'a>(
    pattern: &'p BindingPattern<'a>,
    out: &mut Vec<&'p BindingIdentifier<'a>>,
) {
    match pattern {
        BindingPattern::BindingIdentifier(id) => out.push(id),
        BindingPattern::ObjectPattern(obj) => {
            for prop in &obj.properties {
                collect_bound(&prop.value, out);
            }
            if let Some(rest) = &obj.rest {
                collect_bound(&rest.argument, out);
            }
        }
        BindingPattern::ArrayPattern(arr) => {
            for elem in arr.elements.iter().flatten() {
                collect_bound(elem, out);
            }
            if let Some(rest) = &arr.rest {
                collect_bound(&rest.argument, out);
            }
        }
        BindingPattern::AssignmentPattern(assign) => collect_bound(&assign.left, out),
    }
}

/// The symbols a top-level statement declares for module scope.
fn declared_symbols(stmt: &Statement<'_>, out: &mut Vec<SymbolId>) {
    let mut ids = Vec::new();
    let from_declaration = |decl: &Declaration<'_>, ids: &mut Vec<SymbolId>| match decl {
        Declaration::VariableDeclaration(var) => {
            let mut bound = Vec::new();
            for d in &var.declarations {
                collect_bound(&d.id, &mut bound);
            }
            ids.extend(bound.iter().filter_map(|id| binding_symbol(id)));
        }
        Declaration::FunctionDeclaration(f) => {
            ids.extend(f.id.as_ref().and_then(binding_symbol));
        }
        Declaration::ClassDeclaration(c) => {
            ids.extend(c.id.as_ref().and_then(binding_symbol));
        }
        _ => {}
    };
    match stmt {
        Statement::VariableDeclaration(_)
        | Statement::FunctionDeclaration(_)
        | Statement::ClassDeclaration(_) => {
            if let Some(decl) = stmt.as_declaration() {
                from_declaration(decl, &mut ids);
            }
        }
        Statement::ExportNamedDeclaration(export) => {
            if let Some(decl) = &export.declaration {
                from_declaration(decl, &mut ids);
            }
        }
        Statement::ExportDefaultDeclaration(export) => match &export.declaration {
            ExportDefaultDeclarationKind::FunctionDeclaration(f) => {
                ids.extend(f.id.as_ref().and_then(binding_symbol));
            }
            ExportDefaultDeclarationKind::ClassDeclaration(c) => {
                ids.extend(c.id.as_ref().and_then(binding_symbol));
            }
            _ => {}
        },
        _ => {}
    }
    out.extend(ids);
}

/// `object.name`, or `object["name"]` for a name that is not an identifier.
fn member_text(object: &str, name: &str) -> String {
    if is_identifier_name(name) {
        format!("{object}.{name}")
    } else {
        format!("{object}[{}]", js_string(name))
    }
}

fn js_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

fn member_expression<'a>(
    span: Span,
    object: &'a str,
    name: &'a str,
    ast: AstBuilder<'a>,
) -> MemberExpression<'a> {
    let object = Expression::new_identifier(SPAN, object, &ast);
    if is_identifier_name(name) {
        MemberExpression::new_static_member_expression(
            span,
            object,
            IdentifierName::new(SPAN, name, &ast),
            false,
            &ast,
        )
    } else {
        let key = Expression::new_string_literal(SPAN, name, None, &ast);
        MemberExpression::new_computed_member_expression(span, object, key, false, &ast)
    }
}

/// `(0, callee)` where the reference is called, so the module is not `this`.
fn unbind_if_called<'a>(expr: Expression<'a>, ctx: &Ctx<'a>) -> Expression<'a> {
    if matches!(
        ctx.parent(),
        Ancestor::CallExpressionCallee(_) | Ancestor::TaggedTemplateExpressionTag(_)
    ) {
        let span = expr.span();
        let zero = Expression::new_numeric_literal(SPAN, 0.0, None, NumberBase::Decimal, ctx);
        Expression::new_sequence_expression(span, ArenaVec::from_array_in([zero, expr], ctx), ctx)
    } else {
        expr
    }
}

/// Points every span of a generated statement at the first byte of the code it
/// replaces. Codegen maps a node through its span, and a one-byte span sits
/// inside no comment range, so no source comment is printed twice.
struct PinSpans(Span);

impl<'a> VisitMut<'a> for PinSpans {
    fn visit_span(&mut self, span: &mut Span) {
        *span = self.0;
    }
}

/// Codegen records the one-byte slice under a pinned identifier as its original
/// name, and Node prints a mapping's name as a stack frame's function name. The
/// pass renames nothing it keeps a span for, so the output's names are dropped.
pub fn drop_names(map: oxc_sourcemap::SourceMap<'_>) -> oxc_sourcemap::SourceMap<'_> {
    let mut parts = map.into_parts();
    parts.names.clear();
    parts.token_chunks = None;
    parts.tokens = parts
        .tokens
        .iter()
        .map(|t| {
            oxc_sourcemap::Token::new(
                t.get_dst_line(),
                t.get_dst_col(),
                t.get_src_line(),
                t.get_src_col(),
                t.get_source_id(),
                None,
            )
        })
        .collect();
    oxc_sourcemap::SourceMap::from_parts(parts)
}

/// Parse generated statements into the program's arena. Their own spans would
/// index the snippet, not the source, so each is pinned to `span`, which ties the
/// generated lines to the code they came from in the source map.
fn parse_statements<'a>(
    text: &str,
    span: Span,
    ast: AstBuilder<'a>,
) -> ArenaVec<'a, Statement<'a>> {
    let text = ast.allocator.alloc_str(text);
    let ret = Parser::new(ast.allocator, text, SourceType::cjs()).parse();
    let mut program = ret.program;
    let pin = if span.is_empty() {
        SPAN
    } else {
        Span::new(span.start, span.start + 1)
    };
    PinSpans(pin).visit_program(&mut program);
    let mut body = program.body;
    if let Some(first) = body.first_mut() {
        *first.span_mut() = span;
    }
    body
}

impl<'a> Traverse<'a, ()> for CjsTransform<'a> {
    fn enter_program(&mut self, program: &mut Program<'a>, ctx: &mut Ctx<'a>) {
        let scoping = ctx.scoping();
        self.taken
            .extend(scoping.symbol_names().map(str::to_string));
        self.taken.extend(
            scoping
                .root_unresolved_references()
                .keys()
                .map(|k| k.as_str().to_string()),
        );
        self.import_bindings = vec![None; program.body.len()];

        // Imports first: `export { x }` may come before the `import` that binds `x`.
        for (i, stmt) in program.body.iter().enumerate() {
            let Statement::ImportDeclaration(decl) = stmt else {
                continue;
            };
            if let Some(phase) = decl.phase {
                let what = if matches!(phase, ImportPhase::Source) {
                    "source"
                } else {
                    "defer"
                };
                self.error(
                    &format!(
                        "`import {what}` cannot be compiled to CommonJS. This file compiles to CommonJS because its tsconfig.json sets \"module\": \"commonjs\"."
                    ),
                    decl.span,
                );
                continue;
            }
            let Some(specifiers) = &decl.specifiers else {
                continue;
            };
            if specifiers.is_empty() {
                continue;
            }
            let namespace = specifiers.iter().find_map(|s| match s {
                ImportDeclarationSpecifier::ImportNamespaceSpecifier(ns) => Some(&ns.local),
                _ => None,
            });
            let binding: &'a str = match namespace {
                Some(local) => {
                    if let Some(symbol) = binding_symbol(local) {
                        self.namespace_imports.insert(symbol);
                        self.import_statement.insert(symbol, i);
                    }
                    local.name.as_str()
                }
                None => {
                    let base = module_base_name(decl.source.value.as_str());
                    self.unique(&base, ctx.ast)
                }
            };
            for spec in specifiers {
                let (local, imported): (&BindingIdentifier<'a>, &'a str) = match spec {
                    ImportDeclarationSpecifier::ImportSpecifier(s) => {
                        (&s.local, s.imported.name().as_str())
                    }
                    ImportDeclarationSpecifier::ImportDefaultSpecifier(s) => (&s.local, "default"),
                    ImportDeclarationSpecifier::ImportNamespaceSpecifier(_) => continue,
                };
                if let Some(symbol) = binding_symbol(local) {
                    self.imports.insert(symbol, (binding, imported));
                    self.import_statement.insert(symbol, i);
                }
            }
            self.import_bindings[i] = Some(binding);
        }

        for stmt in &program.body {
            match stmt {
                Statement::ExportNamedDeclaration(decl) if decl.source.is_none() => {
                    if let Some(declaration) = &decl.declaration {
                        match declaration {
                            Declaration::VariableDeclaration(var) => {
                                for d in &var.declarations {
                                    if let BindingPattern::BindingIdentifier(id) = &d.id {
                                        if let Some(symbol) = binding_symbol(id) {
                                            self.exported_vars.insert(symbol, id.name.as_str());
                                        }
                                    } else {
                                        let mut bound = Vec::new();
                                        collect_bound(&d.id, &mut bound);
                                        for id in bound {
                                            if let Some(symbol) = binding_symbol(id) {
                                                self.add_live(symbol, id.name.as_str());
                                            }
                                        }
                                    }
                                }
                            }
                            Declaration::FunctionDeclaration(f) => {
                                if let Some(id) = &f.id
                                    && let Some(symbol) = binding_symbol(id)
                                {
                                    self.add_live(symbol, id.name.as_str());
                                }
                            }
                            Declaration::ClassDeclaration(c) => {
                                if let Some(id) = &c.id
                                    && let Some(symbol) = binding_symbol(id)
                                {
                                    self.add_live(symbol, id.name.as_str());
                                }
                            }
                            _ => {}
                        }
                    }
                }
                Statement::ExportDefaultDeclaration(decl) => {
                    let id = match &decl.declaration {
                        ExportDefaultDeclarationKind::FunctionDeclaration(f) => f.id.as_ref(),
                        ExportDefaultDeclarationKind::ClassDeclaration(c) => c.id.as_ref(),
                        _ => None,
                    };
                    if let Some(symbol) = id.and_then(binding_symbol) {
                        self.add_live(symbol, "default");
                    }
                }
                _ => {}
            }
        }

        // `export { x }` lists, after every declaration-site export is known.
        for stmt in &program.body {
            let Statement::ExportNamedDeclaration(decl) = stmt else {
                continue;
            };
            if decl.source.is_some() || decl.declaration.is_some() {
                continue;
            }
            for spec in &decl.specifiers {
                let ModuleExportName::IdentifierReference(local) = &spec.local else {
                    continue;
                };
                let Some(symbol) = reference_symbol(local, ctx) else {
                    continue;
                };
                let exported = spec.exported.name().as_str();
                if let Some(&at) = self.import_statement.get(&symbol) {
                    // `tsc` re-exports a named import live, a default import or a
                    // namespace by value, straight after the import binds it.
                    let line = match self.imports.get(&symbol) {
                        Some(&(binding, imported)) if imported != "default" => format!(
                            "Object.defineProperty(exports, {}, {{ enumerable: true, get: function () {{ return {}; }} }});",
                            js_string(exported),
                            member_text(binding, imported)
                        ),
                        Some(&(binding, imported)) => format!(
                            "{} = {};",
                            member_text("exports", exported),
                            member_text(binding, imported)
                        ),
                        None => format!(
                            "{} = {};",
                            member_text("exports", exported),
                            local.name.as_str()
                        ),
                    };
                    self.after_import.entry(at).or_default().push(line);
                    continue;
                }
                if let Some(&primary) = self.exported_vars.get(&symbol) {
                    if exported != primary {
                        self.var_aliases.entry(symbol).or_default().push(exported);
                    }
                    continue;
                }
                self.add_live(symbol, exported);
            }
        }
    }

    fn enter_function(&mut self, _: &mut Function<'a>, _: &mut Ctx<'a>) {
        self.function_depth += 1;
    }

    fn exit_function(&mut self, _: &mut Function<'a>, _: &mut Ctx<'a>) {
        self.function_depth -= 1;
    }

    fn enter_arrow_function_expression(
        &mut self,
        _: &mut ArrowFunctionExpression<'a>,
        _: &mut Ctx<'a>,
    ) {
        self.function_depth += 1;
    }

    fn exit_arrow_function_expression(
        &mut self,
        _: &mut ArrowFunctionExpression<'a>,
        _: &mut Ctx<'a>,
    ) {
        self.function_depth -= 1;
    }

    fn enter_for_of_statement(&mut self, stmt: &mut ForOfStatement<'a>, _: &mut Ctx<'a>) {
        if stmt.r#await && self.function_depth == 0 {
            self.error(TOP_LEVEL_AWAIT, stmt.span);
        }
    }

    fn enter_expression(&mut self, expr: &mut Expression<'a>, ctx: &mut Ctx<'a>) {
        match expr {
            Expression::Identifier(ident) => {
                let Some(symbol) = reference_symbol(ident, ctx) else {
                    return;
                };
                let span = ident.span;
                let member = if let Some(&(binding, imported)) = self.imports.get(&symbol) {
                    member_expression(span, binding, imported, ctx.ast)
                } else if let Some(&name) = self.exported_vars.get(&symbol) {
                    member_expression(span, "exports", name, ctx.ast)
                } else {
                    return;
                };
                *expr = unbind_if_called(Expression::from(member), ctx);
            }
            Expression::MetaProperty(meta) if meta.meta.name == "import" => {
                let span = meta.span;
                self.error(IMPORT_META, span);
            }
            Expression::AwaitExpression(await_expr) if self.function_depth == 0 => {
                let span = await_expr.span;
                self.error(TOP_LEVEL_AWAIT, span);
            }
            _ => {}
        }
    }

    fn exit_expression(&mut self, expr: &mut Expression<'a>, ctx: &mut Ctx<'a>) {
        match expr {
            Expression::ImportExpression(import) if import.phase.is_none() => {
                let helper = self.helper(Helper::ImportDynamic, ctx.ast);
                let span = import.span;
                let mut arguments = ArenaVec::with_capacity_in(2, &ctx.ast);
                arguments.push(Argument::from(import.source.take_in(&ctx.ast)));
                if let Some(options) = import.options.take() {
                    arguments.push(Argument::from(options));
                }
                *expr = Expression::new_call_expression(
                    span,
                    Expression::new_identifier(SPAN, helper, ctx),
                    NONE,
                    arguments,
                    false,
                    ctx,
                );
            }
            Expression::AssignmentExpression(assign) => {
                let simple = matches!(assign.left, AssignmentTarget::AssignmentTargetIdentifier(_));
                let mut copies = Vec::new();
                self.rewrite_target(&mut assign.left, &mut copies, ctx);
                if copies.is_empty() {
                    return;
                }
                let span = assign.span;
                if simple {
                    // `exports.alias = exports.x = x = v` keeps the expression's value.
                    let names = copies.pop().map(|c| c.names).unwrap_or_default();
                    let mut value = expr.take_in(&ctx.ast);
                    for name in names.iter().rev() {
                        value = assign_to_exports(span, name, value, ctx.ast);
                    }
                    *expr = value;
                    return;
                }
                // A destructuring assignment: copy each binding it wrote afterwards,
                // keeping its value (the right-hand side) where something reads it.
                let temp = (!value_unused(ctx)).then(|| self.temp(ctx.ast));
                let mut value = expr.take_in(&ctx.ast);
                if let Some(temp) = temp {
                    value = assign_to_local(span, temp, value, ctx.ast);
                }
                let mut items = ArenaVec::with_capacity_in(copies.len() + 2, &ctx.ast);
                items.push(value);
                for copy in &copies {
                    items.push(copy_expression(copy, ctx.ast));
                }
                if let Some(temp) = temp {
                    items.push(Expression::new_identifier(SPAN, temp, ctx));
                }
                *expr = Expression::new_sequence_expression(span, items, ctx);
            }
            Expression::UpdateExpression(update) => {
                let SimpleAssignmentTarget::AssignmentTargetIdentifier(id) = &update.argument
                else {
                    return;
                };
                let Some(symbol) = reference_symbol(id, ctx) else {
                    return;
                };
                let Some((target, names)) = self.write_plan(symbol, id.name.as_str()) else {
                    return;
                };
                if let Target::Exports(name) = target {
                    let member = member_expression(id.span, "exports", name, ctx.ast);
                    update.argument = SimpleAssignmentTarget::from(member);
                }
                if names.is_empty() {
                    return;
                }
                let span = update.span;
                let prefix = update.prefix;
                // A postfix update copies the new value to `exports`, and where its
                // own value is read, a temporary keeps the old one, as `tsc` does:
                // `(exports.x = (_a = x++, x), _a)`.
                let temp = (!prefix && !value_unused(ctx)).then(|| self.temp(ctx.ast));
                let mut original = expr.take_in(&ctx.ast);
                if let Some(temp) = temp {
                    original = assign_to_local(span, temp, original, ctx.ast);
                }
                let mut value = if prefix {
                    original
                } else {
                    Expression::new_sequence_expression(
                        span,
                        ArenaVec::from_array_in([original, read_target(target, ctx.ast)], ctx),
                        ctx,
                    )
                };
                for name in names.iter().rev() {
                    value = assign_to_exports(span, name, value, ctx.ast);
                }
                if let Some(temp) = temp {
                    let read = Expression::new_identifier(SPAN, temp, ctx);
                    value = Expression::new_sequence_expression(
                        span,
                        ArenaVec::from_array_in([value, read], ctx),
                        ctx,
                    );
                }
                *expr = value;
            }
            _ => {}
        }
    }

    fn exit_for_in_statement(&mut self, stmt: &mut ForInStatement<'a>, ctx: &mut Ctx<'a>) {
        self.copy_after_loop_head(&mut stmt.left, &mut stmt.body, ctx);
    }

    fn exit_for_of_statement(&mut self, stmt: &mut ForOfStatement<'a>, ctx: &mut Ctx<'a>) {
        self.copy_after_loop_head(&mut stmt.left, &mut stmt.body, ctx);
    }

    fn exit_program(&mut self, program: &mut Program<'a>, ctx: &mut Ctx<'a>) {
        let ast = ctx.ast;
        if !self.shape.is_module {
            let helpers = self.helper_source();
            if !helpers.is_empty() {
                program
                    .body
                    .splice(0..0, parse_statements(&helpers, SPAN, ast));
            }
            return;
        }

        let old = program.body.take_in(&ast);

        // Where each kept local is copied to `exports`: hoisted for a function, else
        // straight after the top-level statement that declares it.
        let mut declared_at: FxHashMap<SymbolId, usize> = FxHashMap::default();
        for (i, stmt) in old.iter().enumerate() {
            let mut symbols = Vec::new();
            declared_symbols(stmt, &mut symbols);
            for symbol in symbols {
                declared_at.entry(symbol).or_insert(i);
            }
        }
        let scoping = ctx.scoping();
        let statement_declaring = |symbol: SymbolId| -> Option<usize> {
            declared_at.get(&symbol).copied().or_else(|| {
                let span = scoping.symbol_span(symbol);
                if span.is_empty() {
                    return None;
                }
                old.iter()
                    .position(|s| s.span().start <= span.start && span.end <= s.span().end)
            })
        };
        let mut hoisted: Vec<String> = Vec::new();
        let mut after: FxHashMap<usize, Vec<String>> = FxHashMap::default();
        let mut trailing: Vec<String> = Vec::new();
        let mut function_exports: FxHashSet<&'a str> = FxHashSet::default();
        for live in &self.live {
            let local = scoping.symbol_name(live.symbol);
            let is_function = scoping.symbol_flags(live.symbol).is_function();
            for &name in &live.names {
                let line = format!("{} = {local};", member_text("exports", name));
                if is_function {
                    function_exports.insert(name);
                    hoisted.push(line);
                } else if let Some(i) = statement_declaring(live.symbol) {
                    after.entry(i).or_default().push(line);
                } else {
                    trailing.push(line);
                }
            }
        }

        let mut void_names: Vec<&'a str> = Vec::new();
        let mut body = ArenaVec::with_capacity_in(old.len() * 2, &ast);
        for (i, stmt) in old.into_iter().enumerate() {
            match stmt {
                Statement::ImportDeclaration(decl) => {
                    let span = decl.span;
                    let source = js_string(decl.source.value.as_str());
                    let mut text = match (&decl.specifiers, self.import_bindings[i]) {
                        (None, _) if self.shape.empty_imports.contains(&span.start) => {
                            String::new()
                        }
                        (Some(specifiers), Some(binding)) if !specifiers.is_empty() => {
                            let require = format!("require({source})");
                            let init = match self.interop.then(|| import_helper(specifiers)) {
                                Some(Some(helper)) => {
                                    format!("{}({require})", self.helper(helper, ast))
                                }
                                _ => require,
                            };
                            format!("const {binding} = {init};")
                        }
                        _ => format!("require({source});"),
                    };
                    if let Some(lines) = self.after_import.remove(&i) {
                        for line in lines {
                            text.push('\n');
                            text.push_str(&line);
                        }
                    }
                    body.extend(parse_statements(&text, span, ast));
                }
                Statement::ExportAllDeclaration(decl) => {
                    let span = decl.span;
                    let require = format!("require({})", js_string(decl.source.value.as_str()));
                    let text = if let Some(exported) = &decl.exported {
                        let name = exported.name().as_str();
                        void_names.push(name);
                        let value = if self.interop {
                            format!("{}({require})", self.helper(Helper::ImportStar, ast))
                        } else {
                            require
                        };
                        format!("{} = {value};", member_text("exports", name))
                    } else {
                        format!(
                            "{}({require}, exports);",
                            self.helper(Helper::ExportStar, ast)
                        )
                    };
                    body.extend(parse_statements(&text, span, ast));
                }
                Statement::ExportNamedDeclaration(decl) => {
                    let decl = decl.unbox();
                    let span = decl.span;
                    if let Some(source) = &decl.source {
                        let binding = self.unique(&module_base_name(source.value.as_str()), ast);
                        let mut text = format!(
                            "var {binding} = require({});\n",
                            js_string(source.value.as_str())
                        );
                        for spec in &decl.specifiers {
                            let local = spec.local.name().as_str();
                            let exported = spec.exported.name().as_str();
                            void_names.push(exported);
                            let value = if local == "default" && self.interop {
                                let helper = self.helper(Helper::ImportDefault, ast);
                                format!("{helper}({binding}).default")
                            } else {
                                member_text(binding, local)
                            };
                            text.push_str(&format!(
                                "Object.defineProperty(exports, {}, {{ enumerable: true, get: function () {{ return {value}; }} }});\n",
                                js_string(exported)
                            ));
                        }
                        body.extend(parse_statements(&text, span, ast));
                    } else if let Some(declaration) = decl.declaration {
                        match declaration {
                            Declaration::VariableDeclaration(var) => {
                                let var = var.unbox();
                                let kind = var.kind;
                                for d in var.declarations {
                                    let d_span = d.span;
                                    match d.id {
                                        BindingPattern::BindingIdentifier(id) => {
                                            let name = id.name.as_str();
                                            if !self
                                                .shape
                                                .export_import_equals
                                                .iter()
                                                .any(|n| n == name)
                                            {
                                                void_names.push(name);
                                            }
                                            if let Some(init) = d.init {
                                                let value =
                                                    assign_to_exports(d_span, name, init, ast);
                                                body.push(Statement::new_expression_statement(
                                                    d_span, value, &ast,
                                                ));
                                            }
                                        }
                                        pattern => {
                                            let mut bound = Vec::new();
                                            collect_bound(&pattern, &mut bound);
                                            void_names
                                                .extend(bound.iter().map(|id| id.name.as_str()));
                                            let declarator = VariableDeclarator::new(
                                                d_span, kind, pattern, NONE, d.init, false, &ast,
                                            );
                                            let declaration = Declaration::new_variable_declaration(
                                                d_span,
                                                kind,
                                                ArenaVec::from_value_in(declarator, &ast),
                                                false,
                                                &ast,
                                            );
                                            body.push(Statement::from(declaration));
                                        }
                                    }
                                }
                            }
                            Declaration::ClassDeclaration(class) => {
                                if let Some(id) = &class.id {
                                    void_names.push(id.name.as_str());
                                }
                                body.push(Statement::ClassDeclaration(class));
                            }
                            other => body.push(Statement::from(other)),
                        }
                    } else {
                        for spec in &decl.specifiers {
                            let ModuleExportName::IdentifierReference(local) = &spec.local else {
                                continue;
                            };
                            let Some(symbol) = reference_symbol(local, ctx) else {
                                continue;
                            };
                            let exported = spec.exported.name().as_str();
                            if self.import_statement.contains_key(&symbol) {
                                // Exported after the import's `require()`, in `after_import`.
                                void_names.push(exported);
                            } else if let Some(&var_name) = self.exported_vars.get(&symbol) {
                                void_names.push(exported);
                                let line = format!(
                                    "{} = {};",
                                    member_text("exports", exported),
                                    member_text("exports", var_name)
                                );
                                match statement_declaring_index(&declared_at, symbol) {
                                    Some(at) if at > i => after.entry(at).or_default().push(line),
                                    _ => body.extend(parse_statements(&line, spec.span, ast)),
                                }
                            } else if !function_exports.contains(exported) {
                                void_names.push(exported);
                            }
                        }
                    }
                }
                Statement::ExportDefaultDeclaration(decl) => {
                    let decl = decl.unbox();
                    let span = decl.span;
                    match decl.declaration {
                        ExportDefaultDeclarationKind::FunctionDeclaration(mut func) => {
                            if func.id.is_none() {
                                let name = self.unique("default", ast);
                                func.id = Some(BindingIdentifier::new(SPAN, name, &ast));
                                hoisted.push(format!("exports.default = {name};"));
                            }
                            func.r#type = FunctionType::FunctionDeclaration;
                            body.push(Statement::FunctionDeclaration(func));
                        }
                        ExportDefaultDeclarationKind::ClassDeclaration(mut class) => {
                            let anonymous = class.id.is_none();
                            if anonymous {
                                let name = self.unique("default", ast);
                                class.id = Some(BindingIdentifier::new(SPAN, name, &ast));
                            }
                            class.r#type = ClassType::ClassDeclaration;
                            let name = class
                                .id
                                .as_ref()
                                .map(|id| id.name.as_str())
                                .unwrap_or_default();
                            body.push(Statement::ClassDeclaration(class));
                            if anonymous {
                                body.extend(parse_statements(
                                    &format!("exports.default = {name};"),
                                    SPAN,
                                    ast,
                                ));
                            }
                        }
                        ExportDefaultDeclarationKind::TSInterfaceDeclaration(_) => {}
                        kind => {
                            let value = kind.into_expression();
                            let value = assign_to_exports(span, "default", value, ast);
                            body.push(Statement::new_expression_statement(span, value, &ast));
                        }
                    }
                }
                other => body.push(other),
            }
            if let Some(lines) = after.remove(&i) {
                body.extend(parse_statements(&lines.join("\n"), SPAN, ast));
            }
        }
        for line in trailing {
            body.extend(parse_statements(&line, SPAN, ast));
        }

        let mut preamble = String::new();
        if !self.temps.is_empty() {
            preamble.push_str(&format!("var {};\n", self.temps.join(", ")));
        }
        preamble.push_str(&self.helper_source());
        if !self.shape.has_export_assignment {
            preamble.push_str("Object.defineProperty(exports, \"__esModule\", { value: true });\n");
        }
        let mut seen = FxHashSet::default();
        let names: Vec<&str> = void_names
            .iter()
            .rev()
            .copied()
            .filter(|name| !function_exports.contains(name) && seen.insert(*name))
            .collect();
        // `tsc` splits the chain every 50 names to bound the expression's depth.
        for chunk in names.chunks(50) {
            for name in chunk {
                preamble.push_str(&member_text("exports", name));
                preamble.push_str(" = ");
            }
            preamble.push_str("void 0;\n");
        }
        for line in &hoisted {
            preamble.push_str(line);
            preamble.push('\n');
        }
        let mut statements = parse_statements(&preamble, SPAN, ast);
        statements.extend(body);
        program.body = statements;

        if !program.directives.iter().any(Directive::is_use_strict) {
            program
                .directives
                .insert(0, Directive::new_use_strict(&ast));
        }
    }
}

impl<'a> CjsTransform<'a> {
    /// Where a write to `symbol` (spelled `local`) lands, and the export names to
    /// copy it to: every name for a kept local, the aliases for an `export let`.
    fn write_plan(&self, symbol: SymbolId, local: &'a str) -> Option<(Target<'a>, Vec<&'a str>)> {
        if let Some(&name) = self.exported_vars.get(&symbol) {
            let aliases = self.var_aliases.get(&symbol).cloned().unwrap_or_default();
            return Some((Target::Exports(name), aliases));
        }
        let &i = self.live_index.get(&symbol)?;
        Some((Target::Local(local), self.live[i].names.clone()))
    }

    /// Points each `export let` binding `target` writes at `exports.x`, and lists the
    /// bindings whose export names need a copy afterwards. Walks destructuring
    /// patterns, defaults and rest elements included.
    fn rewrite_target(
        &self,
        target: &mut AssignmentTarget<'a>,
        copies: &mut Vec<ExportCopy<'a>>,
        ctx: &Ctx<'a>,
    ) {
        let mut replacement = None;
        match target {
            AssignmentTarget::AssignmentTargetIdentifier(id) => {
                if let Some(symbol) = reference_symbol(id, ctx)
                    && let Some((plan, names)) = self.write_plan(symbol, id.name.as_str())
                {
                    if let Target::Exports(name) = plan {
                        let member = member_expression(id.span, "exports", name, ctx.ast);
                        replacement =
                            Some(AssignmentTarget::from(SimpleAssignmentTarget::from(member)));
                    }
                    push_copy(copies, symbol, plan, names);
                }
            }
            AssignmentTarget::ArrayAssignmentTarget(array) => {
                for element in array.elements.iter_mut().flatten() {
                    self.rewrite_maybe_default(element, copies, ctx);
                }
                if let Some(rest) = &mut array.rest {
                    self.rewrite_target(&mut rest.target, copies, ctx);
                }
            }
            AssignmentTarget::ObjectAssignmentTarget(object) => {
                for property in object.properties.iter_mut() {
                    match property {
                        AssignmentTargetProperty::AssignmentTargetPropertyIdentifier(shorthand) => {
                            let Some(symbol) = reference_symbol(&shorthand.binding, ctx) else {
                                continue;
                            };
                            let Some((plan, names)) =
                                self.write_plan(symbol, shorthand.binding.name.as_str())
                            else {
                                continue;
                            };
                            push_copy(copies, symbol, plan, names);
                            let Target::Exports(name) = plan else {
                                continue;
                            };
                            // `{ x } = o` becomes `{ x: exports.x } = o`.
                            let span = shorthand.span;
                            let at = shorthand.binding.span;
                            let key = PropertyKey::new_static_identifier(at, name, ctx);
                            let member = member_expression(at, "exports", name, ctx.ast);
                            let target =
                                AssignmentTarget::from(SimpleAssignmentTarget::from(member));
                            let binding = match shorthand.init.take() {
                                Some(init) => {
                                    AssignmentTargetMaybeDefault::new_assignment_target_with_default(
                                        span, target, init, ctx,
                                    )
                                }
                                None => AssignmentTargetMaybeDefault::from(target),
                            };
                            *property =
                                AssignmentTargetProperty::new_assignment_target_property_property(
                                    span, key, binding, false, ctx,
                                );
                        }
                        AssignmentTargetProperty::AssignmentTargetPropertyProperty(property) => {
                            self.rewrite_maybe_default(&mut property.binding, copies, ctx);
                        }
                    }
                }
                if let Some(rest) = &mut object.rest {
                    self.rewrite_target(&mut rest.target, copies, ctx);
                }
            }
            _ => {}
        }
        if let Some(replacement) = replacement {
            *target = replacement;
        }
    }

    fn rewrite_maybe_default(
        &self,
        element: &mut AssignmentTargetMaybeDefault<'a>,
        copies: &mut Vec<ExportCopy<'a>>,
        ctx: &Ctx<'a>,
    ) {
        if let AssignmentTargetMaybeDefault::AssignmentTargetWithDefault(with_default) = element {
            self.rewrite_target(&mut with_default.binding, copies, ctx);
        } else if let Some(target) = element.as_assignment_target_mut() {
            self.rewrite_target(target, copies, ctx);
        }
    }

    /// `for (x of xs)` writes `x` on every iteration, so each copy opens the body. A
    /// body that declares its own bindings, which could shadow `x` or `exports`, is
    /// nested in a block the copies open instead.
    fn copy_after_loop_head(
        &self,
        left: &mut ForStatementLeft<'a>,
        body: &mut Statement<'a>,
        ctx: &mut Ctx<'a>,
    ) {
        let Some(target) = left.as_assignment_target_mut() else {
            return;
        };
        let mut copies = Vec::new();
        self.rewrite_target(target, &mut copies, ctx);
        if copies.is_empty() {
            return;
        }
        let ast = ctx.ast;
        let mut statements = ArenaVec::with_capacity_in(copies.len() + 1, &ast);
        for copy in &copies {
            statements.push(Statement::new_expression_statement(
                SPAN,
                copy_expression(copy, ast),
                &ast,
            ));
        }
        if let Statement::BlockStatement(block) = body
            && block
                .scope_id
                .get()
                .is_none_or(|scope| ctx.scoping().get_bindings(scope).is_empty())
        {
            block.body.splice(0..0, statements);
        } else {
            statements.push(body.take_in(&ast));
            *body = Statement::new_block_statement(SPAN, statements, &ast);
        }
    }
}

fn push_copy<'a>(
    copies: &mut Vec<ExportCopy<'a>>,
    symbol: SymbolId,
    target: Target<'a>,
    names: Vec<&'a str>,
) {
    if !names.is_empty() && !copies.iter().any(|c| c.symbol == symbol) {
        copies.push(ExportCopy {
            symbol,
            target,
            names,
        });
    }
}

fn read_target<'a>(target: Target<'a>, ast: AstBuilder<'a>) -> Expression<'a> {
    match target {
        Target::Local(name) => Expression::new_identifier(SPAN, name, &ast),
        Target::Exports(name) => Expression::from(member_expression(SPAN, "exports", name, ast)),
    }
}

/// `exports.a = exports.b = x`.
fn copy_expression<'a>(copy: &ExportCopy<'a>, ast: AstBuilder<'a>) -> Expression<'a> {
    let mut value = read_target(copy.target, ast);
    for name in copy.names.iter().rev() {
        value = assign_to_exports(SPAN, name, value, ast);
    }
    value
}

/// Whether nothing reads the current expression's value.
fn value_unused(ctx: &Ctx<'_>) -> bool {
    for ancestor in ctx.ancestors() {
        match ancestor {
            Ancestor::ParenthesizedExpressionExpression(_) => {}
            Ancestor::ExpressionStatementExpression(_) | Ancestor::ForStatementUpdate(_) => {
                return true;
            }
            _ => return false,
        }
    }
    false
}

/// The interop helper `tsc` wraps an import's `require()` in, by the names it
/// imports: `__importStar` for a namespace, or for `default` beside other names
/// (as a default import or `{ default as x }`); `__importDefault` for `default`
/// alone; none for named imports only.
fn import_helper(specifiers: &[ImportDeclarationSpecifier<'_>]) -> Option<Helper> {
    let mut default_import = false;
    let mut named = 0;
    let mut default_refs = 0;
    for specifier in specifiers {
        match specifier {
            ImportDeclarationSpecifier::ImportNamespaceSpecifier(_) => {
                return Some(Helper::ImportStar);
            }
            ImportDeclarationSpecifier::ImportDefaultSpecifier(_) => default_import = true,
            ImportDeclarationSpecifier::ImportSpecifier(s) => {
                named += 1;
                if s.imported.name() == "default" {
                    default_refs += 1;
                }
            }
        }
    }
    if (default_refs > 0 && default_refs != named) || (named > default_refs && default_import) {
        Some(Helper::ImportStar)
    } else if default_import || default_refs > 0 {
        Some(Helper::ImportDefault)
    } else {
        None
    }
}

fn statement_declaring_index(
    declared_at: &FxHashMap<SymbolId, usize>,
    symbol: SymbolId,
) -> Option<usize> {
    declared_at.get(&symbol).copied()
}

fn assign_to_local<'a>(
    span: Span,
    name: &'a str,
    value: Expression<'a>,
    ast: AstBuilder<'a>,
) -> Expression<'a> {
    let target = SimpleAssignmentTarget::new_assignment_target_identifier(SPAN, name, &ast);
    Expression::new_assignment_expression(
        span,
        AssignmentOperator::Assign,
        AssignmentTarget::from(target),
        value,
        &ast,
    )
}

fn assign_to_exports<'a>(
    span: Span,
    name: &'a str,
    value: Expression<'a>,
    ast: AstBuilder<'a>,
) -> Expression<'a> {
    let target = SimpleAssignmentTarget::from(member_expression(SPAN, "exports", name, ast));
    Expression::new_assignment_expression(
        span,
        AssignmentOperator::Assign,
        AssignmentTarget::from(target),
        value,
        &ast,
    )
}

const IMPORT_META: &str = "`import.meta` is not available in CommonJS. This file compiles to CommonJS because its tsconfig.json sets \"module\": \"commonjs\". Use `__filename` and `__dirname`, or rename the file to .mts to run it as an ES module.";

const TOP_LEVEL_AWAIT: &str = "Top-level `await` is not available in CommonJS. This file compiles to CommonJS because its tsconfig.json sets \"module\": \"commonjs\". Move the `await` into an async function, or rename the file to .mts to run it as an ES module.";
