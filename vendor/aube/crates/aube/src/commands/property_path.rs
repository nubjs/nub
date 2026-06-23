//! Dotted/bracketed property-path navigation for `package.json` editing,
//! a faithful port of pnpm's `@pnpm/object.property-path` (parse + get +
//! set + delete). Used by `pkg` and `set-script`.
//!
//! Path grammar (matching pnpm): `foo.bar.baz`, `.foo.bar`, `foo["baz"]`,
//! `foo['bar'].baz`, `["foo"].bar`, `foo[123]`. A leading `.` is allowed.
//! Bracket segments take a quoted string or an integer literal; dot
//! segments take a bare identifier.
//!
//! Security: `__proto__`, `constructor`, and `prototype` are rejected as
//! path segments (prototype-pollution guard) — same as pnpm. In Rust over
//! `serde_json::Value` there is no prototype to pollute, but we keep the
//! rejection so behavior matches pnpm byte-for-byte and a `pkg set
//! __proto__.x=1` is refused rather than silently writing a literal key.

use miette::miette;
use serde_json::{Map, Value};

/// One resolved path segment: an object key or an array index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Key(String),
    Index(usize),
}

const UNSAFE_KEYS: [&str; 3] = ["__proto__", "constructor", "prototype"];

/// Parse a property-path string into segments. Mirrors pnpm's tokenizer:
/// identifiers after `.` (or at the start), and quoted-string / integer
/// literals inside `[...]`.
pub fn parse(path: &str) -> miette::Result<Vec<Segment>> {
    let chars: Vec<char> = path.chars().collect();
    let mut i = 0;
    let n = chars.len();
    let mut out: Vec<Segment> = Vec::new();
    // `expect_separator` is true once we've emitted a segment and the next
    // thing must be `.` or `[` (not another bare identifier) — this is how
    // pnpm rejects `foo bar` while allowing `foo.bar` and `foo[0]`.
    let mut expect_separator = false;

    while i < n {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '.' {
            // A dot introduces the next identifier segment.
            i += 1;
            // Read the identifier.
            let start = i;
            while i < n && !matches!(chars[i], '.' | '[' | ']') && !chars[i].is_whitespace() {
                i += 1;
            }
            if i == start {
                return Err(miette!(
                    "invalid property path {path:?}: empty segment after `.`"
                ));
            }
            out.push(Segment::Key(chars[start..i].iter().collect()));
            expect_separator = true;
            continue;
        }
        if c == '[' {
            i += 1;
            // Skip whitespace inside brackets.
            while i < n && chars[i].is_whitespace() {
                i += 1;
            }
            if i >= n {
                return Err(miette!("invalid property path {path:?}: unterminated `[`"));
            }
            let seg = if chars[i] == '"' || chars[i] == '\'' {
                let quote = chars[i];
                i += 1;
                let start = i;
                while i < n && chars[i] != quote {
                    i += 1;
                }
                if i >= n {
                    return Err(miette!(
                        "invalid property path {path:?}: unterminated string literal"
                    ));
                }
                let s: String = chars[start..i].iter().collect();
                i += 1; // consume closing quote
                Segment::Key(s)
            } else {
                // Integer literal.
                let start = i;
                while i < n && chars[i].is_ascii_digit() {
                    i += 1;
                }
                if i == start {
                    return Err(miette!(
                        "invalid property path {path:?}: expected string or integer inside `[]`"
                    ));
                }
                let num: String = chars[start..i].iter().collect();
                let idx = num.parse::<usize>().map_err(|_| {
                    miette!("invalid property path {path:?}: bad array index {num:?}")
                })?;
                Segment::Index(idx)
            };
            // Skip whitespace then expect `]`.
            while i < n && chars[i].is_whitespace() {
                i += 1;
            }
            if i >= n || chars[i] != ']' {
                return Err(miette!("invalid property path {path:?}: expected `]`"));
            }
            i += 1;
            out.push(seg);
            expect_separator = true;
            continue;
        }
        // A bare identifier — only legal at the very start (or, per pnpm,
        // never right after another segment without a separator).
        if expect_separator {
            return Err(miette!("invalid property path {path:?}: unexpected {c:?}"));
        }
        let start = i;
        while i < n && !matches!(chars[i], '.' | '[' | ']') && !chars[i].is_whitespace() {
            i += 1;
        }
        out.push(Segment::Key(chars[start..i].iter().collect()));
        expect_separator = true;
    }

    if out.is_empty() {
        return Err(miette!("empty property path"));
    }
    Ok(out)
}

