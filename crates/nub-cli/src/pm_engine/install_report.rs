//! What `nub install` says about itself: the resolved-layout header printed
//! before the engine runs, and the materialization digest printed after linking.
//!
//! Both are two-column blocks — dim label, bright value, dim parenthetical
//! naming where the value came from. They bracket the engine's own progress
//! display, so the order on screen is header → spinner → digest → the engine's
//! success line, which stays last.
//!
//! PROVENANCE IS EXACT, NOT INFERRED. Under the nub embedder profile the chain
//! that can supply a value is short, and every tier in it is readable from here:
//! explicit install CLI flags → env → project config (`nub.jsonc`) →
//! `pnpm-workspace.yaml` (non-empty only when pnpm is the incumbent) → pnpm's
//! global `config.yaml` (when the engine context enables it) → project `.npmrc`
//! → user `.npmrc` → nub's embedder defaults. The tiers aube would otherwise
//! consult are inert for nub by construction — `config_namespace = None` empties
//! both `.config/aube/config.toml` scopes. The global YAML is loaded through the
//! engine's own context-gated loader, so this index follows any identity policy
//! that makes that pnpm-named tier inert. Anything this walk cannot read is
//! reported as nothing at all: a setting with no readable source is dropped from
//! the block rather than printed with a guessed value or a guessed origin,
//! because a wrong provenance is worse than none.

use std::fmt;
use std::path::Path;
use std::sync::RwLock;

use clx::style;

use super::output::OutputFlags;

/// Left margin, and the gap between the label column and the value column.
const INDENT: usize = 2;
const GAP: usize = 2;
/// Fallback width when stderr is not a terminal (piped output, CI logs).
const FALLBACK_COLS: usize = 80;

// ───────────────────────────── provenance ─────────────────────────────

/// Where a resolved setting's value actually came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Source {
    /// An explicit install flag. The value was preserved in the engine's CLI bag,
    /// so this names the setting's canonical flag spelling and winning value.
    Cli(String),
    /// An environment variable, named so the reader can find it.
    Env(String),
    /// The project's `nub.jsonc`, with the field the user wrote.
    ProjectConfig(&'static str),
    WorkspaceYaml,
    GlobalConfigYaml,
    Npmrc,
    /// nub's own built-in value — nothing in the project asked for it.
    Default,
    /// Running in CI, where the global virtual store is off by default. A
    /// different default rather than anything the project asked for, but the
    /// reader still needs it named: the same checkout lays out one way on their
    /// machine and another on the runner, and nothing in the repo explains why.
    Ci,
    /// A declared package matches `disableGlobalVirtualStoreForPackages`, which
    /// forces the whole install project-local. Carries the package that matched
    /// because otherwise this is the one layout the reader cannot account for
    /// from their own config — nothing in it asked, and the trigger is a
    /// transitive fact about something they depend on.
    IncompatiblePackage(String),
}

/// Whether a raw setting string is the engine's idea of true.
///
/// Deliberately delegates to `aube_settings::values::parse_bool` rather than
/// comparing to `"true"`. The tiers this module reads are raw text — an
/// `.npmrc` line or an env var, unparsed — and the engine accepts `1`, `TRUE`
/// and `True` alongside `true`. A local `== "true"` therefore disagreed with the
/// resolver on exactly those spellings, in both directions: `hoist=1` printed
/// the shared store while the engine built a project-local tree with a hidden
/// directory, and `enable-global-virtual-store=1` printed `isolated` while the
/// engine symlinked into the machine-global store. The header's whole claim is
/// that it reproduces the resolver instead of approximating it, so the boolean
/// rule has to be the resolver's own.
fn is_true(raw: &str) -> bool {
    aube_settings::values::parse_bool(raw).unwrap_or(false)
}

/// Render a raw scalar only when the resolver can read it for this setting.
/// Boolean tiers skip malformed values and fall through, exactly as their typed
/// accessors do; printing an ignored value would claim an install took a value
/// it did not use.
fn readable_value(meta: &aube_settings::SettingMeta, raw: &str) -> Option<String> {
    if meta.type_ == "bool" {
        return aube_settings::values::parse_bool(raw).map(|value| value.to_string());
    }
    Some(raw.to_string())
}

/// The items of a comma-separated setting value: trimmed, blanks dropped. Every
/// list-valued setting reaches this module as one such string, whatever tier it
/// came from.
fn comma_items(raw: &str) -> impl Iterator<Item = &str> {
    raw.split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
}

/// The list representation the report passes to its comma-splitting rows.
/// This mirrors aube's `parse_string_list`: JSON-ish arrays and bare lists both
/// collapse to comma-separated items, with empty and quoted entries removed.
fn render_string_list(raw: &str) -> String {
    let trimmed = raw.trim();
    let items: Vec<&str> = match trimmed
        .strip_prefix('[')
        .and_then(|raw| raw.strip_suffix(']'))
    {
        Some(inner) => comma_items(inner)
            .map(|item| item.trim_matches(|ch: char| ch == '"' || ch == '\''))
            .filter(|item| !item.is_empty())
            .collect(),
        None => comma_items(trimmed).collect(),
    };
    items.join(",")
}

/// The toolchain's own name for the package that triggered the opt-out. The two
/// nub seeds are frameworks whose proper names are not their package ids, and
/// the row is prose the reader is meant to recognize — "react-native projects"
/// reads as a typo for the thing they actually use. Anything else is a pattern
/// someone configured themselves, where their own spelling is the right answer.
fn toolchain_display_name(package: &str) -> &str {
    match package {
        "next" => "Next",
        "react-native" => "React Native",
        other => other,
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Source::Cli(flag) => f.write_str(flag),
            Source::Env(var) => f.write_str(var),
            Source::ProjectConfig(field) => write!(f, "nub.jsonc {field}"),
            Source::WorkspaceYaml => f.write_str("pnpm-workspace.yaml"),
            Source::GlobalConfigYaml => f.write_str("pnpm global config.yaml"),
            Source::Npmrc => f.write_str(".npmrc"),
            Source::Default => f.write_str("default"),
            Source::Ci => f.write_str("global virtual store auto-disabled in CI"),
            Source::IncompatiblePackage(name) => write!(
                f,
                "global virtual store auto-disabled in {} projects",
                toolchain_display_name(name)
            ),
        }
    }
}

impl Source {
    /// Whether the reader themselves put this value on a surface nub still
    /// reads for layout. `Default` is not one — it means nothing in the project
    /// asked, which is exactly the state a dropped branded layout setting
    /// explains.
    fn is_authored_layout_surface(&self) -> bool {
        matches!(
            self,
            Source::Cli(_)
                | Source::Env(_)
                | Source::ProjectConfig(_)
                | Source::GlobalConfigYaml
                | Source::Npmrc
        )
    }
}

