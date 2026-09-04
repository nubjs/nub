//! Bake `data/build-jail-catalog.json` into the crate as `static` Rust, and VALIDATE
//! `data/build-jail-catalog-v2.json` that the library embeds (see the v2 note below the imports —
//! everything in this header about codegen describes the v1 document only).
//!
//! WHY CODEGEN RATHER THAN A RUNTIME PARSE. The catalog is fixed at compile time, so a
//! parse failure is a defect in the tree — not a condition the running jail should ever
//! have to handle. Emitting `&'static` literals means there is no runtime parse, no
//! fallible path to get wrong, and no way for a malformed catalog to reach a user: it
//! fails `cargo build`. `include_str!` + a lazy parse would instead defer the same defect
//! to first use, where the only honest responses are panic or silently jail-less grants.
//!
//! WHERE THE VALIDATION LIVES, and why not here. Moving the tables from Rust literals to a
//! data file opened an editing surface a `static` did not have — a contributor can now write
//! `"siblingDirs": ["../../.."]` in JSON where the equivalent in Rust would have been
//! conspicuous. The checks that re-close that surface live in `src/catalog.rs`, pulled in
//! below with `#[path]`, because the dev-only runtime override has to run the SAME ones at
//! load time and a second copy would drift. This build script's remaining job is codegen:
//! parse via the shared validator, fail the build on `Err`, emit `&'static` literals.

#[path = "src/catalog.rs"]
mod catalog;

// THE V2 CATALOG IS VALIDATED HERE BUT NOT CODEGEN'D, and the asymmetry with v1 is deliberate.
// v1's tables are 34 grants of `&'static str`, so literals are free. A v2 catalog is 343 packages of
// owned `String`/`Vec`/`BTreeMap` with nested version bands and per-OS overlays, and it exists in
// order to grow to thousands — emitting constructor code for that would push megabytes of generated
// Rust through rustc on every build to save a single parse. The library therefore `include_str!`s
// the same file and parses it ONCE behind a `LazyLock`.
//
// THE PROPERTY THE HEADER ABOVE CARES ABOUT SURVIVES: a malformed catalog still cannot reach a user,
// because this script parses the very bytes the library embeds, through the very same parser, and
// fails `cargo build` on `Err`. What is given up is only the theoretical infallibility of the
// runtime path — and that parse cannot fail on input the build has already accepted.
//
// `catalog_v2::Entry::grant_for` calls `crate::compiler::version_scope::applies`, so the REAL
// predicate is pulled in under the path it expects rather than stubbed: a stub would be a second
// implementation of narrowest-bound-wins, free to drift from the one the jail actually uses.
// `dead_code` because a build script calls only `parse`: the lookup half (`grant_for`, `applies`,
// `Platform::current`, `emit`) exists for the library and is unreachable here. Allowing is right
// rather than trimming — these files are the LIBRARY's, pulled in verbatim so the validator cannot
// drift, and shaping them around the build script's subset is what would let it drift.
#[allow(dead_code)]
#[path = "src/compiler/version_scope.rs"]
pub mod version_scope;
mod compiler {
    pub use super::version_scope;
}

#[allow(dead_code)]
#[path = "src/catalog_v2.rs"]
mod catalog_v2;

use std::fmt::Write as _;
use std::path::Path;

const CATALOG: &str = "data/build-jail-catalog.json";
const CATALOG_V2: &str = "data/build-jail-catalog-v2.json";

fn main() {
    println!("cargo:rerun-if-changed={CATALOG}");
    println!("cargo:rerun-if-changed={CATALOG_V2}");
    println!("cargo:rerun-if-changed=build.rs");
    // The validator is a separate file now, so a change to it must re-run this script.
    println!("cargo:rerun-if-changed=src/catalog.rs");
    println!("cargo:rerun-if-changed=src/catalog_v2.rs");
    println!("cargo:rerun-if-changed=src/compiler/version_scope.rs");

    let text = std::fs::read_to_string(CATALOG)
        .unwrap_or_else(|e| fail(&format!("cannot read {CATALOG}: {e}")));
    let catalog = catalog::parse(&text).unwrap_or_else(|e| fail(&format!("{CATALOG}: {e}")));

    // GATE THE BAKED V2 DOCUMENT. Parsed and thrown away: the library embeds the same bytes and
    // parses them itself, so the only job here is to turn a malformed catalog into a BUILD failure
    // instead of a jail that quietly grants the wrong thing. Every band is version-parsed as a side
    // effect, which is what makes a bad `versions` range fail here rather than at a user's spawn.
    let v2_text = std::fs::read_to_string(CATALOG_V2)
        .unwrap_or_else(|e| fail(&format!("cannot read {CATALOG_V2}: {e}")));
    let v2 = catalog_v2::parse(&v2_text).unwrap_or_else(|e| fail(&format!("{CATALOG_V2}: {e}")));
    // An EMPTY packages map would parse cleanly and grant nothing to everything — the exact silent
    // failure that cost Windows 67-87% egress. A baked catalog with no packages is a defect.
    if v2.packages.is_empty() {
        fail(&format!(
            "{CATALOG_V2}: parsed cleanly but contains ZERO packages"
        ));
    }

    let out = std::env::var("OUT_DIR").expect("cargo sets OUT_DIR");
    let out = Path::new(&out);
    emit(&out.join("download_hosts.rs"), &gen_hosts(&catalog));
    emit(&out.join("curated_grants.rs"), &gen_grants(&catalog));
    emit(
        &out.join("package_network.rs"),
        &gen_package_network(&catalog),
    );
}