fn reject_unsafe(segments: &[Segment]) -> miette::Result<()> {
    for seg in segments {
        if let Segment::Key(k) = seg
            && UNSAFE_KEYS.contains(&k.as_str())
        {
            return Err(miette!("refusing to use unsafe property-path key {k:?}"));
        }
    }
    Ok(())
}

/// Get the value at `path` in `root`, or `None` if any segment is missing
/// or traverses a non-container. Mirrors pnpm's `getObjectValueByPropertyPath`.
pub fn get<'a>(root: &'a Value, segments: &[Segment]) -> Option<&'a Value> {
    let mut cur = root;
    for seg in segments {
        cur = match (cur, seg) {
            (Value::Object(map), Segment::Key(k)) => map.get(k)?,
            (Value::Array(arr), Segment::Index(idx)) => arr.get(*idx)?,
            // pnpm returns undefined when an array is indexed by a
            // non-numeric segment, or any container/segment mismatch.
            _ => return None,
        };
    }
    Some(cur)
}

/// Set `value` at `path` in `root`, creating intermediate objects/arrays
/// as needed and replacing any node whose shape disagrees with the next
/// segment. Mirrors pnpm's `setObjectValueByPropertyPath`.
pub fn set(root: &mut Value, segments: &[Segment], value: Value) -> miette::Result<()> {
    reject_unsafe(segments)?;
    if segments.is_empty() {
        return Err(miette!("cannot set a value with an empty property path"));
    }
    set_inner(root, segments, value);
    Ok(())
}

fn set_inner(node: &mut Value, segments: &[Segment], value: Value) {
    let (head, rest) = segments.split_first().expect("non-empty checked by caller");
    if rest.is_empty() {
        match head {
            Segment::Key(k) => {
                let map = ensure_object(node);
                map.insert(k.clone(), value);
            }
            Segment::Index(idx) => {
                let arr = ensure_array(node);
                grow_to(arr, *idx);
                arr[*idx] = value;
            }
        }
        return;
    }
    let needs_array = matches!(rest[0], Segment::Index(_));
    match head {
        Segment::Key(k) => {
            let map = ensure_object(node);
            let child = map
                .entry(k.clone())
                .or_insert_with(|| placeholder(needs_array));
            if container_mismatch(child, needs_array) {
                *child = placeholder(needs_array);
            }
            set_inner(child, rest, value);
        }
        Segment::Index(idx) => {
            let arr = ensure_array(node);
            grow_to(arr, *idx);
            let child = &mut arr[*idx];
            if container_mismatch(child, needs_array) {
                *child = placeholder(needs_array);
            }
            set_inner(child, rest, value);
        }
    }
}

/// Delete the value at `path` in `root`. No-op if the path does not
/// resolve. Array elements are removed (shifting), not nulled — mirrors
/// pnpm's `deleteObjectValueByPropertyPath`.
pub fn delete(root: &mut Value, segments: &[Segment]) -> miette::Result<()> {
    reject_unsafe(segments)?;
    if segments.is_empty() {
        return Ok(());
    }
    let (last, parents) = segments.split_last().expect("non-empty checked above");
    // Walk to the parent container.
    let mut cur = root;
    for seg in parents {
        cur = match (cur, seg) {
            (Value::Object(map), Segment::Key(k)) => match map.get_mut(k) {
                Some(v) => v,
                None => return Ok(()),
            },
            (Value::Array(arr), Segment::Index(idx)) => match arr.get_mut(*idx) {
                Some(v) => v,
                None => return Ok(()),
            },
            _ => return Ok(()),
        };
    }
    match (cur, last) {
        (Value::Object(map), Segment::Key(k)) => {
            map.remove(k);
        }
        (Value::Array(arr), Segment::Index(idx)) if *idx < arr.len() => {
            arr.remove(*idx);
        }
        _ => {}
    }
    Ok(())
}

fn placeholder(needs_array: bool) -> Value {
    if needs_array {
        Value::Array(Vec::new())
    } else {
        Value::Object(Map::new())
    }
}

fn container_mismatch(node: &Value, needs_array: bool) -> bool {
    match node {
        Value::Object(_) => needs_array,
        Value::Array(_) => !needs_array,
        _ => true,
    }
}

