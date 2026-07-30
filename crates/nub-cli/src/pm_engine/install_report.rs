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
//! env → project config (`nub.jsonc`) → `pnpm-workspace.yaml` (non-empty only
//! when pnpm is the incumbent) → project `.npmrc` → user `.npmrc` → nub's
//! embedder defaults. The tiers aube would otherwise consult are inert for nub by
//! construction — `config_namespace = None` empties both
//! `.config/aube/config.toml` scopes, and pnpm's global `config.yaml` is cleared
//! unconditionally (see `identity.rs`) — so walking these six in precedence order
//! reproduces the resolver's answer instead of approximating it. Anything this
//! walk cannot read is reported as nothing at all: a setting with no readable
//! source is dropped from the block rather than printed with a guessed value or a
//! guessed origin, because a wrong provenance is worse than none.

use std::path::Path;
use std::sync::{OnceLock, RwLock};

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
    /// An environment variable, named so the reader can find it.
    Env(String),
    /// The project's `nub.jsonc`, with the field the user wrote.
    ProjectConfig(&'static str),
    WorkspaceYaml,
    Npmrc,
    /// nub's own built-in value — nothing in the project asked for it.
    Default,
}

impl Source {
    fn render(&self) -> String {
        match self {
            Source::Env(var) => var.clone(),
            Source::ProjectConfig(field) => format!("nub.jsonc {field}"),
            Source::WorkspaceYaml => "pnpm-workspace.yaml".to_string(),
            Source::Npmrc => ".npmrc".to_string(),
            Source::Default => "default".to_string(),
        }
    }
}

/// The readable settings tiers for one project root, loaded once per install.
pub(super) struct SourceIndex {
    env: Vec<(String, String)>,
    project_config: Vec<(String, String)>,
    /// Settings a `pnpm-workspace.yaml` claims, each with its value when the
    /// setting's type renders as a scalar string. Resolved eagerly at load: the
    /// raw YAML map's element type belongs to aube's yaml crate, which is not a
    /// nub dependency, so it cannot be held in a field here.
    workspace_yaml: Vec<(&'static str, Option<String>)>,
    project_npmrc: Vec<(String, String)>,
    user_npmrc: Vec<(String, String)>,
    embedder_defaults: Vec<(String, String)>,
}

impl SourceIndex {
    pub(super) fn load(cwd: &Path) -> Self {
        let npmrc = aube_registry::config::load_npmrc_entries_split(cwd);
        let raw = aube_manifest::workspace::load_raw(cwd).unwrap_or_default();
        let workspace_yaml = aube_settings::all()
            .iter()
            .filter(|meta| {
                meta.workspace_yaml_keys
                    .iter()
                    .any(|key| aube_settings::workspace_yaml_value(&raw, key).is_some())
            })
            .map(|meta| {
                (
                    meta.name,
                    aube_settings::values::string_from_workspace_yaml(meta.name, &raw),
                )
            })
            .collect();
        Self {
            env: aube_settings::values::capture_env(),
            project_config: aube_util::engine_context().project_config_settings,
            workspace_yaml,
            project_npmrc: npmrc.project,
            user_npmrc: npmrc.user,
            embedder_defaults: aube_settings::embedder_defaults().to_vec(),
        }
    }