/// The readable settings tiers for one project root, loaded once per install.
pub(super) struct SourceIndex {
    cli: Vec<(String, String)>,
    env: Vec<(String, String)>,
    project_config: Vec<(String, String)>,
    /// Settings a `pnpm-workspace.yaml` claims, each normalized to the scalar
    /// representation this report consumes (including comma-rendered string
    /// lists). Resolved eagerly at load: the raw YAML map's element type belongs
    /// to aube's yaml crate, which is not a nub dependency, so it cannot be held
    /// in a field here.
    workspace_yaml: Vec<(&'static str, String)>,
    /// Settings supplied by pnpm v11's global `config.yaml`, loaded through
    /// aube's context-gated loader and interpreted with the same YAML helpers as
    /// the project workspace file.
    global_config_yaml: Vec<(&'static str, String)>,
    project_npmrc: Vec<(String, String)>,
    user_npmrc: Vec<(String, String)>,
    embedder_defaults: Vec<(String, String)>,
    /// Every package name any importer DECLARES, across the root manifest and
    /// each workspace member. Not a settings tier — it is the other half of the
    /// store decision: `disableGlobalVirtualStoreForPackages` is matched against
    /// this set, and a hit forces the whole install project-local. Held here
    /// because the layout row must answer that question and only `load` has the
    /// project root.
    declared_packages: Vec<String>,
    /// Whether a branded config file this project's incumbent owns asks for a
    /// `node_modules` layout — a request nub no longer honors from there.
    branded_layout_ignored: bool,
    /// Whether the engine will read this run as CI, where the global virtual
    /// store is off by default.
    ///
    /// Captured here rather than asked at the point of use, because the answer
    /// is a process global: a `layout_row` that called `is_ci()` itself could
    /// only be tested by skipping the assertion whenever the variable happened
    /// to be set, which on the CI leg that gates merge meant skipping it
    /// always. Reading it once at `load`, where every other tier is also
    /// snapshotted, makes the layout decision a pure function of this struct.
    ci: bool,
}

impl SourceIndex {
    pub(super) fn load(cwd: &Path, cli: &[(String, String)]) -> Self {
        let npmrc = aube_registry::config::load_npmrc_entries_split(cwd);
        let raw = aube_manifest::workspace::load_raw(cwd).unwrap_or_default();
        // Keep the foreign YAML value local to this loader: nub-cli does not
        // depend on aube's YAML crate, but can still normalize it before this
        // index stores an owned string.
        let workspace_yaml_scalar = |meta: &aube_settings::SettingMeta, raw| match meta.type_ {
            "bool" => meta.workspace_yaml_keys.iter().find_map(|key| {
                let value = aube_settings::workspace_yaml_value(raw, key)?;
                value
                    .as_bool()
                    .or_else(|| value.as_str().and_then(aube_settings::values::parse_bool))
                    .map(|value| value.to_string())
            }),
            "list<string>" => meta.workspace_yaml_keys.iter().find_map(|key| {
                let value = aube_settings::workspace_yaml_value(raw, key)?;
                if let Some(items) = value.as_sequence() {
                    return Some(
                        items
                            .iter()
                            .filter_map(|item| item.as_str())
                            .collect::<Vec<_>>()
                            .join(","),
                    );
                }
                value.as_str().map(render_string_list)
            }),
            _ => aube_settings::values::string_from_workspace_yaml(meta.name, raw),
        };
        let yaml_settings = |raw| {
            aube_settings::all()
                .iter()
                .filter(|meta| !aube_settings::workspace_yaml_suppressed(meta))
                .filter_map(|meta| workspace_yaml_scalar(meta, raw).map(|value| (meta.name, value)))
                .collect::<Vec<_>>()
        };
        let workspace_yaml = yaml_settings(&raw);
        let global_config_yaml = yaml_settings(&aube::commands::load_global_config_yaml());
        // The same walk as `workspace_yaml`, filter inverted: that field holds
        // the settings the YAML still supplies, this one asks whether the
        // current pnpm-major posture rejected a layout key.
        let yaml_layout_dropped = aube_settings::all().iter().any(|meta| {
            aube_settings::workspace_yaml_suppressed(meta)
                && meta.layout
                && !meta.npmrc_keys.is_empty()
                && meta
                    .workspace_yaml_keys
                    .iter()
                    .any(|key| aube_settings::workspace_yaml_value(&raw, key).is_some())
        });
        let context = aube_util::engine_context();
        Self {
            cli: cli.to_vec(),
            env: aube_settings::values::capture_env(),
            workspace_yaml,
            global_config_yaml,
            project_npmrc: npmrc.project,
            user_npmrc: npmrc.user,
            embedder_defaults: aube_settings::embedder_defaults().to_vec(),
            declared_packages: declared_packages(cwd),
            branded_layout_ignored: branded_layout_ignored(
                cwd,
                yaml_layout_dropped,
                context.read_yarn_config,
                context.read_bun_config,
            ),
            ci: aube_util::env::is_ci(),
            project_config: context.project_config_settings,
        }
    }

    /// The value in effect for `setting`, and the tier that supplied it when
    /// this index can name one. `None` means no readable tier claims the setting
    /// — or one claims it in a shape this index cannot render, reported the same
    /// way, since a value it cannot read is a value it must not print.
    pub(super) fn resolve(&self, setting: &str) -> Option<(String, Option<Source>)> {
        let meta = aube_settings::find(setting)?;
        // `InstallOptions::cli_flags` contains only explicit install flags. A
        // bag key may be a generic setting override the report cannot spell
        // faithfully from this narrowed representation, so only name declared
        // command flags. The engine still applies those generic keys; the report
        // simply omits an origin it cannot attribute exactly.
        if let Some((flag, value)) = self.cli.iter().rev().find_map(|(flag, raw)| {
            meta.cli_flags
                .contains(&flag.as_str())
                .then(|| readable_value(meta, raw))
                .flatten()
                .map(|value| (flag, value))
        }) {
            let source = Source::Cli(format!("--{flag}={value}"));
            return Some((value, Some(source)));
        }
        // Match the resolver's alias priority first, then its most-recent value
        // rule within that alias. An invalid boolean masks its env tier and lets
        // a lower tier decide; it must not be credited to a different alias.
        for var in meta.env_vars.iter().rev() {
            if !aube_util::env::branded_env_alias_enabled(var) {
                continue;
            }
            if let Some((_, value)) = self.env.iter().rev().find(|(key, _)| key == var) {
                if let Some(value) = readable_value(meta, value) {
                    return Some((value, Some(Source::Env((*var).to_string()))));
                }
                break;
            }
        }
        if let Some((_, raw)) = self
            .project_config
            .iter()
            .rev()
            .find(|(key, _)| key == setting)
        {
            if let Some(value) = readable_value(meta, raw) {
                return Some((
                    value,
                    project_config_field(setting).map(Source::ProjectConfig),
                ));
            }
        }
        for (entries, source) in [
            (&self.workspace_yaml, Source::WorkspaceYaml),
            (&self.global_config_yaml, Source::GlobalConfigYaml),
        ] {
            if let Some((_, value)) = entries.iter().find(|(name, _)| *name == setting) {
                return Some((value.clone(), Some(source)));
            }
        }
        for entries in [&self.project_npmrc, &self.user_npmrc] {
            if let Some(value) = entries.iter().rev().find_map(|(key, raw)| {
                meta.npmrc_keys
                    .contains(&key.as_str())
                    .then(|| readable_value(meta, raw))
                    .flatten()
            }) {
                return Some((value, Some(Source::Npmrc)));
            }
        }
        self.embedder_defaults
            .iter()
            .rev()
            .find_map(|(key, raw)| {
                (key == setting)
                    .then(|| readable_value(meta, raw))
                    .flatten()
            })
            .map(|value| (value, Some(Source::Default)))
    }
}

/// Whether a branded config file nub reads for this project asks for a
/// `node_modules` layout that Nub cannot consume from that source — Yarn's
/// `.yarnrc.yml nodeLinker`, Bun's `bunfig.toml [install].linker`, or a
/// `pnpm-workspace.yaml` layout key under a pnpm major that still uses
/// `.npmrc`. The install header is the only place that drop can surface.
///
/// Each source is gated on the posture that decides whether nub opens that file
/// AT ALL: `pnpm-workspace.yaml` through `load_raw`, which already honors it,
/// and yarn's and bun's passed in by the caller. A `bunfig.toml` sitting in an
/// npm project is read for nothing, so its `linker` is not this compat policy's
/// doing and pointing at a replacement source would misattribute why it went
/// unused.
fn branded_layout_ignored(
    cwd: &Path,
    in_workspace_yaml: bool,
    read_yarn: bool,
    read_bun: bool,
) -> bool {
    in_workspace_yaml
        || (read_yarn && super::yarnrc_node_linker(cwd).is_some())
        || (read_bun && super::bun_config::declares_install_linker(cwd))
}

/// Every package name declared by the root manifest and by each workspace
/// member, matching the importer set `find_gvs_incompatible_trigger` scans.
/// Traversal mirrors `unsupported_config::injected_deps_present`, the other
/// manifest-wide probe the install header depends on.
fn declared_packages(root: &Path) -> Vec<String> {
    let mut roots = vec![root.to_path_buf()];
    roots.extend(
        aube_workspace::find_workspace_packages(root)
            .into_iter()
            .flatten(),
    );
    roots
        .iter()
        .filter_map(|dir| super::cached_aube_manifest(&dir.join("package.json")))
        .flat_map(|manifest| {
            manifest
                .dependencies
                .keys()
                .chain(manifest.dev_dependencies.keys())
                .chain(manifest.optional_dependencies.keys())
                .cloned()
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The `nub.jsonc` field a lowered engine setting came from, for the settings
/// `lower_native_install_settings` writes. Mapping the setting back to what the
/// user typed is the whole point of the parenthetical — naming the engine key
/// would send them looking for a field their config does not have.
fn project_config_field(setting: &str) -> Option<&'static str> {
    Some(match setting {
        "nodeLinker" | "enableGlobalVirtualStore" => "install.linker",
        "hoist" | "hoistPattern" => "install.linker.hoist",
        "shamefullyHoist" | "publicHoistPattern" => "install.publicHoist",
        "disableGlobalVirtualStoreForPackages" | "diskMaterializePackages" => {
            "install.linker.eject"
        }
        "minimumReleaseAge" | "minimumReleaseAgeStrict" => "install.minimumReleaseAge",
        "minimumReleaseAgeExclude" => "install.minimumReleaseAgeExclude",
        _ => return None,
    })
}

// ─────────────────────────── the two-column block ───────────────────────────

/// One styled run on a line. Width math runs on the plain text and the styling
/// is applied only at write time, so an ANSI escape can never be counted as a
/// display column.
struct Piece<'a> {
    text: &'a str,
    /// What precedes this piece when it is not the first thing on its line.
    sep: &'static str,
    dim: bool,
}

/// A block row: a label, the comma-separated values it carries, and the
/// provenance note trailing the last value.
pub(super) struct Row {
    label: &'static str,
    values: Vec<String>,
    note: Option<String>,
}

impl Row {
    fn new(label: &'static str, values: Vec<String>, source: Option<Source>) -> Self {
        Self {
            label,
            values,
            note: source.map(|source| format!("({source})")),
        }
    }
}

/// Where the value column starts, given the widest label in the block. Also the
/// hanging indent every continuation line is padded to.
fn hanging_indent(label_w: usize) -> usize {
    INDENT + label_w + GAP
}

/// Render rows as an unruled two-column block: labels left-aligned in a column
/// sized to the widest one, values wrapped to `cols` with a hanging indent that
/// holds every continuation line in the value column.
fn render_block(rows: &[Row], cols: usize) -> String {
    let label_w = rows.iter().map(|row| row.label.len()).max().unwrap_or(0);
    // A pathologically narrow terminal must still make progress rather than
    // emit one token per line forever.
    let limit = cols.max(hanging_indent(label_w) + 20);
    let mut out = String::new();
    for row in rows {
        let mut pieces: Vec<Piece<'_>> = row
            .values
            .iter()
            .map(|value| Piece {
                text: value.as_str(),
                sep: ", ",
                dim: false,
            })
            .collect();
        if let Some(note) = &row.note {
            pieces.push(Piece {
                text: note.as_str(),
                sep: " ",
                dim: true,
            });
        }
        write_row(&mut out, row.label, label_w, limit, &pieces);
    }
    out
}

fn write_row(out: &mut String, label: &str, label_w: usize, limit: usize, pieces: &[Piece<'_>]) {
    let value_col = hanging_indent(label_w);
    out.push_str(&" ".repeat(INDENT));
    out.push_str(&style::edim(format!("{label:<label_w$}")).to_string());
    out.push_str(&" ".repeat(GAP));
    let mut col = value_col;
    let mut at_line_start = true;
    for piece in pieces {
        let width = piece.text.chars().count();
        // The separator belongs to whichever line the piece lands on, so a
        // wrapped piece sheds it and no continuation opens with a stray comma.
        if !at_line_start && col + piece.sep.len() + width > limit {
            out.push('\n');
            out.push_str(&" ".repeat(value_col));
            col = value_col;
            at_line_start = true;
        }
        if !at_line_start {
            out.push_str(piece.sep);
            col += piece.sep.len();
        }
        if piece.dim {
            out.push_str(&style::edim(piece.text).to_string());
        } else {
            out.push_str(piece.text);
        }
        col += width;
        at_line_start = false;
    }
    out.push('\n');
}

fn stderr_cols() -> usize {
    console::Term::stderr()
        .size_checked()
        .map_or(FALLBACK_COLS, |(_, cols)| cols as usize)
}

// ──────────────────────────── the resolved layout ────────────────────────────

/// Peer- and version-resolution settings worth stating once a project has moved
/// them off their built-in default, in the `.npmrc` spelling the user would have
/// typed. Deliberately short: every entry changes which versions land in the
/// tree. The third field is the engine's own default, so an explicit setting
/// that merely restates it stays quiet.
const RESOLUTION_SETTINGS: &[(&str, &str, &str)] = &[
    ("autoInstallPeers", "auto-install-peers", "true"),
    (
        "strictPeerDependencies",
        "strict-peer-dependencies",
        "false",
    ),
    ("dedupePeerDependents", "dedupe-peer-dependents", "true"),
    ("resolutionMode", "resolution-mode", "highest"),
];

/// Whether a resolved value still sits at the engine's own default for it.
///
/// Three of the four settings above are booleans and one (`resolutionMode`) is
/// a string enum, so compare as booleans when both sides parse that way and fall
/// back to text otherwise. A raw string compare called `auto-install-peers=1` a
/// non-default and printed a row for a setting sitting exactly at its default.
fn at_default(value: &str, default: &str) -> bool {
    match (
        aube_settings::values::parse_bool(value),
        aube_settings::values::parse_bool(default),
    ) {
        (Some(actual), Some(expected)) => actual == expected,
        _ => value == default,
    }
}

/// The layout value, in the vocabulary the reader's own config uses. Both
/// symlink layouts lower to the engine's `isolated`, differing only in
/// `enableGlobalVirtualStore`, so printing the raw engine value would answer a
/// project that asked for `global-virtual-store` with the word `isolated` —
/// while the parenthetical points at the very field that says otherwise.
///
/// When nothing set the store bit, the engine's own default decides it, and
/// that default is the shared store. Reporting the raw `isolated` there
/// described a tree nobody gets: with no config at all the packages symlink
/// into the machine-global store, which is what `global-virtual-store` names.
///
/// Four things flip it back to a project-local store, and they arrive by
/// different routes — which is why this cannot simply read one setting. An
/// explicit `enableGlobalVirtualStore=false` and the `hoist=true` nub pushes
/// for injected dependencies both land in the settings index. The other two do
/// not. A CI environment is derived from `is_ci()` where the engine plans the
/// store, so the only way to report it is to ask the same question aube will.
/// And a declared package on `disableGlobalVirtualStoreForPackages` — nub seeds
/// `next` and `react-native`, so a stock Next.js project with no config at all
/// takes this route — is a fact about the MANIFEST, not about any setting: it
/// reads as unset here while `resolve_global_virtual_store_override` turns it
/// into a whole-install opt-out. Those two get their own [`Source`] variants
/// rather than no parenthetical at all: they are precisely the layouts a reader
/// cannot account for by opening their config, so leaving them bare showed a
/// value that contradicts the documented default with nothing to explain it.
fn layout_row(index: &SourceIndex) -> (String, Option<Source>) {
    let isolated = |source: Option<Source>| ("isolated".to_string(), source);
    let (linker, linker_source) = index
        .resolve("nodeLinker")
        .unwrap_or_else(|| isolated(None));
    if linker != "isolated" {
        return (linker, linker_source);
    }
    // Then the four routes to a project-local store, in the order the engine
    // settles them; the first that fires owns both the word and the note.
    if let Some((shared, source)) = index.resolve("enableGlobalVirtualStore") {
        return if is_true(&shared) {
            ("global-virtual-store".to_string(), source)
        } else {
            isolated(source)
        };
    }
    if index.ci {
        return isolated(Some(Source::Ci));
    }
    if let Some((_, source)) = index.resolve("hoist").filter(|(hoist, _)| is_true(hoist)) {
        return isolated(source);
    }
    match gvs_incompatible_package(index) {
        Some(name) => isolated(Some(Source::IncompatiblePackage(name))),
        None => ("global-virtual-store".to_string(), None),
    }
}

/// The declared package that matches `disableGlobalVirtualStoreForPackages`,
/// the whole-install opt-out `resolve_global_virtual_store_override` applies
/// when nothing set the store bit explicitly. Mirrors that function's guards:
/// `virtualStoreOnly` suppresses the opt-out, and the CI case is already
/// decided by the caller before this is reached.
///
/// Returns the PACKAGE, not the pattern that caught it: `next` is the name the
/// reader recognizes from their own manifest, where a glob out of nub's seeded
/// list is one more thing to go look up. The first match wins — the opt-out is
/// whole-install, so a second one changes nothing about the layout.
fn gvs_incompatible_package(index: &SourceIndex) -> Option<String> {
    if index
        .resolve("virtualStoreOnly")
        .is_some_and(|(value, _)| is_true(&value))
    {
        return None;
    }
    let (raw, _) = index.resolve("disableGlobalVirtualStoreForPackages")?;
    comma_items(&raw).find_map(|pattern| {
        index
            .declared_packages
            .iter()
            .find(|name| aube_linker::package_name_matches(pattern, name))
            .cloned()
    })
}

/// What the layout row says in place of provenance when the project asked for a
/// layout in a file nub no longer takes one from. Deliberately not a warning:
/// the reader's config is still valid for everything else in it, and the whole
/// remedy is the name of the file that would work.
const LAYOUT_POINTER: &str = "configurable via .npmrc node-linker or --node-linker";

/// Always present, even when everything is default: the layout is the one fact
/// that governs how the tree on disk is shaped.
///
/// The pointer displaces provenance only where provenance is nub's own default
/// or nothing at all. A layout the reader wrote into `nub.jsonc`, `.npmrc`, or
/// the environment already has a live surface, so naming that surface answers
/// the question they actually have and the advice would be noise on top of it.
fn linker_row(index: &SourceIndex) -> Row {
    let (layout, source) = layout_row(index);
    let authored = source
        .as_ref()
        .is_some_and(Source::is_authored_layout_surface);
    let mut row = Row::new("linker", vec![layout], source);
    if index.branded_layout_ignored && !authored {
        row.note = Some(format!("({LAYOUT_POINTER})"));
    }
    row
}

/// Both pattern lists answer "where can an undeclared import find this", so they
/// share a row. The patterns ARE the answer — an enumerated package list would
/// be unreadable and a count says nothing at all.
///
/// `shamefullyHoist` is pnpm's sugar for `publicHoistPattern: ['*']`, and the
/// linker honors it as a strict superset — a true flag skips the pattern test
/// for every name rather than adding to the list — so it IS the pattern when
/// set. Reading only the pattern list described a root `node_modules` holding
/// the few names it mentions while the install had put every name there, and
/// said nothing at all when the flag was the only thing set.
fn hoisting_rows(index: &SourceIndex) -> Vec<Row> {
    let shamefully_hoist = index
        .resolve("shamefullyHoist")
        .filter(|(value, _)| is_true(value));
    ["publicHoistPattern", "hoistPattern"]
        .into_iter()
        .filter_map(|setting| {
            let (raw, source) = match (setting, &shamefully_hoist) {
                ("publicHoistPattern", Some((_, source))) => ("*".to_string(), source.clone()),
                _ => index.resolve(setting)?,
            };
            let patterns: Vec<String> = comma_items(&raw).map(str::to_string).collect();
            if patterns.is_empty() {
                return None;
            }
            Some(Row::new("hoisting", patterns, source))
        })
        .collect()
}

/// The resolution settings a project has moved off their built-in default, on
/// one row. Empty when it has moved none, which is the common case.
fn resolution_row(index: &SourceIndex) -> Option<Row> {
    let mut values = Vec::new();
    let mut shared_source = None;
    for (setting, spelling, default) in RESOLUTION_SETTINGS {
        let Some((value, source)) = index
            .resolve(setting)
            .filter(|(value, _)| !at_default(value, default))
        else {
            continue;
        };
        values.push(if is_true(&value) {
            (*spelling).to_string()
        } else {
            format!("{spelling}={value}")
        });
        // One row, one parenthetical: keep it only while every entry agrees, and
        // drop it the moment they don't rather than credit all of them to
        // whichever happened to come first.
        if values.len() == 1 {
            shared_source = source;
        } else if shared_source != source {
            shared_source = None;
        }
    }
    if values.is_empty() {
        return None;
    }
    Some(Row::new("resolution", values, shared_source))
}

pub(super) fn resolved_rows(index: &SourceIndex) -> Vec<Row> {
    let mut rows = vec![linker_row(index)];
    rows.extend(hoisting_rows(index));
    rows.extend(resolution_row(index));
    rows
}

/// Print the resolved layout ahead of the engine's progress display. Silent
/// under `--silent`; otherwise always prints at least the `layout` row, so a
/// default install is one line here and one line at the end.
pub(super) fn print_resolved_layout(
    cwd: &Path,
    output: &OutputFlags,
    cli_flags: &[(String, String)],
) {
    if output.is_silent() {
        return;
    }
    let rows = resolved_rows(&SourceIndex::load(cwd, cli_flags));
    eprint!("{}", render_block(&rows, stderr_cols()));
    eprintln!();
}

// ───────────────────────── the materialization digest ─────────────────────────

/// Why one package ended up as real project-local bytes instead of a symlink
/// into the shared store. Recorded where the decision is made — nub's
/// disk-materialize expansion hook — so the digest reports the plan that ran
/// rather than re-deriving it afterwards.
///
/// There is deliberately no importing SOURCE FILE here: the phantom scanner
/// caches a per-content verdict carrying the undeclared package NAMES only, so a
/// file path would have to be invented.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Reason {
    /// Ships imports it never declared; the names are what the scanner found.
    Undeclared(Vec<String>),
    /// Its type surface imports a peer whose `@types/*` sits at the project root.
    PeerTypes,
    /// Its build script reads or writes the consuming project.
    ProjectContext,
    /// It supplies a gyp file to a dependant's native build, which records a
    /// relative path to it that must not leave the project's virtual store.
    GypProvider,
    /// Its importer's lifecycle script MOVES files out of it, which must never
    /// happen to a directory the shared store hands to every project.
    MutatedByImporter,
    /// Vite below 8.1 cannot read the shared store's `.modules.yaml`.
    LegacyVite,
    /// Named by `install.linker.eject`, or by nub's own built-in seed.
    Configured,
    /// Imports a package that had to move, so it moves too — otherwise it would
    /// keep resolving the store-resident copy and split the singleton.
    ImporterOf(String),
    /// In the closure for a reason this walk could not name — it matched no
    /// seed, and no declared dependency of it was found in the plan.
    ///
    /// Its own variant rather than falling back to [`Reason::Configured`],
    /// which is the shape this had and which was simply false: it told the
    /// reader config named a package config never mentions, and that is a claim
    /// they can go check. Vague and true beats specific and wrong on a line
    /// whose entire job is explaining why something moved.
    Closure,
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Reason::Undeclared(names) => write!(f, "undeclared imports: {}", names.join(", ")),
            Reason::PeerTypes => f.write_str("peer types resolved from the project root"),
            Reason::ProjectContext => f.write_str("build script reads the project"),
            Reason::GypProvider => f.write_str("supplies a gyp file to a native build"),
            Reason::MutatedByImporter => f.write_str("its importer's build script moves its files"),
            Reason::LegacyVite => f.write_str("vite below 8.1"),
            Reason::Configured => f.write_str("named by config"),
            Reason::ImporterOf(spec) => write!(f, "imports {spec}"),
            Reason::Closure => f.write_str("pulled in by the materialized set"),
        }
    }
}

/// One materialized package: what it is, and why it moved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Materialized {
    pub(super) name: String,
    pub(super) version: String,
    pub(super) reason: Reason,
}

