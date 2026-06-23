/// Post-process a `yaml_serde`-emitted pnpm-lock.yaml into the exact
/// shape real pnpm writes. Five tweaks:
///
///   0. Fold `yaml_serde`'s explicit-key form (`? 'KEY'` / `: value`)
///      back into pnpm's inline `'KEY':` form. yaml_serde switches to
///      explicit keys once a mapping key grows past ~128 bytes; deeply
///      nested peer suffixes (an eslint plugin pinned through parser +
///      eslint + typescript) cross that, while pnpm/js-yaml always quote
///      the key inline regardless of length.
///   1. Collapse `resolution:` / `engines:` block maps into flow form
///      (`resolution: {integrity: sha512-…}`). pnpm writes both inline
///      and `yaml_serde` can't be coerced into flow style per-field
///      without a custom emitter.
///   2. Collapse `cpu:` / `os:` / `libc:` block sequences into flow form
///      (`cpu: [arm64]`). pnpm writes these short architecture lists
///      inline; yaml_serde emits them as block sequences.
///   3. Re-indent the remaining block sequences (e.g.
///      `transitivePeerDependencies:`) so the `- item` lines sit two
///      spaces past the key, matching pnpm/js-yaml. yaml_serde aligns
///      list items with their key instead.
///   4. Insert blank-line separators above every top-level section
///      (`settings:`, `importers:`, `packages:`, `snapshots:`, …) and
///      between 2-indent entries inside the entry-bearing sections
///      (`importers:`, `packages:`, `snapshots:`). `catalogs:` is
///      deliberately excluded: pnpm writes the whole nested
///      catalog-name → package → {specifier, version} block tight (no
///      blank line after the header, none between catalog names), so it
///      stays out of the entry-section set.
///
/// The rewrites are textual — not YAML-aware — but the keys aube emits
/// are all simple scalars in the fixed set above, so there's nothing to
/// quote-escape. Validated by `test_write_byte_identical_to_native_pnpm`.
pub(super) fn reformat_for_pnpm_parity(yaml: &str) -> String {
    let folded = fold_explicit_keys(&yaml.lines().collect::<Vec<_>>());
    let lines: Vec<&str> = folded.iter().map(String::as_str).collect();

    // Pass 1: flow-style blocks + block-sequence re-indentation.
    let mut compact: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let stripped = line.trim_start();
        let indent = line.len() - stripped.len();
        let key = stripped.strip_suffix(':');
        let is_flow_candidate = matches!(key, Some("resolution") | Some("engines"));
        if is_flow_candidate && i + 1 < lines.len() {
            let inner_indent = indent + 2;
            let mut entries: Vec<String> = Vec::new();
            let mut all_scalar = true;
            let mut j = i + 1;
            while j < lines.len() {
                let next = lines[j];
                let n_stripped = next.trim_start();
                let n_indent = next.len() - n_stripped.len();
                if n_stripped.is_empty() || n_indent < inner_indent {
                    break;
                }
                if n_indent > inner_indent {
                    // Nested structure (e.g. a `variants:` list inside
                    // a `type: variations` resolution) — flow form
                    // can't represent it with this rewriter; keep the
                    // whole block as-is.
                    all_scalar = false;
                    break;
                }
                match n_stripped.split_once(": ") {
                    Some((k, v)) => entries.push(format!("{k}: {v}")),
                    None => {
                        // A key with no inline value (`variants:`)
                        // introduces a nested block — leave it alone.
                        all_scalar = false;
                        break;
                    }
                }
                j += 1;
            }
            // pnpm renders `binary` / `variations` resolutions in
            // block form even when (like a map-less binary) every
            // field happens to be scalar — match that.
            let block_form_type = entries
                .iter()
                .any(|e| e == "type: binary" || e == "type: variations");
            if all_scalar && !block_form_type && !entries.is_empty() {
                compact.push(format!(
                    "{}{}: {{{}}}",
                    " ".repeat(indent),
                    // `is_flow_candidate` already matched `key` as
                    // `Some("resolution" | "engines")`, so this can't panic.
                    key.unwrap(),
                    entries.join(", ")
                ));
                i = j;
                continue;
            }
        }

        // Flow-style `cpu:` / `os:` / `libc:` sequences. yaml_serde
        // aligns the `- item` lines with the key; collect them and
        // inline as `cpu: [arm64]`. Binding the key name in the pattern
        // (rather than re-matching + `unwrap()`) keeps the branch panic-free.
        if let Some(arch_key @ ("cpu" | "os" | "libc")) = key
            && let Some((items, next_i)) = gather_block_seq(&lines, i, indent)
        {
            compact.push(format!(
                "{}{}: [{}]",
                " ".repeat(indent),
                arch_key,
                items.join(", ")
            ));
            i = next_i;
            continue;
        }

        // Remaining block sequences (`transitivePeerDependencies:`, …):
        // keep block form but push each item two spaces past the key so
        // the indentation matches pnpm.
        if key.is_some()
            && let Some((items, next_i)) = gather_block_seq(&lines, i, indent)
        {
            compact.push(line.to_string());
            for item in items {
                compact.push(format!("{}- {}", " ".repeat(indent + 2), item));
            }
            i = next_i;
            continue;
        }

        compact.push(line.to_string());
        i += 1;
    }

    // Pass 2: blank-line separators.
    // Sections where each 2-indent key-ending-in-`:` is an entry header
    // that pnpm separates with a blank line above. `overrides:` /
    // `time:` / `settings:` carry scalar key→value pairs instead and
    // stay tight. `catalogs:` is also tight: its 2-indent keys are
    // catalog *names* (`default:`), and pnpm emits the whole nested
    // block without blank lines (verified against pnpm v11 output) —
    // including it here would wrongly inject a blank after `catalogs:`
    // and between catalog names.
    const ENTRY_SECTIONS: &[&str] = &["importers:", "packages:", "snapshots:"];
    let mut out = String::with_capacity(yaml.len() + 512);
    let mut in_entries = false;
    for (idx, line) in compact.iter().enumerate() {
        let stripped = line.trim_start();
        let indent = line.len() - stripped.len();
        let is_top = indent == 0 && !stripped.is_empty();
        // Entry headers inside `packages:` / `snapshots:` are always at
        // 2-indent with a `:` in the line. Either trailing (`foo@1:`
        // with a child block below) or inline (`foo@1: {}` for empty
        // snapshots). List markers (`- …`) never appear at this level,
        // so a leading `-` rules out false positives on
        // `ignoredOptionalDependencies:` items.
        let is_entry_header =
            in_entries && indent == 2 && !stripped.starts_with('-') && stripped.contains(':');

        if (is_top && idx > 0) || is_entry_header {
            out.push('\n');
        }
        out.push_str(line);
        out.push('\n');

        if is_top {
            in_entries = ENTRY_SECTIONS.contains(&stripped);
        }
    }
    out
}

