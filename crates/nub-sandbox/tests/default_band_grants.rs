//! A `default` band that names no capability is a DENY, not a fallback.
//!
//! ⛔ THE MECHANISM, because it reads the opposite way round. `Entry::grant_for` picks the narrowest
//! `<X` bound that covers the version and does NOT merge `default` in as a base those bands extend,
//! so a release above every band lands on `default` alone. And a `default` that carries only a
//! `notes` string is not the same as having no entry: an ABSENT package falls back to
//! `catalog_v2::baseline_caps()` and still promotes the baseline's `write_paths`, while a PRESENT
//! entry resolves to `Some(grant)` with every capability empty and returns before the promotion loop
//! (`pm_engine::build_jail`). A notes-only band is therefore STRICTLY TIGHTER than saying nothing.
//!
//! ⛔ WHY THAT IS A DEFECT HERE AND NOT EVERYWHERE. An empty `default` is CORRECT for a package whose
//! current release genuinely does nothing -- it dropped its lifecycle hook, or it ships prebuilt --
//! and many entries are legitimately that. What is not correct is a band nobody ever measured
//! resolving to a hard deny, and the shape alone does not separate the two cases. `electron` is the
//! one where the entry's OWN note settles it -- its `<43.4.0` band records that withdrawing network
//! made a cold install fail `getaddrinfo ENOTFOUND` fetching `electron-v33.4.11-darwin-arm64.zip`
//! from github.com, and the dist-tag has since moved to 44.x, which lands on `default`.
//!
//! ⛔ THE SEPARATOR IS NOW MECHANICAL AND IS ENFORCED CATALOG-WIDE by
//! [`no_cell_denies_everything_unless_its_package_runs_no_lifecycle_hook`], which subsumes the
//! per-entry lists below: the ONLY licence for a cell that widens nothing is that nothing the cell
//! covers executes, because `aube_scripts::has_dep_lifecycle_work` is then false and no grant is
//! ever compiled. 36 entries hold that licence today; every other cell carries at least
//! `write: {deps: true}`.
//!
//! Reads the shipped bytes through `include_str!` rather than the runtime lookup, which consults a
//! dev override and an on-disk update tier first; the subject here is the file in this repository.
use nub_sandbox::catalog_v2::{Catalog, Platform, Scope};

fn shipped() -> Catalog {
    nub_sandbox::catalog_v2::parse(include_str!("../data/build-jail-catalog-v2.json"))
        .expect("the shipped catalog parses; build.rs fails the build otherwise")
}

/// `44.0.0` is above every `electron` band, so it resolves to `default`; `33.4.11` is one of the
/// versions the `<43.4.0` band was measured at. The pair is the control: if the band below stopped
/// granting egress too, the accessor is broken rather than the default band.
#[test]
fn electron_still_reaches_github_at_a_version_above_every_measured_band() {
    let catalog = shipped();
    let entry = catalog
        .packages
        .get("electron")
        .expect("electron has a catalog entry");

    let mut denied: Vec<String> = Vec::new();
    for platform in [Platform::Macos, Platform::Linux, Platform::Windows] {
        // CONTROL first: a version INSIDE the measured band must still carry egress. Without it a
        // failure below is equally well explained by the lookup returning nothing at all.
        let measured = entry.grant_for(Some("33.4.11")).on(platform);
        assert!(
            measured.network,
            "control failed on {}: the measured <43.4.0 band lost its egress, so this file is no \
             longer testing what it claims",
            platform.key()
        );

        let latest = entry.grant_for(Some("44.0.0")).on(platform);
        if !latest.network {
            denied.push(format!("{}: network", platform.key()));
        }
        // The postinstall unpacks the download into the package directory.
        if !latest.write.covers(Scope::Deps) {
            denied.push(format!("{}: write.deps", platform.key()));
        }
    }

    assert!(
        denied.is_empty(),
        "electron@44.0.0 resolves to a band that denies {}. Its postinstall downloads a platform \
         zip from github.com, and a notes-only `default` denies more than having no entry at all: \
         {}",
        denied.len(),
        denied.join(", ")
    );
}

