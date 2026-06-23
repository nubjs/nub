use super::{jsonc::strip_jsonc, parse, raw::is_integrity_hash, source::split_ident, write};
use crate::{DepType, DirectDep, LocalSource, LockedPackage, LockfileGraph};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[test]
fn test_split_ident() {
    assert_eq!(
        split_ident("foo@1.2.3"),
        Some(("foo".to_string(), "1.2.3".to_string()))
    );
    assert_eq!(
        split_ident("@scope/pkg@1.0.0"),
        Some(("@scope/pkg".to_string(), "1.0.0".to_string()))
    );
}

#[test]
fn test_is_integrity_hash() {
    // Real SRI hashes at their exact base64 lengths.
    assert!(is_integrity_hash(&format!("sha512-{}", "A".repeat(88))));
    assert!(is_integrity_hash(&format!("sha256-{}", "A".repeat(44))));
    assert!(is_integrity_hash(&format!("sha1-{}", "A".repeat(28))));
    // base64 body with +, /, and = padding is still valid.
    let mixed = format!("{}+/==", "A".repeat(84));
    assert_eq!(mixed.len(), 88);
    assert!(is_integrity_hash(&format!("sha512-{mixed}")));

    // Github dir-id whose owner is literally a hash algo name —
    // the extra `-` and the wrong length must disqualify it.
    assert!(!is_integrity_hash("sha1-myrepo-abc123"));
    assert!(!is_integrity_hash("sha256-owner-repo-deadbee"));
    // Unknown algo prefix.
    assert!(!is_integrity_hash("foo-bar"));
    // Correct algo prefix but the wrong body length.
    assert!(!is_integrity_hash("sha512-tooshort"));
    // Right length but contains a forbidden `-` (base64 has no `-`).
    let with_dash = format!("sha512-{}-{}", "A".repeat(43), "A".repeat(44));
    assert_eq!(with_dash.len(), "sha512-".len() + 88);
    assert!(!is_integrity_hash(&with_dash));
    // No dash at all.
    assert!(!is_integrity_hash("opaquestring"));
}

#[test]
fn test_strip_jsonc_trailing_comma() {
    let input = r#"{ "a": 1, "b": 2, }"#;
    let out = strip_jsonc(input);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["a"], 1);
    assert_eq!(v["b"], 2);
}

#[test]
fn test_strip_jsonc_line_comment() {
    let input = "{ // comment\n  \"a\": 1 }";
    let out = strip_jsonc(input);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["a"], 1);
}

#[test]
fn test_strip_jsonc_respects_strings() {
    // Make sure we don't strip things that look like comments inside strings
    let input = r#"{ "url": "http://example.com/path" }"#;
    let out = strip_jsonc(input);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["url"], "http://example.com/path");
}

#[test]
fn strip_jsonc_preserves_utf8_string_value() {
    let input = "{ \"name\": \"café\" }";
    let out = strip_jsonc(input);
    assert_eq!(out.len(), input.len());
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["name"], "café");
}

#[test]
fn strip_jsonc_preserves_offsets_for_nonascii_in_comments() {
    let input = "{ // café\n  \"a\": 1 }";
    let out = strip_jsonc(input);
    assert_eq!(out.len(), input.len());
}

/// `strip_jsonc` must preserve byte offsets so a `serde_json` error
/// on the stripped buffer maps 1:1 onto the original file — that's
/// the only reason `parse()` can hand `raw_content` to miette's
/// `NamedSource` and trust the span.
#[test]
fn test_strip_jsonc_preserves_byte_offsets() {
    let cases = [
        "{ \"a\": 1 }",                    // no-op
        "{ // line\n  \"a\": 1 }",         // line comment
        "{ /* block */ \"a\": 1 }",        // block comment
        "{ /* multi\nline */ \"a\": 1 }",  // block spans newline
        "{ \"a\": 1, \"b\": 2, }",         // trailing comma
        "{ \"a\": \"// not a comment\" }", // comment inside string
        "{ \"a\": 1 /* trailing",          // unterminated block
    ];
    for input in cases {
        let out = strip_jsonc(input);
        assert_eq!(
            out.len(),
            input.len(),
            "length mismatch stripping {input:?} -> {out:?}"
        );
        // Every `\n` must land at the same byte offset so line
        // numbers stay stable between the raw and cleaned buffers.
        let raw_nls: Vec<usize> = input.match_indices('\n').map(|(i, _)| i).collect();
        let out_nls: Vec<usize> = out.match_indices('\n').map(|(i, _)| i).collect();
        assert_eq!(raw_nls, out_nls, "newline drift stripping {input:?}");
    }
}

/// Build a placeholder SRI hash of the right shape (88-char base64
/// body for sha512). Tests need real SRI lengths now that
/// `is_integrity_hash` validates them — bogus stand-ins like
/// `sha512-aaa` would be rejected and integrity dropped.
fn fake_sri(tag: char) -> String {
    format!("sha512-{}", tag.to_string().repeat(88))
}

#[test]
fn test_parse_simple() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let sri_foo = fake_sri('a');
    let sri_nested = fake_sri('b');
    let sri_bar = fake_sri('c');
    let content = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": {
      "name": "test",
      "dependencies": {
        "foo": "^1.0.0",
      },
      "devDependencies": {
        "bar": "^2.0.0",
      },
    },
  },
  "packages": {
    "foo": ["foo@1.2.3", "", { "dependencies": { "nested": "^3.0.0" } }, "SRI_FOO"],
    "nested": ["nested@3.1.0", "", {}, "SRI_NESTED"],
    "bar": ["bar@2.5.0", "", {}, "SRI_BAR"],
  }
}"#
    .replace("SRI_FOO", &sri_foo)
    .replace("SRI_NESTED", &sri_nested)
    .replace("SRI_BAR", &sri_bar);
    std::fs::write(tmp.path(), &content).unwrap();
    let graph = parse(tmp.path()).unwrap();

    assert_eq!(graph.packages.len(), 3);
    assert!(graph.packages.contains_key("foo@1.2.3"));
    assert!(graph.packages.contains_key("nested@3.1.0"));
    assert!(graph.packages.contains_key("bar@2.5.0"));

    let foo = &graph.packages["foo@1.2.3"];
    assert_eq!(foo.integrity.as_deref(), Some(sri_foo.as_str()));
    assert_eq!(
        foo.dependencies.get("nested").map(String::as_str),
        Some("3.1.0")
    );

    let root = graph.importers.get(".").unwrap();
    assert_eq!(root.len(), 2);
    assert!(
        root.iter()
            .any(|d| d.name == "foo" && d.dep_type == DepType::Production)
    );
    assert!(
        root.iter()
            .any(|d| d.name == "bar" && d.dep_type == DepType::Dev)
    );
}

/// bun installs a root-workspace `peerDependencies` entry that is not
/// listed in `optionalPeers` — a required root peer is linked into the
/// root `node_modules` like a regular dep. The importer-build pass only
/// walked `dependencies` / `devDependencies` / `optionalDependencies`,
/// so a required root peer was dropped from `importers["."]` and never
/// linked: the package sits in `packages:` but `node_modules/<peer>`
/// goes missing, breaking downstream type-checks/builds that import it.
///
/// Shape taken from elysiajs/elysia's committed bun.lock, whose root
/// peer `openapi-types` (required — absent from `optionalPeers`) went
/// unlinked, found by differential corpus testing against elysia.
#[test]
fn test_parse_links_required_root_workspace_peer() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let sri_cookie = fake_sri('a');
    let sri_openapi = fake_sri('b');
    let sri_typebox = fake_sri('c');
    let content = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": {
      "name": "elysia",
      "dependencies": {
        "cookie": "^1.1.1"
      },
      "peerDependencies": {
        "@sinclair/typebox": ">= 0.34.0 < 1",
        "openapi-types": ">= 12.0.0",
        "typescript": ">= 5.0.0"
      },
      "optionalPeers": [
        "typescript"
      ]
    }
  },
  "packages": {
    "cookie": ["cookie@1.1.1", "", {}, "SRI_COOKIE"],
    "openapi-types": ["openapi-types@12.1.3", "", {}, "SRI_OPENAPI"],
    "@sinclair/typebox": ["@sinclair/typebox@0.34.0", "", {}, "SRI_TYPEBOX"]
  }
}"#
    .replace("SRI_COOKIE", &sri_cookie)
    .replace("SRI_OPENAPI", &sri_openapi)
    .replace("SRI_TYPEBOX", &sri_typebox);
    std::fs::write(tmp.path(), &content).unwrap();
    let graph = parse(tmp.path()).unwrap();

    let root = graph.importers.get(".").unwrap();

    // Required root peers (not in optionalPeers) are linked as direct
    // deps; `typescript` is in optionalPeers and has no package entry,
    // so it stays unlinked — matching bun.
    let openapi = root
        .iter()
        .find(|d| d.name == "openapi-types")
        .expect("required root peer openapi-types must be linked as a direct dep");
    assert_eq!(openapi.dep_path, "openapi-types@12.1.3");
    assert!(
        root.iter().any(|d| d.name == "@sinclair/typebox"),
        "required root peer @sinclair/typebox must be linked"
    );
    assert!(
        !root.iter().any(|d| d.name == "typescript"),
        "optional root peer typescript must not be force-linked"
    );
}