fn emit(path: &Path, src: &str) {
    std::fs::write(path, src)
        .unwrap_or_else(|e| fail(&format!("cannot write {}: {e}", path.display())));
}

fn gen_hosts(catalog: &catalog::Catalog) -> String {
    let mut src = String::from(
        "// @generated by build.rs from data/build-jail-catalog.json — do not edit.\n\
         /// The hosts NUB ITSELF may fetch from in the prefetcher, OUTSIDE the jail and before\n\
         /// a script runs — an SSRF allowlist on nub's own outbound GETs, plus the `$downloads`\n\
         /// net token.\n\
         ///\n\
         /// NOT the build jail's egress allowlist, which is what this comment claimed until\n\
         /// 2026-07-31. The jail does no host filtering at all: `build_jail_net` yields a\n\
         /// per-package BOOLEAN and starts no proxy, so an admitted package reaches every host\n\
         /// it likes and a refused one reaches none. Granting a package egress therefore never\n\
         /// widens this list, and this list never narrows a package's egress. The wording is\n\
         /// corrected loudly rather than quietly because the stale version was read back as a\n\
         /// jail metric — 110 packages were granted egress while this held at 4 hosts, and the\n\
         /// two numbers describe different subsystems. Membership criteria, per-host\n\
         /// provenance, and the record of what was refused live in the catalog JSON.\n\
         pub const DOWNLOAD_HOSTS: &[&str] = &[\n",
    );
    for host in &catalog.download_hosts {
        let _ = writeln!(src, "    {host:?},");
    }
    src.push_str("];\n");
    src
}

fn gen_grants(catalog: &catalog::Catalog) -> String {
    let mut src = String::from(
        "// @generated by build.rs from data/build-jail-catalog.json — do not edit.\n\
         static CURATED_GRANTS: &[(&str, CuratedGrant)] = &[\n",
    );

    for grant in &catalog.package_grants {
        let name = &grant.package;
        let siblings = &grant.sibling_dirs;
        let reads = &grant.project_reads;
        let cwd = grant.project_cwd;
        let writes = match &grant.project_writes {
            None => "ProjectWrites::None".to_string(),
            Some(catalog::ProjectWriteSource::ManifestField(keys)) => {
                format!("ProjectWrites::ManifestField(&{keys:?})")
            }
            Some(catalog::ProjectWriteSource::Literal(paths)) => {
                format!("ProjectWrites::Literal(&{paths:?})")
            }
        };
        // Each chain gets its own `&` so the outer literal is a slice OF SLICES: the derived
        // `Debug` of a `Vec<Vec<String>>` emits a bare `[[..]]`, an array of ARRAYS, and only
        // the inner `&` gives the unsizing coercion something to apply to. Written as a plain
        // `&[..]` reference rather than a `&[..][..]` reslice so the whole literal stays a
        // coercion — the target type is the struct field, which is known — and needs no
        // indexing operation inside a `static` initializer.
        let chains = grant
            .dependency_dirs
            .iter()
            .map(|c| format!("&{c:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        // All three platforms are emitted and the choice is made at RUN time by `cfg!`,
        // because this script runs on the HOST: selecting here would bake the machine that
        // built the binary into a cross-compiled one.
        let homes = grant
            .home_paths
            .iter()
            .map(|h| {
                format!(
                    "HomePath {{ env: {:?}, macos: {}, linux: {}, windows: {} }}",
                    h.env,
                    opt_str(&h.macos),
                    opt_str(&h.linux),
                    opt_str(&h.windows)
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let versions = opt_str(&grant.versions);
        let full_disk = grant.full_disk;
        let _ = write!(
            src,
            "    (\n        {name:?},\n        CuratedGrant {{\n            \
             versions: {versions},\n            \
             full_disk: {full_disk},\n            \
             sibling_dirs: &{siblings:?},\n            dependency_dirs: &[{chains}],\n            \
             home_paths: &[{homes}],\n            \
             project_reads: &{reads:?},\n            \
             project_writes: {writes},\n            project_cwd: {cwd},\n        }},\n    ),\n"
        );
    }

    src.push_str("];\n");
    src
}

fn gen_package_network(catalog: &catalog::Catalog) -> String {
    let mut src = String::from(
        "// @generated by build.rs from data/build-jail-catalog.json — do not edit.\n\
         /// Packages whose build scripts MAY reach the network, as the union of\n\
         /// `networkHosts[].fetchedBy` and `packageNetwork.full` minus\n\
         /// `notGranted.packages`, each paired with the semver range its grant is scoped\n\
         /// to — `None` meaning every version. Sorted by name, so the lookup may\n\
         /// binary-search.\n\
         ///\n\
         /// The range travels WITH the name rather than in a second table because the two\n\
         /// are one grant: a consumer that could read the name alone would be reading a\n\
         /// package as admitted at versions the catalog does not admit it at.\n\
         pub const PACKAGE_NETWORK_ALLOWED: &[(&str, Option<&str>)] = &[\n",
    );
    for grant in &catalog.package_network_allowed {
        let _ = writeln!(
            src,
            "    ({:?}, {}),",
            grant.package,
            opt_str(&grant.versions)
        );
    }
    src.push_str("];\n");
    src
}

/// An `Option<String>` as the `Option<&'static str>` literal the generated tables hold.
fn opt_str(value: &Option<String>) -> String {
    match value {
        Some(s) => format!("Some({s:?})"),
        None => "None".to_string(),
    }
}

fn fail(msg: &str) -> ! {
    // Printed as a cargo error so it surfaces at the top of the build output rather than
    // buried in a panic backtrace.
    println!("cargo:warning={msg}");
    panic!("build-jail catalog is invalid — {msg}");
}
