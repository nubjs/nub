//! The Linux `write.userHome` grants withdrawn once a five-arm ladder showed the write was free.
//!
//! ⛔ THE MEASUREMENT SHAPE, because a green arm proves nothing on its own. Each cell was run under
//! Landlock ABI 7 on five arms: a JAIL-OFF positive control, the shipped grant, the shipped grant
//! minus `network`, the shipped grant minus every reach over the user's home, and an empty grant as
//! the RED negative control. A cell is here only when the minus-home arm is GREEN and at least one
//! other arm went RED naming its own cause -- so the descent proved the retained capabilities
//! necessary rather than merely observing one arm pass.
//!
//! ⛔ AND A WIDE-VERSUS-NARROW HOME DIFFERENTIAL, which is the half that answers the objection
//! below. Comparing the arm's ENTIRE jail `$HOME` under the shipped grant against the same install
//! with the home reach dropped shows nothing the wider grant produced: 16 entries against 16 for
//! `@mui/x-telemetry`, 15 against 15 for `@pact-foundation/pact-node`, 18 against 18 for
//! `@pdftron/pdfnet-node`, each with zero files on either side.
//!
//! ⛔ THIS REVERSES THE 2026-09-01 LINUX HALF OF THE corpus-epoch-71 RESTORE, and the two rows it
//! removed from `repaired_home_write_grants.rs` say so. That restore argued the corpus artifact gate
//! walks only the package's own directory, so a write to the user's REAL home is invisible to the
//! drop arm and the narrowing was never earned. The argument is sound and it does not reach these
//! cells: the OBSERVE census that attributed those writes runs the script with the JAIL OFF, where
//! `$HOME` is the real home. Under the jail the same line resolves to the redirected private home.
//! `@mui/x-telemetry` is the clearest -- `postinstall/storage.js` reads
//! `XDG_CONFIG_HOME || os.homedir() + '/.config'`, the jail's env allowlist drops
//! `XDG_CONFIG_HOME`, and the file lands at `.cache/nub/jail-home/<hash>/.config/mui-x/config.json`.
//! So epoch 71 measured an UNJAILED write and inferred a jailed capability requirement from it.
//!
//! ⛔ NOT EVERY CELL HERE IS THAT STORY. The playwright family and `electron-chromedriver` narrow
//! for the tool-cache reason `macos_home_write_withdrawals.rs` sets out at length:
//! `pm_engine::build_jail` points `PLAYWRIGHT_BROWSERS_PATH` and `electron_config_cache` at
//! `$cache/nub/pm/tools/{ms-playwright,electron-cache}`, which `compiler::preset` grants read-write
//! to every jailed script unconditionally. `@shoelace-style/shoelace` shares it because its
//! `postinstall` is literally `npx playwright install`.
//!
//! ⛔ THESE ARE HAND EDITS ON A GENERATED FILE, WHICH IS WHY THEY NEED PINNING. `build.rs` proves
//! the catalog parses and nothing more; it cannot know that a per-OS overlay says what a measurement
//! said, so a re-bake from the archived records would restore all ten with no signal at all. This
//! file is the Linux counterpart to `macos_home_write_withdrawals.rs`.
//!
//! NOT HERE ON PURPOSE: `unrs-resolver`. Its Linux arms read as a clean narrowing, but the same
//! package was REFUTED on macOS -- the fixture never installed the platform optionalDependency, so
//! the install script took its missing-dependency workaround branch and died fetching from the
//! registry, which proves the fallback needs egress rather than that the package needs the home.
//! An under-grant is worse than an over-grant, and the control below asserts it keeps the grant
//! until that is settled.
//!
//! ⛔ RESOLVED 2026-09-04, AND THE ANSWER IS YES: THE LINUX FIXTURE HAS THE SAME FLAW. Measured on
//! all three platforms at `5d987facc2` with the sweep harness (`--only unrs-resolver --keep-logs`),
//! the child log carries the identical three lines everywhere -- darwin-arm64, linux-x64-gnu and
//! win32-x64-msvc alike:
//!
//! ```text
//! [napi-postinstall] Trying to install package "@unrs/resolver-binding-<platform>" using npm
//! [napi-postinstall] Failed to install ... Cannot find module 'unrs-resolver/package.json'
//! [napi-postinstall] Trying to download "https://registry.npmjs.org/@unrs/resolver-binding-..."
//! ```
//!
//! So the script takes the missing-dependency workaround branch and falls through to a registry
//! DOWNLOAD on every platform; it never reaches whatever the home write exists for.
//!
//! ⛔⛔ WHY THE OBVIOUS EXPERIMENT IS A TRAP, AND WHAT REPLACES IT. A two-arm catalog-override run
//! (`write.userHome` present vs OMITTED -- `false` is rejected by the schema) verdicts `OK` on BOTH
//! arms on ALL THREE platforms. That reads like a clean narrowing and it is worthless, because the
//! VERDICT cannot discriminate here: `napi-postinstall` catches a failed `installUsingNPM` and
//! falls through to a registry download, so the script exits 0 either way.
//!
//! ⛔ AN EARLIER VERSION OF THIS NOTE PRESCRIBED THE WRONG FIX -- it said a valid experiment must
//! first install the platform `optionalDependency` so the script takes its "real" branch. That is
//! backwards. The missing-dependency branch IS the operative one: the write under test happens at
//! `napi-postinstall/lib/index.js:222`, inside the `catch` that runs when `downloadedNodePath`
//! fails, and it resolves `meta.name` (napi-postinstall ITSELF) to reach
//! `<napi-postinstall entry>/node_modules/unrs-resolver`. Installing the optionalDependency makes
//! the script exit before ever reaching it, testing nothing.
//!
//! ⇒ JUDGE BY THE ARTIFACT, NOT THE VERDICT: does `<napi-postinstall entry>/node_modules/
//! unrs-resolver` exist after the run? MEASURED on macOS at `b36de9ea2c` with three arms, one of
//! which is a NEGATIVE CONTROL that must fail:
//!
//! ```text
//! wide    write {deps, project, userHome}  home ACE granted      artifact PRESENT   rc=0
//! narrow  write {deps, project}            home ACE NOT granted  artifact PRESENT   rc=0
//! minimal write {project}                  home ACE NOT granted  artifact ABSENT    rc=1
//!         `-> EPERM: operation not permitted, mkdir
//!             '…/store/napi-postinstall@0.3.4-…/node_modules/unrs-resolver'
//! ```
//!
//! The control failed loudly at exactly the predicted path, which is what makes the other two arms
//! readable: the artifact discriminates, and `deps` is what carries the write -- dropping `deps`
//! breaks the install, dropping `userHome` does not. That is `resolve_declared_dep`'s entry
//! widening doing precisely the job its doc says it was widened for.
//!
//! ⛔ A SECOND, DIFFERENTLY-SHAPED MEASUREMENT LIVES HERE TOO, and conflating the two would put a
//! claim on rows that never earned it. `WITHDRAWN` above is the five-arm ladder. `WITHDRAWN_BANDS`
//! below is a TWO-BINARY DIFFERENTIAL against `46b623e352`, which made
//! `compiler::preset::materialize_tool_leaf` create `$cache/nub/pm/tools/{ms-playwright,
//! electron-cache}` before the confined launch. Before it, `push_rw_path` stamped those leaves
//! `FsOrigin::Speculative` and `backend::linux_grants::compile_mount_plan` DROPPED the rule for a
//! path it could not `open(O_PATH)`, so the package's own `mkdir` hit the read-only `tools` parent.
//!
//! Each band row was run on a real Landlock ABI 7 kernel on two binaries one source line apart --
//! `materialize_tool_leaf` live versus a body replaced by `let _ = (homes, path);` -- with the leaf
//! asserted ABSENT immediately before each arm, because the fault only bites on a machine that has
//! not already run an unjailed install. The neutered arm is the RED control and it named its own
//! cause: `EACCES: permission denied, mkdir '/home/nub/.cache/nub/pm/tools/ms-playwright'`, with 55
//! Landlock rules attached. The live arm attached 57 and installed clean at `network` alone, leaving
//! a 252274856-byte chromium in the leaf. +2 and not +3 because `redirect_npm_prefix` already
//! materialized the third leaf.
//!
//! ⛔ NETWORK IS ASSERTED AS RETAINED, NOT AS MEASURED-NECESSARY, FOR THE BAND ROWS. The ladder rows
//! each had a red arm that named `network`; the band rows did not test it, and the venue could not
//! have answered it -- a `network:false` arm on that host installed 358 MB over the wire, so its
//! egress gate was not enforcing. The shared assertion below still guards against a re-bake dropping
//! egress, which is what it is for; it just is not evidence about these seven cells.
//!
//! ⛔ A THIRD SHAPE, AND IT IS KEPT SEPARATE FOR THE SAME REASON THE SECOND IS. `WITHDRAWN_DESCENT`
//! is the CORPUS DESCENT re-run on a real Linux host: each cell's ladder was walked until the
//! narrowest grant that still reproduces the package's artefacts, and the harness reported that
//! grant as a VERIFIED minimum. It is neither the five-arm ladder nor the two-binary differential,
//! and writing its rows into either list would put a claim on them they did not earn.
//!
//! ⛔ WHAT MAKES A DESCENT ROW CITABLE IS A RED ARM, AND THAT GATE IS WHY THIS LIST IS FOUR ROWS AND
//! NOT TWENTY-NINE. The same batch returned a clean-looking verdict for cells whose every arm came
//! back green -- which means the venue produced NO SIGNAL, not that the package needs nothing, and
//! `@depot/cli`, `keccak`, `purescript` and `ursa-optional` therefore keep their grants and stay in
//! `repaired_home_write_grants.rs`. Every row below had at least one arm go red on an ARTEFACT
//! divergence rather than an exit code: `@netlify/esbuild` reproduces 1836 of 1851 artefacts with
//! `network` dropped against 1851 of 1851 with it retained, and none of the four is an early-exit
//! artefact -- each spec recorded real work, 6 to 62 writes and 2 network peers.
//!
//! ⛔ WHERE A BAND HAD SEVERAL MEASURED VERSIONS THE ROW IS THE UNION, NOT THE LAST ONE MEASURED.
//! `esbuild <0.17.19` is the case that matters: `0.16.17` verified an EMPTY grant, but it produced
//! no red arm at all, while `0.11.23` produced one and verified `network` plus both promotions. The
//! band ships `0.11.23`'s answer, because an under-grant is the direction that breaks a real
//! install. `mbt <1.2.49` is the same shape one notch smaller -- `1.2.7` verified `network` alone
//! and `0.0.9` verified `network` plus `.config/configstore`, so the band carries the promotion.
//!
//! ⛔ THE WITHDRAWN HOME REACH IS REPLACED BY A PROMOTION, WHICH IS THE WHOLE POINT AND IS NOT
//! ASSERTED HERE. Each of the four gains `.config/configstore` on Linux, and the two esbuild cells
//! keep the `.cache/esbuild` promotion they already had. `Scope` cannot express a promotion, so the
//! assertion below covers the write scopes and egress only; the promotions live in the catalog's
//! per-OS `writePaths` and are what the verified minimum actually named.
//!
//! ⛔ A FOURTH SHAPE, AND IT IS THE ONE THAT READS THE GRANT'S OWN TARGET. [`WITHDRAWN_REAL_HOME`] is
//! a CONTROL-VERSUS-NARROW DIFFERENTIAL in which each arm gets its OWN pristine real home. The three
//! shapes above all grade an arm by the package's artefacts; none of them looks at the directory
//! `write.userHome` actually opens. `curated.rs` lowers `Scope::UserHome` to
//! `home_minus_secrets_allows` over `homes.home`, and `build_jail.rs::sandbox_homes` reads that from
//! `HOME` -- so repointing `HOME` per arm (with `XDG_CACHE_HOME` pinned, or the store and the private
//! jail home move with it) makes the grant's effect directly observable. A cell is here only when the
//! narrow arm reproduces the control arm's real-home tree ENTRY FOR ENTRY, its installer output, and
//! its artefact gate.
//!
//! ⛔ THE RED ARM FOR THIS SHAPE IS THE LANDLOCK RULE COUNT, MEASURED PER CELL. Under `RUST_LOG=debug`
//! the target's own jailed spawn prints `confining lifecycle spawn with landlock abi=7 rules=N`, and
//! every row below attaches exactly the rules its narrowing removes: 56 against 55 for the five that
//! drop `userHome` alone, and 57 against 55 for `@bazel/cypress`, which drops `project` with it. So
//! each row carries its own proof that the wide grant MATERIALIZED rather than being a catalog line
//! the compiler dropped, and that the narrow arm ran without it. A hand-built canary supplies the
//! other half on the same host and binary: at `{"write":{"userHome":true}}` an absolute write to the
//! arm's real home reports `WROTE` and the file is there, at `{}` it reports `EACCES` and the home is
//! empty -- while an `os.homedir()` write succeeds at BOTH, because that resolves to the private jail
//! home. That is the mechanism behind every row here.
//!
//! ⛔ AND THE PROMOTION IS ASSERTED FOR THIS LIST, unlike the three above. The `writePaths` promotion
//! is what delivers the product once the home reach is gone -- `truffle` keeps a 0.5.16 solc compiler
//! and `@clerk/shared` keeps `.config/clerk/config.json` in the real home at BOTH grants -- so a
//! re-bake that withdrew the home write and the promotion together would satisfy a withdrawal-only
//! assertion while stranding exactly what these arms measured. Egress is pinned TWO-SIDED here for
//! the same reason it cannot simply be asserted true: `@clerk/shared` and `netlify-cli` were measured
//! with no egress at all (`peers: 0`), so demanding `network` would be asserting something no arm saw.
//!
//! Reads the shipped bytes through `include_str!` rather than the runtime lookup, which consults a
//! dev override and an on-disk update tier first; the subject here is the file in this repository.
use nub_sandbox::catalog_v2::{Catalog, Platform, Scope};