/// Fold `yaml_serde`'s explicit-key form back into pnpm's inline form.
///
/// Past ~128 bytes yaml_serde emits a mapping entry as
///
/// ```text
///   ? 'very…long…key'
///   : dependencies:
///       dep: 1.0.0
///     transitivePeerDependencies:
///     - supports-color
/// ```
///
/// pnpm/js-yaml always write the quoted key inline:
///
/// ```text
///   'very…long…key':
///     dependencies:
///       dep: 1.0.0
///     transitivePeerDependencies:
///     - supports-color
/// ```
///
/// The value block under `: ` already carries pnpm-equivalent indentation,
/// so only the key line and the first value line need rewriting: drop the
/// `? ` indicator, and replace the `: ` indicator with two spaces (or keep
/// an inline `{}` value on the key line). Later passes then collapse/
/// re-indent the value block exactly as they do for inline-key entries.
fn fold_explicit_keys(lines: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let stripped = line.trim_start();
        let indent = line.len() - stripped.len();
        if let Some(key) = stripped.strip_prefix("? ") {
            // The value indicator is the next non-blank line at the same
            // indent (yaml_serde never separates them, but skip blanks
            // defensively).
            let mut j = i + 1;
            while j < lines.len() && lines[j].trim().is_empty() {
                j += 1;
            }
            if let Some(vline) = lines.get(j) {
                let v_stripped = vline.trim_start();
                let v_indent = vline.len() - v_stripped.len();
                let value = v_stripped
                    .strip_prefix(": ")
                    .or_else(|| (v_stripped == ":").then_some(""));
                if v_indent == indent
                    && let Some(rest) = value
                {
                    let pad = " ".repeat(indent);
                    if rest.is_empty() || rest.ends_with(':') {
                        // Block-map value: key on its own line, the first
                        // child re-indented two spaces past the key.
                        out.push(format!("{pad}{key}:"));
                        if !rest.is_empty() {
                            out.push(format!("{pad}  {rest}"));
                        }
                    } else {
                        // Inline value such as `{}` stays on the key line.
                        out.push(format!("{pad}{key}: {rest}"));
                    }
                    i = j + 1;
                    continue;
                }
            }
        }
        out.push(line.to_string());
        i += 1;
    }
    out
}