impl Materialized {
    fn spec(&self) -> String {
        format!("{}@{}", self.name, self.version)
    }
}

static PLAN: RwLock<Vec<Materialized>> = RwLock::new(Vec::new());

/// Record the expansion hook's plan for the digest. Sorted here because the plan
/// is built from hash sets, and an install's output must not reorder run to run.
pub(super) fn record_plan(mut entries: Vec<Materialized>) {
    entries.sort_by(|a, b| (&a.name, &a.version).cmp(&(&b.name, &b.version)));
    *PLAN.write().unwrap_or_else(|error| error.into_inner()) = entries;
}

fn recorded_plan() -> Vec<Materialized> {
    PLAN.read()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
}

pub(super) fn digest_rows(entries: &[Materialized], verbose: bool) -> Vec<Row> {
    if entries.is_empty() {
        return Vec::new();
    }
    // Verbose replaces the joined list rather than annotating it — the detail
    // lines already name every package, so keeping both would print each twice.
    if verbose {
        return entries
            .iter()
            .enumerate()
            .map(|(i, entry)| Row {
                label: if i == 0 { "materialized" } else { "" },
                values: vec![entry.spec()],
                note: Some(format!("({})", entry.reason)),
            })
            .collect();
    }
    vec![
        Row::new(
            "materialized",
            entries.iter().map(Materialized::spec).collect(),
            None,
        ),
        Row {
            label: "",
            values: Vec::new(),
            note: Some("run with --loglevel debug to see why".to_string()),
        },
    ]
}