fn shipped() -> Catalog {
    nub_sandbox::catalog_v2::parse(include_str!("../data/build-jail-catalog-v2.json"))
        .expect("the shipped catalog parses; build.rs fails the build otherwise")
}

/// One withdrawn cell: package, a version that RESOLVES to the band that was measured, that band's
/// label for the failure message, the write scopes the narrowing LEFT IN PLACE, and what WINDOWS
/// grants for `write.userHome` on the same cell. The version is the one the arms actually ran.
///
/// The retained scopes are carried because the risk a narrowing runs is the opposite one: an
/// UNDER-grant, a package that stops building. Eight of these ten keep no write at all, and the two
/// that do had those scopes proved necessary by a red arm -- `@pact-foundation/pact-node`'s empty
/// arm fails `EACCES` opening the project's `.npmrc`, which names `write.project` directly.
///
/// The last field records what WINDOWS grants on the same cell, two-sided ON PURPOSE: pinning the
/// value in both directions is what makes an accidental WIDENING fail as loudly as a narrowing.
/// `@pact-foundation/pact-node` and `@pdftron/pdfnet-node` are `false` because their `win` overlays
/// already granted no write before any of this, and recording that rather than dropping the rows is
/// what keeps them under the same guard.
///
/// ⛔ THIS FIELD ONCE MEANT "the withdrawal is Linux-only, so Windows must not move in EITHER
/// direction", AND THAT IS NO LONGER TRUE OF SIX ROWS. `@playwright/browser-{chromium,firefox,webkit}`,
/// `playwright-{chromium,webkit}` and `electron-chromedriver` are `false` on their OWN Windows
/// measurement rather than by inheriting this one, and the Windows home write they used to carry was never a
/// capability need. The `$cache/nub/pm/tools/{ms-playwright,electron-cache}` leaf these packages are
/// redirected into is granted read-write as `FsOrigin::Speculative`, and `derive_grants`
/// (`backend/windows.rs`) DROPS such a rule when its path is absent -- so on any machine that had
/// not already run an unjailed install the leaf carried no grant, the package's own `mkdir` hit the
/// deliberately read-only `tools` parent, and the ladder escalated until the whole home worked.
/// `46b623e352` materializes the leaves during the compile. Re-measured on a `windows-latest` runner
/// that PROVED the three leaves absent before every arm: the `{network}` arm puts 606 files and a
/// 297,987,584-byte `chrome-win64/chrome.dll` into the free leaf with no home grant at all, while
/// the empty arm collapses it to 1 file. The `browser-firefox` / `browser-webkit` /
/// `playwright-webkit` rows followed on the same ladder in a later batch, scored against the
/// jail-off product rather than a file count: their empty arms reach only 0.123 / 0.025 / 0.028 and
/// their `network` arms reproduce it exactly at rc=0.
///
/// ⛔ AND OF A SEVENTH ROW SINCE 2026-09-02, ON A DIFFERENT MECHANISM. `@mui/x-telemetry` is now
/// `false` too, but not because of any tool-cache leaf: on `windows-latest` at `9.10.0` the
/// base-profile arm reproduced the control's script output exactly, telemetry notice and all, over
/// 7219 paths with nothing missing. The mechanism is the one the Linux withdrawal already traced,
/// one platform over -- `postinstall/storage.js` resolves `XDG_CONFIG_HOME || os.homedir() +
/// '/.config'`, and on Windows `USERPROFILE` is redirected to a per-package private home that
/// `compiler::preset` grants read-write unconditionally, so the config write never touches the real
/// home. `@shoelace-style/shoelace` is the ONE remaining row keeping `true`, because nothing has
/// measured it on Windows.
#[rustfmt::skip]
const WITHDRAWN: &[(&str, &str, &str, &[Scope], bool)] = &[
    ("@mui/x-telemetry",            "9.10.0",  "default", &[Scope::Deps, Scope::Project], false),
    ("@pact-foundation/pact-node",  "10.18.0", "default", &[Scope::Deps, Scope::Project], false),
    ("@pdftron/pdfnet-node",        "12.0.0",  "default", &[],                            false),
    ("@playwright/browser-chromium","1.62.1",  "default", &[],                            false),
    ("@playwright/browser-firefox", "1.62.1",  "default", &[],                            false),
    ("@playwright/browser-webkit",  "1.62.1",  "default", &[],                            false),
    ("@shoelace-style/shoelace",    "2.13.1",  "default", &[],                            true),
    ("electron-chromedriver",       "43.2.0",  "default", &[],                            false),
    ("playwright-chromium",         "1.62.1",  "default", &[],                            false),
    ("playwright-webkit",           "1.62.1",  "default", &[],                            false),
];

