//! The `nub config get`/`set`/`delete` surface for `nub.jsonc` fields.
//!
//! `nub config` is the engine's `.npmrc` verb. A key naming a real `nub.jsonc`
//! field is intercepted in [`crate::pm_engine::store_config_family`] and handled
//! here; every other key keeps its existing `.npmrc` / `pnpm-workspace.yaml`
//! routing untouched. The interception is driven by [`FIELDS`], so a key nub
//! does not own can never be captured away from the engine.
//!
//! Four decisions this module encodes:
//!
//! - **Whole-field addressing.** A dotted path names a leaf setting, never a
//!   position inside one. `install.linker` is a discriminated union whose knob is
//!   only meaningful alongside its own `strategy`, so an in-place
//!   `install.linker.hoist` write could leave a document the reader rejects; the
//!   union is written and cleared as a unit instead, and a JSON literal supplies
//!   the object form.
//! - **Validation is the reader's.** A coerced value is checked by building the
//!   one-key document it would appear in and running
//!   [`crate::project_config::validate_document`] over it, so a rejected `set`
//!   reports exactly what the next run would have reported and no invalid value
//!   ever reaches disk.
//! - **Reading reports configuration, not effect.** An unset field prints
//!   `undefined`, matching what the same command prints for an unset `.npmrc`
//!   key. Built-in defaults and the CLI/environment overlays are a runtime
//!   concern; reporting them here would make `get` disagree with the file `set`
//!   just wrote.
//! - **Scope follows the field.** Writes land in the project file by default and
//!   in the global file under `--global` (or `nub global config`). The `dlx` section is
//!   global-only (a checkout must not widen a consent decision about the
//!   machine), so it ignores the default and refuses an explicit project scope.

use std::path::{Path, PathBuf};

use jsonc_parser::cst::CstInputValue;
use serde_json::{Map, Value};

use crate::project_config::{self, ConfigError};

/// Which `nub.jsonc` files a read or write may touch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    /// No scope flag: the field decides. A read takes the project file then the
    /// global one; a write takes the field's own home.
    Auto,
    Project,
    Global,
}

/// How a shell-supplied string becomes the JSON value the field accepts. One
/// variant per distinct value grammar in the schema — a field's variant is the
/// only thing that decides its coercion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// `true` / `false`.
    Bool,
    /// A single string.
    Str,
    /// `string[]`, written as a JSON array.
    StrList,
    /// `boolean | string | string[]` (`envFile`).
    EnvFile,
    /// `boolean | "warn" | "error"` (`verifyDeps`).
    VerifyDeps,
    /// A strategy name; the object form arrives as JSON (`install.linker`).
    Linker,
    /// `{ ".ext": "loader" }`, written as a JSON object.
    Loader,
}

/// One addressable field of `nub.jsonc`.
pub(crate) struct Field {
    /// The dotted address the user types after `nub config`.
    pub(crate) address: &'static str,
    /// The dotted path written into `nub.jsonc`. Usually the same as
    /// [`Self::address`], but a runtime setting may use a namespaced address to
    /// avoid claiming a pnpm-compatible engine key.
    pub(crate) path: &'static str,
    shape: Shape,
    /// Set for a section only the global file may carry
    /// (`project_config::GLOBAL_ONLY_KEYS`).
    global_only: bool,
}

const FIELDS: &[Field] = &[
    Field {
        address: "nodeCompat",
        path: "nodeCompat",
        shape: Shape::Bool,
        global_only: false,
    },
    Field {
        address: "preload",
        path: "preload",
        shape: Shape::StrList,
        global_only: false,
    },
    Field {
        address: "runtime.nodeOptions",
        path: "nodeOptions",
        shape: Shape::StrList,
        global_only: false,
    },
    Field {
        address: "v8Flags",
        path: "v8Flags",
        shape: Shape::StrList,
        global_only: false,
    },
    Field {
        address: "envFile",
        path: "envFile",
        shape: Shape::EnvFile,
        global_only: false,
    },
    Field {
        address: "loader",
        path: "loader",
        shape: Shape::Loader,
        global_only: false,
    },
    Field {
        address: "conditions",
        path: "conditions",
        shape: Shape::StrList,
        global_only: false,
    },
    Field {
        address: "tsconfig",
        path: "tsconfig",
        shape: Shape::Str,
        global_only: false,
    },
    Field {
        address: "verifyDeps",
        path: "verifyDeps",
        shape: Shape::VerifyDeps,
        global_only: false,
    },
    Field {
        address: "install.linker",
        path: "install.linker",
        shape: Shape::Linker,
        global_only: false,
    },
    Field {
        address: "install.publicHoist",
        path: "install.publicHoist",
        shape: Shape::StrList,
        global_only: false,
    },
    Field {
        address: "install.minimumReleaseAge",
        path: "install.minimumReleaseAge",
        shape: Shape::Str,
        global_only: false,
    },
    Field {
        address: "install.minimumReleaseAgeExclude",
        path: "install.minimumReleaseAgeExclude",
        shape: Shape::StrList,
        global_only: false,
    },
    Field {
        address: "dlx.consent",
        path: "dlx.consent",
        shape: Shape::Str,
        global_only: true,
    },
];