    /// The value in effect for `setting`, and the tier that supplied it when
    /// this index can name one. `None` means no readable tier claims the setting
    /// — or one claims it in a shape this index cannot render, reported the same
    /// way, since a value it cannot read is a value it must not print.
    pub(super) fn resolve(&self, setting: &str) -> Option<(String, Option<Source>)> {
        let meta = aube_settings::find(setting)?;
        // `AUBE_*` aliases are excluded deliberately: `read_branded_settings_env`
        // is off under nub, so naming one would credit a variable the engine
        // never read. Later entries win within a tier, matching the resolver.
        if let Some((var, value)) = self
            .env
            .iter()
            .rev()
            .find(|(k, _)| !k.starts_with("AUBE_") && meta.env_vars.contains(&k.as_str()))
        {
            return Some((value.clone(), Some(Source::Env(var.clone()))));
        }
        if let Some((_, value)) = self.project_config.iter().rev().find(|(k, _)| k == setting) {
            return Some((
                value.clone(),
                project_config_field(setting).map(Source::ProjectConfig),
            ));
        }
        if let Some((_, value)) = self
            .workspace_yaml
            .iter()
            .find(|(name, _)| *name == setting)
        {
            return value
                .clone()
                .map(|value| (value, Some(Source::WorkspaceYaml)));
        }
        for entries in [&self.project_npmrc, &self.user_npmrc] {
            if let Some((_, value)) = entries
                .iter()
                .rev()
                .find(|(k, _)| meta.npmrc_keys.contains(&k.as_str()))
            {
                return Some((value.clone(), Some(Source::Npmrc)));
            }
        }
        self.embedder_defaults
            .iter()
            .rev()
            .find(|(k, _)| k == setting)
            .map(|(_, value)| (value.clone(), Some(Source::Default)))
    }
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
struct Piece {
    text: String,
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
            note: source.map(|source| format!("({})", source.render())),
        }
    }
}

/// Render rows as an unruled two-column block: labels left-aligned in a column
/// sized to the widest one, values wrapped to `cols` with a hanging indent that
/// holds every continuation line in the value column.
fn render_block(rows: &[Row], cols: usize) -> String {
    let label_w = rows.iter().map(|row| row.label.len()).max().unwrap_or(0);
    let value_col = INDENT + label_w + GAP;
    // A pathologically narrow terminal must still make progress rather than
    // emit one token per line forever.
    let limit = cols.max(value_col + 20);
    let mut out = String::new();
    for row in rows {
        let mut pieces: Vec<Piece> = row
            .values
            .iter()
            .map(|value| Piece {
                text: value.clone(),
                sep: ", ",
                dim: false,
            })
            .collect();
        if let Some(note) = &row.note {
            pieces.push(Piece {
                text: note.clone(),
                sep: " ",
                dim: true,
            });
        }
        write_row(&mut out, row.label, label_w, value_col, limit, &pieces);
    }
    out
}