#[test]
fn test_parse_bun_lifecycle_deps_as_dep_path_tails() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let sri_bufferutil = fake_sri('a');
    let sri_node_gyp_build = fake_sri('b');
    let sri_electron = fake_sri('c');
    let sri_electron_get = fake_sri('d');
    let content = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": {
      "dependencies": {
        "bufferutil": "4.0.9",
        "electron": "39.2.7"
      }
    }
  },
  "packages": {
    "bufferutil": ["bufferutil@4.0.9", "", { "dependencies": { "node-gyp-build": "^4.3.0" } }, "SRI_BUFFERUTIL"],
    "node-gyp-build": ["node-gyp-build@4.8.4", "", { "bin": { "node-gyp-build": "bin.js" } }, "SRI_NODE_GYP_BUILD"],
    "electron": ["electron@39.2.7", "", { "dependencies": { "@electron/get": "^2.0.0" } }, "SRI_ELECTRON"],
    "@electron/get": ["@electron/get@2.0.3", "", {}, "SRI_ELECTRON_GET"]
  }
}"#
        .replace("SRI_BUFFERUTIL", &sri_bufferutil)
        .replace("SRI_NODE_GYP_BUILD", &sri_node_gyp_build)
        .replace("SRI_ELECTRON", &sri_electron)
        .replace("SRI_ELECTRON_GET", &sri_electron_get);
    std::fs::write(tmp.path(), &content).unwrap();
    let graph = parse(tmp.path()).unwrap();

    let bufferutil = &graph.packages["bufferutil@4.0.9"];
    assert_eq!(
        bufferutil
            .dependencies
            .get("node-gyp-build")
            .map(String::as_str),
        Some("4.8.4")
    );

    let electron = &graph.packages["electron@39.2.7"];
    assert_eq!(
        electron
            .dependencies
            .get("@electron/get")
            .map(String::as_str),
        Some("2.0.3")
    );

    let root = graph.importers.get(".").unwrap();
    assert!(
        root.iter()
            .any(|d| d.name == "bufferutil" && d.dep_path == "bufferutil@4.0.9")
    );
    assert!(
        root.iter()
            .any(|d| d.name == "electron" && d.dep_path == "electron@39.2.7")
    );
}

#[test]
fn test_parse_multi_version_nested() {
    // bun keys nested packages using "parent/child" paths.
    // Here `bar` exists hoisted at 2.0.0 and nested under `foo` at 1.0.0.
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let sri_top_bar = fake_sri('a');
    let sri_foo = fake_sri('b');
    let sri_nested_bar = fake_sri('c');
    let content = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": {
      "dependencies": { "foo": "^1.0.0", "bar": "^2.0.0" }
    }
  },
  "packages": {
    "bar": ["bar@2.0.0", "", {}, "SRI_TOP_BAR"],
    "foo": ["foo@1.0.0", "", { "dependencies": { "bar": "^1.0.0" } }, "SRI_FOO"],
    "foo/bar": ["bar@1.0.0", "", {}, "SRI_NESTED_BAR"]
  }
}"#
    .replace("SRI_TOP_BAR", &sri_top_bar)
    .replace("SRI_FOO", &sri_foo)
    .replace("SRI_NESTED_BAR", &sri_nested_bar);
    std::fs::write(tmp.path(), &content).unwrap();
    let graph = parse(tmp.path()).unwrap();

    assert!(graph.packages.contains_key("bar@2.0.0"));
    assert!(graph.packages.contains_key("bar@1.0.0"));
    assert!(graph.packages.contains_key("foo@1.0.0"));

    // foo's transitive must be the nested bar@1.0.0
    let foo = &graph.packages["foo@1.0.0"];
    assert_eq!(
        foo.dependencies.get("bar").map(String::as_str),
        Some("1.0.0")
    );

    // Root direct bar is the hoisted 2.0.0
    let root = graph.importers.get(".").unwrap();
    let bar = root.iter().find(|d| d.name == "bar").unwrap();
    assert_eq!(bar.dep_path, "bar@2.0.0");
}

#[test]
fn test_parse_scoped() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let sri = fake_sri('s');
    let content = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": {
      "dependencies": { "@scope/pkg": "^1.0.0" }
    }
  },
  "packages": {
    "@scope/pkg": ["@scope/pkg@1.0.0", "", {}, "SRI"]
  }
}"#
    .replace("SRI", &sri);
    std::fs::write(tmp.path(), &content).unwrap();
    let graph = parse(tmp.path()).unwrap();
    assert!(graph.packages.contains_key("@scope/pkg@1.0.0"));
    let root = graph.importers.get(".").unwrap();
    assert_eq!(root[0].name, "@scope/pkg");
}

/// bun.lock uses a 3-tuple `[ident, { meta }, "owner-repo-commit"]`
/// for GitHub / git deps (no `resolved` slot and no integrity). A
/// naive positional parse would mistake the trailing commit-id
/// string for the metadata object — make sure we recognize the
/// object by type rather than position.
#[test]
fn test_parse_github_dep() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let sri_dep = fake_sri('d');
    let content = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": {
      "dependencies": { "vfs": "github:collinstevens/vfs#0b6ea53" }
    }
  },
  "packages": {
    "vfs": ["vfs@github:collinstevens/vfs#0b6ea53abcdef", { "dependencies": { "dep": "^1.0.0" } }, "collinstevens-vfs-0b6ea53"],
    "dep": ["dep@1.0.0", "", {}, "SRI_DEP"]
  }
}"#
        .replace("SRI_DEP", &sri_dep);
    std::fs::write(tmp.path(), &content).unwrap();
    let graph = parse(tmp.path()).unwrap();

    // The vfs package parsed with its github: version and picked up
    // the transitive dep declared in the metadata slot.
    let vfs_key = "vfs@github:collinstevens/vfs#0b6ea53abcdef";
    assert!(graph.packages.contains_key(vfs_key));
    let vfs = &graph.packages[vfs_key];
    assert_eq!(
        vfs.dependencies.get("dep").map(String::as_str),
        Some("1.0.0")
    );
    // No SRI-shaped hash on the github entry → integrity stays None.
    assert!(vfs.integrity.is_none());

    // The adjacent registry dep's integrity must still round-trip —
    // proves the type-based introspection doesn't break the normal
    // 4-tuple path when mixed with a 3-tuple github entry.
    let dep = &graph.packages["dep@1.0.0"];
    assert_eq!(dep.integrity.as_deref(), Some(sri_dep.as_str()));

    let root = graph.importers.get(".").unwrap();
    assert!(root.iter().any(|d| d.name == "vfs"));
}

#[test]
fn test_parse_prefixless_local_tarball() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let sri = fake_sri('t');
    let content = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": {
      "dependencies": { "local-helper": "file:tarballs/local-helper-1.0.0.tgz" }
    }
  },
  "packages": {
    "local-helper": ["local-helper@tarballs/local-helper-1.0.0.tgz", {}, "SRI"]
  }
}"#
    .replace("SRI", &sri);
    std::fs::write(tmp.path(), &content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    let pkg = &graph.packages["local-helper@tarballs/local-helper-1.0.0.tgz"];
    assert!(
        matches!(pkg.local_source, Some(LocalSource::Tarball(_))),
        "prefixless bun tarball ident must be LocalSource::Tarball, got {:?}",
        pkg.local_source
    );
}

/// Round-trip the same multi-version shape the npm writer test
/// uses: two versions of `bar`, one hoisted, one nested under
/// `foo`. The writer's bun-key form (`foo/bar` instead of
/// `node_modules/foo/node_modules/bar`) must round-trip through
/// the bun parser without losing the nested version.
#[test]
fn test_write_roundtrip_multi_version() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let sri_top = fake_sri('t');
    let sri_foo = fake_sri('f');
    let sri_nested = fake_sri('n');
    let content = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": {
      "dependencies": { "foo": "^1.0.0", "bar": "^2.0.0" }
    }
  },
  "packages": {
    "bar": ["bar@2.0.0", "", {}, "SRI_TOP"],
    "foo": ["foo@1.0.0", "", { "dependencies": { "bar": "^1.0.0" } }, "SRI_FOO"],
    "foo/bar": ["bar@1.0.0", "", {}, "SRI_NESTED"]
  }
}"#
    .replace("SRI_TOP", &sri_top)
    .replace("SRI_FOO", &sri_foo)
    .replace("SRI_NESTED", &sri_nested);
    std::fs::write(tmp.path(), &content).unwrap();
    let graph = parse(tmp.path()).unwrap();

    let manifest = aube_manifest::PackageJson {
        name: Some("test".to_string()),
        version: Some("1.0.0".to_string()),
        dependencies: [
            ("foo".to_string(), "^1.0.0".to_string()),
            ("bar".to_string(), "^2.0.0".to_string()),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    };

    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();
    let reparsed = parse(out.path()).unwrap();

    assert!(reparsed.packages.contains_key("bar@2.0.0"));
    assert!(reparsed.packages.contains_key("bar@1.0.0"));
    assert!(reparsed.packages.contains_key("foo@1.0.0"));
    assert_eq!(
        reparsed.packages["bar@2.0.0"].integrity.as_deref(),
        Some(sri_top.as_str())
    );
    assert_eq!(
        reparsed.packages["bar@1.0.0"].integrity.as_deref(),
        Some(sri_nested.as_str())
    );
    // foo's nested bar dep still resolves to 1.0.0 (nested)
    // rather than snapping to the hoisted 2.0.0.
    assert_eq!(
        reparsed.packages["foo@1.0.0"]
            .dependencies
            .get("bar")
            .map(String::as_str),
        Some("1.0.0")
    );
}