/// Collect a *scalar* `yaml_serde`-emitted block sequence whose key is on
/// `lines[key_idx]`. yaml_serde aligns `- item` lines with their key, so
/// the items sit at `key_indent` (not `key_indent + 2`). Returns the item
/// values (with the `- ` marker stripped) and the index of the first line
/// past the sequence.
///
/// Returns `None` when the key does not introduce a scalar sequence:
/// either the next line is not a `- ` item (a map or scalar), or an item
/// is followed by a deeper continuation line — i.e. the sequence holds
/// map items (the runtime-pin `variants:` / `targets:` lists). Those can't
/// be re-indented by a flat textual shift without corrupting their nested
/// structure, so the caller leaves them exactly as `yaml_serde` wrote them.
fn gather_block_seq(
    lines: &[&str],
    key_idx: usize,
    key_indent: usize,
) -> Option<(Vec<String>, usize)> {
    let mut items = Vec::new();
    let mut j = key_idx + 1;
    while j < lines.len() {
        let next = lines[j];
        let n_stripped = next.trim_start();
        let n_indent = next.len() - n_stripped.len();
        if n_indent != key_indent || !n_stripped.starts_with("- ") {
            break;
        }
        items.push(n_stripped[2..].to_string());
        j += 1;
    }
    if items.is_empty() {
        return None;
    }
    // Pure-scalar guard: a line deeper than the key after the last item
    // is a map-item continuation (e.g. `variants:` holds `- resolution:`
    // blocks). Bail so the caller leaves such sequences untouched.
    //
    // NOTE: this detects map items only via a deeper continuation line.
    // A sequence of single-field inline maps with no continuation
    // (`- type: tarball` directly followed by a sibling key at the key's
    // indent) would slip through and be re-indented as if scalar. aube
    // never emits that shape — every map-item sequence in pnpm-lock
    // (`variants:` / `targets:`) carries nested content — and
    // `leaves_map_item_sequences_untouched` covers the real paths. Any
    // new yaml_serde-emitted sequence shape must re-check this guard.
    if let Some(stop) = lines.get(j) {
        let s = stop.trim_start();
        let stop_indent = stop.len() - s.len();
        if !s.is_empty() && stop_indent > key_indent {
            return None;
        }
    }
    Some((items, j))
}

#[cfg(test)]
mod tests {
    use super::reformat_for_pnpm_parity;

    #[test]
    fn collapses_cpu_os_libc_into_flow_sequences() {
        // yaml_serde aligns block-sequence items with their key; pnpm
        // writes these short architecture lists inline.
        let input = "packages:\n  '@rollup/rollup-darwin-arm64@4.61.0':\n    resolution: {integrity: sha512-aaa==}\n    cpu:\n    - arm64\n    os:\n    - darwin\n    libc:\n    - glibc\n";
        let out = reformat_for_pnpm_parity(input);
        assert!(out.contains("    cpu: [arm64]\n"), "cpu flow:\n{out}");
        assert!(out.contains("    os: [darwin]\n"), "os flow:\n{out}");
        assert!(out.contains("    libc: [glibc]\n"), "libc flow:\n{out}");
        // No leftover block-sequence dashes for these keys.
        assert!(!out.contains("- arm64"), "no block cpu:\n{out}");
        assert!(!out.contains("- darwin"), "no block os:\n{out}");
    }

    #[test]
    fn flow_sequence_keeps_multiple_items_comma_separated() {
        let input = "packages:\n  pkg@1.0.0:\n    os:\n    - darwin\n    - linux\n";
        let out = reformat_for_pnpm_parity(input);
        assert!(out.contains("    os: [darwin, linux]\n"), "{out}");
    }

    #[test]
    fn reindents_transitive_peer_dependencies_two_spaces() {
        // pnpm indents block-sequence items two spaces past the key;
        // yaml_serde aligns them with the key.
        let input = "snapshots:\n  rollup@4.61.0:\n    dependencies:\n      '@types/estree': 1.0.9\n    transitivePeerDependencies:\n    - supports-color\n";
        let out = reformat_for_pnpm_parity(input);
        assert!(
            out.contains("    transitivePeerDependencies:\n      - supports-color\n"),
            "tPD reindented:\n{out}"
        );
    }