/// The field `key` addresses, if any. Matching is exact: a near-miss must fall
/// through to the engine rather than be silently corrected, so a typo lands in
/// `.npmrc` where the user can see it instead of in a file nub validates.
pub(crate) fn field(key: &str) -> Option<&'static Field> {
    FIELDS.iter().find(|f| f.address == key)
}

impl Field {
    fn segments(&self) -> Vec<&'static str> {
        self.path.split('.').collect()
    }

    /// The top-level section the field lives under — `install` for
    /// `install.linker`, the field itself for a root key. This is the unit
    /// `project_config::GLOBAL_ONLY_KEYS` is expressed in.
    fn section(&self) -> &'static str {
        self.path
            .split('.')
            .next()
            .expect("a setting path has a first segment")
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Commands
// ─────────────────────────────────────────────────────────────────────────────

/// Print the configured value, or `undefined` when the field is unset in the
/// files `scope` names.
pub(crate) fn get(field: &Field, scope: Scope, json: bool) -> anyhow::Result<i32> {
    let mut value = None;
    for path in read_targets(field, scope)? {
        value = read_at(&path, field)?;
        if value.is_some() {
            break;
        }
    }
    match (value, json) {
        (Some(v), true) => println!("{v}"),
        (Some(v), false) => println!("{}", render(&v)),
        // The engine's own `config get` prints this bare word for an unset key,
        // in both modes; a nub field must not read differently.
        (None, _) => println!("undefined"),
    }
    Ok(0)
}

/// Validate `raw` against the schema, then write it.
pub(crate) fn set(field: &Field, raw: &str, scope: Scope) -> anyhow::Result<i32> {
    let path = write_target(field, scope)?;
    let value = coerce(field, raw).map_err(|e| anyhow::anyhow!("{}", e.in_file(&path)))?;
    project_config::validate_document(&document(field, value.clone()), field.global_only)
        .map_err(|e| anyhow::anyhow!("{}", e.in_file(&path)))?;

    crate::config::set_json_path(&path, &field.segments(), to_cst(&value))
        .map_err(|e| anyhow::anyhow!("nub config set {}: {}", field.address, e))?;
    crate::pm_engine::present::info(&format!(
        "set {} = {} ({})",
        field.address,
        render(&value),
        path.display()
    ));
    Ok(0)
}

/// Remove the field, restoring whatever it was overriding.
pub(crate) fn delete(field: &Field, scope: Scope) -> anyhow::Result<i32> {
    let path = write_target(field, scope)?;
    let removed = crate::config::unset_json_path(&path, &field.segments())
        .map_err(|e| anyhow::anyhow!("nub config delete {}: {}", field.address, e))?;
    if removed {
        crate::pm_engine::present::info(&format!("removed {} ({})", field.address, path.display()));
    }
    Ok(0)
}

// ─────────────────────────────────────────────────────────────────────────────
// File resolution
// ─────────────────────────────────────────────────────────────────────────────

/// The files a read consults, in precedence order.
fn read_targets(field: &Field, scope: Scope) -> anyhow::Result<Vec<PathBuf>> {
    if field.global_only {
        // Reading a global-only field from the project file would always be
        // `undefined`, so every scope resolves to the global file rather than
        // reporting an absence that cannot be anything else.
        return Ok(vec![global_file(field)?]);
    }
    Ok(match scope {
        // The unscoped read answers "what applies here": the project file wins
        // over the global one, mirroring the precedence a run uses. A global
        // file that does not resolve is simply not consulted.
        Scope::Auto => [project_file()]
            .into_iter()
            .chain(global_file(field).ok())
            .collect(),
        Scope::Project => vec![project_file()],
        Scope::Global => vec![global_file(field)?],
    })
}