/// Byte-parity with a real `bun install`-generated lockfile — the
/// fixture at `tests/fixtures/bun-native.lock` was produced by
/// bun 1.3 against a `{ chalk, picocolors, semver }` manifest. A
/// parse → write round-trip must reproduce the exact bytes;
/// anything less means `aube install --no-frozen-lockfile` churns
/// someone's bun.lock in git when nothing in the graph moved.
/// Covers the format fixes (`configVersion`, no workspace
/// `version`, trailing commas, single-line package arrays) plus
/// the data-model fixes that ride with them (declared-range
/// preservation in `declared_dependencies`, `bin:` map
/// round-trip).
#[test]
fn test_write_byte_identical_to_native_bun() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bun-native.lock");
    // Normalize line endings — Windows' `core.autocrlf=true` can
    // rewrite the checked-out fixture to CRLF even with
    // `.gitattributes eol=lf`; compare against LF form explicitly.
    let original = std::fs::read_to_string(&fixture)
        .unwrap()
        .replace("\r\n", "\n");
    let graph = parse(&fixture).unwrap();
    let manifest = aube_manifest::PackageJson {
        name: Some("aube-lockfile-stability".to_string()),
        version: Some("1.0.0".to_string()),
        dependencies: [
            ("chalk".to_string(), "^4.1.2".to_string()),
            ("picocolors".to_string(), "^1.1.1".to_string()),
            ("semver".to_string(), "^7.6.3".to_string()),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    };

    let tmp = tempfile::NamedTempFile::new().unwrap();
    write(tmp.path(), &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(tmp.path()).unwrap();

    if written != original {
        panic!(
            "bun writer drifted from native bun output.\n\n--- expected ---\n{original}\n--- got ---\n{written}"
        );
    }
}

/// RT-1: the top-level metadata blocks must round-trip
/// byte-identically in bun's native order — `trustedDependencies →
/// patchedDependencies → overrides → catalog` (between `workspaces`
/// and `packages`). The pre-existing byte-identity fixture carries no
/// top-level metadata blocks, so a wrong block order slipped through.
///
/// Scope note: this asserts byte-identity for the four blocks bun and
/// nub render identically. Named `catalogs` (object-of-objects) and an
/// EMPTY `packages` block are deliberately excluded — nub currently
/// renders a named catalog's inner object inline (bun renders it
/// multi-line) and emits `"packages": {\n  }` for empty (bun emits
/// `{}`). Those two rendering drifts are real but out of this lane's
/// scope (B-7 is block *order*, not these renderings); they're tracked
/// as follow-ups. `catalogs` ordering/preservation is still covered
/// below by a parse round-trip.
#[test]
fn test_write_byte_identical_top_level_block_order() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let sri = fake_sri('a');
    // Hand-authored in bun 1.3.x's exact JSONC style: 2-space indent,
    // trailing commas on nested-object members, blocks in bun's order,
    // a non-empty `packages` section.
    let original = format!(
        r#"{{
  "lockfileVersion": 1,
  "configVersion": 1,
  "workspaces": {{
    "": {{
      "name": "root",
      "dependencies": {{
        "lodash": "^4.17.21",
      }},
    }},
  }},
  "trustedDependencies": ["esbuild", "sharp"],
  "patchedDependencies": {{
    "lodash@4.17.21": "patches/lodash@4.17.21.patch",
  }},
  "overrides": {{
    "lodash": "^4.17.21",
  }},
  "catalog": {{
    "react": "^18.2.0",
  }},
  "packages": {{
    "lodash": ["lodash@4.17.21", "", {{}}, "{sri}"],
  }}
}}
"#
    );
    std::fs::write(tmp.path(), &original).unwrap();
    let graph = parse(tmp.path()).unwrap();

    let manifest = aube_manifest::PackageJson {
        name: Some("root".to_string()),
        dependencies: [("lodash".to_string(), "^4.17.21".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(out.path()).unwrap();

    assert_eq!(
        written, original,
        "top-level block order/rendering drifted from bun's native output"
    );

    // Named `catalogs` still round-trips through parse (the byte-render
    // of its inner object is a separate, tracked drift) — guard the
    // data path here so the block isn't silently dropped.
    let with_catalogs = r#"{
  "lockfileVersion": 1,
  "workspaces": { "": { "name": "root" } },
  "catalogs": {
    "evens": { "date-fns": "^2.30.0" }
  },
  "packages": {}
}"#;
    std::fs::write(tmp.path(), with_catalogs).unwrap();
    let graph2 = parse(tmp.path()).unwrap();
    assert_eq!(
        graph2.catalogs["evens"]["date-fns"].specifier, "^2.30.0",
        "named catalogs must survive parse"
    );
    let out2 = tempfile::NamedTempFile::new().unwrap();
    write(out2.path(), &graph2, &manifest).unwrap();
    let reparsed = parse(out2.path()).unwrap();
    assert_eq!(
        reparsed.catalogs["evens"]["date-fns"].specifier, "^2.30.0",
        "named catalogs must survive a write→reparse round-trip"
    );
}

/// RT-2: a non-default registry URL in npm tuple slot 1 must survive a
/// round-trip. bun writes the full registry/tarball URL at slot 1 for
/// a scoped/private-registry dep (`""` only for the default registry);
/// dropping it re-routes the next resolve to the default npm registry
/// (404 / name-squat risk). The pre-existing fixture only has
/// default-registry (`""`) entries, so the data-loss slipped through.
#[test]
fn test_write_byte_identical_non_default_registry_url() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let sri = fake_sri('a');
    // `@acme/widget` resolves from a private registry — its full
    // tarball URL sits in slot 1. `picocolors` is a default-registry
    // dep with the empty slot, so both branches are exercised.
    let original = format!(
        r#"{{
  "lockfileVersion": 1,
  "configVersion": 1,
  "workspaces": {{
    "": {{
      "name": "root",
      "dependencies": {{
        "@acme/widget": "^1.0.0",
        "picocolors": "^1.1.1",
      }},
    }},
  }},
  "packages": {{
    "@acme/widget": ["@acme/widget@1.0.0", "https://npm.acme.internal/@acme/widget/-/widget-1.0.0.tgz", {{}}, "{sri}"],

    "picocolors": ["picocolors@1.1.1", "", {{}}, "{sri}"],
  }}
}}
"#
    );
    std::fs::write(tmp.path(), &original).unwrap();
    let graph = parse(tmp.path()).unwrap();

    // The private-registry URL must land on the model, not be dropped.
    assert_eq!(
        graph.packages["@acme/widget@1.0.0"].tarball_url.as_deref(),
        Some("https://npm.acme.internal/@acme/widget/-/widget-1.0.0.tgz"),
        "non-default registry URL dropped on parse"
    );
    // The default-registry dep keeps the empty slot (None on the model).
    assert_eq!(
        graph.packages["picocolors@1.1.1"].tarball_url, None,
        "default-registry empty slot must parse to None, not a literal empty URL"
    );

    let manifest = aube_manifest::PackageJson {
        name: Some("root".to_string()),
        dependencies: [
            ("@acme/widget".to_string(), "^1.0.0".to_string()),
            ("picocolors".to_string(), "^1.1.1".to_string()),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(out.path()).unwrap();

    assert_eq!(
        written, original,
        "non-default registry URL or default empty-slot drifted on re-emit"
    );
}

/// RT-3: a package carrying both `bin` and `os`/`cpu` must emit them in
/// bun's meta-object order — `os → cpu → libc → bin` (bin LAST). The
/// pre-existing fixtures never had a single entry with both, so the
/// wrong `bin`-before-platform-filters order slipped through.
#[test]
fn test_write_byte_identical_bin_after_platform_filters() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let sri = fake_sri('a');
    // `esbuild` ships a `bin` AND platform filters — bun renders
    // `os`, then `cpu`, then `bin` last on the meta object.
    let original = format!(
        r#"{{
  "lockfileVersion": 1,
  "configVersion": 1,
  "workspaces": {{
    "": {{
      "name": "root",
      "dependencies": {{
        "esbuild": "^0.21.0",
      }},
    }},
  }},
  "packages": {{
    "esbuild": ["esbuild@0.21.0", "", {{ "os": ["darwin", "linux"], "cpu": ["arm64", "x64"], "bin": {{ "esbuild": "bin/esbuild" }} }}, "{sri}"],
  }}
}}
"#
    );
    std::fs::write(tmp.path(), &original).unwrap();
    let graph = parse(tmp.path()).unwrap();

    let manifest = aube_manifest::PackageJson {
        name: Some("root".to_string()),
        dependencies: [("esbuild".to_string(), "^0.21.0".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(out.path()).unwrap();

    assert_eq!(
        written, original,
        "per-package meta field order (os/cpu/libc then bin) drifted from bun's output"
    );
}

/// `configVersion` must echo back whatever was parsed, not a
/// hardcoded `1`. Regression guard for a future bun release that
/// bumps the field — without this, aube would silently downgrade
/// every re-emit and drift against bun's own output.
#[test]
fn test_write_roundtrips_config_version() {
    let project = tempfile::TempDir::new().unwrap();
    let pj = project.path().join("package.json");
    std::fs::write(&pj, r#"{"name":"root","dependencies":{}}"#).unwrap();
    let lock_path = project.path().join("bun.lock");
    std::fs::write(
        &lock_path,
        r#"{
  "lockfileVersion": 1,
  "configVersion": 42,
  "workspaces": {
    "": { "name": "root" }
  },
  "packages": {}
}"#,
    )
    .unwrap();

    let graph = parse(&lock_path).unwrap();
    assert_eq!(graph.bun_config_version, Some(42));

    let manifest = aube_manifest::PackageJson::from_path(&pj).unwrap();
    write(&lock_path, &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(&lock_path).unwrap();
    assert!(
        written.contains("\"configVersion\": 42,"),
        "configVersion must round-trip verbatim, got:\n{written}"
    );
}

/// Hand-authored bun.lock with two workspace entries (root and
/// `packages/app`) round-trips through the parser with both
/// importers populated, and the writer regenerates both
/// workspace entries from the on-disk manifests.
#[test]
fn test_parse_and_write_multi_workspace() {
    use tempfile::TempDir;
    let sri_foo = fake_sri('a');
    let sri_bar = fake_sri('b');

    let project = TempDir::new().unwrap();
    let project_dir = project.path();
    std::fs::write(
        project_dir.join("package.json"),
        r#"{"name":"root","version":"1.0.0","dependencies":{"foo":"^1.0.0"}}"#,
    )
    .unwrap();
    std::fs::create_dir_all(project_dir.join("packages/app")).unwrap();
    std::fs::write(
        project_dir.join("packages/app/package.json"),
        r#"{"name":"app","version":"2.0.0","dependencies":{"bar":"^3.0.0"}}"#,
    )
    .unwrap();

    let lock_path = project_dir.join("bun.lock");
    let content = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": {
      "name": "root",
      "version": "1.0.0",
      "dependencies": { "foo": "^1.0.0" }
    },
    "packages/app": {
      "name": "app",
      "version": "2.0.0",
      "dependencies": { "bar": "^3.0.0" }
    }
  },
  "packages": {
    "foo": ["foo@1.2.3", "", {}, "SRI_FOO"],
    "bar": ["bar@3.1.0", "", {}, "SRI_BAR"]
  }
}"#
    .replace("SRI_FOO", &sri_foo)
    .replace("SRI_BAR", &sri_bar);
    std::fs::write(&lock_path, content).unwrap();

    let graph = parse(&lock_path).unwrap();

    // Both importers are populated with their own direct deps.
    let root = graph.importers.get(".").expect("root importer");
    assert_eq!(root.len(), 1);
    assert_eq!(root[0].name, "foo");
    assert_eq!(root[0].dep_path, "foo@1.2.3");

    let app = graph
        .importers
        .get("packages/app")
        .expect("packages/app importer");
    assert_eq!(app.len(), 1);
    assert_eq!(app[0].name, "bar");
    assert_eq!(app[0].dep_path, "bar@3.1.0");

    // Now write the graph back out and re-parse. The non-root
    // workspace entry must survive the round-trip. Write into the
    // same project dir so the writer can find
    // `packages/app/package.json` alongside the lockfile.
    let manifest =
        aube_manifest::PackageJson::from_path(&project_dir.join("package.json")).unwrap();
    std::fs::remove_file(&lock_path).unwrap();
    write(&lock_path, &graph, &manifest).unwrap();

    let reparsed = parse(&lock_path).unwrap();
    assert!(reparsed.importers.contains_key("."));
    assert!(reparsed.importers.contains_key("packages/app"));
    let app = &reparsed.importers["packages/app"];
    assert_eq!(app.len(), 1);
    assert_eq!(app[0].name, "bar");
    assert_eq!(app[0].dep_path, "bar@3.1.0");
    // And the raw text keeps the workspace block by key.
    let raw = std::fs::read_to_string(&lock_path).unwrap();
    assert!(raw.contains("\"packages/app\""));
    assert!(raw.contains("\"name\": \"app\""));
}