fn ensure_object(node: &mut Value) -> &mut Map<String, Value> {
    if !node.is_object() {
        *node = Value::Object(Map::new());
    }
    node.as_object_mut().expect("just ensured object")
}

fn ensure_array(node: &mut Value) -> &mut Vec<Value> {
    if !node.is_array() {
        *node = Value::Array(Vec::new());
    }
    node.as_array_mut().expect("just ensured array")
}

fn grow_to(arr: &mut Vec<Value>, idx: usize) {
    while arr.len() <= idx {
        arr.push(Value::Null);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn segs(path: &str) -> Vec<Segment> {
        parse(path).unwrap()
    }

    #[test]
    fn parses_dot_bracket_and_mixed_paths() {
        assert_eq!(
            segs("foo.bar.baz"),
            vec![
                Segment::Key("foo".into()),
                Segment::Key("bar".into()),
                Segment::Key("baz".into())
            ]
        );
        assert_eq!(
            segs(".foo.bar"),
            vec![Segment::Key("foo".into()), Segment::Key("bar".into())]
        );
        assert_eq!(
            segs(r#"foo["baz"]"#),
            vec![Segment::Key("foo".into()), Segment::Key("baz".into())]
        );
        assert_eq!(
            segs("foo['bar'].baz"),
            vec![
                Segment::Key("foo".into()),
                Segment::Key("bar".into()),
                Segment::Key("baz".into())
            ]
        );
        assert_eq!(
            segs(r#"["foo"].bar"#),
            vec![Segment::Key("foo".into()), Segment::Key("bar".into())]
        );
        assert_eq!(
            segs("foo[123]"),
            vec![Segment::Key("foo".into()), Segment::Index(123)]
        );
    }

    #[test]
    fn rejects_malformed_paths() {
        assert!(parse("foo[").is_err());
        assert!(parse("foo[bar").is_err());
        assert!(parse("").is_err());
        assert!(parse("foo bar").is_err());
    }

    #[test]
    fn get_navigates_objects_and_arrays_and_misses_cleanly() {
        let v = json!({"a": {"b": [10, 20]}});
        assert_eq!(get(&v, &segs("a.b[1]")), Some(&json!(20)));
        assert_eq!(get(&v, &segs("a.b[5]")), None);
        assert_eq!(get(&v, &segs("a.missing")), None);
        // Indexing an object by number, or a scalar by anything, misses.
        assert_eq!(get(&v, &segs("a[0]")), None);
    }

    #[test]
    fn set_creates_intermediates_and_replaces_shape_mismatch() {
        let mut v = json!({});
        set(&mut v, &segs("scripts.test"), json!("vitest")).unwrap();
        assert_eq!(v, json!({"scripts": {"test": "vitest"}}));

        // A scalar in the way of a deeper write is replaced with a container.
        let mut v2 = json!({"a": 5});
        set(&mut v2, &segs("a.b"), json!(1)).unwrap();
        assert_eq!(v2, json!({"a": {"b": 1}}));

        // Numeric next-segment forces an array container.
        let mut v3 = json!({});
        set(&mut v3, &segs("a[1]"), json!("x")).unwrap();
        assert_eq!(v3, json!({"a": [null, "x"]}));
    }

    #[test]
    fn delete_removes_keys_and_splices_array_elements() {
        let mut v = json!({"a": {"b": 1, "c": 2}});
        delete(&mut v, &segs("a.b")).unwrap();
        assert_eq!(v, json!({"a": {"c": 2}}));

        let mut arr = json!({"a": [10, 20, 30]});
        delete(&mut arr, &segs("a[1]")).unwrap();
        assert_eq!(arr, json!({"a": [10, 30]}));

        // Missing path is a no-op.
        let mut v2 = json!({"a": 1});
        delete(&mut v2, &segs("x.y.z")).unwrap();
        assert_eq!(v2, json!({"a": 1}));
    }

    #[test]
    fn unsafe_keys_are_rejected_on_set_and_delete() {
        let mut v = json!({});
        assert!(set(&mut v, &segs("__proto__.polluted"), json!(true)).is_err());
        assert!(set(&mut v, &segs("constructor"), json!(1)).is_err());
        assert!(delete(&mut v, &segs("prototype.x")).is_err());
    }
}