/// The same defect, generalised over the entries where a measurement settles it.
///
/// ⛔ THE SHAPE IS NOT THE DEFECT, WHICH IS WHY THIS LIST IS NAMED RATHER THAN DERIVED. When it was
/// written, 94 entries carried a `default` that widened nothing while a lower band granted
/// something, and the discriminator used to pick these nine out was per-OS and historical -- take
/// the HIGHEST measured version on each OS where the band grants, and ask whether ITS measurement
/// was also empty. These are the entries where it was not AND the current release still runs an
/// install script.
///
/// ⛔ THAT DISCRIMINATOR IS NOW KNOWN TO BE TOO WEAK, and this list survives only because it is a
/// narrower and still-true claim. "The highest measured version also measured empty" reads as
/// evidence that the package needs nothing; it is not. The grant search materialises its cheapest
/// rung as NO CATALOG ENTRY for the package under test (`harness/search.mjs`: `if
/// (state.atoms.size === 0) return withFloor({ packages })`, commented "State 0 is the BASE PROFILE,
/// and the catalog spells that as NO ENTRY for this package"), and an absent package takes
/// [`catalog_v2::baseline_caps`]. So an empty measurement means the BASELINE sufficed -- egress,
/// `write: {deps}` and the promotion list -- and never that nothing was needed. The search has no
/// rung below that, so a cell granting less than it was never measured at all. The catalog-wide
/// guard at the bottom of this file is the form that follows from it.
///
/// The `default` grants here were set equal to the band, per-OS blocks included, because the
/// versions that land on `default` are unmeasured and an unmeasured version takes the measured
/// grant until someone measures it. The assertion is deliberately weaker than that equality --
/// "grants SOMETHING where the band grants something" -- so that a later real measurement may
/// narrow one of these without having to delete the test.
#[test]
fn no_repaired_entry_denies_on_an_os_its_measured_band_grants() {
    let catalog = shipped();

    // Repaired 2026-09-01. Each dropped its `default` to a notes-only band while its measured band
    // still granted, and each ships a current release that runs an install script.
    const REPAIRED: &[&str] = &[
        "electron",
        "@opencode-ai/cli",
        "@pulumi/docker-build",
        "@pulumi/kubernetes",
        "@apollo/protobufjs",
        "@heroui/shared-utils",
        "@progress/kendo-licensing",
        "leveldown",
        "subrequests",
    ];

    // Above every `<X` bound in the catalog, so it lands on `default` for all of these without
    // pinning the test to a dist-tag that moves under it.
    const ABOVE_EVERY_BAND: &str = "9999.0.0";

    let mut denied: Vec<String> = Vec::new();
    let mut granting_cells = 0usize;

    for name in REPAIRED {
        let entry = catalog
            .packages
            .get(*name)
            .unwrap_or_else(|| panic!("{name} has a catalog entry"));
        let band = entry
            .versions
            .first()
            .unwrap_or_else(|| panic!("{name} has at least one measured band"));

        for platform in [Platform::Macos, Platform::Linux, Platform::Windows] {
            let measured = band.grant.on(platform);
            if measured.widens_nothing() {
                // The band grants nothing here either, so an empty `default` says the same thing.
                continue;
            }
            granting_cells += 1;

            if entry
                .grant_for(Some(ABOVE_EVERY_BAND))
                .on(platform)
                .widens_nothing()
            {
                denied.push(format!("{name} on {}", platform.key()));
            }
        }
    }

    // CONTROL. Without it every band could have lost its grant and the loop above would pass by
    // skipping every cell -- the exact failure this file exists to catch, inverted.
    assert!(
        granting_cells >= REPAIRED.len(),
        "control failed: only {granting_cells} of the repaired entries' bands grant anything at \
         all, so this test is no longer exercising the case it names"
    );

    assert!(
        denied.is_empty(),
        "{} repaired entr(y/ies) went back to denying everything on an OS their own measured band \
         grants: {}",
        denied.len(),
        denied.join(", ")
    );
}