/// Non-root workspace entries must carry `version`, `bin`, and
/// `optionalPeers` (bun's compact form of
/// `peerDependenciesMeta[name].optional`). Root stays minimal —
/// bun's own output omits those three on the root entry because
/// the adjacent project `package.json` is authoritative.
#[test]
fn test_write_workspace_entry_carries_version_bin_and_optional_peers() {
    use tempfile::TempDir;

    let project = TempDir::new().unwrap();
    let project_dir = project.path();
    std::fs::write(
        project_dir.join("package.json"),
        r#"{"name":"root","version":"1.0.0"}"#,
    )
    .unwrap();
    std::fs::create_dir_all(project_dir.join("packages/drifti")).unwrap();
    std::fs::write(
        project_dir.join("packages/drifti/package.json"),
        r#"{
  "name": "@redact/drifti",
  "version": "0.0.1",
  "bin": { "drifti": "./dist/cli/bin.mjs" },
  "peerDependencies": {
    "@electric-sql/pglite": "*",
    "kysely": "*"
  },
  "peerDependenciesMeta": {
    "kysely": { "optional": true },
    "@electric-sql/pglite": { "optional": true },
    "not-optional": { "optional": false }
  }
}"#,
    )
    .unwrap();

    let mut importers = BTreeMap::new();
    importers.insert(".".to_string(), vec![]);
    importers.insert("packages/drifti".to_string(), vec![]);
    let graph = LockfileGraph {
        importers,
        ..Default::default()
    };

    let manifest =
        aube_manifest::PackageJson::from_path(&project_dir.join("package.json")).unwrap();
    let lock_path = project_dir.join("bun.lock");
    write(&lock_path, &graph, &manifest).unwrap();

    let raw = std::fs::read_to_string(&lock_path).unwrap();
    let v: serde_json::Value = serde_json::from_str(&strip_jsonc(&raw)).unwrap();
    let drifti = &v["workspaces"]["packages/drifti"];
    assert_eq!(drifti["name"], "@redact/drifti");
    assert_eq!(drifti["version"], "0.0.1");
    assert_eq!(drifti["bin"]["drifti"], "./dist/cli/bin.mjs");
    // Sorted alphabetically even though package.json lists keys
    // out of order, and the `optional: false` entry is excluded.
    let optional_peers: Vec<&str> = drifti["optionalPeers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap())
        .collect();
    assert_eq!(optional_peers, vec!["@electric-sql/pglite", "kysely"]);

    // `bin` must render inline — bun's own output puts it on one
    // line (`"bin": { "drifti": "./dist/cli/bin.mjs" }`). A
    // multi-line render here would produce the exact diff the
    // writer is trying to avoid.
    assert!(
        raw.contains(r#""bin": { "drifti": "./dist/cli/bin.mjs" },"#),
        "bin rendered multi-line or unexpected shape:\n{raw}"
    );

    // Root entry stays minimal: no version/bin/optionalPeers.
    let root = &v["workspaces"][""];
    assert!(
        root.get("version").is_none(),
        "root carried version: {root}"
    );
    assert!(root.get("bin").is_none(), "root carried bin: {root}");
    assert!(
        root.get("optionalPeers").is_none(),
        "root carried optionalPeers: {root}"
    );
}

/// Workspace-link packages must appear in `packages:` as
/// `[name@workspace:path]` so `bun install --frozen-lockfile`
/// can wire up the workspace dep without re-reading every
/// workspace package.json. Dropping them produces a lockfile
/// that errors with "Cannot find package" on the next install.
#[test]
fn test_write_emits_workspace_link_packages() {
    use crate::LocalSource;
    use std::path::PathBuf;

    let tmp_dir = tempfile::TempDir::new().unwrap();
    let project_dir = tmp_dir.path();
    std::fs::write(
        project_dir.join("package.json"),
        r#"{"name":"root","version":"1.0.0"}"#,
    )
    .unwrap();
    std::fs::create_dir_all(project_dir.join("packages/app")).unwrap();
    std::fs::write(
        project_dir.join("packages/app/package.json"),
        r#"{"name":"my-app","version":"0.1.0"}"#,
    )
    .unwrap();

    let mut packages = BTreeMap::new();
    packages.insert(
        "my-app@0.1.0".to_string(),
        LockedPackage {
            name: "my-app".to_string(),
            version: "0.1.0".to_string(),
            dep_path: "my-app@0.1.0".to_string(),
            local_source: Some(LocalSource::Link(PathBuf::from("packages/app"))),
            ..Default::default()
        },
    );
    let mut importers = BTreeMap::new();
    importers.insert(".".to_string(), vec![]);
    importers.insert("packages/app".to_string(), vec![]);
    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };

    let manifest =
        aube_manifest::PackageJson::from_path(&project_dir.join("package.json")).unwrap();
    let lock_path = project_dir.join("bun.lock");
    write(&lock_path, &graph, &manifest).unwrap();

    let raw = std::fs::read_to_string(&lock_path).unwrap();
    let v: serde_json::Value = serde_json::from_str(&strip_jsonc(&raw)).unwrap();
    let pkgs = v["packages"].as_object().unwrap();
    let entry = pkgs
        .get("my-app")
        .expect("workspace-link package missing from `packages`");
    let arr = entry.as_array().expect("entry must be a JSON array");
    assert_eq!(arr.len(), 1, "no-deps workspace entry must be `[ident]`");
    assert_eq!(arr[0].as_str(), Some("my-app@workspace:packages/app"));
    let ws = v["workspaces"].as_object().unwrap();
    assert!(ws.contains_key("packages/app"));
}