/// The Linux tool-cache-leaf cells, same tuple shape as [`WITHDRAWN`] but a different measurement:
/// the two-binary differential the module doc sets out, one arm per row at `network` alone.
///
/// The version in each row is the one that ARM ACTUALLY RAN, and it resolves to the named band
/// through `Entry::grant_for`'s narrowest-bound rule -- `31.7.7` lands on `<32.3.3`, not `<43.2.0`.
///
/// `playwright-chromium` keeps `write.deps` on both bands even though the passing arm carried none:
/// it mirrors macOS, and one measured version is not grounds to drop a SECOND capability across a
/// whole band. An under-grant is the direction that breaks a real install.
///
/// ⛔ THE LAST FIELD IS THE SAME TWO-SIDED WINDOWS PIN AS IN [`WITHDRAWN`], AND ONE ROW MOVED
/// 2026-09-02. `playwright <1.62.1` is now `false`: re-measured at `1.37.1` on a `windows-latest`
/// runner, its base-profile arm downloaded Chromium 116.0.5845.82, FFMPEG v1009, Firefox 115.0 and
/// Webkit 17.0 into `AppData/Local/nub/pm/tools/ms-playwright` with byte-identical script output to
/// the control -- the same free tool-cache leaf, and the same reasoning, that took the Linux side of
/// this very row. `1.37.1` is the HIGHEST version in the band that declares an install script (129
/// of the 186 it covers do), so the band's easy end is not shadowing a lower one. The remaining
/// `true` rows are untouched: nothing has re-measured them on Windows.
#[rustfmt::skip]
const WITHDRAWN_BANDS: &[(&str, &str, &str, &[Scope], bool)] = &[
    ("@playwright/browser-chromium","1.61.1",  "<1.62.1", &[],              true),
    ("electron",                    "39.8.9",  "<43.4.0", &[],              true),
    ("electron-chromedriver",       "39.8.9",  "<43.2.0", &[],              true),
    ("electron-chromedriver",       "31.7.7",  "<32.3.3", &[],              true),
    ("playwright",                  "1.31.0",  "<1.62.1", &[],              false),
    ("playwright-chromium",         "0.17.0",  "<1.62.1", &[Scope::Deps],   true),
    ("playwright-chromium",         "0.15.0",  "<0.16.0", &[Scope::Deps],   false),
];

