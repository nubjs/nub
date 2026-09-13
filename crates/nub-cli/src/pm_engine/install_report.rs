//! What `nub install` says about itself: the resolved-layout header printed
//! before the engine runs, and the materialization digest printed after linking.
//!
//! Both are two-column blocks — dim label, bright value, dim parenthetical
//! naming where the value came from. They bracket the engine's own progress
//! display, so the order on screen is header → spinner → digest → the engine's
//! success line, which stays last.
//!
//! PROVENANCE IS EXACT, NOT INFERRED. Under nub's own identity the chain that
//! can supply a value is short, and every tier in it is readable from here:
//! explicit install CLI flags → `npm_config_*` env → project config
//! (`nub.jsonc`) → `pnpm-workspace.yaml` (non-empty only when pnpm is the
//! incumbent) → pnpm's global `config.yaml` (pnpm 11+ incumbents only) →
//! project `.npmrc` → user `.npmrc` → nub's own defaults. Every tier is read
//! through the SAME nub-side code the install itself resolves settings with
//! ([`super::host_settings`], [`super::config_read`]), so the report cannot
//! describe a chain the install does not walk. Anything this walk cannot read
//! is reported as nothing at all: a setting with no readable source is dropped
//! from the block rather than printed with a guessed value or a guessed origin,
//! because a wrong provenance is worse than none.

use std::fmt;
use std::path::Path;
use std::sync::RwLock;

use clx::style;
use nub_settings::meta as settings_meta;
use nub_settings::meta::SettingMeta;

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