/// Workspace-to-workspace deps must survive emission. When `app`
/// depends on `lib` via `workspace:*`, `app`'s `packages:` entry
/// has to carry that dep edge in its meta or bun's frozen-install
/// pass can't wire it up. The dep target is another `LocalSource::Link`
/// package, not a registry one, so the membership check has to
/// accept workspace dep_paths in addition to canonical entries.
#[test]
fn test_write_preserves_workspace_to_workspace_dep_edge() {
    use crate::LocalSource;
    use std::path::PathBuf;
    use tempfile::TempDir;

    let project = TempDir::new().unwrap();
    let project_dir = project.path();
    std::fs::write(
        project_dir.join("package.json"),
        r#"{"name":"root","version":"1.0.0"}"#,
    )
    .unwrap();
    std::fs::create_dir_all(project_dir.join("packages/app")).unwrap();
    std::fs::create_dir_all(project_dir.join("packages/lib")).unwrap();
    std::fs::write(
        project_dir.join("packages/app/package.json"),
        r#"{"name":"app","version":"0.1.0","dependencies":{"lib":"workspace:*"}}"#,
    )
    .unwrap();
    std::fs::write(
        project_dir.join("packages/lib/package.json"),
        r#"{"name":"lib","version":"0.1.0"}"#,
    )
    .unwrap();

    let mut packages = BTreeMap::new();
    packages.insert(
        "app@workspace:packages/app".to_string(),
        LockedPackage {
            name: "app".to_string(),
            version: "workspace:packages/app".to_string(),
            dep_path: "app@workspace:packages/app".to_string(),
            local_source: Some(LocalSource::Link(PathBuf::from("packages/app"))),
            dependencies: [("lib".to_string(), "workspace:packages/lib".to_string())].into(),
            declared_dependencies: [("lib".to_string(), "workspace:*".to_string())].into(),
            ..Default::default()
        },
    );
    packages.insert(
        "lib@workspace:packages/lib".to_string(),
        LockedPackage {
            name: "lib".to_string(),
            version: "workspace:packages/lib".to_string(),
            dep_path: "lib@workspace:packages/lib".to_string(),
            local_source: Some(LocalSource::Link(PathBuf::from("packages/lib"))),
            ..Default::default()
        },
    );
    let mut importers = BTreeMap::new();
    importers.insert(".".to_string(), vec![]);
    importers.insert("packages/app".to_string(), vec![]);
    importers.insert("packages/lib".to_string(), vec![]);
    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };

    let manifest =
        aube_manifest::PackageJson::from_path(&project_dir.join("package.json")).unwrap();
    let lock_path = project_dir.join("bun.lock");
    write(&lock_path, &graph, &manifest).unwrap();

    let raw = std::fs::read_to_string(&lock_path).unwrap();
    let v: serde_json::Value = serde_json::from_str(&strip_jsonc(&raw)).unwrap();
    let app_entry = v["packages"]["app"].as_array().unwrap();
    assert_eq!(
        app_entry.len(),
        2,
        "workspace entry with deps must be `[ident, {{ meta }}]`"
    );
    assert_eq!(app_entry[0].as_str(), Some("app@workspace:packages/app"));
    assert_eq!(
        app_entry[1]["dependencies"]["lib"].as_str(),
        Some("workspace:*"),
        "workspace-to-workspace dep edge dropped"
    );
}

/// Parse → write → parse round-trip preserves a workspace entry
/// in `packages:`. Bun emits `[ident]` (and optionally `[ident,
/// { meta }]` when the workspace declares deps); both shapes must
/// survive without churning to the registry-package 4-tuple form.
#[test]
fn test_roundtrip_workspace_entry_in_packages_section() {
    use tempfile::TempDir;
    let project = TempDir::new().unwrap();
    let project_dir = project.path();
    std::fs::write(
        project_dir.join("package.json"),
        r#"{"name":"root","version":"1.0.0"}"#,
    )
    .unwrap();
    std::fs::create_dir_all(project_dir.join("packages/app")).unwrap();
    std::fs::write(
        project_dir.join("packages/app/package.json"),
        r#"{"name":"app","version":"0.1.0"}"#,
    )
    .unwrap();

    let lock_path = project_dir.join("bun.lock");
    let content = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": { "name": "root", "version": "1.0.0" },
    "packages/app": { "name": "app", "version": "0.1.0" }
  },
  "packages": {
    "app": ["app@workspace:packages/app"]
  }
}"#;
    std::fs::write(&lock_path, content).unwrap();

    let graph = parse(&lock_path).unwrap();
    let manifest =
        aube_manifest::PackageJson::from_path(&project_dir.join("package.json")).unwrap();
    std::fs::remove_file(&lock_path).unwrap();
    write(&lock_path, &graph, &manifest).unwrap();

    let raw = std::fs::read_to_string(&lock_path).unwrap();
    let v: serde_json::Value = serde_json::from_str(&strip_jsonc(&raw)).unwrap();
    let arr = v["packages"]["app"]
        .as_array()
        .expect("workspace entry survived as array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0].as_str(), Some("app@workspace:packages/app"));
}

/// When the root and a non-root workspace declare the same dep
/// name at *different* versions, the writer must emit a
/// consistent top-level `packages` entry and still walk the
/// chosen version's transitive deps. Regression test for a
/// corruption in `build_hoist_tree`'s root-seeding loop: without
/// name-dedupe, the second version would overwrite the first in
/// `placed` but never get queued, so neither version's
/// transitive deps were walked correctly and the top-level entry
/// pointed at a package whose deps were never expanded.
#[test]
fn test_write_dedupes_duplicate_direct_deps_across_workspaces() {
    use tempfile::TempDir;

    let project = TempDir::new().unwrap();
    let project_dir = project.path();
    std::fs::write(
        project_dir.join("package.json"),
        r#"{"name":"root","dependencies":{"foo":"^1.0.0"}}"#,
    )
    .unwrap();
    std::fs::create_dir_all(project_dir.join("packages/app")).unwrap();
    std::fs::write(
        project_dir.join("packages/app/package.json"),
        r#"{"name":"app","dependencies":{"foo":"^2.0.0"}}"#,
    )
    .unwrap();

    let mut packages = BTreeMap::new();
    packages.insert(
        "foo@1.0.0".to_string(),
        LockedPackage {
            name: "foo".to_string(),
            version: "1.0.0".to_string(),
            dep_path: "foo@1.0.0".to_string(),
            dependencies: [("bar".to_string(), "2.0.0".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        },
    );
    packages.insert(
        "foo@2.0.0".to_string(),
        LockedPackage {
            name: "foo".to_string(),
            version: "2.0.0".to_string(),
            dep_path: "foo@2.0.0".to_string(),
            ..Default::default()
        },
    );
    packages.insert(
        "bar@2.0.0".to_string(),
        LockedPackage {
            name: "bar".to_string(),
            version: "2.0.0".to_string(),
            dep_path: "bar@2.0.0".to_string(),
            ..Default::default()
        },
    );
    let mut importers = BTreeMap::new();
    importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "foo".to_string(),
            dep_path: "foo@1.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: None,
        }],
    );
    importers.insert(
        "packages/app".to_string(),
        vec![DirectDep {
            name: "foo".to_string(),
            dep_path: "foo@2.0.0".to_string(),
            dep_type: DepType::Production,
            specifier: None,
        }],
    );
    let graph = LockfileGraph {
        importers,
        packages,
        ..Default::default()
    };

    let manifest =
        aube_manifest::PackageJson::from_path(&project_dir.join("package.json")).unwrap();
    let lock_path = project_dir.join("bun.lock");
    write(&lock_path, &graph, &manifest).unwrap();

    let reparsed = parse(&lock_path).unwrap();
    // The root's version wins the hoisted `foo` slot (BTreeMap
    // iteration puts `.` before `packages/app`), and `bar` — only
    // reachable by walking root-foo's transitive deps — must be
    // present. Before the fix, `foo@2.0.0` would overwrite
    // `foo@1.0.0` in `placed` but never get queued, and neither
    // version's transitive deps (including `bar`) would make it
    // into the output.
    let foo = reparsed.packages.get("foo@1.0.0").expect("foo@1.0.0");
    assert_eq!(foo.version, "1.0.0");
    assert!(
        reparsed.packages.contains_key("bar@2.0.0"),
        "root foo's transitive `bar` was dropped: {:?}",
        reparsed.packages.keys().collect::<Vec<_>>()
    );
}

