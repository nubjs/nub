//! A transpiled TypeScript file's script identity is the `file:` URL Node's own
//! type-stripping reports, not a bare filesystem path.
//!
//! nub appends a `//# sourceURL` to every transpiled body, and that value is what
//! the inspector reports as `Debugger.scriptParsed`'s `url` — the key editors and
//! DevTools match breakpoints against. Node reports the module's URL there
//! (`pathToFileURL(filename).href`, via `convertCJSFilenameToURL`); emitting the
//! path instead gave every `.ts` file a different identity under nub than under
//! `node`. Upstream pins the same contract in
//! `test/parallel/test-inspector-strip-types.js`, including `hasSourceURL`.
//!
//! Deliberately NOT asserted here: the rendered stack frame. nub runs with
//! `--enable-source-maps`, and Node's source-map frame formatter strips the
//! `file://` scheme off the mapped source by design
//! (`prepare_stack_trace.js`, `originalSourceNoScheme`), so a mapped frame shows a
//! bare path whatever the sourceURL says. The inspector URL is the surface this
//! bug is actually about.
//!
//! The fixture supplies its OWN expectation — `pathToFileURL(__filename).href`
//! plus URLs resolved against it — so the assertion is a differential against
//! Node's URL for the same files rather than path arithmetic repeated in the
//! test. Each fixture directory is run TWICE against a private cache directory:
//! the sourceURL is baked into the cached body, so a cold transform and a warm
//! cache hit are separate claims.

use std::path::{Path, PathBuf};
use std::process::Command;

fn nub_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nub"))
}

fn host_node_usable() -> bool {
    Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success())
}

/// A `.ts` entry that pulls in one dependency per module format, so all three
/// load paths (entry, `require` of `.cts`, `import` of `.mts`) are covered by one
/// process. `Debugger.enable` is posted before either dependency loads, which is
/// what makes their `scriptParsed` notifications observable.
const PROBE: &str = r#"const inspector = require("node:inspector");
const { pathToFileURL } = require("node:url");

const session = new inspector.Session();
session.connect();
const seen: Record<string, boolean> = {};
session.on("Debugger.scriptParsed", ({ params }: any) => {
  if (/\.[cm]?ts$/.test(params.url)) seen[params.url] = params.hasSourceURL;
});
session.post("Debugger.enable", async () => {
  require("./dep.cts");
  await import("./dep.mts");
  const self: string = pathToFileURL(__filename).href;
  const expected: string[] = [self, new URL("dep.cts", self).href, new URL("dep.mts", self).href];
  console.log(JSON.stringify({ expected, seen }));
});
"#;

/// Write the fixture into `dir`, run it, and assert every TypeScript file in the
/// graph reported Node's own URL for itself as its script identity.
fn assert_script_urls(dir: &Path, cache: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("package.json"), r#"{ "type": "commonjs" }"#).unwrap();
    std::fs::write(dir.join("probe.ts"), PROBE).unwrap();
    std::fs::write(
        dir.join("dep.cts"),
        "const y: number = 2;\nmodule.exports = y;\n",
    )
    .unwrap();
    std::fs::write(dir.join("dep.mts"), "export const z: number = 3;\n").unwrap();

    for pass in ["cold", "warm"] {
        let output = Command::new(nub_binary())
            .arg("probe.ts")
            .current_dir(dir)
            .env("XDG_CACHE_HOME", cache)
            .output()
            .expect("failed to spawn nub");
        assert!(
            output.status.success(),
            "nub exited {:?} ({pass}) in {}\nstderr: {}",
            output.status,
            dir.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        let parsed: serde_json::Value = serde_json::from_str(stdout.trim())
            .unwrap_or_else(|e| panic!("fixture must emit JSON ({pass}), got {stdout:?}: {e}"));

        for expected in parsed["expected"].as_array().unwrap() {
            let href = expected.as_str().unwrap();
            assert!(
                href.starts_with("file:///"),
                "({pass}) the fixture's own oracle must be a file: URL, got {href:?}"
            );
            assert_eq!(
                parsed["seen"][href],
                serde_json::json!(true),
                "({pass}) {href} must reach the inspector under Node's own URL for it, \
                 with hasSourceURL set.\n  reported: {}",
                parsed["seen"]
            );
        }

        // Without this, a cache directory that never got written — the mkdirSync
        // in transform-core.mjs swallows its error and disables the cache — would
        // silently turn the warm pass into a second cold one, and the entry whose
        // sourceURL is baked in would go untested.
        if pass == "cold" {
            let entries = std::fs::read_dir(cache.join("nub/transpile"))
                .map(|d| d.count())
                .unwrap_or(0);
            assert!(
                entries > 0,
                "the cold pass must leave entries in {}, or the warm pass tests nothing",
                cache.join("nub/transpile").display()
            );
        }
    }
}

/// The baseline contract, plus the host-band plumbing.
///
/// The directory name carries four of the five characters whose escaping depends
/// on the host's Node version, so this is the end-to-end witness that
/// `process.version` reaches the encoder: hand `transformCached` the wrong value
/// and nub spells them the other way while the fixture's oracle — the host's own
/// `pathToFileURL` — does not, and the assertion fails. `|` is left out because
/// Windows forbids it in a filename; `[`, `]`, `^` and `~` are all legal there, so
/// the Windows leg exercises the same plumbing.
///
/// The surrounding temp path has nothing to escape, so it covers the ordinary
/// no-op case in the same run.
#[test]
fn transpiled_typescript_reports_its_file_url_to_the_inspector() {
    if !host_node_usable() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    assert_script_urls(&temp.path().join("w[x]y^z~q"), &temp.path().join("cache"));
}

/// The characters Windows cannot put in a filename, so the test above can never
/// reach them: `%`, `#` and `?` from the always-escaped set, and `|`, the fifth
/// band-sensitive character. A naive `format!("file://{path}")` passes the test
/// above and fails here.
#[cfg(unix)]
#[test]
fn percent_encoded_path_matches_node() {
    if !host_node_usable() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    assert_script_urls(&temp.path().join("a b%c#d?e|f"), &temp.path().join("cache"));
}
