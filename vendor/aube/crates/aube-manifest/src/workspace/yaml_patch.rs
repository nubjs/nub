//! Diff a parsed-then-mutated workspace yaml mapping into a minimal
//! set of edits and apply them to the original source. Comments and
//! formatting on untouched keys survive every edit.
//!
//! The module is a thin wrapper around `yamlpatch` plus a manual
//! block-mapping injector. yamlpatch handles `Remove` / `Replace` /
//! scalar `Add` correctly. Its `Op::Add` for non-empty *mapping*
//! values is broken upstream (it strips the nested indentation
//! hierarchy and produces invalid YAML where a child key lands at the
//! parent's column), so any new sub-mapping is rendered to a
//! block-style YAML string here and inserted at the right byte
//! offset instead.
//!
//! The only public entry is [`apply_diff`].
use std::path::Path;
use yaml_serde::{Mapping, Value};
use yamlpatch::{Op, Patch, apply_yaml_patches};
use yamlpath::{Component, Document, Route};

/// Indentation step new entries are rendered with. Two spaces
/// matches pnpm's canonical workspace yaml layout. Reading the
/// step from the source (so an existing four-space file stays
/// four-space) is left for a later pass — every aube install
/// plus existing pnpm workspaces use two.
const INDENT_STEP: usize = 2;