/// When a workspace directory path (e.g. `packages/app`) happens
/// to share its first segment with a literal npm package name,
/// the parser must not wrongly resolve a workspace dep to that
/// package's nested entry. Here there's an npm package literally
/// named `packages` with a nested `bar@9.9.9`, and the workspace
/// `packages/app` depends on `bar`. The workspace's `bar` must
/// resolve to the hoisted `bar@1.0.0`, not to `packages/bar`'s
/// `9.9.9`.
#[test]
fn test_parse_workspace_path_does_not_alias_npm_package() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let sri = fake_sri('a');
    let content = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": { "dependencies": { "packages": "^1.0.0" } },
    "packages/app": {
      "name": "app",
      "dependencies": { "bar": "^1.0.0" }
    }
  },
  "packages": {
    "bar": ["bar@1.0.0", "", {}, "SRI"],
    "packages": ["packages@1.0.0", "", { "dependencies": { "bar": "^9.0.0" } }, "SRI"],
    "packages/bar": ["bar@9.9.9", "", {}, "SRI"]
  }
}"#
    .replace("SRI", &sri);
    std::fs::write(tmp.path(), &content).unwrap();
    let graph = parse(tmp.path()).unwrap();

    let app = graph
        .importers
        .get("packages/app")
        .expect("packages/app importer");
    let bar = app.iter().find(|d| d.name == "bar").expect("bar dep");
    assert_eq!(
        bar.dep_path, "bar@1.0.0",
        "workspace `bar` must resolve to hoisted 1.0.0, not packages/bar@9.9.9"
    );
}

/// Bun scopes non-hoisted direct deps under the workspace package
/// name, not the workspace directory path. A workspace at
/// `packages/z-app` named `z-app` can therefore depend on
/// `z-app/tslib` while another workspace gets the hoisted `tslib`.
#[test]
fn test_parse_workspace_dep_prefers_workspace_name_scope() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let sri = fake_sri('a');
    let content = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": { "name": "root" },
    "packages/a-other": {
      "name": "a-other",
      "dependencies": { "tslib": "2.8.1" }
    },
    "packages/z-app": {
      "name": "z-app",
      "dependencies": { "tslib": "2.4.0" }
    }
  },
  "packages": {
    "a-other": ["a-other@workspace:packages/a-other"],
    "tslib": ["tslib@2.8.1", "", {}, "SRI"],
    "z-app": ["z-app@workspace:packages/z-app"],
    "z-app/tslib": ["tslib@2.4.0", "", {}, "SRI"]
  }
}"#
    .replace("SRI", &sri);
    std::fs::write(tmp.path(), &content).unwrap();
    let graph = parse(tmp.path()).unwrap();

    let other = graph
        .importers
        .get("packages/a-other")
        .expect("packages/a-other importer");
    let hoisted_tslib = other.iter().find(|d| d.name == "tslib").expect("tslib dep");
    assert_eq!(
        hoisted_tslib.dep_path, "tslib@2.8.1",
        "sibling workspace must still resolve to the hoisted tslib"
    );

    let app = graph
        .importers
        .get("packages/z-app")
        .expect("packages/z-app importer");
    let tslib = app.iter().find(|d| d.name == "tslib").expect("tslib dep");
    assert_eq!(
        tslib.dep_path, "tslib@2.4.0",
        "workspace dep must resolve to z-app/tslib, not hoisted tslib"
    );
}

#[test]
fn test_parse_rebases_workspace_scoped_local_tarball() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let sri = fake_sri('a');
    let content = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": { "name": "root" },
    "packages/app": {
      "name": "app",
      "dependencies": { "local-tar": "file:../../vendor/local-tar-1.0.0.tgz" }
    }
  },
  "packages": {
    "app": ["app@workspace:packages/app"],
    "app/local-tar": ["local-tar@../../vendor/local-tar-1.0.0.tgz", {}, "SRI"]
  }
}"#
    .replace("SRI", &sri);
    std::fs::write(tmp.path(), &content).unwrap();
    let graph = parse(tmp.path()).unwrap();

    let local_tar = graph
        .packages
        .values()
        .find(|p| p.name == "local-tar")
        .expect("local-tar package");
    assert_eq!(local_tar.version, "../../vendor/local-tar-1.0.0.tgz");
    assert_eq!(
        local_tar.local_source,
        Some(LocalSource::Tarball(PathBuf::from(
            "vendor/local-tar-1.0.0.tgz"
        )))
    );
}

/// Top-level `overrides` / `patchedDependencies` / `trustedDependencies`
/// and the unnamed `catalog` / named `catalogs` blocks must round-trip
/// verbatim — bun preserves all five on re-emit, so aube dropping any
/// of them is a real-repo churn source on every install. Keep this
/// test format-agnostic (no SRI hashes, no packages) so it only
/// exercises the metadata-preservation path.
#[test]
fn test_roundtrip_top_level_metadata() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": { "name": "root" }
  },
  "overrides": {
    "lodash": "^4.17.21",
    "lodash>debug": "^4.0.0"
  },
  "patchedDependencies": {
    "lodash@4.17.21": "patches/lodash@4.17.21.patch"
  },
  "trustedDependencies": ["sharp", "esbuild"],
  "catalog": {
    "react": "^18.2.0"
  },
  "catalogs": {
    "evens": { "date-fns": "^2.30.0" }
  },
  "packages": {}
}"#;
    std::fs::write(tmp.path(), content).unwrap();
    let graph = parse(tmp.path()).unwrap();

    assert_eq!(
        graph.overrides.get("lodash").map(String::as_str),
        Some("^4.17.21")
    );
    assert_eq!(
        graph.overrides.get("lodash>debug").map(String::as_str),
        Some("^4.0.0")
    );
    assert_eq!(
        graph
            .patched_dependencies
            .get("lodash@4.17.21")
            .map(String::as_str),
        Some("patches/lodash@4.17.21.patch")
    );
    assert_eq!(
        graph.trusted_dependencies,
        vec!["sharp".to_string(), "esbuild".to_string()],
        "trustedDependencies must preserve bun's original order on parse"
    );
    assert_eq!(graph.catalogs["default"]["react"].specifier, "^18.2.0");
    assert_eq!(graph.catalogs["evens"]["date-fns"].specifier, "^2.30.0");

    let manifest = aube_manifest::PackageJson {
        name: Some("root".to_string()),
        ..Default::default()
    };
    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(out.path()).unwrap();

    // Every round-tripped block must appear in the re-emitted
    // lockfile — the exact rendering is implementation-defined
    // but a substring check is enough to catch regression.
    assert!(
        written.contains("\"overrides\""),
        "overrides dropped:\n{written}"
    );
    assert!(
        written.contains("\"patchedDependencies\""),
        "patchedDependencies dropped:\n{written}"
    );
    assert!(
        written.contains("\"trustedDependencies\""),
        "trustedDependencies dropped:\n{written}"
    );
    // trustedDependencies must round-trip in insertion order
    // (bun writes [sharp, esbuild] — alphabetized emit would
    // produce a gratuitous diff against bun's own output).
    let sharp_at = written
        .find("\"sharp\"")
        .expect("sharp in trustedDependencies");
    let esbuild_at = written
        .find("\"esbuild\"")
        .expect("esbuild in trustedDependencies");
    assert!(
        sharp_at < esbuild_at,
        "trustedDependencies reordered on write — expected sharp before esbuild:\n{written}"
    );
    assert!(
        written.contains("\"catalog\""),
        "catalog dropped:\n{written}"
    );
    assert!(
        written.contains("\"catalogs\""),
        "catalogs dropped:\n{written}"
    );

    let reparsed = parse(out.path()).unwrap();
    assert_eq!(reparsed.overrides, graph.overrides);
    assert_eq!(reparsed.patched_dependencies, graph.patched_dependencies);
    assert_eq!(reparsed.trusted_dependencies, graph.trusted_dependencies);
    assert_eq!(reparsed.catalogs["default"]["react"].specifier, "^18.2.0");
}