fn write_target(field: &Field, scope: Scope) -> anyhow::Result<PathBuf> {
    if field.global_only {
        if scope == Scope::Project {
            // The reader raises this same refusal on a project file carrying
            // the section; catching it at the write keeps the two consistent.
            return Err(anyhow::anyhow!(
                "{}",
                ConfigError::GlobalOnlyKey {
                    key: field.section().to_string(),
                }
            ));
        }
        return global_file(field);
    }
    Ok(match scope {
        Scope::Global => global_file(field)?,
        // Writes default to the project file; `--global` selects the user file.
        Scope::Auto | Scope::Project => project_file(),
    })
}

/// The project file a write lands in: the one discovery would read, else a new
/// file at the project root so a `set` from a subdirectory does not create a
/// second `nub.jsonc` that shadows nothing.
pub(crate) fn project_file() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if let Some(found) = project_config::discover_project_config(&cwd) {
        return found;
    }
    for dir in cwd.ancestors() {
        if dir.join("package.json").is_file() {
            return dir.join(project_config::FILE_NAME);
        }
    }
    cwd.join(project_config::FILE_NAME)
}

fn global_file(field: &Field) -> anyhow::Result<PathBuf> {
    crate::config::config_path().ok_or_else(|| {
        anyhow::anyhow!(
            "nub config: `{}` is stored globally, but no config directory resolves\n\
             \x20\x20set XDG_CONFIG_HOME or HOME to a writable directory",
            field.path
        )
    })
}

/// Read one field's raw value out of `path`. A file that does not parse is an
/// error rather than an absence: reporting `undefined` for a file nub would
/// refuse to run would be a wrong answer, not a missing one. A file that exists
/// but cannot be READ (over the size cap, not UTF-8, not a regular file) fails
/// the same run for the same reason, so only genuine absence is an absence.
fn read_at(path: &Path, field: &Field) -> anyhow::Result<Option<Value>> {
    let text = match crate::jsonc::read_guarded(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(anyhow::anyhow!("{}", ConfigError::Io(e).in_file(path))),
    };
    let Some(root) = crate::jsonc::parse_to_value(&text)
        .map_err(|e| anyhow::anyhow!("{}", ConfigError::Parse(e).in_file(path)))?
    else {
        return Ok(None);
    };
    let mut cursor = &root;
    for segment in field.segments() {
        match cursor.get(segment) {
            Some(next) => cursor = next,
            None => return Ok(None),
        }
    }
    Ok(Some(cursor.clone()))
}

// ─────────────────────────────────────────────────────────────────────────────
// Values
// ─────────────────────────────────────────────────────────────────────────────

/// The one-key document `value` would appear in, so the schema validator sees
/// the field in its real position.
fn document(field: &Field, value: Value) -> Map<String, Value> {
    let segments = field.segments();
    let (outermost, inner) = segments
        .split_first()
        .expect("a setting path has a first segment");
    let mut current = value;
    for segment in inner.iter().rev() {
        current = Value::Object(Map::from_iter([(segment.to_string(), current)]));
    }
    Map::from_iter([(outermost.to_string(), current)])
}

/// Turn a shell argument into the JSON value the field's grammar describes.
///
/// A structured branch takes JSON verbatim. Structured values require that
/// exact form rather than a second comma/equals mini-language, so every array
/// element and object entry round-trips without shell-string ambiguity. Scalar
/// fields and union-scalar branches keep their plain spellings, even when a
/// string happens to begin with `[` or `{`.
fn coerce(field: &Field, raw: &str) -> Result<Value, ConfigError> {
    let trimmed = raw.trim_start();
    let structured = match field.shape {
        Shape::StrList | Shape::EnvFile => trimmed.starts_with('['),
        Shape::Loader | Shape::Linker => trimmed.starts_with('{'),
        Shape::Bool | Shape::Str | Shape::VerifyDeps => false,
    };
    if structured {
        return serde_json::from_str(raw).map_err(|e| ConfigError::Value {
            path: field.path.into(),
            message: format!("invalid JSON: {e}"),
        });
    }
    Ok(match field.shape {
        // Coercion is identical for these three: a boolean spelling becomes a
        // boolean and anything else stays a string. Their grammars differ only
        // in which non-boolean strings the validator then accepts.
        Shape::Bool | Shape::VerifyDeps | Shape::EnvFile => {
            parse_bool(raw).map_or_else(|| Value::String(raw.into()), Value::Bool)
        }
        Shape::Str | Shape::Linker => Value::String(raw.into()),
        // The two shapes with no scalar spelling of their own: a bare shell
        // string here is a missing pair of brackets or braces, not a value.
        Shape::StrList => {
            return Err(ConfigError::Value {
                path: field.path.into(),
                message: r#"expected a JSON array (for example, ["./setup.ts","tsx/esm"])"#.into(),
            });
        }
        Shape::Loader => {
            return Err(ConfigError::Value {
                path: field.path.into(),
                message: r#"expected a JSON object (for example, {".graphql":"text"})"#.into(),
            });
        }
    })
}