/// One unit of a structural diff. `Yp` operations go through
/// yamlpatch; `Add` operations are injected directly because
/// yamlpatch's `Op::Add` mishandles non-empty nested mappings.
enum Edit {
    Yp(Patch<'static>),
    Add {
        route_keys: Vec<String>,
        key: String,
        value: serde_yaml::Value,
    },
}

/// Compute the minimal edit list that turns `before` into `after`
/// and apply it to `source`. Returns the source unchanged when
/// the diff is empty.
pub(super) fn apply_diff(
    path: &Path,
    source: &str,
    before: &Mapping,
    after: &Mapping,
) -> Result<Vec<u8>, crate::Error> {
    let mut edits = Vec::new();
    diff_into(before, after, &[], path, &mut edits)?;
    if edits.is_empty() {
        return Ok(source.as_bytes().to_vec());
    }

    // Step 1: yamlpatch-handled ops (Remove + Replace + scalar
    // Add). These are surgical and order-independent: yamlpatch
    // applies them sequentially against the tree-sitter doc,
    // re-querying after each step.
    let yp_patches: Vec<Patch<'static>> = edits
        .iter()
        .filter_map(|e| match e {
            Edit::Yp(p) => Some(p.clone()),
            _ => None,
        })
        .collect();
    let mut current = if yp_patches.is_empty() {
        source.to_string()
    } else {
        let document =
            Document::new(source.to_string()).map_err(|e| yp_err(path, e.to_string()))?;
        apply_yaml_patches(&document, &yp_patches)
            .map_err(|e| yp_err(path, e.to_string()))?
            .source()
            .to_string()
    };

    // Step 2: direct injections for new keys whose value is a
    // mapping. Sort outer-most first so a parent that only just
    // came into existence is queryable for its children. Within
    // the same depth, preserve insertion order.
    let mut adds: Vec<(Vec<String>, String, serde_yaml::Value)> = edits
        .into_iter()
        .filter_map(|e| match e {
            Edit::Add {
                route_keys,
                key,
                value,
            } => Some((route_keys, key, value)),
            _ => None,
        })
        .collect();
    adds.sort_by_key(|(r, _, _)| r.len());
    for (route_keys, key, value) in adds {
        current = inject_entry(&current, &route_keys, &key, &value, path)?;
    }

    Ok(current.into_bytes())
}

/// Walk `before` and `after` recursively, pushing `Edit`s for
/// every structural difference. Mapping-valued additions become
/// `Edit::Add` (handled outside yamlpatch); everything else maps
/// to a yamlpatch `Patch`. Non-string keys cause a hard error
/// rather than silent data loss.
fn diff_into(
    before: &Mapping,
    after: &Mapping,
    route: &[String],
    path: &Path,
    out: &mut Vec<Edit>,
) -> Result<(), crate::Error> {
    let route_obj: Route<'static> = Route::from(
        route
            .iter()
            .cloned()
            .map(Component::from)
            .collect::<Vec<_>>(),
    );
    for (k, _) in before.iter() {
        let key = key_str(path, k)?;
        if !after.contains_key(k) {
            out.push(Edit::Yp(Patch {
                route: route_obj.with_key(key.to_string()),
                operation: Op::Remove,
            }));
        }
    }
    for (k, after_v) in after.iter() {
        let key = key_str(path, k)?;
        match before.get(k) {
            None => out.push(Edit::Add {
                route_keys: route.to_vec(),
                key: key.to_string(),
                value: to_serde_value(path, after_v)?,
            }),
            Some(before_v) if before_v != after_v => {
                if let (Some(bm), Some(am)) = (before_v.as_mapping(), after_v.as_mapping()) {
                    let mut sub = route.to_vec();
                    sub.push(key.to_string());
                    diff_into(bm, am, &sub, path, out)?;
                } else if matches!(after_v.as_mapping(), Some(m) if !m.is_empty()) {
                    // Type-change to a non-empty sub-mapping (e.g.
                    // scalar -> nested mapping). yamlpatch's
                    // Op::Replace serializes the mapping value via
                    // the same path as Op::Add, which strips nested
                    // indentation and lands the children at the
                    // parent's column. Split into Remove + manual
                    // injection so step 2 can re-emit the children
                    // with their canonical indent.
                    out.push(Edit::Yp(Patch {
                        route: route_obj.with_key(key.to_string()),
                        operation: Op::Remove,
                    }));
                    out.push(Edit::Add {
                        route_keys: route.to_vec(),
                        key: key.to_string(),
                        value: to_serde_value(path, after_v)?,
                    });
                } else {
                    out.push(Edit::Yp(Patch {
                        route: route_obj.with_key(key.to_string()),
                        operation: Op::Replace(after_v.clone()),
                    }));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Inject a fresh `<key>: <value>` block-style entry into
/// `source` at the end of the route's mapping. Top-level routes
/// (empty) append at end-of-file. Nested routes look up the
/// parent feature via yamlpath, then insert just past its end
/// span at the parent's child indent.
fn inject_entry(
    source: &str,
    route_keys: &[String],
    key: &str,
    value: &serde_yaml::Value,
    path: &Path,
) -> Result<String, crate::Error> {
    if route_keys.is_empty() {
        let entry = render_entry(key, value, 0);
        let mut result = source.to_string();
        if !result.is_empty() && !result.ends_with('\n') {
            result.push('\n');
        }
        result.push_str(&entry);
        return Ok(result);
    }

    let document = Document::new(source.to_string()).map_err(|e| yp_err(path, e.to_string()))?;
    let route_obj: Route<'static> = Route::from(
        route_keys
            .iter()
            .cloned()
            .map(Component::from)
            .collect::<Vec<_>>(),
    );
    let feature = document
        .query_exact(&route_obj)
        .map_err(|e| yp_err(path, e.to_string()))?
        .ok_or_else(|| {
            yp_err(
                path,
                format!("parent route {route_keys:?} not found in source"),
            )
        })?;
    // `extract_with_leading_whitespace` walks the byte span back
    // over any pure-space prefix on the parent's first line, so
    // the snapshot mirrors the original column the children sit
    // at — `extract` alone would start mid-line and drop the
    // indent the new entry needs to inherit.
    let parent_content = document.extract_with_leading_whitespace(&feature);
    let child_indent = detect_child_indent(parent_content, route_keys.len());
    let entry = render_entry(key, value, child_indent);

    let mut insert_at = feature.location.byte_span.1;
    // Trim back over trailing whitespace so the new entry lands
    // just after the parent block's last content line, before any
    // trailing blank lines that belong to the document footer.
    let bytes = source.as_bytes();
    while insert_at > 0 && matches!(bytes[insert_at - 1], b'\n' | b' ') {
        insert_at -= 1;
    }
    let mut result = source.to_string();
    let mut prefix = String::new();
    if insert_at == 0 || bytes[insert_at - 1] != b'\n' {
        prefix.push('\n');
    }
    prefix.push_str(&entry);
    result.insert_str(insert_at, &prefix);
    Ok(result)
}

/// Render `<key>: <value>` as block-style YAML lines, each
/// indented by `indent` spaces. Non-empty mapping values nest
/// recursively; non-empty sequence values emit as block
/// sequences with `- ` items at child indent; everything else
/// is emitted as a scalar value after the colon.
fn render_entry(key: &str, value: &serde_yaml::Value, indent: usize) -> String {
    let pad = " ".repeat(indent);
    match value {
        serde_yaml::Value::Mapping(m) if !m.is_empty() => {
            let mut out = format!("{pad}{}:\n", scalar_key_str(key));
            for (k, v) in m {
                let child_key = match k {
                    serde_yaml::Value::String(s) => s.clone(),
                    other => render_scalar(other),
                };
                out.push_str(&render_entry(&child_key, v, indent + INDENT_STEP));
            }
            out
        }
        serde_yaml::Value::Sequence(seq) if !seq.is_empty() => {
            // Block-sequence under a mapping key. Without an
            // explicit arm the catch-all below would feed the
            // sequence through `render_scalar`, which emits a
            // multi-line `- a\n- b` chunk that lands inline on the
            // `key:` line and produces structurally invalid YAML.
            let mut out = format!("{pad}{}:\n", scalar_key_str(key));
            let item_pad = " ".repeat(indent + INDENT_STEP);
            for item in seq {
                if matches!(
                    item,
                    serde_yaml::Value::Mapping(_) | serde_yaml::Value::Sequence(_)
                ) {
                    // Nested mapping/sequence as a list item: defer
                    // to serde_yaml for inner shape, then attach
                    // the dash to the first emitted line and pad
                    // every continuation line so it stays inside
                    // the same item.
                    let raw = serde_yaml::to_string(item).unwrap_or_default();
                    let mut first = true;
                    for line in raw.lines() {
                        if first {
                            first = false;
                            out.push_str(&item_pad);
                            out.push_str("- ");
                        } else {
                            out.push_str(&item_pad);
                            out.push_str("  ");
                        }
                        out.push_str(line);
                        out.push('\n');
                    }
                } else {
                    out.push_str(&item_pad);
                    out.push_str("- ");
                    out.push_str(&render_scalar(item));
                    out.push('\n');
                }
            }
            out
        }
        _ => format!("{pad}{}: {}\n", scalar_key_str(key), render_scalar(value)),
    }
}

/// Re-serialize a single scalar through serde_yaml so YAML
/// quoting (escapes, leading-special-char handling) matches what
/// the rest of the file already uses. Trailing newlines from the
/// emitter are stripped — the caller owns its own line break.
fn render_scalar(value: &serde_yaml::Value) -> String {
    let raw = serde_yaml::to_string(value).unwrap_or_default();
    raw.trim_end().to_string()
}

/// Render a mapping key for emission, quoting only when the YAML
/// 1.2 plain-scalar grammar requires it. Defers to serde_yaml's
/// emitter so the rules stay in lockstep with the rest of the
/// file: identifiers like `b@2.0.0` and `is-positive@3.1.0`
/// round-trip unquoted (the `@` is reserved only at the *start*
/// of a scalar), while keys that lead with a reserved indicator
/// or contain flow/quote/comment characters get the canonical
/// quoted form serde_yaml would have produced.
fn scalar_key_str(key: &str) -> String {
    let raw = serde_yaml::to_string(&serde_yaml::Value::String(key.to_string()))
        .unwrap_or_else(|_| format!("{key}\n"));
    raw.trim_end().to_string()
}

/// Inspect a parent block-mapping's source text to decide what
/// indent its new children should land at. The slice handed in
/// here comes from `extract_with_leading_whitespace`, so its
/// first line is already a child of the parent route — return
/// that first non-empty/non-comment line's leading whitespace.
/// Falls back to `parent_depth * INDENT_STEP` (the parent's own
/// column plus one more step) when the parent is empty: a depth-2
/// parent with no children otherwise had its children land at
/// column 2 alongside the parent itself rather than column 4.
fn detect_child_indent(parent_content: &str, parent_depth: usize) -> usize {
    for line in parent_content.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        return line.len() - trimmed.len();
    }
    parent_depth * INDENT_STEP
}

/// Extract a string view of a mapping key, erroring out for any
/// non-string variant. yaml_serde mappings allow non-string keys
/// in principle but every workspace-yaml shape aube edits uses
/// string keys exclusively, and silently dropping anything else
/// would lose data on the rewrite.
fn key_str<'a>(path: &Path, value: &'a Value) -> Result<&'a str, crate::Error> {
    match value {
        Value::String(s) => Ok(s.as_str()),
        other => Err(yp_err(
            path,
            format!("workspace yaml mapping key must be a string, got {other:?}"),
        )),
    }
}

/// Bridge `yaml_serde::Value` (our typed parse type) to
/// `serde_yaml::Value` (the manual injector's render type).
/// yaml_serde is
/// the maintained fork of serde_yaml 0.9, so a YAML round-trip is
/// lossless for every variant we use (scalars, sequences,
/// mappings, tagged values). Errors on either side propagate
/// instead of panicking — they're vanishingly rare but a
/// workspace edit is a poor place to crash the process.
fn to_serde_value(path: &Path, value: &Value) -> Result<serde_yaml::Value, crate::Error> {
    let raw = yaml_serde::to_string(value).map_err(|e| yp_err(path, e.to_string()))?;
    serde_yaml::from_str(&raw).map_err(|e| yp_err(path, e.to_string()))
}

fn yp_err(path: &Path, msg: String) -> crate::Error {
    crate::Error::YamlParse(path.to_path_buf(), msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_child_indent_reads_existing_child_indent() {
        assert_eq!(detect_child_indent("    foo: 1\n", 2), 4);
        assert_eq!(detect_child_indent("  foo: 1\n", 1), 2);
    }

    #[test]
    fn detect_child_indent_skips_blank_and_comment_lines() {
        assert_eq!(detect_child_indent("\n    # note\n    foo: 1\n", 2), 4);
    }

    #[test]
    fn detect_child_indent_falls_back_to_parent_depth() {
        // Depth-2 parent (e.g. `catalogs.evens`) with no children:
        // children should land at column 4, not column 2.
        assert_eq!(detect_child_indent("", 2), 4);
        // Depth-1 parent: children at column 2.
        assert_eq!(detect_child_indent("", 1), 2);
        // Depth-3 parent: children at column 6.
        assert_eq!(detect_child_indent("", 3), 6);
    }
}