/// The cells the corpus DESCENT narrowed on a real Linux host, same tuple shape as [`WITHDRAWN`]
/// and a third body of evidence: a verified minimum backed by a red arm, per the module doc.
///
/// The version in each row is one the descent ACTUALLY RAN, and it resolves to the named band
/// through `Entry::grant_for`'s narrowest-bound rule -- `0.4.3` lands on `<1.5.4`, not `<10.7.1`.
///
/// All four keep no write at all. `mbt`'s outer `write.project` goes with the home reach on this OS
/// because neither of its verified arms carried a write scope, which is what macOS had already
/// settled for that band. `react-native-purchases` is narrowed on `<1.5.4` ONLY: its wider
/// `<10.7.1` band keeps `write:"disk"`, because `2.4.1` passed no state there even at that width.
#[rustfmt::skip]
const WITHDRAWN_DESCENT: &[(&str, &str, &str, &[Scope], bool)] = &[
    ("@netlify/esbuild",            "0.13.6",  "<0.14.39", &[],             true),
    ("esbuild",                     "0.11.23", "<0.17.19", &[],             true),
    ("mbt",                         "0.0.9",   "<1.2.49",  &[],             false),
    ("react-native-purchases",      "0.4.3",   "<1.5.4",   &[],             false),
];

/// The cells narrowed by the real-home differential the module doc sets out, and a FOURTH body of
/// evidence: package, a version an arm actually ran, that version's band, the write scopes the
/// narrowing left in place, the egress the arms measured, and the promotion that now carries the
/// product.
///
/// Egress and `writePaths` are pinned TWO-SIDED rather than asserted-present, which is what makes
/// this list's guard different from the one above it. Two of these cells were measured with no
/// egress at all, so "network is still true" would be a claim no arm here supports; and the
/// promotion is the thing that replaced the withdrawn home reach, so it has to move under the same
/// guard as the withdrawal rather than being left to a re-bake.
///
/// SEVEN ROWS OVER SIX CELLS. `@clerk/shared` appears twice on purpose: both ends of its band were
/// measured (`2.9.2` and `4.9.0`, artefact gates `306/306` and `801/801`), and two rows is what
/// asserts that both ends still RESOLVE to the narrowed band rather than only the one that happens
/// to be written first. Rule of this file's own doc -- the top of a band is its easy end -- so a band
/// narrowed on one version would not be citable.
///
/// `@bazel/cypress` is the only row keeping a write scope. Its `npm_version_check.js` reads
/// `package.json` and `process.env` and throws only inside a Bazel context, so it writes nothing at
/// all: `writes: 0, peers: 0` with `pids: 14`, and the control arm left its real home EMPTY even
/// WITH the grant. `write.deps` is kept regardless, because one measured version is not grounds to
/// drop a second capability across a `default` that also answers for every future release.
/// One real-home-differential row: package, a version an arm ran, that version's band, the write
/// scopes the narrowing left in place, the egress its arms measured, and the promotion that now
/// carries the product. Named because the tuple carries two more fields than the lists above and
/// `clippy::type_complexity` refuses it inline -- and because a reader needs the field order stated
/// somewhere other than a comment above the data.
type RealHomeRow = (
    &'static str,
    &'static str,
    &'static str,
    &'static [Scope],
    bool,
    &'static [&'static str],
);

