//! The single PM pin reader. Every consumer that needs to know "which package
//! manager, which version" goes through here — there is no second pin parser.
//!
//! Resolution sources, in precedence order:
//!   1. `.yarnrc.yml`'s `yarnPath:` — a committed Berry release short-circuits
//!      everything (run that file directly; never provision).
//!   2. `package.json#packageManager` — the Corepack standard.
//!   3. `package.json#devEngines.packageManager` (object form only).
//!
//! Unpinned (none of the above) is a valid state: [`resolve_target`] /
//! [`resolve_pin`] return `None`, and provisioning falls back to lockfile
//! inference elsewhere.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::Pm;
use crate::workspace::detect::detect_project;

/// A resolved PM pin: which manager, and the version spec if one was stated.
///
/// `version` is `None` when the manager is known but the version is not — e.g.
/// inferred from a lockfile rather than a `packageManager` field. There is no
/// `Exact`/`Inferred` enum; a present `String` is the literal spec (Corepack
/// hash suffix kept verbatim — see [`classify_yarn`] / [`parse_spec`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PmPin {
    pub pm: Pm,
    pub version: Option<String>,
}

/// What provisioning should do with a resolved project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PmTarget {
    /// A committed Berry release at this path — exec it directly, never download.
    YarnPath(PathBuf),
    /// Provision (download/cache) the pinned manager.
    Provision(PmPin),
    /// A bare Berry pin (`yarn@>=2`) with no `yarnPath` — Nub can't synthesize a
    /// Berry release, so the engine surfaces a clear error.
    BerryNoYarnPath,
}

/// WHICH field supplied the pin — the provenance `nub pm which` must report. The
/// resolution sources have a precedence order (yarnPath > packageManager >
/// devEngines), but the source they fire from is not recoverable from a [`PmPin`]
/// alone: a devEngines-only pin and a packageManager pin both produce the same
/// `PmPin`, so the CLI mislabels the former as "resolved from packageManager"
/// unless the resolver hands back the true source. This enum is that signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinSource {
    /// A committed Berry release named by `.yarnrc.yml`'s `yarnPath:`.
    YarnPath,
    /// The Corepack-standard `package.json#packageManager` field.
    PackageManager,
    /// The `package.json#devEngines.packageManager` fallback.
    DevEngines,
}

impl std::fmt::Display for PinSource {
    /// The exact provenance phrasing `nub pm which` prints after "resolved from ".
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            PinSource::YarnPath => ".yarnrc.yml yarnPath",
            PinSource::PackageManager => "packageManager",
            PinSource::DevEngines => "devEngines.packageManager",
        })
    }
}

/// A resolved [`PmTarget`] with its provenance and any advisory warnings. The
/// source is `None` only for [`PmTarget::BerryNoYarnPath`] reached via a bare
/// Berry `packageManager`/`devEngines` pin — there the CLI surfaces an error, not
/// a provenance line. Warnings are structured (never printed here): the CLI owns
/// all stderr output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PmTargetResolution {
    pub target: PmTarget,
    pub source: Option<PinSource>,
    pub warnings: Vec<PinWarning>,
}

/// A resolved [`PmPin`] with its provenance and any advisory warnings — the
/// pin-level analogue of [`PmTargetResolution`], for `nub pm update`/`switch`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PmPinResolution {
    pub pin: PmPin,
    pub source: PinSource,
    pub warnings: Vec<PinWarning>,
}

/// A structured advisory the CLI prints to stderr (resolve.rs never writes to
/// stderr itself — output is the CLI's). Each variant carries the data its
/// [`Display`] renders, so the message text lives in one place and tests assert
/// on the variant, not on a formatted string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinWarning {
    /// A pin field is present but unusable (`yarn@^4` in the exact-only
    /// `packageManager`; an unsupported manager; a missing version) — the project
    /// resolves as unpinned. `field` names where it came from, `spec` is the raw
    /// value, `reason` is the parser's message.
    Ignored {
        field: PinField,
        spec: String,
        reason: String,
    },
    /// `packageManager` and `devEngines.packageManager` both exist but disagree —
    /// either a different PM name, or a `packageManager` version the devEngines
    /// range does not admit. The legacy field is what nub executes, so it wins;
    /// the warning names the conflict.
    Disagreement {
        package_manager: String,
        dev_engines: String,
        kind: DisagreementKind,
    },
}

/// Which pin field a [`PinWarning::Ignored`] refers to — drives the field name in
/// the message and keeps the two channels (legacy vs devEngines) distinct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinField {
    PackageManager,
    DevEngines,
}

impl std::fmt::Display for PinField {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            PinField::PackageManager => "packageManager",
            PinField::DevEngines => "devEngines.packageManager",
        })
    }
}

/// The two ways `packageManager` and `devEngines.packageManager` can disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisagreementKind {
    /// Different package managers named (`pnpm` vs `yarn`).
    Name,
    /// Same PM, but the `packageManager` exact version is outside the devEngines
    /// range (`pnpm@9.1.0` vs `devEngines …version: "^10"`).
    Version,
}

impl std::fmt::Display for PinWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PinWarning::Ignored {
                field,
                spec,
                reason,
            } => write!(f, "nub: ignoring {field} \"{spec}\": {reason}"),
            PinWarning::Disagreement {
                package_manager,
                dev_engines,
                kind,
            } => match kind {
                DisagreementKind::Name => write!(
                    f,
                    "nub: packageManager \"{package_manager}\" and devEngines.packageManager \
                     \"{dev_engines}\" name different package managers — running packageManager \
                     \"{package_manager}\""
                ),
                DisagreementKind::Version => write!(
                    f,
                    "nub: packageManager \"{package_manager}\" is outside the \
                     devEngines.packageManager range \"{dev_engines}\" — running packageManager \
                     \"{package_manager}\""
                ),
            },
        }
    }
}

/// Resolve what to run for the project at `cwd`. `None` means unpinned (no
/// `.yarnrc.yml yarnPath`, no `packageManager`, no `devEngines.packageManager`).
///
/// The provenance-free wrapper. New callers that need to report WHICH field
/// supplied the pin (`nub pm which`) should use [`resolve_target_with_source`].
pub fn resolve_target(cwd: &Path) -> Option<PmTarget> {
    resolve_target_with_source(cwd).map(|r| r.target)
}

/// [`resolve_target`] plus the provenance and any structured warnings the CLI
/// should surface. The provenance is what fixes the `nub pm which` mislabel: a
/// devEngines-only pin now reports `devEngines.packageManager`, not
/// `packageManager`. Warnings (present-but-unusable specs, packageManager ⇄
/// devEngines disagreement) are returned, never printed — the CLI owns stderr.
pub fn resolve_target_with_source(cwd: &Path) -> Option<PmTargetResolution> {
    // 1. A committed Berry release short-circuits everything — no pin field is
    //    read, so no field-level warnings can fire.
    if let Some(path) = committed_yarn_path(cwd) {
        return Some(PmTargetResolution {
            target: PmTarget::YarnPath(path),
            source: Some(PinSource::YarnPath),
            warnings: Vec::new(),
        });
    }

    // 2. + 3. The pin from packageManager / devEngines.
    let manifest = root_manifest(cwd)?;
    let resolved = pin_from_manifest(&manifest);
    let Some((pin, source)) = resolved.pin else {
        // Unpinned, but a present-but-unusable field may still have warned; an
        // unpinned project surfaces nothing to run, so the warnings are dropped
        // here (resolve_pin_with_source is the channel that returns them).
        return None;
    };
    let target = if pin.pm == Pm::YarnBerry {
        // Berry pinned but no committed release to run — unresolvable.
        PmTarget::BerryNoYarnPath
    } else {
        PmTarget::Provision(pin)
    };
    Some(PmTargetResolution {
        target,
        source: Some(source),
        warnings: resolved.warnings,
    })
}