    #[test]
    fn catalogs_block_stays_tight_like_pnpm() {
        // pnpm v11 writes `catalogs:` as one tight nested block: no
        // blank line after the header, none between catalog names. Only
        // the top-level section separators (blank line *before*
        // `catalogs:` and before the next section) apply.
        let input = "settings:\n  autoInstallPeers: true\ncatalogs:\n  default:\n    esbuild:\n      specifier: ^0.27.0\n      version: 0.27.7\n  evens:\n    is-even:\n      specifier: ^1.0.0\n      version: 1.0.0\nimporters:\n  .:\n    dependencies:\n      esbuild:\n        specifier: 'catalog:'\n        version: 0.27.7\n";
        let out = reformat_for_pnpm_parity(input);
        // No blank line after the `catalogs:` header…
        assert!(
            out.contains("catalogs:\n  default:\n"),
            "tight header:\n{out}"
        );
        // …and none between catalog names.
        assert!(
            out.contains("      version: 0.27.7\n  evens:\n"),
            "tight catalog names:\n{out}"
        );
        // Top-level separators are still present: a blank line before
        // `catalogs:` and before the following `importers:` section.
        assert!(
            out.contains("\n\ncatalogs:\n"),
            "blank before catalogs:\n{out}"
        );
        assert!(
            out.contains("\n\nimporters:\n"),
            "blank before importers:\n{out}"
        );
    }

    #[test]
    fn folds_explicit_long_keys_into_inline_form() {
        // yaml_serde emits a >128-byte mapping key in explicit `? `/`: `
        // form; pnpm always writes the quoted key inline. A multi-key value
        // block (dependencies + optionalDependencies + transitivePeer-
        // Dependencies) must end up indented exactly like an inline-key
        // snapshot — every child carried over verbatim, not just the first.
        let long = "@typescript-eslint/eslint-plugin@7.18.0(@typescript-eslint/parser@7.18.0(eslint@8.57.1)(typescript@5.6.3))(eslint@8.57.1)(typescript@5.6.3)";
        let input = format!(
            "snapshots:\n  ? '{long}'\n  : dependencies:\n      '@typescript-eslint/parser': 7.18.0(eslint@8.57.1)(typescript@5.6.3)\n    optionalDependencies:\n      typescript: 5.6.3\n    transitivePeerDependencies:\n    - supports-color\n"
        );
        let out = reformat_for_pnpm_parity(&input);
        assert!(
            out.contains(&format!("  '{long}':\n")),
            "inline key:\n{out}"
        );
        assert!(
            out.contains("    dependencies:\n      '@typescript-eslint/parser': 7.18.0(eslint@8.57.1)(typescript@5.6.3)\n"),
            "deps reindented:\n{out}"
        );
        assert!(
            out.contains("    optionalDependencies:\n      typescript: 5.6.3\n"),
            "optionalDependencies carried over verbatim:\n{out}"
        );
        assert!(
            out.contains("    transitivePeerDependencies:\n      - supports-color\n"),
            "tPD reindented:\n{out}"
        );
        assert!(!out.contains("? '"), "no explicit key left:\n{out}");
        assert!(!out.contains("\n  : "), "no value indicator left:\n{out}");
    }

    #[test]
    fn folds_explicit_long_key_with_empty_map_value() {
        // An empty snapshot (`{}`) with a long key keeps the value inline.
        let long = "a-very-long-package-name-that-definitely-exceeds-the-yaml-serde-explicit-key-threshold@1.0.0(peer-one@1.0.0)(peer-two@2.0.0)(peer-three@3.0.0)";
        let input = format!("snapshots:\n  ? '{long}'\n  : {{}}\n");
        let out = reformat_for_pnpm_parity(&input);
        assert!(
            out.contains(&format!("  '{long}': {{}}\n")),
            "inline empty:\n{out}"
        );
        assert!(!out.contains("? '"), "no explicit key left:\n{out}");
    }

    #[test]
    fn leaves_map_item_sequences_untouched() {
        // A runtime-pin `variants:` / `targets:` block holds map items
        // (each `- resolution:` carries a nested block). A flat +2 shift
        // can't re-indent those without desyncing the nested keys, so the
        // rewriter must leave them exactly as yaml_serde wrote them.
        let input = "packages:\n  node@runtime:24.4.1:\n    resolution:\n      type: variations\n      variants:\n      - resolution:\n          archive: tarball\n          type: binary\n        targets:\n        - cpu: arm64\n          os: darwin\n";
        let out = reformat_for_pnpm_parity(input);
        // `variants:` map items stay at the key's own indent (untouched).
        assert!(
            out.contains("      variants:\n      - resolution:\n"),
            "{out}"
        );
        // Inner `targets:` map items likewise untouched.
        assert!(
            out.contains("        targets:\n        - cpu: arm64\n"),
            "{out}"
        );
        // The variations resolution is not flow-collapsed.
        assert!(!out.contains("resolution: {type: variations"), "{out}");
    }
}