#[rustfmt::skip]
const WITHDRAWN_REAL_HOME: &[RealHomeRow] = &[
    ("@aws-amplify/cli", "14.5.1", "default", &[],            true,  &[".amplify", ".config/configstore"]),
    ("@bazel/cypress",   "5.8.1",  "default", &[Scope::Deps], true,  &[]),
    ("@clerk/shared",    "2.9.2",  "<4.29.1", &[],            false, &[".config/clerk"]),
    ("@clerk/shared",    "4.9.0",  "<4.29.1", &[],            false, &[".config/clerk"]),
    ("@coze/cli",        "0.3.5",  "<0.3.6",  &[],            true,  &[]),
    ("netlify-cli",      "25.6.2", "default", &[],            false, &[".config/netlify"]),
    ("truffle",          "5.11.5", "default", &[],            true,  &[".config/truffle-nodejs"]),
];

/// The Linux home write is gone, and neither egress nor the retained write scopes went with it.
///
/// Egress is asserted alongside the withdrawal because a re-bake that dropped BOTH would satisfy a
/// withdrawal-only assertion while breaking every one of these packages: each one's red arm named
/// `network`, so it is the capability these cells were measured to NEED.
#[test]
fn a_withdrawn_cell_grants_no_linux_home_write_and_keeps_what_its_arms_needed() {
    let catalog = shipped();
    // COLLECTED, not asserted per row: a re-bake moves a whole class at once, and a panic on the
    // first row reports 1 of 10 -- which reads as an isolated typo rather than the systematic
    // restoration it is, and costs a rebuild per cell to enumerate.
    let mut wrong: Vec<String> = Vec::new();

    for (pkg, version, band, keeps_write, _) in WITHDRAWN
        .iter()
        .chain(WITHDRAWN_BANDS)
        .chain(WITHDRAWN_DESCENT)
    {
        let entry = catalog
            .packages
            .get(*pkg)
            .unwrap_or_else(|| panic!("{pkg} has no catalog entry at all"));
        let caps = entry.grant_for(Some(version)).on(Platform::Linux);

        if caps.write.covers(Scope::UserHome) {
            wrong.push(format!(
                "{pkg}@{version} [band {band}] linux: write.userHome is back"
            ));
        }
        if !caps.network {
            wrong.push(format!(
                "{pkg}@{version} [band {band}] linux: network was withdrawn"
            ));
        }
        for scope in *keeps_write {
            if !caps.write.covers(*scope) {
                wrong.push(format!(
                    "{pkg}@{version} [band {band}] linux: write.{} was withdrawn",
                    scope.as_str()
                ));
            }
        }
    }

    assert!(
        wrong.is_empty(),
        "{} Linux cell(s) no longer match what their arms measured. Each ran one of the three \
         measurements this file documents -- a five-arm ladder, a two-binary \
         `materialize_tool_leaf` differential, or a corpus descent to a verified minimum backed by \
         a red arm: the home write bought nothing observable and what was kept was proved \
         necessary, so neither may move without a new measurement:\n  {}",
        wrong.len(),
        wrong.join("\n  ")
    );
}