/// Resolve just the pin (for `nub pm which` / `nub pm update`). `None` means no
/// `packageManager` and no `devEngines.packageManager` field.
///
/// The pin is read from the workspace root, not just the nearest `package.json`:
/// a monorepo pins `packageManager` once at the root, and a member's `package.json`
/// rarely carries it. Reading at the workspace root keeps the *read* symmetric with
/// [`write_declared_pm`]'s *write* (both target [`pin_target_dir`]) — a `nub pm use`
/// in a member writes the declaration where the next `resolve_pin` will find it.
///
/// A pin field that is PRESENT but unusable (`yarn@^4`, `bun@1.1.0`) resolves as
/// unpinned, but never silently: one warning lands on stderr naming the field, the
/// raw spec, and why it was ignored. pnpm warns here and corepack hard-errors;
/// saying nothing and falling through to a PATH PM would be the worst of the three.
pub fn resolve_pin(cwd: &Path) -> Option<PmPin> {
    resolve_pin_with_source(cwd).map(|r| r.pin)
}

/// [`resolve_pin`] plus provenance and the structured warnings — the channel that
/// returns the present-but-unusable and disagreement advisories for the CLI to
/// print. `None` means no usable pin (the same condition as [`resolve_pin`]);
/// warnings about an *ignored* field are lost on `None` because there is no
/// resolution to attach them to — they are advisory on a pin that *did* resolve.
pub fn resolve_pin_with_source(cwd: &Path) -> Option<PmPinResolution> {
    let manifest = root_manifest(cwd)?;
    let resolved = pin_from_manifest(&manifest);
    let (pin, source) = resolved.pin?;
    Some(PmPinResolution {
        pin,
        source,
        warnings: resolved.warnings,
    })
}

/// The pure core of [`resolve_pin`]: the pin (with its source) from an
/// already-parsed manifest, plus the structured warnings — a present-but-unusable
/// field, and a `packageManager` ⇄ `devEngines` disagreement when both are
/// present. Absent fields are silent (`pin: None`, no warnings).
struct ManifestPin {
    /// The resolved pin and which field supplied it, or `None` when unpinned.
    pin: Option<(PmPin, PinSource)>,
    warnings: Vec<PinWarning>,
}

fn pin_from_manifest(manifest: &serde_json::Value) -> ManifestPin {
    // `packageManager` wins; `devEngines.packageManager` (object form) is the
    // fallback. Both are parsed by the same spec parser. An unusable
    // `packageManager` does NOT fall through to devEngines — the project stated a
    // pin nub can't honor, so it runs unpinned (with the warning), never on a
    // half-matching secondary field.
    if let Some(spec) = manifest.get("packageManager").and_then(|v| v.as_str()) {
        return match parse_spec(spec) {
            Ok(pin) => {
                // Both fields present + agreeing → silent; disagreeing → warn,
                // packageManager (the field nub executes) wins.
                let mut warnings = Vec::new();
                if let Some(w) = disagreement_warning(spec, &pin, manifest) {
                    warnings.push(w);
                }
                ManifestPin {
                    pin: Some((pin, PinSource::PackageManager)),
                    warnings,
                }
            }
            Err(err) => ManifestPin {
                pin: None,
                warnings: vec![PinWarning::Ignored {
                    field: PinField::PackageManager,
                    spec: spec.to_string(),
                    reason: err.to_string(),
                }],
            },
        };
    }
    // devEngines carries name + version as separate keys; feed them straight to
    // the shared classifier. A name with no version is valid here (version stays
    // `None`), so unlike `packageManager` there's no required-version check.
    let Some(dev) = manifest
        .get("devEngines")
        .and_then(|d| d.get("packageManager"))
    else {
        return ManifestPin {
            pin: None,
            warnings: Vec::new(),
        };
    };
    let Some(name) = dev.get("name").and_then(|v| v.as_str()) else {
        return ManifestPin {
            pin: None,
            warnings: Vec::new(),
        };
    };
    let version = dev.get("version").and_then(|v| v.as_str());
    match classify(name, version) {
        Ok(pin) => ManifestPin {
            pin: Some((pin, PinSource::DevEngines)),
            warnings: Vec::new(),
        },
        Err(err) => {
            let spec = match version {
                Some(v) => format!("{name}@{v}"),
                None => name.to_string(),
            };
            ManifestPin {
                pin: None,
                warnings: vec![PinWarning::Ignored {
                    field: PinField::DevEngines,
                    spec,
                    reason: err.to_string(),
                }],
            }
        }
    }
}

/// When a `packageManager` pin resolves AND a `devEngines.packageManager` exists,
/// flag a disagreement: a different PM name, or a `packageManager` exact version
/// the devEngines range does not admit. Returns `None` when they agree, when
/// devEngines is absent/malformed, or when its version can't be range-checked (a
/// name-only devEngines entry constrains only the name). The legacy
/// `packageManager` is what nub runs, so it always "wins"; this only warns.
fn disagreement_warning(
    pm_spec: &str,
    pm_pin: &PmPin,
    manifest: &serde_json::Value,
) -> Option<PinWarning> {
    let dev = manifest
        .get("devEngines")
        .and_then(|d| d.get("packageManager"))?;
    // Mirror the identity probe's array handling: the last named entry governs.
    let entry = match dev {
        serde_json::Value::Array(items) => items
            .iter()
            .rev()
            .find(|e| e.get("name").and_then(|n| n.as_str()).is_some())?,
        other => other,
    };
    let dev_name = entry.get("name")?.as_str()?.trim();
    if dev_name.is_empty() {
        return None;
    }
    let dev_version = entry.get("version").and_then(|v| v.as_str());
    let dev_spec = match dev_version {
        Some(v) => format!("{dev_name}@{v}"),
        None => dev_name.to_string(),
    };

    // Name mismatch: yarn classic vs Berry both print `yarn`, so compare on the
    // Display name (the user-visible PM), not the classic/Berry split.
    if dev_name != pm_pin.pm.to_string() {
        return Some(PinWarning::Disagreement {
            package_manager: pm_spec.trim().to_string(),
            dev_engines: dev_spec,
            kind: DisagreementKind::Name,
        });
    }

    // Same PM: does packageManager's exact version satisfy the devEngines range?
    // Strip the Corepack hash suffix before parsing (it rides as semver build
    // metadata and isn't part of the comparison). A devEngines entry with no
    // version, or a version/range either side can't parse, isn't a checkable
    // disagreement — skip rather than warn on noise.
    let (Some(pm_version), Some(dev_range)) = (pm_pin.version.as_deref(), dev_version) else {
        return None;
    };
    let pm_bare = pm_version.split('+').next().unwrap_or(pm_version);
    let parsed = semver::Version::parse(pm_bare).ok()?;
    let req = semver::VersionReq::parse(&crate::pm::registry::normalize_range(dev_range)).ok()?;
    if !req.matches(&parsed) {
        return Some(PinWarning::Disagreement {
            package_manager: pm_spec.trim().to_string(),
            dev_engines: dev_spec,
            kind: DisagreementKind::Version,
        });
    }
    None
}

/// The project's PM identity at the NAME level — the declared-first identity
/// probe (wiki/commands/pm/identity-policy.md, Axiom 1: declaration outranks
/// lockfile inference). Unlike [`resolve_pin`] it never reads a project as
/// unpinned just because the pin's *version* is unusable, and it sees identity
/// channels that carry no provisionable version:
///
///   - a committed `yarnPath` with no `packageManager` field at all → yarn Berry;
///   - a present-but-unusable spec (`yarn@^4`) → still names yarn;
///   - an out-of-scope manager (`bun@1.1.0`) → still names bun (the guard
///     refuses a cross-PM pin even toward a PM nub doesn't provision).
///
/// `berry` is true when the yarn identity is Berry (committed yarnPath, pinned
/// major >= 2, or a versionless yarn beside a `.yarnrc.yml`). Read-only and
/// silent — the guard is a probe, not a resolution, so no warnings print here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PmIdentity {
    pub name: String,
    pub berry: bool,
}

/// The raw `(name, version)` the project's pin fields declare at the workspace
/// root — no usability filtering, no name allowlist, and the Corepack
/// `+sha512` suffix stripped from the version. The role-first UA's input: the
/// lifecycle UA impersonates the *declared* identity (and its pinned version)
/// even when the pin is unusable for provisioning, so this reader sees what
/// [`resolve_pin`] would warn about and ignore.
pub fn declared_pm_raw(cwd: &Path) -> Option<(String, Option<String>)> {
    let manifest = root_manifest(cwd)?;
    let (name, version) = raw_pin_name_version(&manifest)?;
    let version = version.map(|v| {
        v.split_once('+')
            .map_or(v.as_str(), |(bare, _)| bare)
            .to_string()
    });
    Some((name, version))
}