fn parse_bool(raw: &str) -> Option<bool> {
    match raw {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// Render a value the way `set` accepts it back, so `get` and `set` round-trip.
/// Use `--json` for the exact JSON form.
fn render(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn to_cst(value: &Value) -> CstInputValue {
    match value {
        Value::Null => CstInputValue::Null,
        Value::Bool(b) => CstInputValue::Bool(*b),
        Value::Number(n) => CstInputValue::Number(n.to_string()),
        Value::String(s) => CstInputValue::String(s.clone()),
        Value::Array(items) => CstInputValue::Array(items.iter().map(to_cst).collect()),
        Value::Object(entries) => CstInputValue::Object(
            entries
                .iter()
                .map(|(k, v)| (k.clone(), to_cst(v)))
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coerced(key: &str, raw: &str) -> Value {
        coerce(field(key).expect("known field"), raw).expect("coercible")
    }

    /// A field the schema accepts but the CLI cannot address is invisible to
    /// `nub config`, and nothing else would catch the omission when a key is
    /// added to the schema.
    #[test]
    fn every_schema_field_is_addressable() {
        let mut expected: Vec<String> = Vec::new();
        for key in project_config::ROOT_KEYS {
            match *key {
                // Editor metadata, and the two section objects whose leaves are
                // enumerated below.
                "$schema" | "install" | "dlx" => {}
                other => expected.push(other.to_string()),
            }
        }
        expected.extend(
            project_config::INSTALL_KEYS
                .iter()
                .map(|k| format!("install.{k}")),
        );
        expected.extend(project_config::DLX_KEYS.iter().map(|k| format!("dlx.{k}")));

        let missing: Vec<_> = expected
            .iter()
            .filter(|k| !FIELDS.iter().any(|f| f.path == k.as_str()))
            .collect();
        assert!(
            missing.is_empty(),
            "schema fields with no CLI row: {missing:?}"
        );

        let stale: Vec<_> = FIELDS
            .iter()
            .map(|f| f.path)
            .filter(|p| !expected.iter().any(|k| k == p))
            .collect();
        assert!(stale.is_empty(), "CLI rows with no schema field: {stale:?}");

        // The write target must follow the reader's scope rule, not a second
        // hand-maintained opinion about which sections are global.
        for f in FIELDS {
            assert_eq!(
                f.global_only,
                project_config::GLOBAL_ONLY_KEYS.contains(&f.section()),
                "{} disagrees with GLOBAL_ONLY_KEYS",
                f.path
            );
        }
    }

    #[test]
    fn cli_addresses_do_not_claim_engine_names_or_npmrc_aliases() {
        let collisions: Vec<_> = FIELDS
            .iter()
            .flat_map(|field| {
                aube_settings::all()
                    .iter()
                    .filter(move |engine| {
                        field.address == engine.name || engine.npmrc_keys.contains(&field.address)
                    })
                    .map(move |engine| format!("{} ↔ {}", field.address, engine.name))
            })
            .collect();
        assert!(
            collisions.is_empty(),
            "nub.jsonc CLI addresses must not capture engine config keys: {collisions:?}"
        );

        let runtime = field("runtime.nodeOptions").expect("runtime alias is addressable");
        assert_eq!(runtime.path, "nodeOptions");
        assert!(
            field("nodeOptions").is_none(),
            "the bare pnpm-compatible key belongs to the engine"
        );
    }

    #[test]
    fn shell_arguments_coerce_to_the_typed_value_each_field_declares() {
        assert_eq!(coerced("nodeCompat", "true"), Value::Bool(true));
        assert_eq!(
            coerced("tsconfig", "./tsconfig.json"),
            Value::String("./tsconfig.json".into())
        );
        assert_eq!(
            coerced("tsconfig", "[generated]/tsconfig.json"),
            Value::String("[generated]/tsconfig.json".into()),
            "a scalar path is not reinterpreted as JSON from its first byte"
        );
        assert_eq!(
            coerced("preload", r#"["./a.ts","./b.ts"]"#),
            serde_json::json!(["./a.ts", "./b.ts"])
        );
        assert_eq!(coerced("envFile", "false"), Value::Bool(false));
        assert_eq!(
            coerced("envFile", ".env.local"),
            Value::String(".env.local".into())
        );
        assert_eq!(
            coerced("envFile", ".env,.env.local"),
            Value::String(".env,.env.local".into())
        );
        assert_eq!(
            coerced("verifyDeps", "error"),
            Value::String("error".into())
        );
        assert_eq!(
            coerced("install.linker", "hoisted"),
            Value::String("hoisted".into())
        );
        assert_eq!(
            coerced("loader", r#"{".graphql":"text",".rules":"yaml"}"#),
            serde_json::json!({ ".graphql": "text", ".rules": "yaml" })
        );

        // Structured values use JSON verbatim, including elements containing
        // commas.
        assert_eq!(
            coerced(
                "install.linker",
                r#"{"strategy":"isolated","hoist":["@types/*"]}"#
            ),
            serde_json::json!({ "strategy": "isolated", "hoist": ["@types/*"] })
        );
        assert_eq!(
            coerced("preload", r#"["./a,b.ts"]"#),
            serde_json::json!(["./a,b.ts"])
        );

        let bad = coerce(field("install.linker").unwrap(), "{not json}").unwrap_err();
        assert!(bad.to_string().contains("invalid JSON"), "{bad}");
        let list = coerce(field("preload").unwrap(), "./a.ts,./b.ts").unwrap_err();
        assert!(list.to_string().contains("expected a JSON array"), "{list}");
        let object = coerce(field("loader").unwrap(), ".graphql=text").unwrap_err();
        assert!(
            object.to_string().contains("expected a JSON object"),
            "{object}"
        );
    }

    #[test]
    fn rendering_round_trips_through_coercion() {
        for (key, raw) in [
            ("nodeCompat", "true"),
            ("preload", r#"["./a.ts","./b.ts"]"#),
            ("envFile", r#"[".env",".env.local"]"#),
            ("verifyDeps", "warn"),
            ("install.minimumReleaseAge", "3d"),
            ("install.linker", "hoisted"),
        ] {
            assert_eq!(render(&coerced(key, raw)), raw, "{key} must round-trip");
        }
    }

    /// The writer must reject exactly what the reader rejects, with the reader's
    /// own wording — a value that passes here and fails on the next run is the
    /// failure mode the shared validator exists to prevent.
    #[test]
    fn invalid_values_are_refused_with_the_file_parsers_message() {
        let bad_duration = field("install.minimumReleaseAge").unwrap();
        let value = coerce(bad_duration, "3").unwrap();
        let err = project_config::validate_document(&document(bad_duration, value), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid duration `3`"), "{err}");
        assert!(err.contains("s|m|h|d|w"), "{err}");

        let compat = field("nodeCompat").unwrap();
        let value = coerce(compat, "yes").unwrap();
        let err = project_config::validate_document(&document(compat, value), false)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("`nodeCompat` in nub.jsonc must be a boolean"),
            "{err}"
        );

        let linker = field("install.linker").unwrap();
        let value = coerce(linker, "flat").unwrap();
        let err = project_config::validate_document(&document(linker, value), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown strategy `flat`"), "{err}");
    }

    #[test]
    fn dlx_is_global_only_at_the_write_boundary() {
        let consent = field("dlx.consent").unwrap();
        assert!(consent.global_only);

        crate::config::with_config_home(|home| {
            let expected = home.join("nub").join(project_config::FILE_NAME);
            let err = write_target(consent, Scope::Project)
                .unwrap_err()
                .to_string();
            assert!(
                err.contains(&format!(
                    "`dlx` in nub.jsonc is configured globally: move it to {}",
                    expected.display()
                )),
                "{err}"
            );
            assert_eq!(write_target(consent, Scope::Auto).unwrap(), expected);
            assert_eq!(write_target(consent, Scope::Global).unwrap(), expected);
        });
    }
}