/// The real-home differential's cells: the home write is gone, and the three things its arms
/// measured alongside the withdrawal have not moved with it.
///
/// SEPARATE FROM THE TEST ABOVE, not folded into its chain, and the reason is one line of it: that
/// test asserts `network` is still true for every row it walks. Two cells here were measured with
/// `peers: 0` and carry no egress, so adding them to that chain would fail on a fact their arms
/// never claimed -- and relaxing the assertion there would quietly weaken the guard on the ten rows
/// that did each have a red arm naming `network`.
#[test]
fn a_real_home_differential_cell_grants_no_linux_home_write_and_still_carries_its_promotion() {
    let catalog = shipped();
    // COLLECTED rather than asserted per row, for the reason the sibling test gives: a re-bake
    // moves the whole class at once, and a panic on the first row reports 1 of 7.
    let mut wrong: Vec<String> = Vec::new();

    for (pkg, version, band, keeps_write, network, promotes) in WITHDRAWN_REAL_HOME {
        let entry = catalog
            .packages
            .get(*pkg)
            .unwrap_or_else(|| panic!("{pkg} has no catalog entry at all"));
        let caps = entry.grant_for(Some(version)).on(Platform::Linux);
        let at = format!("{pkg}@{version} [band {band}] linux");

        if caps.write.covers(Scope::UserHome) {
            wrong.push(format!("{at}: write.userHome is back"));
        }
        for scope in *keeps_write {
            if !caps.write.covers(*scope) {
                wrong.push(format!("{at}: write.{} was withdrawn", scope.as_str()));
            }
        }
        // Two-sided: the arms measured this value, so a re-bake must not move it EITHER way.
        if caps.network != *network {
            wrong.push(format!(
                "{at}: network is {}, expected {network} -- the value its arms ran at",
                caps.network
            ));
        }
        // The promotion is what delivers the product now that the home reach is gone, so it is
        // pinned as an exact set: dropping one strands the payload, adding one is unmeasured reach.
        if caps.write_paths != *promotes {
            wrong.push(format!(
                "{at}: writePaths is {:?}, expected {promotes:?} -- the promotion that carried this \
                 cell's product into the real home at BOTH arms",
                caps.write_paths
            ));
        }
    }

    assert!(
        wrong.is_empty(),
        "{} Linux cell(s) no longer match the real-home differential that narrowed them. Each was \
         run twice on a Landlock ABI 7 host, control against narrow, with a pristine real home per \
         arm: the narrow arm reproduced the control arm's real-home tree entry for entry, its \
         installer output and its artefact gate, while attaching one fewer Landlock rule. Neither \
         the withdrawal nor what was kept beside it may move without a new measurement:\n  {}",
        wrong.len(),
        wrong.join("\n  ")
    );
}