pub fn project_pm_identity(cwd: &Path) -> Option<PmIdentity> {
    if committed_yarn_path(cwd).is_some() {
        return Some(PmIdentity {
            name: "yarn".to_string(),
            berry: true,
        });
    }
    let manifest = root_manifest(cwd)?;
    let (name, version) = raw_pin_name_version(&manifest)?;
    let berry = name == "yarn" && {
        let yarnrc_present = pin_target_dir(cwd).join(".yarnrc.yml").is_file();
        classify_yarn(version.as_deref(), yarnrc_present) == Pm::YarnBerry
    };
    Some(PmIdentity { name, berry })
}

/// The raw `(name, version)` a manifest's pin fields state, with NO usability
/// check — `packageManager` first, then `devEngines.packageManager` (object or
/// array; an array yields its LAST named entry, the one the spec's
/// ignore-earlier/error-last semantics make govern). The identity probe's
/// reader; [`pin_from_manifest`] stays the strict, warning-emitting one.
fn raw_pin_name_version(manifest: &serde_json::Value) -> Option<(String, Option<String>)> {
    if let Some(spec) = manifest.get("packageManager").and_then(|v| v.as_str()) {
        let spec = spec.trim();
        let (name, version) = match spec.split_once('@') {
            Some((n, v)) => (n, Some(v.to_string())),
            None => (spec, None),
        };
        return (!name.is_empty()).then(|| (name.to_string(), version));
    }
    let dev = manifest.get("devEngines")?.get("packageManager")?;
    let entry = match dev {
        serde_json::Value::Array(items) => items
            .iter()
            .rev()
            .find(|e| e.get("name").and_then(|n| n.as_str()).is_some())?,
        other => other,
    };
    let name = entry.get("name")?.as_str()?.trim();
    if name.is_empty() {
        return None;
    }
    let version = entry
        .get("version")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    Some((name.to_string(), version))
}

/// The `package.json` value at [`pin_target_dir`] — the workspace root if one is
/// above `cwd`, else the nearest project root. The detected project already parsed
/// the nearest manifest; only a distinct workspace root needs a second read.
fn root_manifest(cwd: &Path) -> Option<serde_json::Value> {
    let project = detect_project(cwd)?;
    match &project.workspace_root {
        Some(ws) if *ws != project.root => {
            let content = std::fs::read_to_string(ws.join("package.json")).ok()?;
            serde_json::from_str(&content).ok()
        }
        _ => Some(project.manifest),
    }
}

/// The result of [`write_declared_pm`]: the manifest written, and the
/// `devEngines.packageManager` range written beside the pin (the caller's
/// summary line echoes it; `None` when the caller asked for the pin-only
/// write — `nub pm update`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredPmWrite {
    pub path: PathBuf,
    pub dev_engines_range: Option<String>,
}

/// Write the `packageManager` declaration into the workspace-root
/// `package.json` — the identity-setting write behind `nub pm use` (and the
/// version bump behind `nub pm update`). Per the PM identity policy
/// (wiki/commands/pm/identity-policy.md, Axiom 3) this is the ONLY code path
/// that writes the field:
///
///   - `packageManager: "<name>@<exact>+sha512.<hex>"` — the resolved record
///     corepack / yarn-classic-1.22.21+ / turbo execute. The field is
///     exact-only across the ecosystem, so `version` must be the
///     already-resolved exact version, and `sha512_hex` the hex digest of the
///     verified tarball (computed by the caller — pin-implies-fetch: you can't
///     write an honest hash without the bytes).
///   - when `maintain_dev_engines` is true (`nub pm use` — the
///     identity-setting verb), `devEngines.packageManager` is written
///     alongside — `{ "name": "<name>", "version": "^<exact>", "onFail":
///     "warn" }`, replacing any prior value (object or array form) wholesale
///     while `devEngines` siblings (`runtime`/`os`/`cpu`/`libc`) survive.
///     Not duplication (the 2026-06-10 ruling that killed never-create):
///     devEngines is the range + policy npm/pnpm enforce natively (no
///     corepack needed; survives corepack death), `packageManager` the exact
///     pin for the corepack/turbo dialect — `use` maintains both together so
///     they cannot drift. `onFail: "warn"` is load-bearing: npm 11 enforces
///     devEngines (EBADDEVENGINES), so a bare `{name}` entry hard-breaks
///     every `npx`/`npm` invocation in the repo for teammates — found live
///     by the conformance harness's round-trip leg.
///   - when false (`nub pm update`), the field is untouched: the existing
///     devEngines range is the USER'S constraint that update floats within —
///     rewriting it to the caret of the new exact would silently narrow
///     their stated intent.
///
/// `name` is a string (not [`Pm`]) because `use bun` declares a manager nub
/// doesn't provision. All other manifest content survives; indentation, line
/// endings, and the trailing newline are reproduced (see [`edit_manifest`]).
/// Errors if no `package.json` exists at the target dir — Nub never creates
/// one (no silent scaffolding).
pub fn write_declared_pm(
    name: &str,
    version: &str,
    sha512_hex: &str,
    cwd: &Path,
    maintain_dev_engines: bool,
) -> Result<DeclaredPmWrite> {
    // Fail closed on a non-exact version: writing `packageManager: "pnpm@^9"`
    // breaks corepack (hard error), pnpm (warn + drop), and yarn classic 1.22.21+
    // (startup exit 1) — the caller resolves ranges/tags BEFORE writing. A '+' is
    // rejected too: the hash suffix is appended here, never passed in.
    if version.contains('+') || semver::Version::parse(version).is_err() {
        bail!(
            "write_declared_pm needs an exact resolved version (e.g. 9.1.0), got \"{version}\" — \
             ranges/tags are resolved before the declaration is written"
        );
    }

    let range = maintain_dev_engines.then(|| format!("^{version}"));
    let path = edit_manifest(cwd, |obj| {
        obj.insert(
            "packageManager".to_string(),
            serde_json::Value::String(format!("{name}@{version}+sha512.{sha512_hex}")),
        );
        if let Some(range) = &range {
            let dev = obj
                .entry("devEngines")
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
            if let Some(dev) = dev.as_object_mut() {
                dev.insert(
                    "packageManager".to_string(),
                    serde_json::json!({ "name": name, "version": range, "onFail": "warn" }),
                );
            }
        }
    })?;
    Ok(DeclaredPmWrite {
        path,
        dev_engines_range: range,
    })
}

/// Detected surface formatting of a manifest: the per-level indent unit (taken
/// from the first indented line — tabs vs N spaces), the line ending (CRLF vs
/// LF), and whether the file ends with a newline. A single-line / unindented file
/// gets the 2-space default; a file with no newline at all is treated as LF.
struct ManifestFormat {
    indent: String,
    /// `true` when the file uses Windows `\r\n` line endings — reproduced on
    /// write so a pin edit never silently converts a CRLF manifest to LF (which
    /// reads as a whole-file diff in git).
    crlf: bool,
    trailing_newline: bool,
}

fn detect_format(content: &str) -> ManifestFormat {
    // `\r\n` if any CRLF appears: serde emits `\n`-joined output, so we
    // post-process to `\r\n` only when the source was CRLF. A lone `\r` (old-Mac)
    // isn't a case real package.json files hit; treat it as LF.
    let crlf = content.contains("\r\n");
    // `.lines()` strips both `\n` and a trailing `\r`, so indent detection is
    // line-ending-agnostic.
    let indent = content
        .lines()
        .find_map(|line| {
            let ws_len = line.len() - line.trim_start_matches([' ', '\t']).len();
            (ws_len > 0 && !line.trim().is_empty()).then(|| line[..ws_len].to_string())
        })
        .unwrap_or_else(|| "  ".to_string());
    ManifestFormat {
        indent,
        crlf,
        // A CRLF file's terminator is `\r\n`; either ending counts as "ends with
        // a newline" for the trailing-newline state.
        trailing_newline: content.ends_with('\n'),
    }
}