/// The same defect where a `<X` band can NEVER be the answer, because every version that executes
/// anything is a PRERELEASE.
///
/// ⛔ WHY THESE ARE NOT THE CASE ABOVE. The entries in `REPAIRED` were caught by "the current release
/// still runs an install script". None of these does, and that is exactly what makes them worse
/// rather than better. `version_scope::applies` cannot admit a prerelease to a plain `<X` bound at
/// all, so for a package whose hook-bearing versions are ALL prereleases the band is unreachable for
/// every version that actually runs, and `default` is the only grant those versions can ever get. A
/// notes-only `default` therefore denied everything to the ONLY executing versions of the package --
/// an under-grant with no version left anywhere in the entry to correct it.
///
/// ⛔ THE COUNTS ARE THE POINT, AND THEY ARE NOT SMALL. Enumerated from both packument flavours
/// (`scripts` from the full document, `hasInstallScript` from the abbreviated one -- each is absent
/// from the other, and reading either off the wrong document manufactures a convincing fake zero):
///
///   @tensorflow/tfjs-backend-wasm     2 of 88    @pulumi/azuread      239 of 952
///   angularx-qrcode                   3 of 122   @pulumi/cloudflare   204 of 887
///   @eth-optimism/core-utils         28 of 228   @pulumi/datadog      153 of 814
///   @eth-optimism/sdk               236 of 500   @pulumi/postgresql   139 of 682
///
/// 1,004 hook-bearing versions in total, and EVERY ONE is a prerelease: no stable release above any
/// of these bounds runs anything, because each package dropped its hook in later stables. No version
/// of any of the eight sets `gypfile`, and `binding.gyp` is absent from packed tarballs sampled
/// across each range, so `implicit_install_script` cannot fire either.
#[test]
fn a_prerelease_only_hook_bearing_entry_grants_on_the_band_its_versions_actually_reach() {
    let catalog = shipped();

    // (package, a prerelease that declares a lifecycle hook, a release INSIDE the `<X` band)
    const SUBJECTS: &[(&str, &str, &str)] = &[
        ("@tensorflow/tfjs-backend-wasm", "1.4.0-alpha2", "3.0.0"),
        ("angularx-qrcode", "1.7.0-beta.5", "13.0.0"),
        (
            "@eth-optimism/core-utils",
            "0.0.0-develop-20230815225108",
            "0.13.1",
        ),
        ("@eth-optimism/sdk", "0.0.0-develop-20230815225108", "3.2.1"),
        ("@pulumi/azuread", "0.0.1-dev.1556229421", "5.9.0"),
        ("@pulumi/cloudflare", "0.0.1-dev.1552002909", "5.9.0"),
        ("@pulumi/datadog", "0.0.1-dev.1561157133", "4.9.0"),
        ("@pulumi/postgresql", "0.18.1-dev.1561141856", "3.9.0"),
    ];

    let mut denied: Vec<String> = Vec::new();

    for (name, prerelease, in_band) in SUBJECTS {
        let entry = catalog
            .packages
            .get(*name)
            .unwrap_or_else(|| panic!("{name} has a catalog entry"));

        // CONTROL ON THE ROUTING, not on the grant. If a prerelease ever started matching a `<X`
        // bound, every assertion below would silently be about the band instead of `default`, and
        // this test would keep passing while testing something else.
        //
        // ⛔ WHAT THE `assert_ne!` ACTUALLY RESTS ON, because it is weaker than it looks for six of
        // the eight. The repair sets `default` equal to its band, so for those six the two grants
        // are capability-identical BY CONSTRUCTION and differ only in `notes`. `Grant` derives
        // `PartialEq` over every field including `notes`, so the pair is still distinguishable and
        // the equality above still pins the routing -- but the capability assertion below is what
        // carries the grant, and this control must not be read as proving more than routing.
        assert_eq!(
            entry.grant_for(Some(prerelease)),
            &entry.default,
            "{name}@{prerelease} no longer falls through to `default`, so this test is no longer \
             exercising the prerelease fallthrough it is named for"
        );
        assert_ne!(
            entry.grant_for(Some(in_band)),
            &entry.default,
            "control failed: {name}'s band and `default` now grant the same thing, so the \
             fallthrough assertion above cannot distinguish them"
        );

        for platform in [Platform::Macos, Platform::Linux, Platform::Windows] {
            if entry
                .grant_for(Some(prerelease))
                .on(platform)
                .widens_nothing()
            {
                denied.push(format!("{name}@{prerelease} on {}", platform.key()));
            }
        }
    }

    assert!(
        denied.is_empty(),
        "{} cell(s) deny everything to the only versions of their package that execute anything, \
         which is strictly tighter than having no catalog entry: {}",
        denied.len(),
        denied.join(", ")
    );
}