fn write_row(
    out: &mut String,
    label: &str,
    label_w: usize,
    value_col: usize,
    limit: usize,
    pieces: &[Piece],
) {
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
        out.push_str(&if piece.dim {
            style::edim(&piece.text).to_string()
        } else {
            piece.text.clone()
        });
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
/// Three things flip it back to a project-local store, and they arrive by
/// different routes — which is why this cannot simply read one setting. An
/// explicit `enableGlobalVirtualStore=false` and the `hoist=true` nub pushes
/// for injected dependencies both land in the settings index. A CI environment
/// does not: `Linker::new` derives it from `is_ci()` at construction, so the
/// only way to report it is to ask the same question aube will.
fn layout_row(index: &SourceIndex) -> (String, Option<Source>) {
    let (linker, linker_source) = index
        .resolve("nodeLinker")
        .unwrap_or_else(|| ("isolated".to_string(), None));
    if linker != "isolated" {
        return (linker, linker_source);
    }
    match index.resolve("enableGlobalVirtualStore") {
        Some((shared, source)) if shared == "true" => ("global-virtual-store".to_string(), source),
        Some((_, source)) => ("isolated".to_string(), source),
        None if aube_util::env::is_ci() => ("isolated".to_string(), None),
        None => match index.resolve("hoist") {
            Some((hoist, source)) if hoist == "true" => ("isolated".to_string(), source),
            _ => ("global-virtual-store".to_string(), None),
        },
    }
}

pub(super) fn resolved_rows(index: &SourceIndex) -> Vec<Row> {
    let mut rows = Vec::new();

    // Always present, even when everything is default: the layout is the one
    // fact that governs how the tree on disk is shaped.
    let (layout, layout_source) = layout_row(index);
    rows.push(Row::new("layout", vec![layout], layout_source));

    // Both pattern lists answer "where can an undeclared import find this", so
    // they share a row. The patterns ARE the answer — an enumerated package list
    // would be unreadable and a count says nothing at all.
    for setting in ["publicHoistPattern", "hoistPattern"] {
        let Some((raw, source)) = index.resolve(setting) else {
            continue;
        };
        let patterns: Vec<String> = raw
            .split(',')
            .map(str::trim)
            .filter(|pattern| !pattern.is_empty())
            .map(str::to_string)
            .collect();
        if patterns.is_empty() {
            continue;
        }
        rows.push(Row::new("hoisting", patterns, source));
    }

    let mut values = Vec::new();
    let mut shared_source = None;
    for (setting, spelling, default) in RESOLUTION_SETTINGS {
        let Some((value, source)) = index.resolve(setting) else {
            continue;
        };
        if value == *default {
            continue;
        }
        values.push(if value == "true" {
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
    if !values.is_empty() {
        rows.push(Row::new("resolution", values, shared_source));
    }
    rows
}

/// Print the resolved layout ahead of the engine's progress display. Silent
/// under `--silent`; otherwise always prints at least the `layout` row, so a
/// default install is one line here and one line at the end.
pub(super) fn print_resolved_layout(cwd: &Path, output: &OutputFlags) {
    if output.is_silent() {
        return;
    }
    let rows = resolved_rows(&SourceIndex::load(cwd));
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
    /// Vite below 8.1 cannot read the shared store's `.modules.yaml`.
    LegacyVite,
    /// Named by `install.linker.eject`, or by nub's own built-in seed.
    Configured,
    /// Imports a package that had to move, so it moves too — otherwise it would
    /// keep resolving the store-resident copy and split the singleton.
    ImporterOf(String),
}

impl Reason {
    fn render(&self) -> String {
        match self {
            Reason::Undeclared(names) => format!("undeclared imports: {}", names.join(", ")),
            Reason::PeerTypes => "peer types resolved from the project root".to_string(),
            Reason::ProjectContext => "build script reads the project".to_string(),
            Reason::LegacyVite => "vite below 8.1".to_string(),
            Reason::Configured => "named by config".to_string(),
            Reason::ImporterOf(spec) => format!("imports {spec}"),
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

static PLAN: OnceLock<RwLock<Vec<Materialized>>> = OnceLock::new();

fn plan() -> &'static RwLock<Vec<Materialized>> {
    PLAN.get_or_init(|| RwLock::new(Vec::new()))
}

/// Record the expansion hook's plan for the digest. Sorted here because the plan
/// is built from hash sets, and an install's output must not reorder run to run.
pub(super) fn record_plan(mut entries: Vec<Materialized>) {
    entries.sort_by(|a, b| (&a.name, &a.version).cmp(&(&b.name, &b.version)));
    match plan().write() {
        Ok(mut slot) => *slot = entries,
        Err(poisoned) => *poisoned.into_inner() = entries,
    }
}

fn recorded_plan() -> Vec<Materialized> {
    match plan().read() {
        Ok(slot) => slot.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
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
                note: Some(format!("({})", entry.reason.render())),
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

    /// The quiet common case: nothing in the project moved a setting, so the
    /// header is the single layout line.
    #[test]
    fn default_install_renders_one_line() {
        let rendered = plain(&render_block(
            &[row("layout", &["isolated"], Some(Source::Default))],
            80,
        ));
        assert_eq!(rendered, "  layout  isolated (default)\n");
    }

    /// Labels share one column and values start at a single hanging indent, so
    /// the block reads as a table without drawing one.
    #[test]
    fn labels_and_values_align_on_one_column() {
        let rendered = plain(&render_block(
            &[
                row(
                    "layout",
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
            "  layout      isolated (nub.jsonc install.linker)\n\
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
            env: Vec::new(),
            project_config: vec![
                ("nodeLinker".to_string(), "isolated".to_string()),
                ("enableGlobalVirtualStore".to_string(), "true".to_string()),
            ],
            workspace_yaml: Vec::new(),
            project_npmrc: Vec::new(),
            user_npmrc: Vec::new(),
            embedder_defaults: Vec::new(),
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
            layout_row(&project_local).0,
            "isolated",
            "an explicit project-local store keeps the plain isolated word"
        );

        // Nothing set the store bit, so the engine's default decides — and that
        // default is the shared store. Reporting the raw `isolated` here named a
        // tree nobody gets: with no config the packages symlink into the
        // machine-global store. Skipped under CI, where `Linker::new` derives
        // the project-local store from the environment instead of a setting.
        let unset = SourceIndex {
            project_config: Vec::new(),
            embedder_defaults: vec![("nodeLinker".to_string(), "isolated".to_string())],
            ..project_local
        };
        if !aube_util::env::is_ci() {
            assert_eq!(
                layout_row(&unset),
                ("global-virtual-store".to_string(), None)
            );
        }

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
    }

    /// Nothing materialized prints nothing: materialization is routine, and a
    /// run without any must not grow a block announcing that.
    #[test]
    fn empty_digest_renders_nothing() {
        assert!(digest_rows(&[], false).is_empty());
        assert!(digest_rows(&[], true).is_empty());
    }

    /// Provenance names the surface the reader can act on: the `nub.jsonc` field
    /// they wrote, the file they edited, the variable they exported, or nub's own
    /// default.
    #[test]
    fn provenance_names_the_authored_surface() {
        assert_eq!(
            Source::ProjectConfig("install.publicHoist").render(),
            "nub.jsonc install.publicHoist"
        );
        assert_eq!(Source::Npmrc.render(), ".npmrc");
        assert_eq!(Source::WorkspaceYaml.render(), "pnpm-workspace.yaml");
        assert_eq!(Source::Default.render(), "default");
        assert_eq!(
            Source::Env("npm_config_node_linker".to_string()).render(),
            "npm_config_node_linker"
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
            env: Vec::new(),
            project_config: vec![("nodeLinker".to_string(), "hoisted".to_string())],
            workspace_yaml: Vec::new(),
            project_npmrc: vec![("node-linker".to_string(), "isolated".to_string())],
            user_npmrc: Vec::new(),
            embedder_defaults: vec![("nodeLinker".to_string(), "isolated".to_string())],
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

    /// A branded `AUBE_*` variable is not a source under nub: the profile turns
    /// that alias family off, so crediting one would name a variable the engine
    /// never read.
    #[test]
    fn branded_env_aliases_are_not_a_source() {
        let index = SourceIndex {
            env: vec![("AUBE_NODE_LINKER".to_string(), "hoisted".to_string())],
            project_config: Vec::new(),
            workspace_yaml: Vec::new(),
            project_npmrc: Vec::new(),
            user_npmrc: Vec::new(),
            embedder_defaults: vec![("nodeLinker".to_string(), "isolated".to_string())],
        };
        assert_eq!(
            index.resolve("nodeLinker"),
            Some(("isolated".to_string(), Some(Source::Default)))
        );
    }

    /// The resolution row carries a parenthetical only while every entry on it
    /// agrees on where it came from.
    #[test]
    fn mixed_sources_drop_the_shared_parenthetical() {
        let index = SourceIndex {
            env: Vec::new(),
            project_config: Vec::new(),
            workspace_yaml: Vec::new(),
            project_npmrc: vec![("auto-install-peers".to_string(), "false".to_string())],
            user_npmrc: vec![("strict-peer-dependencies".to_string(), "true".to_string())],
            embedder_defaults: Vec::new(),
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