/// Read → edit → atomically rewrite the `package.json` at [`pin_target_dir`] (the
/// same workspace-root rule [`crate::version_management`]'s pin uses). The shared
/// body of every pin writer:
///
///   - **Formatting-preserving** (the Volta steal): the file's detected indent
///     unit, line ending (CRLF vs LF), and trailing-newline presence are
///     reproduced, and `serde_json`'s `preserve_order` keeps the user's key
///     order — a pin write must not reformat the manifest.
///   - **Atomic, crash-safe rewrite**: write a sibling temp file then `rename`
///     over the target. A `package.json` carries the user's whole manifest, so a
///     torn write (crash / full disk mid-`write`) must never leave it truncated —
///     the original survives until the rename, and the rename is atomic on the
///     same filesystem.
///   - Errors if no `package.json` exists at the target dir — Nub never creates
///     one (no silent scaffolding).
fn edit_manifest(
    cwd: &Path,
    edit: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>),
) -> Result<PathBuf> {
    let dir = pin_target_dir(cwd);
    let path = dir.join("package.json");
    if !path.is_file() {
        bail!(
            "no package.json at {} to write packageManager into",
            dir.display()
        );
    }

    let content =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let format = detect_format(&content);
    let mut manifest: serde_json::Value =
        serde_json::from_str(&content).with_context(|| format!("parsing {}", path.display()))?;

    let obj = manifest
        .as_object_mut()
        .with_context(|| format!("{} is not a JSON object", path.display()))?;
    edit(obj);

    let serialized = serialize_manifest(&manifest, &format)
        .with_context(|| format!("serializing {}", path.display()))?;

    let tmp = dir.join(format!(".package.json.nub-{}.tmp", std::process::id()));
    std::fs::write(&tmp, &serialized).with_context(|| format!("writing {}", tmp.display()))?;
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("replacing {}", path.display()));
    }

    Ok(path)
}

/// Public face of [`edit_manifest`] for the CLI's `pm use nub` / `use pnpm`
/// migration edits (the one verb sanctioned to restructure identity-bearing
/// manifest fields). Same contract: format-preserving, atomic, never
/// scaffolds a missing `package.json`.
pub fn edit_root_manifest(
    cwd: &Path,
    edit: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>),
) -> Result<PathBuf> {
    edit_manifest(cwd, edit)
}

/// Pretty-print with the detected indent unit (vs `to_string_pretty`'s hardwired
/// two spaces) and reproduce the line ending (CRLF vs LF) and trailing-newline
/// state. serde's pretty formatter always emits `\n`; a CRLF source has every
/// `\n` rewritten to `\r\n` after serialization so the diff stays the two pin
/// keys, never a line-ending flip across the whole file.
fn serialize_manifest(manifest: &serde_json::Value, format: &ManifestFormat) -> Result<String> {
    use serde::Serialize;
    let mut buf = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(format.indent.as_bytes());
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, formatter);
    manifest.serialize(&mut ser).context("serializing JSON")?;
    let mut out = String::from_utf8(buf).context("serialized JSON is not UTF-8")?;
    if format.crlf {
        // The serialized body has no `\r` of its own (serde emits `\n`), so a
        // plain replace is exact and idempotent.
        out = out.replace('\n', "\r\n");
    }
    if format.trailing_newline {
        out.push_str(if format.crlf { "\r\n" } else { "\n" });
    }
    Ok(out)
}

/// Resolve where a pin is written: the workspace root if one is above `cwd`,
/// else the nearest project root, else `cwd`. Mirrors `manage::pin_target_dir`'s
/// rule (a `packageManager` pin is repo-wide, like the Node pin).
fn pin_target_dir(cwd: &Path) -> PathBuf {
    if let Some(project) = detect_project(cwd) {
        return project.workspace_root.unwrap_or(project.root);
    }
    cwd.to_path_buf()
}

/// Parse a `packageManager`-style spec (`name@version`). The `version` is
/// mandatory in this strict form (Corepack requires it); a value with no `@`
/// errors naming the required `name@version` shape. The Corepack hash suffix
/// (`yarn@4.2.2+sha512.xxxx`) is kept verbatim in `version` — resolution never
/// lies about what was written; the engine strips it before download.
///
/// Public so the CLI's pin consumers parse through the SAME pin parser the
/// `packageManager` reader uses — there is no second spec parser.
pub fn parse_spec(spec: &str) -> Result<PmPin> {
    let spec = spec.trim();
    let (name, version) = spec.split_once('@').with_context(|| {
        format!("packageManager \"{spec}\" must be in name@version form (e.g. pnpm@9.1.0)")
    })?;
    classify(name, Some(version))
}

/// Map a `(name, version)` pair to a [`PmPin`], applying the yarn classic/berry
/// split. The version (with any hash suffix) is stored verbatim.
fn classify(name: &str, version: Option<&str>) -> Result<PmPin> {
    let pm = match name {
        "npm" => Pm::Npm,
        "pnpm" => Pm::Pnpm,
        "yarn" => {
            // A *pinned* yarn (version present) must classify by major to pick the
            // classic-tarball vs Berry provisioning path. A dist-tag/range whose
            // version has no leading numeric major (`yarn@stable`, `yarn@berry`)
            // can't be split, and Corepack requires an exact version anyway — reject
            // it here naming the requirement, rather than silently provisioning the
            // wrong (classic-tarball) artifact for a Berry tag. A genuinely absent
            // version (the lockfile-inference seam) still flows through the yarnrc
            // signal in `classify_yarn`.
            if let Some(v) = version {
                if yarn_major(v).is_none() {
                    bail!(
                        "yarn \"{v}\" must be an exact version (e.g. yarn@4.2.2) — \
                         dist-tags and ranges (yarn@stable, yarn@berry) are unsupported \
                         in a yarn pin"
                    );
                }
            }
            classify_yarn(version, false)
        }
        other => bail!("unsupported package manager \"{other}\" — nub manages npm, pnpm, and yarn"),
    };
    Ok(PmPin {
        pm,
        version: version.map(str::to_string),
    })
}

/// The single yarn classic-vs-Berry classifier:
///   - pinned: a `version` is present → major `>= 2` is Berry.
///   - no usable version → fall back to the `.yarnrc.yml` presence signal
///     (`yarnrc_present`): a sibling means Berry, otherwise classic.
///
/// The pinned route ([`classify`]) calls this with the version. The unpinned route
/// does NOT reach here — it defers to whatever `yarn` is on PATH and so needs no
/// classic/Berry split. The `None`-version arm is the seam a future
/// provisioning-from-lockfile path would use; it is exercised by tests but has no
/// production caller today.
fn classify_yarn(version: Option<&str>, yarnrc_present: bool) -> Pm {
    match version.and_then(yarn_major) {
        Some(major) if major >= 2 => Pm::YarnBerry,
        Some(_) => Pm::Yarn,
        // No usable version: fall back to the .yarnrc.yml signal.
        None if yarnrc_present => Pm::YarnBerry,
        None => Pm::Yarn,
    }
}

/// Extract the major version from a yarn spec, tolerating the Corepack hash
/// suffix (`4.2.2+sha512.…`) and partial versions (`4`, `4.2`). The major is the
/// leading run of digits before the first `.`, `+`, or `-`.
fn yarn_major(version: &str) -> Option<u32> {
    let leading: String = version.chars().take_while(|c| c.is_ascii_digit()).collect();
    leading.parse().ok()
}