/// The 37 packages licensed to keep a cell that grants NOTHING, because nothing the cell covers
/// executes and so no grant is ever compiled for it.
///
/// ⛔ THE LICENCE IS "EXECUTES NOTHING", NOT "MEASURED EMPTY", and the difference is the whole
/// reason this list exists. `aube_scripts::has_dep_lifecycle_work` is the gate: false when the
/// manifest declares none of `preinstall`/`install`/`postinstall` AND
/// `implicit_install_script` cannot fire, which needs a `binding.gyp` in the PACKED TARBALL. A
/// package that passes that gate never reaches the jail, so what its cell says is inert.
///
/// ⛔ VERIFIED PER COVERED VERSION, NOT PER PACKAGE, because a cell is a BAND. A plain `<X` bound
/// admits no prerelease (`version_scope::applies`), so `default` catches every prerelease PLUS the
/// stables above the highest bound -- for `electron` that is 718 versions, 665 of which declare an
/// install hook. Each name below was enumerated over exactly the versions its empty cell covers,
/// from the FULL packument (`scripts` lives only there; `hasInstallScript` only in the abbreviated
/// one, and reading either off the wrong document manufactures a convincing zero). Zero of 37
/// declares a hook at any covered version.
///
/// ⛔ THE REGISTRY'S `gypfile` FLAG IS NOT THE IMPLICIT-HOOK TEST AND WOULD HAVE MISSED THE ONE REAL
/// CASE. `better-sqlite3@13.0.3` -- the single version its `default` covers -- ships a `binding.gyp`
/// with no `install` or `preinstall`, so `implicit_install_script` returns `node-gyp rebuild`; the
/// packument reports `gypfile` unset and `hasInstallScript` false for it. It is absent from this
/// list for that reason. Packed tarballs were read across each remaining band's range as the check.
#[rustfmt::skip]
const EXECUTES_NOTHING: &[&str] = &[
    "@copilotkit/aimock",
    "@hyperjump/json-pointer",
    "@hyperjump/json-schema",
    "@hyperjump/pact",
    "@lottiefiles/lottie-player",
    "@nuxt/components",
    "@stdlib/math-base-assert-is-nan",
    "@stdlib/math-base-napi-binary",
    "@stdlib/number-float64-base-to-words",
    "@team-plain/typescript-sdk",
    "@vscode/ripgrep",
    "angularx-qrcode",
    "aws-iot-device-sdk-v2",
    "axios-cache-interceptor",
    "ctrlc-windows",
    "cz-customizable",
    "docxtemplater",
    "eckles",
    "electron-vite",
    "es-check",
    "eth-gas-reporter",
    "exiftool-vendored",
    "farmhash",
    "flow-bin",
    "impit",
    "json-schema-library",
    "postcss-rtlcss",
    "powerbi-models",
    "prisma-json-types-generator",
    "protoc",
    "qlobber",
    "rc-editor-core",
    "scrollmirror",
    "snappy",
    "squawk-cli",
    "victory-bar",
    "victory-scatter",
];