/// Non-registry specifier classes (github:, file:, link:, https:,
/// workspace:) must parse into `LocalSource` rather than fall
/// through as registry pins. The installer routes by
/// `LocalSource`, so mis-classification here sends the package
/// through the default registry and either 404s or downloads the
/// wrong tarball — bug class #1 in the parity report.
#[test]
fn test_parse_routes_non_registry_specs_to_localsource() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": {
      "dependencies": {
        "vfs": "github:collinstevens/vfs#0b6ea53",
        "localdir": "file:./vendor/localdir",
        "localtgz": "file:./vendor/thing.tgz",
        "sibling": "link:../sibling",
        "remote": "https://example.com/thing.tgz"
      }
    }
  },
  "packages": {
    "vfs": ["vfs@github:collinstevens/vfs#0b6ea53abcdef", {}, "collinstevens-vfs-0b6ea53abcdef"],
    "localdir": ["localdir@file:./vendor/localdir", {}],
    "localtgz": ["localtgz@file:./vendor/thing.tgz", {}],
    "sibling": ["sibling@link:../sibling", {}],
    "remote": ["remote@https://example.com/thing.tgz", {}]
  }
}"#;
    std::fs::write(tmp.path(), content).unwrap();
    let graph = parse(tmp.path()).unwrap();

    let vfs = graph
        .packages
        .values()
        .find(|p| p.name == "vfs")
        .expect("vfs package");
    assert!(
        matches!(vfs.local_source, Some(LocalSource::Git(_))),
        "github dep must be LocalSource::Git, got {:?}",
        vfs.local_source
    );

    let localdir = graph
        .packages
        .values()
        .find(|p| p.name == "localdir")
        .expect("localdir package");
    assert!(
        matches!(localdir.local_source, Some(LocalSource::Directory(_))),
        "file:./dir must be LocalSource::Directory, got {:?}",
        localdir.local_source
    );

    let localtgz = graph
        .packages
        .values()
        .find(|p| p.name == "localtgz")
        .expect("localtgz package");
    assert!(
        matches!(localtgz.local_source, Some(LocalSource::Tarball(_))),
        "file:./*.tgz must be LocalSource::Tarball, got {:?}",
        localtgz.local_source
    );

    let sibling = graph
        .packages
        .values()
        .find(|p| p.name == "sibling")
        .expect("sibling package");
    assert!(
        matches!(sibling.local_source, Some(LocalSource::Link(_))),
        "link: must be LocalSource::Link, got {:?}",
        sibling.local_source
    );

    let remote = graph
        .packages
        .values()
        .find(|p| p.name == "remote")
        .expect("remote package");
    assert!(
        matches!(remote.local_source, Some(LocalSource::RemoteTarball(_))),
        "https://*.tgz must be LocalSource::RemoteTarball, got {:?}",
        remote.local_source
    );
}

#[test]
fn test_parse_bun_workspace_package_path_as_link_target() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let content = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": { "name": "root" },
    "packages/app": {
      "name": "app",
      "dependencies": { "lib": "workspace:*" }
    },
    "packages/lib": { "name": "lib" }
  },
  "packages": {
    "app": ["app@workspace:packages/app"],
    "lib": ["lib@workspace:packages/lib"]
  }
}"#;
    std::fs::write(tmp.path(), content).unwrap();
    let graph = parse(tmp.path()).unwrap();

    let lib = graph.packages.get("lib@workspace:packages/lib").unwrap();
    assert_eq!(
        lib.local_source.as_ref().and_then(LocalSource::path),
        Some(Path::new("packages/lib"))
    );

    let app_deps = graph.importers.get("packages/app").unwrap();
    assert_eq!(app_deps[0].dep_path, "lib@workspace:packages/lib");
}

/// npm-alias ident: bun writes `<real>@<version>` as the ident
/// string while using the alias name as the `packages[]` hoist
/// key. Aube's earlier writer emitted `<alias>@<version>` and
/// produced a gratuitous diff against bun's own output. Cover
/// both parse (populates `alias_of`) and write (emits real name
/// in ident).
#[test]
fn test_parse_and_write_npm_alias() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let sri = fake_sri('a');
    let content = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": { "dependencies": { "h3-v2": "npm:h3@2.0.1" } }
  },
  "packages": {
    "h3-v2": ["h3@2.0.1", "", {}, "SRI"]
  }
}"#
    .replace("SRI", &sri);
    std::fs::write(tmp.path(), &content).unwrap();
    let graph = parse(tmp.path()).unwrap();
    let h3 = graph
        .packages
        .values()
        .find(|p| p.name == "h3-v2")
        .expect("h3-v2 package");
    assert_eq!(h3.alias_of.as_deref(), Some("h3"));
    assert_eq!(h3.version, "2.0.1");

    let manifest = aube_manifest::PackageJson {
        name: Some("root".to_string()),
        dependencies: [("h3-v2".to_string(), "npm:h3@2.0.1".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(out.path()).unwrap();

    // Ident reads `h3@2.0.1` (registry identity), not `h3-v2@...`.
    assert!(
        written.contains("\"h3@2.0.1\""),
        "expected ident `h3@2.0.1`, got:\n{written}"
    );
    assert!(
        !written.contains("\"h3-v2@2.0.1\""),
        "alias-name ident leaked into packages entry:\n{written}"
    );
}

/// Per-entry meta blocks bun preserves that aube historically
/// dropped: `peerDependencies`, `optionalPeers`, `os`, `cpu`,
/// `libc`. Round-trip through a single package entry and confirm
/// every field survives re-parse.
#[test]
fn test_roundtrip_peer_and_platform_metadata() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let sri = fake_sri('a');
    let content = r#"{
  "lockfileVersion": 1,
  "workspaces": { "": { "dependencies": { "foo": "^1.0.0" } } },
  "packages": {
    "foo": ["foo@1.0.0", "", {
      "peerDependencies": { "react": "^18.0.0" },
      "optionalPeers": ["react"],
      "os": ["darwin", "linux"],
      "cpu": ["arm64", "x64"],
      "libc": ["glibc"]
    }, "SRI"]
  }
}"#
    .replace("SRI", &sri);
    std::fs::write(tmp.path(), &content).unwrap();
    let graph = parse(tmp.path()).unwrap();
    let foo = &graph.packages["foo@1.0.0"];
    assert_eq!(
        foo.peer_dependencies.get("react").map(String::as_str),
        Some("^18.0.0")
    );
    assert!(
        foo.peer_dependencies_meta
            .get("react")
            .is_some_and(|m| m.optional)
    );
    assert_eq!(
        foo.os.as_slice(),
        &["darwin".to_string(), "linux".to_string()]
    );
    assert_eq!(
        foo.cpu.as_slice(),
        &["arm64".to_string(), "x64".to_string()]
    );
    assert_eq!(foo.libc.as_slice(), &["glibc".to_string()]);

    let manifest = aube_manifest::PackageJson {
        name: Some("root".to_string()),
        dependencies: [("foo".to_string(), "^1.0.0".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();
    let reparsed = parse(out.path()).unwrap();
    let foo2 = &reparsed.packages["foo@1.0.0"];
    assert_eq!(foo2.peer_dependencies, foo.peer_dependencies);
    assert_eq!(foo2.os, foo.os);
    assert_eq!(foo2.cpu, foo.cpu);
    assert_eq!(foo2.libc, foo.libc);
}

#[test]
fn test_parse_scalar_platform_metadata() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let sri = fake_sri('a');
    let content = r#"{
  "lockfileVersion": 1,
  "workspaces": { "": { "dependencies": { "@esbuild/darwin-arm64": "0.27.2" } } },
  "packages": {
    "@esbuild/darwin-arm64": ["@esbuild/darwin-arm64@0.27.2", "", {
      "os": "darwin",
      "cpu": "arm64",
      "libc": "glibc"
    }, "SRI"]
  }
}"#
    .replace("SRI", &sri);
    std::fs::write(tmp.path(), &content).unwrap();

    let graph = parse(tmp.path()).unwrap();
    let pkg = &graph.packages["@esbuild/darwin-arm64@0.27.2"];
    assert_eq!(pkg.os.as_slice(), &["darwin".to_string()]);
    assert_eq!(pkg.cpu.as_slice(), &["arm64".to_string()]);
    assert_eq!(pkg.libc.as_slice(), &["glibc".to_string()]);
}

/// Workspace-level `peerDependencies` must survive round-trip
/// through the serde-flatten `extra` map even though aube's
/// typed workspace model doesn't claim the field directly. The
/// prior revision had a typed slot that silently drained bun's
/// peer block without plumbing it anywhere — regression guard.
#[test]
fn test_roundtrip_workspace_peer_dependencies() {
    use tempfile::TempDir;

    let project = TempDir::new().unwrap();
    let project_dir = project.path();
    std::fs::write(
        project_dir.join("package.json"),
        r#"{"name":"root","version":"1.0.0"}"#,
    )
    .unwrap();
    std::fs::create_dir_all(project_dir.join("packages/app")).unwrap();
    // Non-root workspace's package.json deliberately omits
    // peerDependencies; the lockfile is the only place they live.
    std::fs::write(
        project_dir.join("packages/app/package.json"),
        r#"{"name":"app","version":"2.0.0"}"#,
    )
    .unwrap();

    let lock_path = project_dir.join("bun.lock");
    std::fs::write(
        &lock_path,
        r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": { "name": "root" },
    "packages/app": {
      "name": "app",
      "version": "2.0.0",
      "peerDependencies": { "react": "^18.0.0" }
    }
  },
  "packages": {}
}"#,
    )
    .unwrap();

    let graph = parse(&lock_path).unwrap();
    let app_extras = graph
        .workspace_extra_fields
        .get("packages/app")
        .expect("packages/app workspace_extra_fields entry");
    let peers = app_extras
        .get("peerDependencies")
        .and_then(serde_json::Value::as_object)
        .expect("peerDependencies captured in extras");
    assert_eq!(peers.get("react").and_then(|v| v.as_str()), Some("^18.0.0"));

    let manifest =
        aube_manifest::PackageJson::from_path(&project_dir.join("package.json")).unwrap();
    write(&lock_path, &graph, &manifest).unwrap();
    let written = std::fs::read_to_string(&lock_path).unwrap();
    assert!(
        written.contains("\"peerDependencies\""),
        "workspace peerDependencies dropped on re-emit:\n{written}"
    );
    assert!(
        written.contains("\"react\""),
        "workspace peerDependencies.react dropped on re-emit:\n{written}"
    );
}