/// Read the single `yarnPath:` key from `.yarnrc.yml` at the project root,
/// resolved relative to that root. A committed Berry release lives there
/// (`.yarn/releases/yarn-4.2.2.cjs`).
///
/// Public so the CLI's Berry-pin refusal can tell "commit a release" apart from
/// "you already committed one" (the message must not instruct the user to do
/// what they already did).
///
/// This is a hand line-scan for one flat top-level `yarnPath:` key, mirroring
/// `workspace::filter::read_pnpm_workspace`'s idiom — nub-core takes no YAML
/// dependency. LIMITATION: only a single, top-level, unindented `yarnPath:`
/// entry is recognized; a nested or multi-document form is not (no real-world
/// `.yarnrc.yml` nests `yarnPath`).
pub fn committed_yarn_path(cwd: &Path) -> Option<PathBuf> {
    // A Berry monorepo commits `.yarnrc.yml` (and the release it points at) at the
    // workspace root, not in each member, so resolve at the workspace root — the
    // same dir [`resolve_pin`] reads the pin from. `yarnPath` is relative to the
    // file that declares it, so the join base must be that same root.
    let root = pin_target_dir(cwd);
    let path = root.join(".yarnrc.yml");
    let content = std::fs::read_to_string(&path).ok()?;
    for line in content.lines() {
        // Top-level keys are unindented; a leading space means nested config.
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("yarnPath:") {
            let value = strip_yaml_value(rest);
            if !value.is_empty() {
                return Some(root.join(value));
            }
        }
    }
    None
}