/// The control for the real-home differential, and without it that test passes on a catalog that
/// withdrew the home write on every platform at once.
///
/// The measurement was Linux-only -- one host, one kernel, one binary -- so the other two platforms
/// must not have moved, in EITHER direction. Three of the six cells still grant `write.userHome` on
/// Windows and nothing here has measured them there; the other three lost it to the win32 lane's own
/// re-measurement on 2026-09-02, which is recorded in their catalog notes. macOS is `false`
/// throughout because every one of these cells already carried a `macos` block withdrawing the
/// write before this change -- which is corroboration for the Linux result rather than evidence for
/// it, since the two platforms resolve different promotion paths.
#[test]
fn the_real_home_differential_did_not_move_macos_or_windows() {
    let catalog = shipped();
    let mut moved: Vec<String> = Vec::new();

    #[rustfmt::skip]
    let expected: &[(&str, &str, bool, bool)] = &[
        // package,           version,  macOS userHome, win userHome
        ("@aws-amplify/cli",  "14.5.1", false,          true),
        ("@bazel/cypress",    "5.8.1",  false,          false),
        ("@clerk/shared",     "4.9.0",  false,          false),
        ("@coze/cli",         "0.3.5",  false,          true),
        ("netlify-cli",       "25.6.2", false,          true),
        ("truffle",           "5.11.5", false,          false),
    ];

    for (pkg, version, on_macos, on_win) in expected {
        let entry = catalog
            .packages
            .get(*pkg)
            .unwrap_or_else(|| panic!("{pkg} has no catalog entry at all"));
        for (platform, want) in [(Platform::Macos, on_macos), (Platform::Windows, on_win)] {
            let got = entry
                .grant_for(Some(version))
                .on(platform)
                .write
                .covers(Scope::UserHome);
            if got != *want {
                moved.push(format!(
                    "{pkg}@{version} {platform:?}: write.userHome is {got}, expected {want}"
                ));
            }
        }
    }

    assert!(
        moved.is_empty(),
        "{} cell(s) moved on a platform the real-home differential never ran on. That measurement \
         was one Linux host, so a change here is either an unmeasured widening or a withdrawal \
         that escaped its platform:\n  {}",
        moved.len(),
        moved.join("\n  ")
    );
}