/// Print the digest between the end of linking and the engine's success line.
/// Nothing prints when nothing moved, which is the common case.
pub(super) fn print_digest(output: &OutputFlags, uses_shared_store: bool, is_noop: bool) {
    // Off the shared store every package is already project-local, so there is
    // no subset to report and the word "materialized" would mean nothing.
    if output.is_silent() || is_noop || !uses_shared_store {
        return;
    }
    let entries = recorded_plan();
    if entries.is_empty() {
        return;
    }
    let rows = digest_rows(&entries, output.is_debug());
    eprintln!();
    eprint!("{}", render_block(&rows, stderr_cols()));
    eprintln!();
}

/// Register the digest with the engine so it lands after linking and before the
/// engine's own success line, keeping that line last. Set-once; with no host
/// registered the engine calls nothing.
pub(super) fn register(output: OutputFlags) {
    aube::commands::install::set_pre_summary_hook(Box::new(move |summary| {
        // BEFORE the digest, so a failure the user has to act on is not scrolled away by the layout
        // report under it. Accumulated across the install and emitted exactly once — see
        // `build_jail::report_jail_failures`, which also explains why the message makes no claim
        // about WHY a script failed.
        super::build_jail::report_jail_failures();
        print_digest(&output, summary.uses_shared_store, summary.is_noop);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(text: &str) -> String {
        console::strip_ansi_codes(text).to_string()
    }

    fn row(label: &'static str, values: &[&str], source: Option<Source>) -> Row {
        Row::new(
            label,
            values.iter().map(|value| (*value).to_string()).collect(),
            source,
        )
    }

    fn materialized(spec: &str, reason: Reason) -> Materialized {
        let (name, version) = spec.rsplit_once('@').unwrap();
        Materialized {
            name: name.to_string(),
            version: version.to_string(),
            reason,
        }
    }

    /// An index no tier claims anything in, to be filled one tier at a time with
    /// `..empty_index()`. Spelling the whole struct out per test buried which
    /// field each one was actually about.
    fn empty_index() -> SourceIndex {
        SourceIndex {
            cli: Vec::new(),
            env: Vec::new(),
            project_config: Vec::new(),
            workspace_yaml: Vec::new(),
            global_config_yaml: Vec::new(),
            project_npmrc: Vec::new(),
            user_npmrc: Vec::new(),
            embedder_defaults: Vec::new(),
            declared_packages: Vec::new(),
            branded_layout_ignored: false,
            ci: false,
        }
    }

    fn engine_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::pm_engine::ENGINE_GLOBAL_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// Holds the engine lock and restores the process-global engine context on
    /// DROP, not by a statement at the end: the asserts in between can panic, and
    /// a tail restore would leave every later test in this binary reading the
    /// postures this one set. Since every holder takes the lock with
    /// `unwrap_or_else(into_inner)`, they proceed on that corrupted state rather
    /// than failing — one real failure would spray unrelated ones and bury which
    /// test actually broke.
    struct EngineGuard {
        context: aube_util::EngineContext,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl EngineGuard {
        fn take() -> Self {
            Self {
                _lock: engine_lock(),
                context: aube_util::engine_context(),
            }
        }
    }

    impl Drop for EngineGuard {
        fn drop(&mut self) {
            aube_util::set_engine_context(self.context.clone());
        }
    }

    /// The quiet common case: nothing in the project moved a setting, so the
    /// header is the single layout line.
    #[test]
    fn default_install_renders_one_line() {
        let rendered = plain(&render_block(
            &[row("linker", &["isolated"], Some(Source::Default))],
            80,
        ));
        assert_eq!(rendered, "  linker  isolated (default)\n");
    }

    /// Labels share one column and values start at a single hanging indent, so
    /// the block reads as a table without drawing one.
    #[test]
    fn labels_and_values_align_on_one_column() {
        let rendered = plain(&render_block(
            &[
                row(
                    "linker",
                    &["isolated"],
                    Some(Source::ProjectConfig("install.linker")),
                ),
                row(
                    "hoisting",
                    &["@types/*", "*eslint*"],
                    Some(Source::ProjectConfig("install.publicHoist")),
                ),
                row(
                    "resolution",
                    &["auto-install-peers", "strict-peer-dependencies"],
                    Some(Source::Npmrc),
                ),
            ],
            100,
        ));
        assert_eq!(
            rendered,
            "  linker      isolated (nub.jsonc install.linker)\n\
             \x20 hoisting    @types/*, *eslint* (nub.jsonc install.publicHoist)\n\
             \x20 resolution  auto-install-peers, strict-peer-dependencies (.npmrc)\n"
        );
    }

    /// A long value wraps to the terminal width with every continuation line in
    /// the value column — the shape a ~40-package digest has to hold.
    #[test]
    fn long_values_wrap_with_a_hanging_indent() {
        let entries: Vec<Materialized> = (0..40)
            .map(|i| materialized(&format!("package-{i:02}@1.0.0"), Reason::Configured))
            .collect();
        let rendered = plain(&render_block(&digest_rows(&entries, false), 80));
        let value_col = INDENT + "materialized".len() + GAP;

        let mut lines = rendered.lines();
        assert!(
            lines
                .next()
                .unwrap()
                .starts_with("  materialized  package-00@1.0.0, "),
            "first line must open the value column: {rendered}"
        );
        for line in rendered.lines() {
            assert!(
                line.chars().count() <= 80,
                "line exceeds the terminal width ({}): {line:?}",
                line.chars().count()
            );
        }
        for line in lines {
            assert_eq!(
                line.len() - line.trim_start().len(),
                value_col,
                "continuation must hang at the value column: {line:?}"
            );
        }
        // Wrapping must neither drop a package nor split one across lines.
        for entry in &entries {
            assert!(
                rendered.contains(&entry.spec()),
                "{} went missing",
                entry.spec()
            );
        }
        assert!(rendered.contains("run with --loglevel debug to see why"));
    }

    /// The debug view attaches a reason per package, including the closure edge
    /// that explains a package nothing flagged directly.
    #[test]
    fn debug_view_attaches_a_reason_per_package() {
        let entries = vec![
            materialized(
                "my-plugin@1.0.0",
                Reason::ImporterOf("vite@7.2.1".to_string()),
            ),
            materialized(
                "vite@7.2.1",
                Reason::Undeclared(vec!["postcss".to_string()]),
            ),
        ];
        let rendered = plain(&render_block(&digest_rows(&entries, true), 100));
        assert_eq!(
            rendered,
            "  materialized  my-plugin@1.0.0 (imports vite@7.2.1)\n\
             \x20               vite@7.2.1 (undeclared imports: postcss)\n"
        );
        // The joined list is REPLACED, not annotated — no package appears twice.
        assert_eq!(rendered.matches("vite@7.2.1 (").count(), 1);
    }

    /// The layout speaks the vocabulary of the config that set it. Both symlink
    /// layouts lower to the engine's `isolated`, so a project that asked for the
    /// shared store must not be told `isolated` by a line whose parenthetical
    /// points straight back at the field that says otherwise.
    #[test]
    fn layout_names_the_strategy_the_project_wrote() {
        let shared = SourceIndex {
            project_config: vec![
                ("nodeLinker".to_string(), "isolated".to_string()),
                ("enableGlobalVirtualStore".to_string(), "true".to_string()),
            ],
            ..empty_index()
        };
        assert_eq!(
            layout_row(&shared),
            (
                "global-virtual-store".to_string(),
                Some(Source::ProjectConfig("install.linker"))
            )
        );

        let project_local = SourceIndex {
            project_config: vec![
                ("nodeLinker".to_string(), "isolated".to_string()),
                ("enableGlobalVirtualStore".to_string(), "false".to_string()),
            ],
            ..shared
        };
        assert_eq!(
            layout_row(&project_local),
            (
                "isolated".to_string(),
                Some(Source::ProjectConfig("install.linker"))
            ),
            "an explicit project-local store keeps the plain isolated word, \
             still pointing at the field that asked for it"
        );

        // Nothing set the store bit, so the engine's default decides — and that
        // default is the shared store. Reporting the raw `isolated` here named a
        // tree nobody gets: with no config the packages symlink into the
        // machine-global store.
        let unset = SourceIndex {
            project_config: Vec::new(),
            embedder_defaults: vec![("nodeLinker".to_string(), "isolated".to_string())],
            ..project_local
        };
        assert_eq!(
            layout_row(&unset),
            ("global-virtual-store".to_string(), None)
        );

        // The injected-deps carve-out: nub pushes an explicit `hoist=true` for a
        // project that declares one, and the hidden tree it needs only exists
        // under a project-local store.
        let injected = SourceIndex {
            embedder_defaults: vec![
                ("nodeLinker".to_string(), "isolated".to_string()),
                ("hoist".to_string(), "true".to_string()),
            ],
            ..unset
        };
        assert_eq!(layout_row(&injected).0, "isolated");

        let hoisted = SourceIndex {
            embedder_defaults: vec![("nodeLinker".to_string(), "hoisted".to_string())],
            ..injected
        };
        assert_eq!(layout_row(&hoisted).0, "hoisted");

        // Nothing set, but the run is CI, where the engine derives the
        // project-local store from the environment rather than any setting.
        // This arm used to be untestable: `layout_row` asked `is_ci()` itself,
        // so the no-config assertion above had to be skipped whenever the
        // variable happened to be set — which on the leg that gates merge was
        // always. Reading it from the index instead makes both arms hermetic.
        let in_ci = SourceIndex {
            embedder_defaults: vec![("nodeLinker".to_string(), "isolated".to_string())],
            ci: true,
            ..empty_index()
        };
        assert_eq!(
            layout_row(&in_ci),
            ("isolated".to_string(), Some(Source::Ci))
        );
    }

    /// The tiers this module reads are raw text, and the engine's `parse_bool`
    /// accepts `1`/`TRUE`/`True` as true. Deciding the layout with `== "true"`
    /// disagreed with the resolver on exactly those spellings — in BOTH
    /// directions, so the header could print the opposite of the tree the
    /// install was about to build.
    #[test]
    fn boolean_settings_are_read_the_way_the_engine_reads_them() {
        let with_npmrc = |key: &str, value: &str| SourceIndex {
            project_npmrc: vec![(key.to_string(), value.to_string())],
            embedder_defaults: vec![("nodeLinker".to_string(), "isolated".to_string())],
            ..empty_index()
        };

        // The engine symlinks into the machine-global store for each of these,
        // so the row must not say `isolated`.
        for spelling in ["1", "TRUE", "True"] {
            assert_eq!(
                layout_row(&with_npmrc("enableGlobalVirtualStore", spelling)).0,
                "global-virtual-store",
                "`enableGlobalVirtualStore={spelling}` is true to the engine"
            );
        }

        // The other direction: `hoist=1` makes the engine veto the shared store
        // and build the hidden tree, so the row must not claim the shared one.
        assert_eq!(
            layout_row(&with_npmrc("hoist", "1")).0,
            "isolated",
            "`hoist=1` is true to the engine, which vetoes the shared store"
        );

        // And `virtualStoreOnly=1` suppresses the package opt-out engine-side.
        let store_only = SourceIndex {
            embedder_defaults: vec![
                ("nodeLinker".to_string(), "isolated".to_string()),
                (
                    "disableGlobalVirtualStoreForPackages".to_string(),
                    "next".to_string(),
                ),
            ],
            declared_packages: vec!["next".to_string()],
            ..with_npmrc("virtualStoreOnly", "1")
        };
        assert_eq!(layout_row(&store_only).0, "global-virtual-store");
    }

    /// A package the shared store cannot serve takes the store project-local
    /// without any setting saying so — nub seeds `next`, so a stock Next.js
    /// project with no config at all lands here. The store bit reads as unset,
    /// which is why the manifest has to be consulted: reporting the settings
    /// index alone told that project `global-virtual-store` while every package
    /// on disk was a real project-local directory.
    #[test]
    fn a_gvs_incompatible_dependency_reports_the_project_local_store() {
        let index = SourceIndex {
            embedder_defaults: vec![
                ("nodeLinker".to_string(), "isolated".to_string()),
                (
                    "disableGlobalVirtualStoreForPackages".to_string(),
                    "next,react-native".to_string(),
                ),
            ],
            declared_packages: vec!["next".to_string(), "debug".to_string()],
            ..empty_index()
        };
        // Named, not bare: `next` is the whole reason this project reads
        // `isolated` where the documented default is the shared store, and it
        // appears in no config file the reader could check.
        assert_eq!(
            layout_row(&index),
            (
                "isolated".to_string(),
                Some(Source::IncompatiblePackage("next".to_string()))
            )
        );

        // Only a DECLARED name triggers it; the seed on its own must not drag
        // every project off the shared store.
        let untriggered = SourceIndex {
            declared_packages: vec!["debug".to_string()],
            ..index
        };
        assert_eq!(
            layout_row(&untriggered),
            ("global-virtual-store".to_string(), None)
        );

        // `virtualStoreOnly` suppresses the opt-out engine-side, so the label
        // must not claim the project-local store the engine will not build.
        let store_only = SourceIndex {
            declared_packages: vec!["next".to_string()],
            project_npmrc: vec![("virtualStoreOnly".to_string(), "true".to_string())],
            ..untriggered
        };
        assert_eq!(layout_row(&store_only).0, "global-virtual-store");

        // CI outranks the package trigger: the engine's own resolution only
        // reaches the opt-out when the run is not CI, and the layout is
        // project-local either way, so the row must attribute it to the reason
        // that actually decided.
        let in_ci = SourceIndex {
            ci: true,
            declared_packages: vec!["next".to_string()],
            ..store_only
        };
        assert_eq!(
            layout_row(&in_ci),
            ("isolated".to_string(), Some(Source::Ci))
        );
    }

    /// `shamefully-hoist` hoists every name in the graph, so it reports as the
    /// `*` it is — both when a narrower pattern list sits alongside it (which
    /// the flag overrides wholesale) and when it is the only thing set.
    #[test]
    fn shamefully_hoist_reports_the_pattern_it_actually_is() {
        let index = SourceIndex {
            project_npmrc: vec![
                ("shamefully-hoist".to_string(), "true".to_string()),
                ("public-hoist-pattern".to_string(), "ms".to_string()),
            ],
            ..empty_index()
        };
        let rows = resolved_rows(&index);
        let hoisting = rows.iter().find(|row| row.label == "hoisting").unwrap();
        assert_eq!(hoisting.values, vec!["*"]);
        assert_eq!(hoisting.note.as_deref(), Some("(.npmrc)"));

        // nub's own `install.publicHoist` writes `shamefullyHoist=false`
        // alongside the patterns, so the narrowing must survive it.
        let narrowed = SourceIndex {
            project_config: vec![
                ("shamefullyHoist".to_string(), "false".to_string()),
                ("publicHoistPattern".to_string(), "@types/*".to_string()),
            ],
            project_npmrc: Vec::new(),
            ..index
        };
        let rows = resolved_rows(&narrowed);
        let hoisting = rows.iter().find(|row| row.label == "hoisting").unwrap();
        assert_eq!(hoisting.values, vec!["@types/*"]);
    }

    /// A project that wrote its layout into another tool's config file gets one
    /// pointer at the file that would work — and only where nothing it wrote on
    /// a surface nub still reads supplied the layout, since naming that surface
    /// is the more useful answer. A layout the ENVIRONMENT derived — CI, or a
    /// dependency the shared store cannot serve — is not such a surface, so the
    /// pointer survives it: the pointer displaces the reason deliberately,
    /// because knowing where to set the layout is what the reader can act on and
    /// setting it explicitly wins over either derived route anyway.
    #[test]
    fn a_dropped_branded_layout_points_at_the_neutral_surface() {
        let dropped = SourceIndex {
            branded_layout_ignored: true,
            ..empty_index()
        };
        let note = |index: &SourceIndex| {
            resolved_rows(index)
                .into_iter()
                .find(|row| row.label == "linker")
                .unwrap()
                .note
        };
        assert_eq!(
            note(&dropped).as_deref(),
            Some("(configurable via .npmrc node-linker or --node-linker)")
        );

        let quiet = SourceIndex {
            branded_layout_ignored: false,
            ..dropped
        };
        assert_eq!(
            note(&quiet),
            None,
            "a project carrying no such setting keeps the row it has always had"
        );

        let via_nub_jsonc = SourceIndex {
            project_config: vec![("nodeLinker".to_string(), "hoisted".to_string())],
            branded_layout_ignored: true,
            ..quiet
        };
        assert_eq!(
            note(&via_nub_jsonc).as_deref(),
            Some("(nub.jsonc install.linker)"),
            "already configured through nub.jsonc — provenance, not advice"
        );

        let via_npmrc = SourceIndex {
            project_config: Vec::new(),
            project_npmrc: vec![("node-linker".to_string(), "hoisted".to_string())],
            ..via_nub_jsonc
        };
        assert_eq!(note(&via_npmrc).as_deref(), Some("(.npmrc)"));

        // Neither derived route is a surface the reader authored, so both keep
        // the pointer rather than reporting the reason they came from.
        let in_ci = SourceIndex {
            project_npmrc: Vec::new(),
            ci: true,
            ..via_npmrc
        };
        assert_eq!(layout_row(&in_ci).1, Some(Source::Ci));
        assert_eq!(
            note(&in_ci).as_deref(),
            Some("(configurable via .npmrc node-linker or --node-linker)"),
            "a CI-derived layout is nothing the project wrote"
        );

        let incompatible = SourceIndex {
            ci: false,
            embedder_defaults: vec![(
                "disableGlobalVirtualStoreForPackages".to_string(),
                "next".to_string(),
            )],
            declared_packages: vec!["next".to_string()],
            ..in_ci
        };
        assert_eq!(
            layout_row(&incompatible).1,
            Some(Source::IncompatiblePackage("next".to_string()))
        );
        assert_eq!(
            note(&incompatible).as_deref(),
            Some("(configurable via .npmrc node-linker or --node-linker)"),
            "a dependency-derived layout is nothing the project wrote either"
        );
    }

    /// The three branded files layout was taken back from, each detected in the
    /// shape its own tool writes, and each gated on the posture that decides
    /// whether nub reads that file at all.
    #[test]
    fn each_branded_layout_source_is_detected() {
        let _guard = EngineGuard::take();
        aube_util::update_engine_context(|c| {
            c.read_branded_pnpm_config = true;
            c.read_layout_from_workspace_yaml = false;
            c.read_yarn_config = true;
            c.read_bun_config = true;
        });

        let project = |files: &[(&str, &str)]| {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(
                dir.path().join("package.json"),
                r#"{"name":"app","version":"1.0.0"}"#,
            )
            .unwrap();
            for (name, body) in files {
                std::fs::write(dir.path().join(name), body).unwrap();
            }
            dir
        };
        let detected =
            |dir: &tempfile::TempDir| SourceIndex::load(dir.path(), &[]).branded_layout_ignored;

        for (file, body) in [
            ("pnpm-workspace.yaml", "nodeLinker: hoisted\n"),
            ("pnpm-workspace.yaml", "modulesDir: vendor_modules\n"),
            (".yarnrc.yml", "nodeLinker: node-modules\n"),
            ("bunfig.toml", "[install]\nlinker = \"hoisted\"\n"),
        ] {
            assert!(
                detected(&project(&[(file, body)])),
                "{file} asks for a layout, so the row must say where to set one: {body:?}"
            );
        }

        assert!(
            !detected(&project(&[])),
            "a project with none of these files must print the row it always has"
        );
        assert!(
            !detected(&project(&[(
                "pnpm-workspace.yaml",
                "autoInstallPeers: false\n"
            )])),
            "the probe keys on a layout setting, not on the file's presence"
        );

        // The gate: a file nub never opens went unread for its own reason, and
        // blaming the compat layout policy for it would send the reader to the wrong fix.
        let bun_only = project(&[("bunfig.toml", "[install]\nlinker = \"hoisted\"\n")]);
        aube_util::update_engine_context(|c| c.read_bun_config = false);
        assert!(!detected(&bun_only));
    }

    /// Nothing materialized prints nothing: materialization is routine, and a
    /// run without any must not grow a block announcing that.
    #[test]
    fn empty_digest_renders_nothing() {
        assert!(digest_rows(&[], false).is_empty());
        assert!(digest_rows(&[], true).is_empty());
    }

    /// A package in the closure whose edge could not be located must not claim
    /// config named it. `Reason::Configured` was the fallback for that case, so
    /// the digest told the reader to go look in `install.linker.eject` for a
    /// package that is not there — a specific, checkable, wrong answer on the
    /// one line whose whole job is saying why something moved.
    #[test]
    fn an_unattributed_closure_member_does_not_blame_config() {
        assert_eq!(
            Reason::Closure.to_string(),
            "pulled in by the materialized set"
        );
        assert_ne!(
            Reason::Closure.to_string(),
            Reason::Configured.to_string(),
            "the two must stay distinguishable — collapsing them is the defect"
        );
    }

    /// The plan is built from hash sets, so `record_plan` sorts before storing
    /// or an install's own output reorders between identical runs. The digest
    /// tests all call `digest_rows` on already-ordered input and would not
    /// notice the sort disappearing; this goes through the recording path.
    #[test]
    fn a_recorded_plan_is_ordered_regardless_of_insertion() {
        let _guard = engine_lock();
        struct RestorePlan(Vec<Materialized>);
        impl Drop for RestorePlan {
            fn drop(&mut self) {
                record_plan(std::mem::take(&mut self.0));
            }
        }
        let _restore = RestorePlan(recorded_plan());

        let entry = |name: &str, version: &str| Materialized {
            name: name.to_string(),
            version: version.to_string(),
            reason: Reason::Closure,
        };
        record_plan(vec![
            entry("zod", "3.23.8"),
            entry("next", "15.0.0"),
            entry("next", "14.2.0"),
            entry("acorn", "8.12.1"),
        ]);

        let ordered: Vec<_> = recorded_plan().iter().map(Materialized::spec).collect();
        assert_eq!(
            ordered,
            ["acorn@8.12.1", "next@14.2.0", "next@15.0.0", "zod@3.23.8"],
            "sorted by name then version, not by insertion"
        );
    }

    /// Provenance names the surface the reader can act on: the `nub.jsonc` field
    /// they wrote, the file they edited, the variable they exported, or nub's own
    /// default. The last two name no surface at all, because there is none —
    /// they answer the question a bare value leaves open when the layout was
    /// decided by the environment or by something the project merely depends on.
    ///
    /// The seeded triggers additionally render the toolchain's own name, which is
    /// not its package id; a user-configured pattern has no proper name to know,
    /// so it renders as whatever they wrote.
    #[test]
    fn provenance_names_the_authored_surface() {
        assert_eq!(
            Source::Cli("--node-linker=hoisted".to_string()).to_string(),
            "--node-linker=hoisted"
        );
        assert_eq!(
            Source::ProjectConfig("install.publicHoist").to_string(),
            "nub.jsonc install.publicHoist"
        );
        assert_eq!(Source::Npmrc.to_string(), ".npmrc");
        assert_eq!(Source::WorkspaceYaml.to_string(), "pnpm-workspace.yaml");
        assert_eq!(
            Source::GlobalConfigYaml.to_string(),
            "pnpm global config.yaml"
        );
        assert_eq!(Source::Default.to_string(), "default");
        assert_eq!(
            Source::Env("npm_config_node_linker".to_string()).to_string(),
            "npm_config_node_linker"
        );
        assert_eq!(
            Source::Ci.to_string(),
            "global virtual store auto-disabled in CI"
        );
        assert_eq!(
            Source::IncompatiblePackage("next".to_string()).to_string(),
            "global virtual store auto-disabled in Next projects"
        );
        assert_eq!(
            Source::IncompatiblePackage("react-native".to_string()).to_string(),
            "global virtual store auto-disabled in React Native projects"
        );
        assert_eq!(
            Source::IncompatiblePackage("some-local-pkg".to_string()).to_string(),
            "global virtual store auto-disabled in some-local-pkg projects"
        );
    }

    /// Every setting attributable to `nub.jsonc` must map to a field that exists
    /// there. The parenthetical is a pointer, and one aimed at a field the user
    /// cannot find is worse than no pointer at all — `resolve` drops the
    /// attribution entirely rather than name the engine's own key.
    #[test]
    fn project_config_fields_cover_every_lowered_setting() {
        for setting in [
            "nodeLinker",
            "enableGlobalVirtualStore",
            "hoist",
            "hoistPattern",
            "shamefullyHoist",
            "publicHoistPattern",
            "disableGlobalVirtualStoreForPackages",
            "diskMaterializePackages",
            "minimumReleaseAge",
            "minimumReleaseAgeStrict",
            "minimumReleaseAgeExclude",
        ] {
            let field = project_config_field(setting)
                .unwrap_or_else(|| panic!("{setting} has no nub.jsonc field"));
            assert!(field.starts_with("install."), "{setting} → {field}");
        }
        assert_eq!(project_config_field("registry"), None);
    }

    /// The tiers really are walked in the engine's precedence order, and a
    /// setting no readable tier claims yields no row rather than a guess.
    #[test]
    fn resolution_walks_tiers_in_precedence_order() {
        let index = SourceIndex {
            project_config: vec![("nodeLinker".to_string(), "hoisted".to_string())],
            project_npmrc: vec![("node-linker".to_string(), "isolated".to_string())],
            embedder_defaults: vec![("nodeLinker".to_string(), "isolated".to_string())],
            ..empty_index()
        };
        assert_eq!(
            index.resolve("nodeLinker"),
            Some((
                "hoisted".to_string(),
                Some(Source::ProjectConfig("install.linker"))
            ))
        );

        let npmrc_only = SourceIndex {
            project_config: Vec::new(),
            ..index
        };
        assert_eq!(
            npmrc_only.resolve("nodeLinker"),
            Some(("isolated".to_string(), Some(Source::Npmrc)))
        );

        let defaults_only = SourceIndex {
            project_npmrc: Vec::new(),
            ..npmrc_only
        };
        assert_eq!(
            defaults_only.resolve("nodeLinker"),
            Some(("isolated".to_string(), Some(Source::Default)))
        );

        let nothing = SourceIndex {
            embedder_defaults: Vec::new(),
            ..defaults_only
        };
        assert_eq!(nothing.resolve("nodeLinker"), None);
        assert!(
            resolved_rows(&nothing).iter().all(|row| row.note.is_none()),
            "an unattributable value must carry no parenthetical"
        );
    }

    /// Explicit install flags are the report's highest-priority tier. The
    /// engine receives this same bag in `InstallOptions`, so `--node-linker`
    /// must not be attributed to a lower file that it overrode.
    #[test]
    fn cli_flags_outrank_every_file_tier_with_the_canonical_spelling() {
        let index = SourceIndex {
            cli: vec![("node-linker".to_string(), "hoisted".to_string())],
            env: vec![("npm_config_node_linker".to_string(), "isolated".to_string())],
            project_config: vec![("nodeLinker".to_string(), "isolated".to_string())],
            workspace_yaml: vec![("nodeLinker", "isolated".to_string())],
            global_config_yaml: vec![("nodeLinker", "isolated".to_string())],
            project_npmrc: vec![("nodeLinker".to_string(), "isolated".to_string())],
            user_npmrc: vec![("nodeLinker".to_string(), "isolated".to_string())],
            embedder_defaults: vec![("nodeLinker".to_string(), "isolated".to_string())],
            ..empty_index()
        };
        assert_eq!(
            index.resolve("nodeLinker"),
            Some((
                "hoisted".to_string(),
                Some(Source::Cli("--node-linker=hoisted".to_string()))
            ))
        );
    }

    /// pnpm v11's global config sits below the project workspace file but above
    /// either `.npmrc` scope. SourceIndex receives it only from the engine's
    /// `read_pnpm_global_config`-gated loader, so a disabled context leaves this
    /// tier empty rather than inventing a global source.
    #[test]
    fn global_config_yaml_has_the_engine_precedence_tier() {
        let index = SourceIndex {
            global_config_yaml: vec![("nodeLinker", "hoisted".to_string())],
            project_npmrc: vec![("nodeLinker".to_string(), "isolated".to_string())],
            user_npmrc: vec![("nodeLinker".to_string(), "isolated".to_string())],
            ..empty_index()
        };
        assert_eq!(
            index.resolve("nodeLinker"),
            Some(("hoisted".to_string(), Some(Source::GlobalConfigYaml)))
        );
        let project_workspace = SourceIndex {
            workspace_yaml: vec![("nodeLinker", "isolated".to_string())],
            ..index
        };
        assert_eq!(
            project_workspace.resolve("nodeLinker"),
            Some(("isolated".to_string(), Some(Source::WorkspaceYaml)))
        );
    }

    /// pnpm v11's workspace and global YAML list accessors return lists, while
    /// this report stores renderable strings. Their comma representation must
    /// preserve the same winning tier for the hoisting row rather than falling
    /// through to a lower `.npmrc` setting.
    #[test]
    fn pnpm_v11_yaml_list_tiers_preserve_hoisting_provenance() {
        let index = SourceIndex {
            workspace_yaml: vec![("publicHoistPattern", "vitest,@types/*".to_string())],
            global_config_yaml: vec![("publicHoistPattern", "eslint".to_string())],
            project_npmrc: vec![("public-hoist-pattern".to_string(), "lodash".to_string())],
            ..empty_index()
        };
        let hoisting = |index: &SourceIndex| {
            resolved_rows(index)
                .into_iter()
                .find(|row| row.label == "hoisting")
                .unwrap()
        };
        let workspace = hoisting(&index);
        assert_eq!(workspace.values, vec!["vitest", "@types/*"]);
        assert_eq!(workspace.note.as_deref(), Some("(pnpm-workspace.yaml)"));

        let global = hoisting(&SourceIndex {
            workspace_yaml: Vec::new(),
            ..index
        });
        assert_eq!(global.values, vec!["eslint"]);
        assert_eq!(global.note.as_deref(), Some("(pnpm global config.yaml)"));
    }

    /// Exercise the actual pnpm-workspace.yaml reader too: YAML sequences use
    /// the same string-list representation as the resolver, so the report can
    /// both show every pattern and keep its source rather than falling through.
    #[test]
    fn pnpm_v11_workspace_yaml_sequences_keep_list_provenance() {
        let _guard = EngineGuard::take();
        aube_util::update_engine_context(|context| {
            context.read_branded_pnpm_config = true;
            context.read_layout_from_workspace_yaml = true;
            context.read_pnpm_global_config = false;
        });
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            project.path().join("package.json"),
            r#"{"name":"app","version":"1.0.0"}"#,
        )
        .unwrap();
        std::fs::write(
            project.path().join("pnpm-workspace.yaml"),
            "publicHoistPattern:\n  - vitest\n  - '@types/*'\n",
        )
        .unwrap();

        let index = SourceIndex::load(project.path(), &[]);
        assert_eq!(
            index.resolve("publicHoistPattern"),
            Some(("vitest,@types/*".to_string(), Some(Source::WorkspaceYaml)))
        );
        let hoisting = resolved_rows(&index)
            .into_iter()
            .find(|row| row.label == "hoisting")
            .unwrap();
        assert_eq!(hoisting.values, vec!["vitest", "@types/*"]);
        assert_eq!(hoisting.note.as_deref(), Some("(pnpm-workspace.yaml)"));
    }

    /// Within either `.npmrc` scope, the last assignment wins. This is distinct
    /// from scope precedence: project still outranks user after each scope has
    /// selected its own final entry.
    #[test]
    fn npmrc_tiers_use_later_entry_wins_order() {
        let index = SourceIndex {
            project_npmrc: vec![
                ("nodeLinker".to_string(), "hoisted".to_string()),
                ("node-linker".to_string(), "isolated".to_string()),
            ],
            user_npmrc: vec![
                ("nodeLinker".to_string(), "isolated".to_string()),
                ("node-linker".to_string(), "hoisted".to_string()),
            ],
            ..empty_index()
        };
        assert_eq!(
            index.resolve("nodeLinker"),
            Some(("isolated".to_string(), Some(Source::Npmrc)))
        );
        let user_only = SourceIndex {
            project_npmrc: Vec::new(),
            ..index
        };
        assert_eq!(
            user_only.resolve("nodeLinker"),
            Some(("hoisted".to_string(), Some(Source::Npmrc)))
        );
    }

    /// Boolean parsing is part of precedence: malformed values are ignored,
    /// allowing a lower tier (or an earlier valid entry in the same `.npmrc`)
    /// to decide just as the generated resolver does.
    #[test]
    fn malformed_boolean_tiers_fall_through_to_the_resolver_winner() {
        let index = SourceIndex {
            cli: vec![("enable-global-virtual-store".to_string(), "yes".to_string())],
            env: vec![(
                "npm_config_enable_global_virtual_store".to_string(),
                "0".to_string(),
            )],
            project_npmrc: vec![
                ("enable-global-virtual-store".to_string(), "1".to_string()),
                ("enableGlobalVirtualStore".to_string(), "yes".to_string()),
            ],
            ..empty_index()
        };
        assert_eq!(
            index.resolve("enableGlobalVirtualStore"),
            Some((
                "false".to_string(),
                Some(Source::Env(
                    "npm_config_enable_global_virtual_store".to_string()
                ))
            )),
            "an invalid CLI boolean falls through to the valid env tier"
        );

        let no_env = SourceIndex {
            env: Vec::new(),
            ..index
        };
        assert_eq!(
            no_env.resolve("enableGlobalVirtualStore"),
            Some(("true".to_string(), Some(Source::Npmrc))),
            "the later malformed .npmrc assignment cannot mask its earlier valid value"
        );
    }

    /// A branded `AUBE_*` variable is not a source under nub: the profile turns
    /// that alias family off, so crediting one would name a variable the engine
    /// never read.
    #[test]
    fn branded_env_aliases_are_not_a_source() {
        let index = SourceIndex {
            env: vec![("AUBE_NODE_LINKER".to_string(), "hoisted".to_string())],
            embedder_defaults: vec![("nodeLinker".to_string(), "isolated".to_string())],
            ..empty_index()
        };
        assert_eq!(
            index.resolve("nodeLinker"),
            Some(("isolated".to_string(), Some(Source::Default)))
        );
    }

    /// The same aube gate that skips pnpm-branded aliases under a non-pnpm
    /// incumbent must govern this provenance pass; otherwise the report names a
    /// value the resolver did not read.
    #[test]
    fn pnpm_branded_env_aliases_follow_the_engine_context_gate() {
        let _guard = EngineGuard::take();
        let index = SourceIndex {
            env: vec![("PNPM_CONFIG_NODE_LINKER".to_string(), "hoisted".to_string())],
            embedder_defaults: vec![("nodeLinker".to_string(), "isolated".to_string())],
            ..empty_index()
        };

        aube_util::update_engine_context(|context| context.read_branded_pnpm_config = false);
        assert_eq!(
            index.resolve("nodeLinker"),
            Some(("isolated".to_string(), Some(Source::Default)))
        );

        aube_util::update_engine_context(|context| context.read_branded_pnpm_config = true);
        assert_eq!(
            index.resolve("nodeLinker"),
            Some((
                "hoisted".to_string(),
                Some(Source::Env("PNPM_CONFIG_NODE_LINKER".to_string()))
            ))
        );
    }

    /// The resolution row carries a parenthetical only while every entry on it
    /// agrees on where it came from.
    #[test]
    fn mixed_sources_drop_the_shared_parenthetical() {
        let index = SourceIndex {
            project_npmrc: vec![("auto-install-peers".to_string(), "false".to_string())],
            user_npmrc: vec![("strict-peer-dependencies".to_string(), "true".to_string())],
            ..empty_index()
        };
        let rows = resolved_rows(&index);
        let resolution = rows.iter().find(|row| row.label == "resolution").unwrap();
        assert_eq!(
            resolution.values,
            vec!["auto-install-peers=false", "strict-peer-dependencies"]
        );
        // Both landed in `.npmrc`, user and project scope alike, so the row can
        // still name it.
        assert_eq!(resolution.note.as_deref(), Some("(.npmrc)"));

        let mixed = SourceIndex {
            env: vec![(
                "npm_config_auto_install_peers".to_string(),
                "false".to_string(),
            )],
            ..index
        };
        let rows = resolved_rows(&mixed);
        let resolution = rows.iter().find(|row| row.label == "resolution").unwrap();
        assert_eq!(resolution.note, None);
    }
}