/// Extract a scalar YAML value from the text after a `key:`. A quoted value is
/// taken verbatim (quotes stripped); an unquoted value has a trailing inline
/// `# comment` removed — `yarnPath: .yarn/releases/x.cjs # pinned` is the path,
/// not `… # pinned`. Comments are only recognized on unquoted values (a `#`
/// inside quotes is part of the path).
fn strip_yaml_value(rest: &str) -> &str {
    let rest = rest.trim();
    for quote in ['"', '\''] {
        if let Some(inner) = rest.strip_prefix(quote) {
            if let Some(end) = inner.find(quote) {
                return &inner[..end];
            }
        }
    }
    // Unquoted: an inline comment starts at the first ` #` (space then hash);
    // a bare `#` mid-token is not a comment in flow scalars.
    match rest.split_once(" #") {
        Some((value, _comment)) => value.trim_end(),
        None => rest,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A unique temp dir under the system temp root (NOT under $HOME, so the
    /// detect walk-up can't escape into a stray ancestor package.json). Mirrors
    /// `manage.rs`'s `tmpdir`.
    fn tmpdir(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "nub-pm-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_pkg(dir: &Path, json: &str) {
        std::fs::write(dir.join("package.json"), json).unwrap();
    }

    #[test]
    fn resolve_pin_reads_package_manager_then_dev_engines_then_none() {
        // 1. packageManager field is the primary source.
        let dir = tmpdir("pin-pkgmgr");
        write_pkg(&dir, r#"{"packageManager":"pnpm@9.1.0"}"#);
        assert_eq!(
            resolve_pin(&dir),
            Some(PmPin {
                pm: Pm::Pnpm,
                version: Some("9.1.0".to_string())
            })
        );

        // 2. No packageManager → devEngines.packageManager (object form).
        let dir = tmpdir("pin-devengines");
        write_pkg(
            &dir,
            r#"{"devEngines":{"packageManager":{"name":"pnpm","version":"9.1.0"}}}"#,
        );
        assert_eq!(
            resolve_pin(&dir),
            Some(PmPin {
                pm: Pm::Pnpm,
                version: Some("9.1.0".to_string())
            })
        );

        // 3. Neither field → unpinned.
        let dir = tmpdir("pin-none");
        write_pkg(&dir, r#"{"name":"app"}"#);
        assert_eq!(resolve_pin(&dir), None);
    }

    #[test]
    fn yarn_classic_vs_berry_split_by_major_and_keeps_hash_suffix() {
        let dir = tmpdir("yarn-classic");
        write_pkg(&dir, r#"{"packageManager":"yarn@1.22.19"}"#);
        assert_eq!(resolve_pin(&dir).unwrap().pm, Pm::Yarn);

        let dir = tmpdir("yarn-berry");
        write_pkg(&dir, r#"{"packageManager":"yarn@3.0.0"}"#);
        assert_eq!(resolve_pin(&dir).unwrap().pm, Pm::YarnBerry);

        // The Corepack hash suffix is preserved byte-for-byte in `version`.
        let dir = tmpdir("yarn-hash");
        write_pkg(&dir, r#"{"packageManager":"yarn@4.2.2+sha512.abc"}"#);
        let pin = resolve_pin(&dir).unwrap();
        assert_eq!(pin.pm, Pm::YarnBerry);
        assert_eq!(pin.version.as_deref(), Some("4.2.2+sha512.abc"));
    }

    #[test]
    fn yarn_disambiguated_by_yarnrc_when_only_lockfile_present() {
        // With no usable version, `.yarnrc.yml` presence decides classic vs Berry —
        // the no-pin seam (see `classify_yarn`'s doc); no production caller yet.
        assert_eq!(
            classify_yarn(None, false),
            Pm::Yarn,
            "yarn.lock alone (no .yarnrc.yml) is classic"
        );
        assert_eq!(
            classify_yarn(None, true),
            Pm::YarnBerry,
            "a sibling .yarnrc.yml flips lockfile-only yarn to Berry"
        );
    }

    #[test]
    fn resolve_target_yarn_path_short_circuits_to_yarn_path() {
        let dir = tmpdir("target-yarnpath");
        // A committed Berry release + a Berry pin: yarnPath must win, never
        // Provision/BerryNoYarnPath.
        write_pkg(&dir, r#"{"packageManager":"yarn@4.2.2"}"#);
        let release = dir.join(".yarn/releases");
        std::fs::create_dir_all(&release).unwrap();
        let release_file = release.join("yarn-4.2.2.cjs");
        std::fs::write(&release_file, "// yarn\n").unwrap();
        std::fs::write(
            dir.join(".yarnrc.yml"),
            "yarnPath: .yarn/releases/yarn-4.2.2.cjs\n",
        )
        .unwrap();

        assert_eq!(resolve_target(&dir), Some(PmTarget::YarnPath(release_file)));
    }

    #[test]
    fn resolve_target_bare_berry_without_yarn_path_is_unresolvable() {
        let dir = tmpdir("target-berry-bare");
        write_pkg(&dir, r#"{"packageManager":"yarn@4.2.2"}"#);
        assert_eq!(resolve_target(&dir), Some(PmTarget::BerryNoYarnPath));
    }

    #[test]
    fn unsupported_manager_and_missing_version_are_named_errors() {
        // bun is out of scope → error names the supported set.
        let dir = tmpdir("err-bun");
        write_pkg(&dir, r#"{"packageManager":"bun@1.1.0"}"#);
        // resolve_pin resolves the unusable pin as None (after warning on stderr —
        // see `present_but_unusable_pin_warns_once_and_resolves_unpinned`); the
        // underlying parser carries the message.
        let err = parse_spec("bun@1.1.0").unwrap_err().to_string();
        assert!(
            err.contains("npm, pnpm, and yarn"),
            "bun error must name the supported set, got: {err}"
        );
        assert_eq!(resolve_pin(&dir), None);

        // packageManager with no @version → error names the required form.
        let err = parse_spec("pnpm").unwrap_err().to_string();
        assert!(
            err.contains("name@version"),
            "missing-version error must name the form, got: {err}"
        );
    }

    #[test]
    fn write_declared_pm_preserves_siblings_and_errors_without_package_json() {
        let dir = tmpdir("write-pin");
        write_pkg(
            &dir,
            "{\n  \"name\": \"app\",\n  \"scripts\": {\n    \"build\": \"tsc\"\n  }\n}\n",
        );
        let written = write_declared_pm("pnpm", "9.1.0", "abc", &dir, true)
            .unwrap()
            .path;
        assert_eq!(written, dir.join("package.json"));

        let manifest: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&written).unwrap()).unwrap();
        assert_eq!(
            manifest["packageManager"].as_str(),
            Some("pnpm@9.1.0+sha512.abc"),
            "the pin is written"
        );
        assert_eq!(
            manifest["name"].as_str(),
            Some("app"),
            "sibling keys survive the round-trip"
        );
        assert_eq!(
            manifest["scripts"]["build"].as_str(),
            Some("tsc"),
            "nested sibling keys survive the round-trip"
        );

        // No package.json at the target dir → error, never create one.
        let empty = tmpdir("write-pin-empty");
        let err = write_declared_pm("npm", "10.0.0", "abc", &empty, true)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("no package.json"),
            "missing-manifest error must say so, got: {err}"
        );
        assert!(
            !empty.join("package.json").exists(),
            "write_declared_pm must not scaffold a package.json"
        );
    }

    #[test]
    fn pin_is_read_and_written_at_the_workspace_root_from_a_member() {
        // A monorepo pins `packageManager` once at the root; a member's package.json
        // carries none. Resolving from the member must still find the root pin (read
        // symmetric with write), and a committed Berry release lives at the root too.
        let root = tmpdir("ws-root");
        write_pkg(
            &root,
            r#"{"packageManager":"yarn@4.2.2","workspaces":["packages/*"]}"#,
        );
        let release = root.join(".yarn/releases");
        std::fs::create_dir_all(&release).unwrap();
        let release_file = release.join("yarn-4.2.2.cjs");
        std::fs::write(&release_file, "// yarn\n").unwrap();
        std::fs::write(
            root.join(".yarnrc.yml"),
            "yarnPath: .yarn/releases/yarn-4.2.2.cjs\n",
        )
        .unwrap();

        let member = root.join("packages").join("app");
        std::fs::create_dir_all(&member).unwrap();
        write_pkg(&member, r#"{"name":"@mono/app"}"#);

        // Pin reads the root field even though the member has none.
        assert_eq!(
            resolve_pin(&member).unwrap().pm,
            Pm::YarnBerry,
            "resolve_pin must walk to the workspace root for the pin"
        );
        // yarnPath resolves at the root, with its relative path joined onto the root.
        assert_eq!(
            resolve_target(&member),
            Some(PmTarget::YarnPath(release_file)),
            "the committed Berry release at the workspace root must resolve from a member"
        );
        // A `nub pm use` in the member writes to the SAME root file resolve reads.
        assert_eq!(
            write_declared_pm("pnpm", "9.1.0", "abc", &member, true)
                .unwrap()
                .path,
            root.join("package.json")
        );
    }

    #[test]
    fn yarn_dist_tag_or_range_pin_is_rejected_naming_the_exact_version_rule() {
        // A non-numeric yarn version (`yarn@stable`, `yarn@berry`) can't be split
        // into classic-vs-Berry and Corepack requires an exact version — so the
        // parser errors here rather than silently misclassifying it as classic and
        // attempting a doomed classic-tarball provision.
        for spec in ["yarn@stable", "yarn@berry"] {
            let err = parse_spec(spec).unwrap_err().to_string();
            assert!(
                err.contains("exact version"),
                "{spec} must be rejected naming the exact-version rule, got: {err}"
            );
        }
        // An exact version (even partial) still classifies fine.
        assert_eq!(parse_spec("yarn@4").unwrap().pm, Pm::YarnBerry);
        assert_eq!(parse_spec("yarn@1.22.19").unwrap().pm, Pm::Yarn);
    }

    #[test]
    fn yarn_path_value_drops_inline_comments_and_honors_quotes() {
        // The single yarnPath reader must not fold a trailing ` # comment` into the
        // path, and must take a quoted value (with spaces) verbatim.
        assert_eq!(
            strip_yaml_value(" .yarn/releases/y.cjs"),
            ".yarn/releases/y.cjs"
        );
        assert_eq!(
            strip_yaml_value(" .yarn/releases/y.cjs # pinned"),
            ".yarn/releases/y.cjs",
            "an inline comment must not become part of the path"
        );
        assert_eq!(
            strip_yaml_value(r#" ".yarn/releases/with space.cjs""#),
            ".yarn/releases/with space.cjs",
            "a quoted value keeps its spaces and is taken verbatim"
        );

        // End to end: a commented yarnPath still resolves to the real release path.
        let dir = tmpdir("yarnpath-comment");
        write_pkg(&dir, r#"{"packageManager":"yarn@4.2.2"}"#);
        let release = dir.join(".yarn/releases");
        std::fs::create_dir_all(&release).unwrap();
        let release_file = release.join("yarn-4.2.2.cjs");
        std::fs::write(&release_file, "// yarn\n").unwrap();
        std::fs::write(
            dir.join(".yarnrc.yml"),
            "yarnPath: .yarn/releases/yarn-4.2.2.cjs # committed Berry\n",
        )
        .unwrap();
        assert_eq!(resolve_target(&dir), Some(PmTarget::YarnPath(release_file)));
    }

    #[test]
    fn present_but_unusable_pin_warns_once_and_resolves_unpinned() {
        // A yarn range in `packageManager` is unusable (classify needs an exact
        // version for the classic/Berry split) — it must resolve as unpinned WITH
        // exactly one structured warning naming the field, the raw spec, and the
        // reason. The warning vec holds exactly that one entry.
        let manifest: serde_json::Value =
            serde_json::from_str(r#"{"packageManager":"yarn@^4"}"#).unwrap();
        let resolved = pin_from_manifest(&manifest);
        assert!(
            resolved.pin.is_none(),
            "an unusable pin resolves as unpinned"
        );
        match resolved.warnings.as_slice() {
            [
                PinWarning::Ignored {
                    field: PinField::PackageManager,
                    spec,
                    reason,
                },
            ] => {
                assert_eq!(spec, "yarn@^4", "the warning carries the raw spec");
                assert!(
                    reason.contains("exact version"),
                    "the reason names the exact-version rule, got: {reason}"
                );
            }
            other => panic!("expected one packageManager Ignored warning, got: {other:?}"),
        }

        // The same rule covers the devEngines fallback, naming its field.
        let manifest: serde_json::Value = serde_json::from_str(
            r#"{"devEngines":{"packageManager":{"name":"yarn","version":"^4"}}}"#,
        )
        .unwrap();
        let resolved = pin_from_manifest(&manifest);
        assert!(resolved.pin.is_none());
        assert!(
            matches!(
                resolved.warnings.as_slice(),
                [PinWarning::Ignored {
                    field: PinField::DevEngines,
                    ..
                }]
            ),
            "an unusable devEngines pin warns naming its field, got: {:?}",
            resolved.warnings
        );

        // Absent fields are NOT a warning — unpinned is a valid, silent state.
        let manifest: serde_json::Value = serde_json::from_str(r#"{"name":"app"}"#).unwrap();
        let resolved = pin_from_manifest(&manifest);
        assert!(resolved.pin.is_none());
        assert!(resolved.warnings.is_empty());

        // The Ignored warning's Display is the stderr line the CLI prints.
        assert_eq!(
            PinWarning::Ignored {
                field: PinField::PackageManager,
                spec: "yarn@^4".to_string(),
                reason: "boom".to_string(),
            }
            .to_string(),
            "nub: ignoring packageManager \"yarn@^4\": boom"
        );
    }

    #[test]
    fn resolution_reports_the_true_pin_source() {
        // A devEngines-only pin must report devEngines.packageManager, not
        // packageManager — the mislabel the provenance enum fixes.
        let dir = tmpdir("src-devengines");
        write_pkg(
            &dir,
            r#"{"devEngines":{"packageManager":{"name":"pnpm","version":"9.1.0"}}}"#,
        );
        let r = resolve_pin_with_source(&dir).unwrap();
        assert_eq!(r.source, PinSource::DevEngines);
        assert_eq!(r.pin.pm, Pm::Pnpm);
        assert_eq!(
            r.source.to_string(),
            "devEngines.packageManager",
            "the Display is the phrase `nub pm which` prints after 'resolved from '"
        );

        // A packageManager pin reports packageManager.
        let dir = tmpdir("src-pkgmgr");
        write_pkg(&dir, r#"{"packageManager":"pnpm@9.1.0"}"#);
        let r = resolve_target_with_source(&dir).unwrap();
        assert_eq!(r.source, Some(PinSource::PackageManager));
        assert_eq!(PinSource::PackageManager.to_string(), "packageManager");

        // A committed yarnPath reports the yarnPath source and never reads a
        // pin field (so no warnings).
        let dir = tmpdir("src-yarnpath");
        write_pkg(&dir, r#"{"name":"app"}"#);
        let release = dir.join(".yarn/releases");
        std::fs::create_dir_all(&release).unwrap();
        std::fs::write(release.join("yarn-4.2.2.cjs"), "// yarn\n").unwrap();
        std::fs::write(
            dir.join(".yarnrc.yml"),
            "yarnPath: .yarn/releases/yarn-4.2.2.cjs\n",
        )
        .unwrap();
        let r = resolve_target_with_source(&dir).unwrap();
        assert_eq!(r.source, Some(PinSource::YarnPath));
        assert!(r.warnings.is_empty());
        assert_eq!(PinSource::YarnPath.to_string(), ".yarnrc.yml yarnPath");
    }

    #[test]
    fn disagreeing_package_manager_and_dev_engines_warn_with_package_manager_winning() {
        // Different PM named in each field → a Name disagreement; the pin still
        // resolves to packageManager (the field nub executes).
        let manifest: serde_json::Value = serde_json::from_str(
            r#"{"packageManager":"pnpm@9.1.0","devEngines":{"packageManager":{"name":"yarn","version":"^4"}}}"#,
        )
        .unwrap();
        let resolved = pin_from_manifest(&manifest);
        assert_eq!(resolved.pin.unwrap().0.pm, Pm::Pnpm);
        match resolved.warnings.as_slice() {
            [
                PinWarning::Disagreement {
                    package_manager,
                    dev_engines,
                    kind: DisagreementKind::Name,
                },
            ] => {
                assert_eq!(package_manager, "pnpm@9.1.0");
                assert_eq!(dev_engines, "yarn@^4");
            }
            other => panic!("expected a Name disagreement, got: {other:?}"),
        }

        // Same PM, but packageManager's exact version is outside the devEngines
        // range → a Version disagreement.
        let manifest: serde_json::Value = serde_json::from_str(
            r#"{"packageManager":"pnpm@9.1.0","devEngines":{"packageManager":{"name":"pnpm","version":"^10"}}}"#,
        )
        .unwrap();
        let resolved = pin_from_manifest(&manifest);
        assert!(matches!(
            resolved.warnings.as_slice(),
            [PinWarning::Disagreement {
                kind: DisagreementKind::Version,
                ..
            }]
        ));

        // In range → no warning (and the hash suffix is stripped before the
        // satisfaction check).
        let manifest: serde_json::Value = serde_json::from_str(
            r#"{"packageManager":"pnpm@9.1.0+sha512.abc","devEngines":{"packageManager":{"name":"pnpm","version":"^9.0.0"}}}"#,
        )
        .unwrap();
        assert!(
            pin_from_manifest(&manifest).warnings.is_empty(),
            "an in-range exact version (hash suffix ignored) must not warn"
        );

        // A name-only devEngines entry constrains only the name; a matching name
        // with no range is not a checkable version disagreement.
        let manifest: serde_json::Value = serde_json::from_str(
            r#"{"packageManager":"pnpm@9.1.0","devEngines":{"packageManager":{"name":"pnpm"}}}"#,
        )
        .unwrap();
        assert!(pin_from_manifest(&manifest).warnings.is_empty());

        // The two Disagreement Displays both end by naming what nub runs.
        let name = PinWarning::Disagreement {
            package_manager: "pnpm@9.1.0".to_string(),
            dev_engines: "yarn@^4".to_string(),
            kind: DisagreementKind::Name,
        }
        .to_string();
        assert!(
            name.contains("different package managers")
                && name.contains("running packageManager \"pnpm@9.1.0\""),
            "name-disagreement Display, got: {name}"
        );
        let version = PinWarning::Disagreement {
            package_manager: "pnpm@9.1.0".to_string(),
            dev_engines: "pnpm@^10".to_string(),
            kind: DisagreementKind::Version,
        }
        .to_string();
        assert!(
            version.contains("outside the devEngines.packageManager range")
                && version.contains("running packageManager"),
            "version-disagreement Display, got: {version}"
        );
    }

    #[test]
    fn write_declared_pm_maintains_dev_engines_for_use_and_leaves_it_for_update() {
        // The identity-setting write (maintain=true): packageManager gets the
        // exact+hash record AND devEngines.packageManager is rewritten
        // wholesale to {name, ^exact, onFail:"warn"} — onFail:warn is
        // load-bearing (npm 11 EBADDEVENGINES would otherwise hard-break
        // teammates' npx). devEngines siblings survive untouched.
        let dir = tmpdir("pair");
        write_pkg(
            &dir,
            concat!(
                "{\n",
                "  \"name\": \"app\",\n",
                "  \"devEngines\": {\n",
                "    \"runtime\": { \"name\": \"node\", \"version\": \"^22\" },\n",
                "    \"cpu\": { \"name\": \"arm64\" },\n",
                "    \"packageManager\": { \"name\": \"npm\", \"version\": \"10.0.0\" }\n",
                "  }\n",
                "}\n"
            ),
        );
        let write = write_declared_pm("pnpm", "9.1.0", "abc123", &dir, true).unwrap();
        assert_eq!(write.dev_engines_range.as_deref(), Some("^9.1.0"));
        let m: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&write.path).unwrap()).unwrap();
        assert_eq!(
            m["packageManager"].as_str(),
            Some("pnpm@9.1.0+sha512.abc123"),
            "packageManager carries the exact version + artifact hash"
        );
        assert_eq!(
            m["devEngines"]["packageManager"],
            serde_json::json!({"name": "pnpm", "version": "^9.1.0", "onFail": "warn"}),
            "use rewrites devEngines wholesale beside the pin"
        );
        assert_eq!(
            m["devEngines"]["runtime"]["version"].as_str(),
            Some("^22"),
            "devEngines.runtime sibling survives"
        );
        assert_eq!(
            m["devEngines"]["cpu"]["name"].as_str(),
            Some("arm64"),
            "devEngines.cpu sibling survives"
        );

        // use on a manifest WITHOUT devEngines creates it (the never-create
        // rule was killed 2026-06-10 — devEngines is the range + policy
        // npm/pnpm enforce natively).
        let dir = tmpdir("pair-fresh");
        write_pkg(&dir, "{\n  \"name\": \"app\"\n}\n");
        let write = write_declared_pm("yarn", "1.22.19", "fff", &dir, true).unwrap();
        let m: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&write.path).unwrap()).unwrap();
        assert_eq!(
            m["devEngines"]["packageManager"],
            serde_json::json!({"name": "yarn", "version": "^1.22.19", "onFail": "warn"})
        );

        // The pin-only write (maintain=false — `nub pm update`): an existing
        // devEngines range is the USER'S constraint update floats within, so
        // it survives byte-verbatim; an absent one is not created.
        let dir = tmpdir("pair-update");
        write_pkg(
            &dir,
            r#"{"devEngines":{"packageManager":{"name":"pnpm","version":"^9","onFail":"download"}}}"#,
        );
        let write = write_declared_pm("pnpm", "9.1.0", "aa", &dir, false).unwrap();
        assert_eq!(write.dev_engines_range, None);
        let m: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&write.path).unwrap()).unwrap();
        assert_eq!(
            m["devEngines"]["packageManager"],
            serde_json::json!({"name": "pnpm", "version": "^9", "onFail": "download"}),
            "update must leave the user's devEngines constraint verbatim"
        );
        let dir = tmpdir("pair-update-fresh");
        write_pkg(&dir, "{\n  \"name\": \"app\"\n}\n");
        write_declared_pm("pnpm", "9.1.0", "aa", &dir, false).unwrap();
        let m: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("package.json")).unwrap())
                .unwrap();
        assert!(
            m.get("devEngines").is_none(),
            "the pin-only write must not create devEngines"
        );

        // A range/tag must be resolved BEFORE the write — the writer fails closed
        // rather than committing a `packageManager` value corepack chokes on.
        for bad in ["^9", "latest", "9.1.0+sha512.abc"] {
            let err = write_declared_pm("pnpm", bad, "aa", &dir, true)
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("exact resolved version"),
                "\"{bad}\" must be rejected naming the exact-version rule, got: {err}"
            );
        }
    }

    #[test]
    fn pair_write_preserves_indent_style_and_trailing_newline_byte_for_byte() {
        // A pin write must not reformat the manifest: the detected indent unit
        // (2-space / 4-space / tab) and the trailing-newline state are reproduced
        // exactly — the only diff is the two pin keys. Byte-equality, so any
        // reformat shows up verbatim in the failure output.
        fn render(lines: &[(usize, &str)], indent: &str, trailing_newline: bool) -> String {
            let mut s = lines
                .iter()
                .map(|(depth, line)| format!("{}{line}", indent.repeat(*depth)))
                .collect::<Vec<_>>()
                .join("\n");
            if trailing_newline {
                s.push('\n');
            }
            s
        }
        let input: &[(usize, &str)] = &[
            (0, "{"),
            (1, r#""name": "app","#),
            (1, r#""devEngines": {"#),
            (2, r#""os": {"#),
            (3, r#""name": "darwin""#),
            (2, "}"),
            (1, "}"),
            (0, "}"),
        ];
        let expected: &[(usize, &str)] = &[
            (0, "{"),
            (1, r#""name": "app","#),
            (1, r#""devEngines": {"#),
            (2, r#""os": {"#),
            (3, r#""name": "darwin""#),
            (2, "}"),
            (1, "},"),
            (1, r#""packageManager": "pnpm@9.1.0+sha512.cafe01""#),
            (0, "}"),
        ];
        // Tab + no trailing newline in one case: both detections are independent.
        for (indent, trailing) in [("  ", true), ("    ", true), ("\t", false)] {
            let dir = tmpdir("pair-fmt");
            write_pkg(&dir, &render(input, indent, trailing));
            let path = write_declared_pm("pnpm", "9.1.0", "cafe01", &dir, false)
                .unwrap()
                .path;
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                render(expected, indent, trailing),
                "indent {indent:?} / trailing newline {trailing} must be preserved"
            );
        }
    }

    #[test]
    fn pair_write_preserves_line_endings_and_trailing_newline_state() {
        // A pin write must not flip a CRLF manifest to LF (that reads as a
        // whole-file diff in git), and must match the source's trailing-newline
        // presence exactly. Four combinations of {CRLF, LF} × {trailing nl,
        // none}; byte-equality so any drift shows verbatim in the failure.
        let body = [
            r#"{"#,
            r#"  "name": "app","#,
            r#"  "packageManager": "pnpm@9.1.0+sha512.cafe""#,
            r#"}"#,
        ];
        // The input already carries a pin; the writer replaces it in place
        // (devEngines is never created). Build the expected output by hand so
        // the assertion is on real bytes, not a re-serialization.
        let expected_lines = [
            r#"{"#,
            r#"  "name": "app","#,
            r#"  "packageManager": "pnpm@9.2.0+sha512.beef""#,
            r#"}"#,
        ];

        for crlf in [false, true] {
            for trailing in [false, true] {
                let nl = if crlf { "\r\n" } else { "\n" };
                let mut input = body.join(nl);
                if trailing {
                    input.push_str(nl);
                }
                let mut want = expected_lines.join(nl);
                if trailing {
                    want.push_str(nl);
                }

                let dir = tmpdir("pair-eol");
                write_pkg(&dir, &input);
                let path = write_declared_pm("pnpm", "9.2.0", "beef", &dir, false)
                    .unwrap()
                    .path;
                let got = std::fs::read_to_string(&path).unwrap();
                assert_eq!(
                    got, want,
                    "crlf={crlf} trailing_newline={trailing} must round-trip byte-for-byte"
                );
                // Cross-check the negatives directly: an LF write has no `\r`; a
                // no-trailing-newline write doesn't end in a newline.
                if !crlf {
                    assert!(!got.contains('\r'), "an LF source must stay LF");
                }
                assert_eq!(
                    got.ends_with('\n'),
                    trailing,
                    "trailing-newline presence must match the source"
                );
            }
        }
    }

    #[test]
    fn write_declared_pm_replaces_atomically_and_leaves_no_temp_file() {
        // The rewrite goes through a sibling temp + rename; on success no `.tmp`
        // litter remains, and the target holds exactly the new pin.
        let dir = tmpdir("write-atomic");
        write_pkg(&dir, "{\n  \"name\": \"app\"\n}\n");
        write_declared_pm("npm", "10.9.0", "abc", &dir, true).unwrap();

        let leftover: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp") || n.starts_with(".package.json"))
            .collect();
        assert!(
            leftover.is_empty(),
            "the atomic rename must leave no temp file behind, found: {leftover:?}"
        );
        let manifest: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("package.json")).unwrap())
                .unwrap();
        assert_eq!(
            manifest["packageManager"].as_str(),
            Some("npm@10.9.0+sha512.abc")
        );
    }

    #[test]
    fn write_declared_pm_replaces_the_array_form_wholesale() {
        // The devEngines spec allows the array form ("any of these"). `use`
        // sets ONE identity, so the maintained write replaces the whole value
        // with the single governing object — keeping a multi-tool array
        // alongside an exact packageManager pin would preserve exactly the
        // split-brain state `use` exists to clear.
        let dir = tmpdir("pair-array");
        write_pkg(
            &dir,
            r#"{"devEngines":{"packageManager":[{"name":"bun","onFail":"ignore"},{"name":"npm","version":"10.0.0"}]}}"#,
        );
        let write = write_declared_pm("pnpm", "9.1.0", "abc", &dir, true).unwrap();
        assert_eq!(write.dev_engines_range.as_deref(), Some("^9.1.0"));
        let m: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&write.path).unwrap()).unwrap();
        assert_eq!(
            m["devEngines"]["packageManager"],
            serde_json::json!({"name": "pnpm", "version": "^9.1.0", "onFail": "warn"}),
            "the array form is replaced wholesale by the single identity entry"
        );
    }

    #[test]
    fn project_pm_identity_sees_channels_resolve_pin_reads_as_unpinned() {
        // (a) A Berry project pinned solely via a committed yarnPath (no
        // packageManager field) — resolve_pin says unpinned, but the identity is
        // yarn Berry (declared-first identity per the PM identity policy).
        let dir = tmpdir("ident-yarnpath");
        write_pkg(&dir, r#"{"name":"app"}"#);
        let release = dir.join(".yarn/releases");
        std::fs::create_dir_all(&release).unwrap();
        std::fs::write(release.join("yarn-4.2.2.cjs"), "// yarn\n").unwrap();
        std::fs::write(
            dir.join(".yarnrc.yml"),
            "yarnPath: .yarn/releases/yarn-4.2.2.cjs\n",
        )
        .unwrap();
        assert_eq!(
            resolve_pin(&dir),
            None,
            "no pin field — resolve_pin is None"
        );
        assert_eq!(
            project_pm_identity(&dir),
            Some(PmIdentity {
                name: "yarn".to_string(),
                berry: true
            }),
            "a committed yarnPath IS a yarn-Berry identity"
        );

        // (b) A present-but-unusable spec still names its PM (and classifies
        // yarn@^4 + no yarnrc via the version-less seam — classic here).
        let dir = tmpdir("ident-range");
        write_pkg(&dir, r#"{"packageManager":"yarn@^4"}"#);
        assert_eq!(
            project_pm_identity(&dir).map(|i| i.name),
            Some("yarn".to_string()),
            "an unusable version must not erase the PM identity"
        );

        // (c) An out-of-scope manager still names itself; a Berry pin reads as
        // berry; no channel at all is None.
        let dir = tmpdir("ident-bun");
        write_pkg(&dir, r#"{"packageManager":"bun@1.1.0"}"#);
        assert_eq!(
            project_pm_identity(&dir),
            Some(PmIdentity {
                name: "bun".to_string(),
                berry: false
            })
        );
        let dir = tmpdir("ident-berry");
        write_pkg(&dir, r#"{"packageManager":"yarn@4.2.2"}"#);
        assert_eq!(
            project_pm_identity(&dir),
            Some(PmIdentity {
                name: "yarn".to_string(),
                berry: true
            })
        );
        let dir = tmpdir("ident-none");
        write_pkg(&dir, r#"{"name":"app"}"#);
        assert_eq!(project_pm_identity(&dir), None);

        // (d) devEngines array form: the LAST named entry governs the identity.
        let dir = tmpdir("ident-array");
        write_pkg(
            &dir,
            r#"{"devEngines":{"packageManager":[{"name":"bun"},{"name":"pnpm","version":"^9"}]}}"#,
        );
        assert_eq!(
            project_pm_identity(&dir).map(|i| i.name),
            Some("pnpm".to_string()),
            "the array's last named entry (spec: error-last) is the identity"
        );
    }
}