/// Guards the ENUMERATION itself. The test above iterates `WITHDRAWN`, so emptying or trimming that
/// list makes it pass while asserting nothing -- a failure mode it cannot see from the inside.
#[test]
fn every_measured_linux_withdrawal_is_still_enumerated() {
    assert_eq!(
        (
            WITHDRAWN.len(),
            WITHDRAWN_BANDS.len(),
            WITHDRAWN_DESCENT.len(),
            WITHDRAWN_REAL_HOME.len()
        ),
        (10, 7, 4, 7),
        "a withdrawal list changed size; a row may only leave it alongside a measurement that \
         restores the grant in the catalog. The four are counted SEPARATELY because they rest on \
         different evidence -- a five-arm ladder, a two-binary `materialize_tool_leaf` \
         differential, a corpus descent to a verified minimum backed by a red arm, and a \
         control-versus-narrow real-home differential -- and a row may not migrate between them \
         either"
    );
}

/// The control, and without it the test above passes on a catalog that granted nothing anywhere.
///
/// Two independent halves, because they fail for different reasons. WINDOWS: every withdrawn cell
/// holds exactly the `write.userHome` its OWN Windows measurement settled on -- one of the ten
/// still grant it, and a blanket removal would satisfy the assertion above while silently widening
/// the change to cells nothing measured on that platform. LINUX: a
/// sibling must STILL grant the home there, which is what proves the Linux accessor reports one
/// when a cell has it. `windows-build-tools` carries `write:"disk"`, so it also exercises the
/// `Reach::Disk` arm of `covers` rather than only the scope-set arm. (`unrs-resolver` was the
/// second such sibling until its grant was withdrawn on three-platform evidence.)
#[test]
fn windows_matches_its_own_measurement_and_the_held_siblings_keep_their_grants() {
    let catalog = shipped();
    let mut lost: Vec<String> = Vec::new();

    for (pkg, version, band, _, win_keeps_home_write) in WITHDRAWN
        .iter()
        .chain(WITHDRAWN_BANDS)
        .chain(WITHDRAWN_DESCENT)
    {
        let on_win = catalog
            .packages
            .get(*pkg)
            .unwrap_or_else(|| panic!("{pkg} has no catalog entry at all"))
            .grant_for(Some(version))
            .on(Platform::Windows)
            .write
            .covers(Scope::UserHome);
        if on_win != *win_keeps_home_write {
            lost.push(format!(
                "{pkg}@{version} [band {band}] win: write.userHome is {on_win}, expected \
                 {win_keeps_home_write}; each row pins the Windows grant its own measurement \
                 settled on, so Windows must not move until something re-measures THAT cell"
            ));
        }
    }

    // `unrs-resolver` WAS held here and is now WITHDRAWN -- the grant was measured unnecessary on
    // all three platforms (see this module's doc), so holding it would pin a grant nothing needs.
    // That leaves ONE sibling, which still exercises the Linux accessor exactly as before; if a
    // future withdrawal takes `windows-build-tools` too, this loop goes vacuous and needs a
    // replacement row rather than deletion.
    for (pkg, version, why) in [(
        "windows-build-tools",
        "0.1.8",
        "it carries `write:\"disk\"`, which no measurement has touched",
    )] {
        if !catalog
            .packages
            .get(pkg)
            .unwrap_or_else(|| panic!("{pkg} has no catalog entry at all"))
            .grant_for(Some(version))
            .on(Platform::Linux)
            .write
            .covers(Scope::UserHome)
        {
            lost.push(format!(
                "{pkg}@{version} linux: write.userHome was withdrawn, but {why}"
            ));
        }
    }

    assert!(
        lost.is_empty(),
        "{} control(s) failed, so the withdrawal test above is no longer testing what it \
         claims:\n  {}",
        lost.len(),
        lost.join("\n  ")
    );
}