/// No cell in the shipped catalog may grant NOTHING unless its package executes nothing.
///
/// ⛔ WHY THIS IS AN INVARIANT AND NOT A LIST OF REPAIRS. A cell granting nothing is not a tight
/// grant, it is a grant BELOW THE CHEAPEST THING EVER MEASURED. The grant search's bottom rung is
/// spelled as no catalog entry, which resolves to [`catalog_v2::baseline_caps`] -- egress,
/// `write: {deps}`, and `BASELINE_WRITE_PATHS`. So `grant: {}` in a corpus record means "the
/// baseline sufficed", and a cell narrower than the baseline was never tested. That is the same
/// reading `windows_base_profile_withdrawals.rs` states for its own 28 rows -- "a pass at that rung
/// licenses withdrawing the real-home write and the whole-disk write, and NOTHING MORE ... it does
/// not license withdrawing `write.deps`" -- and this generalises it from win32 to every cell.
///
/// 279 cells were lifted onto `write: {deps: true}` for exactly that reason. `write.deps` is the
/// floor rather than the full baseline because the network axis is a separate question the base
/// rung cannot answer either way, and because a floor on the write axis alone leaves the wide-cell
/// census untouched -- `deps` is neither `userHome` nor `disk`.
#[test]
fn no_cell_denies_everything_unless_its_package_runs_no_lifecycle_hook() {
    let catalog = shipped();
    let licensed: std::collections::BTreeSet<&str> = EXECUTES_NOTHING.iter().copied().collect();

    let mut offending: Vec<String> = Vec::new();
    let mut licence_used: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    let mut empty_cells = 0usize;

    for (name, entry) in &catalog.packages {
        let bands = std::iter::once(("default", &entry.default))
            .chain(entry.versions.iter().map(|b| (b.range.as_str(), &b.grant)));
        for (range, grant) in bands {
            for platform in [Platform::Macos, Platform::Linux, Platform::Windows] {
                if !grant.on(platform).widens_nothing() {
                    continue;
                }
                empty_cells += 1;
                match licensed.get(name.as_str()) {
                    Some(licensed_name) => {
                        licence_used.insert(licensed_name);
                    }
                    None => offending.push(format!("{name} [band {range}] on {}", platform.key())),
                }
            }
        }
    }

    // CONTROL 1 — the walk reaches cells at all. Without it a change that made `widens_nothing`
    // always false, or that emptied `packages`, would pass this test by examining nothing.
    assert!(
        empty_cells > 0,
        "control failed: not one cell in the shipped catalog widens nothing, so this test cannot \
         distinguish a repaired catalog from a broken traversal"
    );

    // CONTROL 2 — every licence is live. A name whose cells all grant something is a DEAD entry,
    // and a dead entry silently pre-licenses the next empty cell that package acquires.
    let dead: Vec<&str> = EXECUTES_NOTHING
        .iter()
        .copied()
        .filter(|n| !licence_used.contains(n))
        .collect();
    assert!(
        dead.is_empty(),
        "{} name(s) in EXECUTES_NOTHING no longer have any cell that grants nothing, so the licence \
         is dead and would pre-approve a future under-grant. Drop them:\n  {}",
        dead.len(),
        dead.join("\n  ")
    );

    assert!(
        offending.is_empty(),
        "{} cell(s) grant NOTHING for a package that runs a lifecycle hook. That is strictly \
         tighter than having no catalog entry, because absence takes `baseline_caps()` -- egress, \
         `write: {{deps}}` and the promotion list -- and the grant search has no rung below it, so \
         nothing ever measured the package without them. Spell the cell `write: {{deps: true}}` at \
         minimum, or add the package to EXECUTES_NOTHING with the per-covered-version evidence \
         that nothing it covers runs one:\n  {}",
        offending.len(),
        offending.join("\n  ")
    );
}