// bun records a git dep as `[ident, {meta}, "<owner>-<repo>-<commit>",
// integrity]` where the ident keeps the git specifier form
// (`ms@github:vercel/ms#<commit>`) — verified against bun 1.3.14, whose
// frozen install also requires the repo-tag element and fails
// "Failed to resolve root prod dependency" when the entry is missing
// entirely. The resolver keys git packages by their hashed dep_path
// (`ms@git+<hash>`), which never matches the `name@version` canonical
// key, so the writer used to drop them from `packages` wholesale.
#[test]
fn git_sourced_packages_are_emitted_with_bun_git_tuple_shape() {
    let sha = "1c6264b795492e8fdecbc82cb8802fcfbfc08d26";
    let sri = fake_sri('g');
    let local = LocalSource::Git(crate::GitSource {
        url: "https://github.com/vercel/ms.git".to_string(),
        committish: Some("2.1.3".to_string()),
        resolved: sha.to_string(),
        integrity: None,
        subpath: None,
    });
    let dep_path = local.dep_path("ms");
    let mut graph = LockfileGraph::default();
    graph.packages.insert(
        dep_path.clone(),
        LockedPackage {
            name: "ms".to_string(),
            version: "2.1.3".to_string(),
            integrity: Some(sri.clone()),
            dep_path: dep_path.clone(),
            local_source: Some(local),
            ..Default::default()
        },
    );
    graph.importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "ms".to_string(),
            dep_path,
            dep_type: DepType::Production,
            specifier: Some("github:vercel/ms#2.1.3".to_string()),
        }],
    );

    let manifest = aube_manifest::PackageJson {
        name: Some("test".to_string()),
        version: Some("1.0.0".to_string()),
        dependencies: [("ms".to_string(), "github:vercel/ms#2.1.3".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();

    let body = std::fs::read_to_string(out.path()).unwrap();
    // bun pins idents to the SHORT 7-char sha; the full form makes
    // bun 1.3.14 exit 0 without materializing the package.
    let short = &sha[..7];
    assert!(
        body.contains(&format!(
            "\"ms\": [\"ms@github:vercel/ms#{short}\", {{}}, \"vercel-ms-{short}\"]"
        )),
        "git dep must be emitted as bun's git tuple; got:\n{body}"
    );
    assert!(
        !body.contains(&sri),
        "a fresh resolve's own tarball SRI must not be written as bun's \
         git integrity (bun verifies it against the artifact it fetches): {body}"
    );

    // The reader reconstructs the git source from the written entry.
    let reparsed = parse(out.path()).unwrap();
    let pkg = reparsed
        .packages
        .values()
        .find(|p| p.name == "ms")
        .expect("git package must survive a write/parse round-trip");
    let Some(LocalSource::Git(git)) = &pkg.local_source else {
        panic!("expected git local source, got {:?}", pkg.local_source);
    };
    assert_eq!(git.url, "https://github.com/vercel/ms.git");
    assert_eq!(git.resolved, short);
}

// A bun-authored git entry (short-sha ident, bun's own pack integrity)
// must round-trip verbatim — bun verifies that integrity against the
// artifact it fetches, so re-keying the ident or dropping the hash
// would break the next frozen install.
#[test]
fn bun_authored_git_entries_round_trip_with_their_integrity() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let sri = fake_sri('b');
    let content = format!(
        r#"{{
  "lockfileVersion": 1,
  "configVersion": 1,
  "workspaces": {{
    "": {{
      "name": "git-rt",
      "dependencies": {{ "ms": "github:vercel/ms#2.1.3" }},
    }},
  }},
  "packages": {{
    "ms": ["ms@github:vercel/ms#1c6264b", {{}}, "vercel-ms-1c6264b", "{sri}"],
  }}
}}"#
    );
    std::fs::write(tmp.path(), &content).unwrap();
    let graph = parse(tmp.path()).unwrap();

    let manifest = aube_manifest::PackageJson {
        name: Some("git-rt".to_string()),
        version: Some("1.0.0".to_string()),
        dependencies: [("ms".to_string(), "github:vercel/ms#2.1.3".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();
    let body = std::fs::read_to_string(out.path()).unwrap();
    assert!(
        body.contains(&format!(
            "\"ms\": [\"ms@github:vercel/ms#1c6264b\", {{}}, \"vercel-ms-1c6264b\", \"{sri}\"]"
        )),
        "bun-authored git entry must round-trip verbatim; got:\n{body}"
    );
}

// A hosted-git dependency the resolver fetched through a codeload archive
// arrives as a `RemoteTarball { git_hosted: true }`, NOT a
// `LocalSource::Git`. The writer must still emit bun's git tuple (git-spec
// ident + `owner-repo-sha` repo-tag, no integrity), because a cold-cache
// `bun install --frozen-lockfile` fetches the dep from GitHub and rejects
// the registry-shaped collapse with `IntegrityCheckFailed`. This is the
// shape both `aube install` (fresh resolve) and a pnpm-v9 lockfile feed the
// writer, so it is the real-world git-dep path — not the synthetic
// `LocalSource::Git` one the test above covers.
#[test]
fn git_hosted_remote_tarball_is_emitted_as_bun_git_tuple() {
    let sha = "1c6264b795492e8fdecbc82cb8802fcfbfc08d26";
    // The resolver's own codeload-tarball SRI — a different artifact than
    // the one bun hashes, so it must NOT be written as bun's integrity.
    let aube_sri = fake_sri('r');
    let local = LocalSource::RemoteTarball(crate::RemoteTarballSource {
        url: format!("https://codeload.github.com/vercel/ms/tar.gz/{sha}"),
        integrity: aube_sri.clone(),
        git_hosted: true,
    });
    let dep_path = local.dep_path("ms");
    let mut graph = LockfileGraph::default();
    graph.packages.insert(
        dep_path.clone(),
        LockedPackage {
            name: "ms".to_string(),
            version: "2.1.3".to_string(),
            integrity: Some(aube_sri.clone()),
            dep_path: dep_path.clone(),
            local_source: Some(local),
            ..Default::default()
        },
    );
    graph.importers.insert(
        ".".to_string(),
        vec![DirectDep {
            name: "ms".to_string(),
            dep_path,
            dep_type: DepType::Production,
            specifier: Some("git+https://github.com/vercel/ms.git#1c6264b".to_string()),
        }],
    );
    let manifest = aube_manifest::PackageJson {
        name: Some("test".to_string()),
        version: Some("1.0.0".to_string()),
        dependencies: [(
            "ms".to_string(),
            format!("git+https://github.com/vercel/ms.git#{sha}"),
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    };

    let out = tempfile::NamedTempFile::new().unwrap();
    write(out.path(), &graph, &manifest).unwrap();
    let body = std::fs::read_to_string(out.path()).unwrap();

    let short = &sha[..7];
    assert!(
        body.contains(&format!(
            "\"ms\": [\"ms@github:vercel/ms#{short}\", {{}}, \"vercel-ms-{short}\"]"
        )),
        "a git_hosted tarball must serialize as bun's git tuple (git-spec \
         ident + repo-tag, no integrity); got:\n{body}"
    );
    assert!(
        !body.contains(&aube_sri),
        "the resolver's codeload SRI must not leak in as bun's git \
         integrity (bun verifies its own pack hash): {body}"
    );
    assert!(
        !body.contains("\"ms@2.1.3\""),
        "the git dep must NOT collapse into a registry-shaped `ms@2.1.3` \
         entry (the bug that fails bun's cold-cache frozen install):\n{body}"
    );
}