/// A raw setting string as a boolean, or `None` when the install would not
/// read one from it.
///
/// The tiers this module reads are raw text — an `.npmrc` line or an env var,
/// unparsed — so the report has to apply the same rule the settings layer does
/// when it types that text. That rule is the literal `true`/`false` pair and
/// nothing else: `host_settings::typed` offers `serde_json::Value::Bool` only
/// for those two spellings, and the engine's own `.npmrc` reader agrees, so
/// `hoist=1` is not a truthy hoist but a value the install REFUSES by name.
/// Accepting `1`/`yes`/`TRUE` here would print a layout no install can produce.
fn parse_bool(raw: &str) -> Option<bool> {
    match raw.trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// Whether a raw setting string is the install's idea of true. Unreadable text
/// is not true — but it is not false either, which is why the tier walk asks
/// [`parse_bool`] rather than this when deciding whether a tier claims a value
/// at all.
fn is_true(raw: &str) -> bool {
    parse_bool(raw).unwrap_or(false)
}

/// Render a raw scalar only when the settings layer can read it for this
/// setting. A boolean tier whose text is not `true`/`false` is skipped rather
/// than rendered: the install would refuse that line outright, so printing the
/// value would claim it took one.
fn readable_value(meta: &SettingMeta, raw: &str) -> Option<String> {
    if meta.type_ == "bool" {
        return parse_bool(raw).map(|value| value.to_string());
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

/// The toolchain's own name for the package that triggered the opt-out. The nub
/// seeds are frameworks whose proper names are not their package ids, and the
/// row is prose the reader is meant to recognize — "react-native projects"
/// reads as a typo for the thing they actually use. Anything else is a pattern
/// someone configured themselves, where their own spelling is the right answer.
fn toolchain_display_name(package: &str) -> &str {
    match package {
        "next" => "Next",
        "react-native" => "React Native",
        "remix" => "Remix",
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
    /// `npm_config_*` variables that name a setting: the VARIABLE, the setting
    /// it names, and its raw text. The variable is carried because it is what
    /// the provenance parenthetical prints, and the settings layer's own
    /// environment tier has already collapsed the two spellings of each one.
    env: Vec<(String, String, String)>,
    project_config: Vec<(String, String)>,
    /// What a `pnpm-workspace.yaml` supplies, keyed by the YAML key as written.
    /// Layout keys never appear: the same reader the `nub config` surface uses
    /// drops them, which is the whole layout axis.
    workspace_yaml: Vec<(String, String)>,
    /// The same, from pnpm's global `config.yaml` — non-empty only under a
    /// pnpm 11+ incumbent, the first major that keeps settings there.
    global_config_yaml: Vec<(String, String)>,
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
    /// `node_modules` layout — a request Nub no longer honors from there.
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
    /// Whether the engine this run uses applies the two whole-install store
    /// opt-outs the layout row can otherwise DERIVE: a declared package
    /// matching `disableGlobalVirtualStoreForPackages`, and the injected-deps
    /// `hoist=true` veto.
    ///
    /// Both belong to the vendored engine. pnpm 12 declares neither setting as
    /// a field of the settings struct nub hands it, and nub's own settings
    /// layer pushes neither value, so under that engine nothing but an
    /// explicit `enableGlobalVirtualStore` or CI takes the store project-local
    /// — and a row naming Next as the reason would describe a tree the install
    /// does not build. Snapshotted for the same reason `ci` is: it keeps the
    /// layout decision a pure function of this struct rather than of the
    /// ambient engine selection, which no test could then vary.
    /// The declared framework whose resolver cannot reach a shared store, when
    /// this engine is the one that has to decide that itself. `None` under the
    /// vendored engine, which is handed the candidate names as a setting and
    /// matches them against [`Self::declared_packages`] instead — the two
    /// routes read one definition ([`super::store_locality_breaker`]), so the
    /// row cannot name a framework the install ignored, or stay silent about
    /// one it acted on.
    store_locality_breaker: Option<&'static str>,
    derives_store_optouts: bool,
}

impl SourceIndex {
    pub(super) fn load(cwd: &Path, cli: &[(String, String)]) -> Self {
        let root = super::host_settings::workspace_root(cwd);
        // Split by PATH rather than by position: `npmrc_files` drops a file
        // that does not exist, so a project with no user `.npmrc` would
        // otherwise have its own file read as the user scope and lose to
        // nothing.
        let user_path = super::config_read::user_npmrc_path();
        let (mut project_npmrc, mut user_npmrc) = (Vec::new(), Vec::new());
        for (path, text) in super::host_settings::npmrc_files(&root) {
            let entries = super::config_read::entries_of(&text);
            if user_path.as_deref() == Some(path.as_path()) {
                user_npmrc = entries;
            } else {
                project_npmrc = entries;
            }
        }
        let install = crate::project_config::load_project_config(cwd)
            .ok()
            .flatten()
            .map(|loaded| loaded.values.install)
            .unwrap_or_default();
        // The install's OWN lowering, not a second reading of `nub.jsonc`:
        // `install.linker: "global"` is `nodeLinker` plus
        // `enableGlobalVirtualStore` in exactly one place, and a copy here
        // could disagree with the tree the install builds.
        let project_config = super::host_settings::supplied_settings(&install)
            .into_iter()
            .filter_map(|(key, value)| Some((key, super::config_read::render(value)?)))
            .collect();
        // The engine is the only engine, so the report never derives these:
        // the vendored engine was the one that needed them derived. The field
        // stays because the rendering below still branches on it and both
        // arms are unit-tested.
        let derives_store_optouts = false;
        Self {
            cli: cli.to_vec(),
            env: super::host_settings::env_settings_sourced(),
            project_config,
            workspace_yaml: super::config_read::branded_yaml(
                &root,
                super::config_read::BrandedSource::WorkspaceYaml,
            ),
            global_config_yaml: super::config_read::branded_yaml(
                &root,
                super::config_read::BrandedSource::GlobalConfig,
            ),
            project_npmrc,
            user_npmrc,
            embedder_defaults: super::nub_config_defaults(cwd),
            declared_packages: if derives_store_optouts {
                declared_packages(&root)
            } else {
                Vec::new()
            },
            branded_layout_ignored: branded_layout_ignored(
                cwd,
                super::config_read::branded_yaml_layout_dropped(&root),
            ),
            ci: std::env::var_os("CI").is_some(),
            store_locality_breaker: (!derives_store_optouts)
                .then(|| super::store_locality_breaker(&root, &super::workspace_members(&root)))
                .flatten(),
            derives_store_optouts,
        }
    }

    /// The value in effect for `setting`, and the tier that supplied it when
    /// this index can name one. `None` means no readable tier claims the setting
    /// — or one claims it in a shape this index cannot render, reported the same
    /// way, since a value it cannot read is a value it must not print.
    pub(super) fn resolve(&self, setting: &str) -> Option<(String, Option<Source>)> {
        let meta = settings_meta::find(setting)?;
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
        // Last variable in environment order wins, which is the order the
        // settings layer lifts them in. Matched on the SETTING the layer
        // resolved rather than on `meta.env_vars`: only an `npm_config_`
        // prefix reaches this tier, in either case, so the brand-prefixed
        // aliases the table still lists (`AUBE_*`) are excluded structurally
        // — crediting one would name a variable no install consulted.
        if let Some((var, _, raw)) = self
            .env
            .iter()
            .rev()
            .find(|(_, named, _)| named == meta.name)
            && let Some(value) = readable_value(meta, raw)
        {
            return Some((value, Some(Source::Env(var.clone()))));
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
            if let Some((_, raw)) = entries
                .iter()
                .find(|(key, _)| meta.workspace_yaml_keys.contains(&key.as_str()))
                && let Some(value) = readable_value(meta, raw)
            {
                return Some((value, Some(source)));
            }
        }
        for entries in [&self.project_npmrc, &self.user_npmrc] {
            if let Some(value) = entries.iter().rev().find_map(|(key, raw)| {
                npmrc_key_names(meta, key)
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

/// Whether an `.npmrc` key spells `meta`.
///
/// Camel-casing is the rule the settings layer itself applies — `.npmrc` keys
/// arrive kebab-cased and `host_settings::lift` camel-cases each one before
/// looking it up — so `node-linker` and `nodeLinker` are one key there and must
/// be one key here. The table's own alias list is consulted too, for the
/// settings whose `.npmrc` spelling is not a case transform of their name.
fn npmrc_key_names(meta: &SettingMeta, key: &str) -> bool {
    meta.npmrc_keys.contains(&key) || pnpm_config::naming_cases::to_camel_case(key) == meta.name
}

/// npm's layout keys live in `.npmrc`, which Nub reads under every incumbent.
/// An explicit `install-strategy` always requests a layout; the deprecated
/// boolean forms request one only when enabled.
fn npm_layout_key_present(cwd: &Path) -> bool {
    // `install-strategy` discloses on any value: npm's own default is `hoisted`,
    // which nub also does not install, so writing it down is still a request nub
    // declines. The two deprecated booleans only count when truthy — `false` is
    // npm's default and asks for nothing.
    super::unsupported_config::npmrc_scalar_value(cwd, "install-strategy", false).is_some()
        || ["global-style", "legacy-bundling"].iter().any(|key| {
            super::unsupported_config::npmrc_scalar_value(cwd, key, false).is_some_and(|value| {
                let value = value.trim();
                value.is_empty() || value.eq_ignore_ascii_case("true")
            })
        })
}

/// Whether a branded config file Nub reads for this project asks for a
/// `node_modules` layout that Nub does not take from that source. The install
/// header is the only place that ignored request can surface.
///
/// Two sources, and there used to be four. The pnpm check covers the project
/// `pnpm-workspace.yaml` and the global `config.yaml`; npm's keys need no gate
/// because they live in the neutral `.npmrc` cascade. The yarn and bun arms are
/// gone with the postures that fed them: Nub reads yarn and bun configuration
/// for NO setting now, so a `nodeLinker` in `.yarnrc.yml` is not a layout
/// request Nub declined but a file Nub never opened — and sending the reader to
/// `nub.jsonc` over it would explain the wrong thing.
fn branded_layout_ignored(cwd: &Path, in_pnpm_yaml: bool) -> bool {
    in_pnpm_yaml || npm_layout_key_present(cwd)
}

/// Every package name declared by the root manifest and by each workspace
/// member — the importer set the whole-install store opt-out is matched
/// against.
///
/// Discovered through nub's own workspace walk, which reads the neutral
/// `workspaces` field and falls back to `pnpm-workspace.yaml` only under a
/// pnpm incumbent. That gate is the point: a member list assembled from a
/// branded file nub does not otherwise read would answer with packages no
/// install of this project resolves.
fn declared_packages(root: &Path) -> Vec<String> {
    read_manifest(&root.join("package.json"))
        .into_iter()
        .chain(
            nub_core::workspace::filter::discover_members(root)
                .into_iter()
                .map(|member| member.manifest),
        )
        .flat_map(|manifest| {
            ["dependencies", "devDependencies", "optionalDependencies"]
                .into_iter()
                .filter_map(|field| manifest.get(field)?.as_object())
                .flat_map(|deps| deps.keys().cloned())
                .collect::<Vec<_>>()
        })
        .collect()
}

fn read_manifest(path: &Path) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(nub_core::strip_utf8_bom(&text)).ok()
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
/// a string enum, so compare as booleans when both sides parse that way and
/// fall back to text otherwise — which is what keeps a table default spelled
/// `"true"` comparable with a resolved value that reached here as text.
fn at_default(value: &str, default: &str) -> bool {
    match (parse_bool(value), parse_bool(default)) {
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
/// not. A CI environment is derived from the `CI` variable where the engine
/// plans the store, so the only way to report it is to ask the same question.
/// And a declared package on `disableGlobalVirtualStoreForPackages` — the
/// vendored engine seeds `next` and `react-native`, so a stock Next.js project
/// with no config at all takes this route — is a fact about the MANIFEST, not
/// about any setting: it reads as unset here while the engine turns it into a
/// whole-install opt-out. Those two get their own [`Source`] variants rather
/// than no parenthetical at all: they are precisely the layouts a reader cannot
/// account for by opening their config, so leaving them bare showed a value
/// that contradicts the documented default with nothing to explain it.
///
/// The last two routes are the VENDORED engine's alone, which is what
/// [`SourceIndex::derives_store_optouts`] gates — see that field.
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
    if index.derives_store_optouts {
        if let Some((_, source)) = index.resolve("hoist").filter(|(hoist, _)| is_true(hoist)) {
            return isolated(source);
        }
        if let Some(name) = gvs_incompatible_package(index) {
            return isolated(Some(Source::IncompatiblePackage(name)));
        }
    } else if let Some(name) = &index.store_locality_breaker {
        // The same opt-out under the other engine, reached through the
        // predicate rather than the setting. There is no
        // `disableGlobalVirtualStoreForPackages` for this engine to read, so
        // the match happens in nub and the install acts on the answer
        // directly ([`super::host_settings`]); a report that still asked the
        // setting would say `global-virtual-store` for exactly the projects
        // the install now keeps project-local.
        return isolated(Some(Source::IncompatiblePackage((*name).to_string())));
    }
    ("global-virtual-store".to_string(), None)
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
const LAYOUT_POINTER: &str =
    "configurable via nub.jsonc install.linker, .npmrc node-linker, or --node-linker";

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

/// The setting flags a command line carries, in the bag shape [`SourceIndex`]
/// resolves against: the flag's own kebab spelling without its dashes, and the
/// value it was given.
///
/// Scanned here because the pnpm engine owns the grammar and hands the parse
/// back to nobody — where the vendored engine builds the same bag from its own
/// parsed args. Only a flag the settings table DECLARES is admitted, so a host
/// flag, a value, or a positional can never be read as one; a spelling the
/// table does not carry is omitted rather than guessed, which is the rule
/// `resolve` already applies to a bag key it cannot attribute exactly.
pub(super) fn cli_setting_flags(argv: &[std::ffi::OsString]) -> Vec<(String, String)> {
    let declared = |flag: &str| settings_meta::all().find(|meta| meta.cli_flags.contains(&flag));
    let mut out = Vec::new();
    let mut args = argv.iter().filter_map(|arg| arg.to_str()).peekable();
    while let Some(arg) = args.next() {
        let Some(body) = arg.strip_prefix("--") else {
            continue;
        };
        let (flag, inline) = match body.split_once('=') {
            Some((flag, value)) => (flag, Some(value)),
            None => (body, None),
        };
        // `--no-<flag>` is the engine's negation of a boolean, and it names no
        // flag of its own, so it is resolved against the positive spelling.
        let (flag, negated) = match flag.strip_prefix("no-") {
            Some(positive) if declared(positive).is_some() => (positive, true),
            _ => (flag, false),
        };
        let Some(meta) = declared(flag) else {
            continue;
        };
        let value = match (inline, meta.type_, negated) {
            (Some(value), _, _) => value.to_string(),
            (None, "bool", negated) => (!negated).to_string(),
            // A non-boolean takes the next token, and only when there is one
            // that is not itself a flag: `nub install --node-linker` alone is a
            // command line the engine refuses, not a layout request.
            (None, _, _) => match args.peek().filter(|next| !next.starts_with('-')) {
                Some(_) => args.next().unwrap_or_default().to_string(),
                None => continue,
            },
        };
        out.push((flag.to_string(), value));
    }
    out
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
    ///
    /// `derives_store_optouts` is true here so every layout arm stays
    /// exercisable; the one test about the engine that DOESN'T derive them
    /// turns it off explicitly, which is what makes the difference legible.
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
            store_locality_breaker: None,
            branded_layout_ignored: false,
            ci: false,
            derives_store_optouts: true,
        }
    }

    /// A tier keyed by setting name — `nub.jsonc`, the defaults — from string
    /// literals.
    fn named(entries: &[(&str, &str)]) -> Vec<(String, String)> {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    /// The environment tier as `load` builds it: every `npm_config_*` variable
    /// that names a setting, carrying the variable, the setting and the text.
    fn env(entries: &[(&str, &str, &str)]) -> Vec<(String, String, String)> {
        entries
            .iter()
            .map(|(var, setting, value)| {
                (
                    (*var).to_string(),
                    (*setting).to_string(),
                    (*value).to_string(),
                )
            })
            .collect()
    }

    fn engine_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::pm_engine::ENGINE_GLOBAL_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner())
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

    /// The tiers this module reads are raw text, and the boolean vocabulary the
    /// install types that text with is the literal `true`/`false` pair.
    ///
    /// CONTRACT CHANGE. This row previously asserted the opposite for `1`,
    /// `TRUE` and `True` — the vendored engine's `parse_bool` accepted all
    /// three, and the header had to match it or print the opposite of the tree
    /// the install would build. The settings layer nub hands the pnpm engine
    /// types an `.npmrc` scalar through `serde`, which reads a boolean from
    /// `true`/`false` alone, and pnpm's own reader agrees. So `hoist=1` is not
    /// a truthy hoist: it is a line the install REFUSES by name. The rule the
    /// header has to reproduce moved, so the assertions moved with it.
    #[test]
    fn boolean_settings_are_read_the_way_the_install_reads_them() {
        let with_npmrc = |key: &str, value: &str| SourceIndex {
            project_npmrc: vec![(key.to_string(), value.to_string())],
            embedder_defaults: named(&[("nodeLinker", "isolated")]),
            ..empty_index()
        };

        for spelling in ["true", "TRUE", "True", "1", "yes"] {
            let index = with_npmrc("enableGlobalVirtualStore", spelling);
            let expected = spelling == "true";
            assert_eq!(
                index.resolve("enableGlobalVirtualStore").is_some(),
                expected,
                "only the literal `true` types as a boolean; `{spelling}` does not"
            );
            // Unreadable is not the same as false: the tier is SKIPPED, so the
            // layout falls through to what nothing-set yields rather than being
            // credited a value the install never took.
            assert_eq!(layout_row(&index).0, "global-virtual-store");
        }

        // The other direction, and the one that used to read backwards: a
        // `hoist` the install refuses must not veto the shared store here.
        assert_eq!(
            layout_row(&with_npmrc("hoist", "1")).0,
            "global-virtual-store",
            "`hoist=1` is a value the install refuses, not a hidden-tree request"
        );
        assert_eq!(
            layout_row(&with_npmrc("hoist", "true")).0,
            "isolated",
            "`hoist=true` still vetoes the shared store"
        );

        // `virtualStoreOnly` suppresses the package opt-out engine-side, and
        // only its literal spelling does.
        let store_only = |value: &str| SourceIndex {
            embedder_defaults: named(&[
                ("nodeLinker", "isolated"),
                ("disableGlobalVirtualStoreForPackages", "next"),
            ]),
            declared_packages: vec!["next".to_string()],
            ..with_npmrc("virtualStoreOnly", value)
        };
        assert_eq!(layout_row(&store_only("true")).0, "global-virtual-store");
        assert_eq!(
            layout_row(&store_only("1")).0,
            "isolated",
            "an unreadable `virtualStoreOnly` suppresses nothing"
        );
    }

    /// The two whole-install store opt-outs the layout row DERIVES FROM
    /// SETTINGS belong to the vendored engine alone. pnpm 12 declares neither
    /// setting as a field of the settings struct nub hands it, so a row reading
    /// them under that engine would answer from values nothing consumes.
    ///
    /// The framework opt-out itself did not go away with the setting — it moved.
    /// Nub does the matching now and the install acts on the answer, so the row
    /// reads the same verdict the install did
    /// ([`SourceIndex::store_locality_breaker`]) rather than re-deriving it
    /// from a setting. What the engine arm below pins is that the SETTING is
    /// inert there, not that the behaviour is.
    #[test]
    fn the_derived_store_optouts_are_the_vendored_engines_alone() {
        let vendored = SourceIndex {
            embedder_defaults: named(&[
                ("nodeLinker", "isolated"),
                ("disableGlobalVirtualStoreForPackages", "next"),
                ("hoist", "true"),
            ]),
            declared_packages: vec!["next".to_string()],
            ..empty_index()
        };
        assert_eq!(layout_row(&vendored).0, "isolated");

        let engine = SourceIndex {
            derives_store_optouts: false,
            ..vendored
        };
        assert_eq!(
            layout_row(&engine),
            ("global-virtual-store".to_string(), None),
            "neither the seeded package nor the injected-deps hoist decides \
             anything under the engine that carries neither setting"
        );

        // The two routes that survive, because the engine really does take
        // them: an explicit store bit, and CI.
        let explicit = SourceIndex {
            project_npmrc: named(&[("enable-global-virtual-store", "false")]),
            ..engine
        };
        assert_eq!(
            layout_row(&explicit),
            ("isolated".to_string(), Some(Source::Npmrc))
        );

        let in_ci = SourceIndex {
            project_npmrc: Vec::new(),
            ci: true,
            ..explicit
        };
        assert_eq!(
            layout_row(&in_ci),
            ("isolated".to_string(), Some(Source::Ci))
        );

        // And the route the framework opt-out takes NOW: the same declared
        // `next`, with the verdict already resolved for the install rather
        // than read off a setting. The row follows the install.
        let resolved = SourceIndex {
            derives_store_optouts: false,
            store_locality_breaker: Some("next"),
            embedder_defaults: named(&[("nodeLinker", "isolated")]),
            ..empty_index()
        };
        assert_eq!(
            layout_row(&resolved),
            (
                "isolated".to_string(),
                Some(Source::IncompatiblePackage("next".to_string()))
            ),
            "the row must follow the install's own store decision, and name \
             the framework that forced it"
        );
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
            Some(
                "(configurable via nub.jsonc install.linker, .npmrc node-linker, or --node-linker)"
            )
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
            Some(
                "(configurable via nub.jsonc install.linker, .npmrc node-linker, or --node-linker)"
            ),
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
            Some(
                "(configurable via nub.jsonc install.linker, .npmrc node-linker, or --node-linker)"
            ),
            "a dependency-derived layout is nothing the project wrote either"
        );
    }

    /// The branded files layout is taken back from, each detected in the shape
    /// its own tool writes.
    ///
    /// CONTRACT CHANGE. This row used to cover `.yarnrc.yml` and `bunfig.toml`
    /// too, each behind the posture that decided whether nub opened the file.
    /// Nub reads yarn and bun configuration for NO setting now, so those files
    /// are not layout requests nub declined — they are files nub never opened,
    /// and pointing their author at `nub.jsonc install.linker` would explain
    /// the wrong thing. What remains is what nub still reads: pnpm's YAML
    /// under a pnpm incumbent, and npm's keys in the neutral `.npmrc`.
    #[test]
    fn each_branded_layout_source_is_detected() {
        // A pnpm incumbent, because that is the gate on reading the branded
        // YAML at all — without it the file is simply not nub's to read.
        let project = |files: &[(&str, &str)]| {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(
                dir.path().join("package.json"),
                r#"{"name":"app","version":"1.0.0","packageManager":"pnpm@10.4.1"}"#,
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
            // npm's keys live in `.npmrc`, which nub reads under every
            // incumbent. `install-strategy=nested` used to ABORT; dropping that
            // must not trade a loud refusal for silence.
            (".npmrc", "install-strategy=nested\n"),
            (".npmrc", "install-strategy=hoisted\n"),
            (".npmrc", "legacy-bundling=true\n"),
            (".npmrc", "global-style=true\n"),
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
        // npm's default for both booleans. A project that spells out the default
        // has asked for nothing, so disclosing would be noise.
        for body in ["legacy-bundling=false\n", "global-style=false\n"] {
            assert!(
                !detected(&project(&[(".npmrc", body)])),
                "an explicitly-default npm boolean is not a layout request: {body:?}"
            );
        }
        for body in ["legacy-bundling=TRUE\n", "global-style=True\n"] {
            assert!(
                detected(&project(&[(".npmrc", body)])),
                "npm booleans are case-insensitive: {body:?}"
            );
        }
        assert!(
            !detected(&project(&[(".npmrc", "node-linker=hoisted\n")])),
            "the neutral spelling IS read, so it is not a dropped setting"
        );

        // Yarn's and Bun's files, written exactly as their own tools write
        // them, disclose NOTHING — the negative half of the contract change
        // above, and the reason this is asserted rather than merely deleted.
        for (file, body) in [
            (".yarnrc.yml", "nodeLinker: node-modules\n"),
            ("bunfig.toml", "[install]\nlinker = \"hoisted\"\n"),
        ] {
            assert!(
                !detected(&project(&[(file, body)])),
                "nub reads no {file} for any setting, so its layout key is not \
                 a request nub declined: {body:?}"
            );
        }
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
            Source::IncompatiblePackage("remix".to_string()).to_string(),
            "global virtual store auto-disabled in Remix projects"
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
    /// install receives the same flags, so `--node-linker` must not be
    /// attributed to a lower file that it overrode.
    ///
    /// Spelled with `autoInstallPeers` on the YAML tiers rather than
    /// `nodeLinker`: a layout key can no longer reach them at all, and pinning
    /// this test to a combination the loader cannot produce would make it a
    /// test of the struct rather than of precedence.
    #[test]
    fn cli_flags_outrank_every_file_tier_with_the_canonical_spelling() {
        let index = SourceIndex {
            cli: named(&[("node-linker", "hoisted")]),
            env: env(&[("npm_config_node_linker", "nodeLinker", "isolated")]),
            project_config: named(&[("nodeLinker", "isolated")]),
            project_npmrc: named(&[("nodeLinker", "isolated")]),
            user_npmrc: named(&[("nodeLinker", "isolated")]),
            embedder_defaults: named(&[("nodeLinker", "isolated")]),
            ..empty_index()
        };
        assert_eq!(
            index.resolve("nodeLinker"),
            Some((
                "hoisted".to_string(),
                Some(Source::Cli("--node-linker=hoisted".to_string()))
            ))
        );

        let yaml_tiers = SourceIndex {
            cli: named(&[("auto-install-peers", "false")]),
            env: env(&[("npm_config_auto_install_peers", "autoInstallPeers", "true")]),
            project_config: named(&[("autoInstallPeers", "true")]),
            workspace_yaml: named(&[("autoInstallPeers", "true")]),
            global_config_yaml: named(&[("autoInstallPeers", "true")]),
            project_npmrc: named(&[("auto-install-peers", "true")]),
            user_npmrc: named(&[("auto-install-peers", "true")]),
            embedder_defaults: named(&[("autoInstallPeers", "true")]),
            ..empty_index()
        };
        assert_eq!(
            yaml_tiers.resolve("autoInstallPeers"),
            Some((
                "false".to_string(),
                Some(Source::Cli("--auto-install-peers=false".to_string()))
            ))
        );
    }

    /// pnpm v11's global config sits below the project workspace file but above
    /// either `.npmrc` scope. Both YAML tiers arrive only under a pnpm
    /// incumbent, so a nub-identity project leaves them empty rather than
    /// inventing a branded source.
    #[test]
    fn global_config_yaml_has_the_engine_precedence_tier() {
        let index = SourceIndex {
            global_config_yaml: named(&[("autoInstallPeers", "false")]),
            project_npmrc: named(&[("auto-install-peers", "true")]),
            user_npmrc: named(&[("auto-install-peers", "true")]),
            ..empty_index()
        };
        assert_eq!(
            index.resolve("autoInstallPeers"),
            Some(("false".to_string(), Some(Source::GlobalConfigYaml)))
        );
        let project_workspace = SourceIndex {
            workspace_yaml: named(&[("autoInstallPeers", "true")]),
            ..index
        };
        assert_eq!(
            project_workspace.resolve("autoInstallPeers"),
            Some(("true".to_string(), Some(Source::WorkspaceYaml)))
        );
    }

    /// Run the real `pnpm-workspace.yaml` reader against the two shapes it has
    /// to tell apart, on one file: a resolution setting it supplies, and a
    /// hoisting pattern it does not.
    ///
    /// CONTRACT CHANGE. This row used to assert the opposite of its second
    /// half — a YAML `publicHoistPattern` reaching the hoisting row with
    /// `(pnpm-workspace.yaml)` beside it — because the vendored engine's reader
    /// was asked for it with the layout suppression turned OFF. Every hoisting
    /// setting is `layout`-flagged, and nub takes layout from `nub.jsonc`,
    /// `.npmrc` or the command line alone, so NO hoisting row can ever carry a
    /// branded-YAML source. What the file can still supply is the resolution
    /// row, which is what this now pins — along with the disclosure that the
    /// dropped key earns.
    ///
    /// The manifest declares pnpm because that incumbency is the only thing
    /// that makes the file nub's to read at all: under nub's own identity the
    /// tier is empty by design, and a fixture without the declaration would
    /// assert a reader works while it reads nothing.
    #[test]
    fn a_pnpm_workspace_yaml_supplies_resolution_and_never_hoisting() {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            project.path().join("package.json"),
            r#"{"name":"app","version":"1.0.0","packageManager":"pnpm@11.3.0"}"#,
        )
        .unwrap();
        std::fs::write(
            project.path().join("pnpm-workspace.yaml"),
            "autoInstallPeers: false\npublicHoistPattern:\n  - vitest\n  - '@types/*'\n",
        )
        .unwrap();

        let index = SourceIndex::load(project.path(), &[]);
        assert_eq!(
            index.resolve("autoInstallPeers"),
            Some(("false".to_string(), Some(Source::WorkspaceYaml)))
        );
        assert_eq!(
            index.resolve("publicHoistPattern"),
            None,
            "a hoisting pattern is a layout setting, which this file never supplies"
        );

        let rows = resolved_rows(&index);
        let resolution = rows.iter().find(|row| row.label == "resolution").unwrap();
        assert_eq!(resolution.values, vec!["auto-install-peers=false"]);
        assert_eq!(resolution.note.as_deref(), Some("(pnpm-workspace.yaml)"));
        assert!(
            rows.iter().all(|row| row.label != "hoisting"),
            "no hoisting row, because nothing nub reads asked for one"
        );
        // The dropped key is not silently gone: the linker row says where a
        // layout CAN be set, which is the whole point of dropping it.
        let linker = rows.iter().find(|row| row.label == "linker").unwrap();
        assert_eq!(
            linker.note.as_deref(),
            Some(&format!("({LAYOUT_POINTER})")[..])
        );
    }

    /// Either YAML tier reaches the resolution ROW with its own name on it, and
    /// a settled multi-entry row still collapses to one parenthetical when both
    /// entries came from the same file.
    #[test]
    fn pnpm_yaml_tiers_preserve_resolution_provenance() {
        let index = SourceIndex {
            workspace_yaml: named(&[
                ("autoInstallPeers", "false"),
                ("strictPeerDependencies", "true"),
            ]),
            project_npmrc: named(&[("auto-install-peers", "true")]),
            ..empty_index()
        };
        let resolution = |index: &SourceIndex| {
            resolved_rows(index)
                .into_iter()
                .find(|row| row.label == "resolution")
                .unwrap()
        };
        let workspace = resolution(&index);
        assert_eq!(
            workspace.values,
            vec!["auto-install-peers=false", "strict-peer-dependencies"]
        );
        assert_eq!(workspace.note.as_deref(), Some("(pnpm-workspace.yaml)"));

        let global = resolution(&SourceIndex {
            workspace_yaml: Vec::new(),
            global_config_yaml: named(&[("autoInstallPeers", "false")]),
            project_npmrc: Vec::new(),
            ..index
        });
        assert_eq!(global.values, vec!["auto-install-peers=false"]);
        assert_eq!(global.note.as_deref(), Some("(pnpm global config.yaml)"));
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

    /// Boolean parsing is part of precedence: a value the install will not type
    /// as a boolean is ignored, letting a lower tier — or an earlier valid entry
    /// in the same `.npmrc` — decide, just as the install's own merge does.
    #[test]
    fn malformed_boolean_tiers_fall_through_to_the_resolver_winner() {
        let index = SourceIndex {
            cli: named(&[("enable-global-virtual-store", "yes")]),
            env: env(&[(
                "npm_config_enable_global_virtual_store",
                "enableGlobalVirtualStore",
                "false",
            )]),
            project_npmrc: named(&[
                ("enable-global-virtual-store", "true"),
                ("enableGlobalVirtualStore", "yes"),
            ]),
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

    /// The parenthetical names the variable the install actually read — not a
    /// spelling picked off the settings table's alias list.
    ///
    /// CONTRACT CHANGE. Two tests used to sit here, one per brand-prefixed
    /// alias family (`AUBE_*`, `PNPM_CONFIG_*`), each flipping a process-global
    /// engine posture to prove the report skipped a variable the resolver
    /// skipped. This walk no longer goes through `meta.env_vars` at all: it
    /// reads the tier the settings layer BUILT, which admits the `npm_config_`
    /// prefix and nothing else, so the skip is structural and the postures that
    /// gated it are gone. The prefix rule is asserted where it lives —
    /// `host_settings::tests::a_brand_prefixed_variable_is_not_a_setting_source`
    /// — and what remains here is this module's own half.
    #[test]
    fn the_environment_tier_names_the_variable_the_install_read() {
        let index = SourceIndex {
            env: env(&[
                ("NPM_CONFIG_NODE_LINKER", "nodeLinker", "isolated"),
                ("npm_config_node_linker", "nodeLinker", "hoisted"),
            ]),
            embedder_defaults: named(&[("nodeLinker", "isolated")]),
            ..empty_index()
        };
        assert_eq!(
            index.resolve("nodeLinker"),
            Some((
                "hoisted".to_string(),
                Some(Source::Env("npm_config_node_linker".to_string()))
            )),
            "two spellings of one setting resolve last-wins, and the winner is named"
        );

        // A variable whose value the install would refuse leaves the tier to a
        // lower one rather than being credited a value nothing took.
        let refused = SourceIndex {
            env: env(&[("npm_config_hoist", "hoist", "1")]),
            embedder_defaults: named(&[("hoist", "true")]),
            ..empty_index()
        };
        assert_eq!(
            refused.resolve("hoist"),
            Some(("true".to_string(), Some(Source::Default)))
        );
    }

    /// The resolution row carries a parenthetical only while every entry on it
    /// agrees on where it came from.
    #[test]
    fn mixed_sources_drop_the_shared_parenthetical() {
        let index = SourceIndex {
            project_npmrc: named(&[("auto-install-peers", "false")]),
            user_npmrc: named(&[("strict-peer-dependencies", "true")]),
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
            env: env(&[("npm_config_auto_install_peers", "autoInstallPeers", "false")]),
            ..index
        };
        let rows = resolved_rows(&mixed);
        let resolution = rows.iter().find(|row| row.label == "resolution").unwrap();
        assert_eq!(resolution.note, None);
    }
}
