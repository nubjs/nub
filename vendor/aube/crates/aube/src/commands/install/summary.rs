pub(super) fn print_already_up_to_date() {
    if clx::progress::output() == clx::progress::ProgressOutput::Text {
        return;
    }
    use clx::style;
    use std::io::Write;
    // Routed through the shared `aube_prefix_line` helper so this
    // site and `print_install_summary`'s no-op branch can't drift —
    // both produce `aube VERSION by jdx.dev · ✓ Already up to date`.
    let msg = format!(
        "{} {}",
        style::egreen("✓").bold(),
        style::ebold("Already up to date"),
    );
    let line = crate::progress::aube_prefix_line(&msg);
    let _ = writeln!(std::io::stderr(), "{line}");
}

pub(super) fn print_direct_dependency_summary(
    graph: &aube_lockfile::LockfileGraph,
    manifests: &[(String, aube_manifest::PackageJson)],
    direct_dep_info: &std::collections::HashMap<String, aube_resolver::DirectDepInfo>,
) {
    use clx::style;
    let importers: Vec<(&String, &Vec<aube_lockfile::DirectDep>)> = graph
        .importers
        .iter()
        .filter(|(_, deps)| !deps.is_empty())
        .collect();
    if importers.is_empty() {
        return;
    }
    let show_importer_headers = importers.len() > 1;
    for (idx, (importer, deps)) in importers.iter().enumerate() {
        if idx > 0 {
            eprintln!();
        }
        if show_importer_headers {
            let label = direct_dependency_importer_label(importer, manifests);
            eprintln!("{}{}", style::ebold(&label), style::edim(":"));
        }
        print_direct_dependency_section(
            graph,
            deps,
            aube_lockfile::DepType::Production,
            direct_dep_info,
        );
        print_direct_dependency_section(
            graph,
            deps,
            aube_lockfile::DepType::Optional,
            direct_dep_info,
        );
        print_direct_dependency_section(graph, deps, aube_lockfile::DepType::Dev, direct_dep_info);
    }
    eprintln!();
}

fn direct_dependency_importer_label(
    importer: &str,
    manifests: &[(String, aube_manifest::PackageJson)],
) -> String {
    manifests
        .iter()
        .find(|(path, _)| path == importer)
        .and_then(|(_, manifest)| manifest.name.clone())
        .unwrap_or_else(|| importer.to_string())
}

pub(super) fn should_print_human_install_summary() -> bool {
    let flags = super::super::global_output_flags();
    !flags.silent && !flags.ndjson
}

fn print_direct_dependency_section(
    graph: &aube_lockfile::LockfileGraph,
    deps: &[aube_lockfile::DirectDep],
    dep_type: aube_lockfile::DepType,
    direct_dep_info: &std::collections::HashMap<String, aube_resolver::DirectDepInfo>,
) {
    use clx::style;
    let mut deps: Vec<&aube_lockfile::DirectDep> =
        deps.iter().filter(|dep| dep.dep_type == dep_type).collect();
    if deps.is_empty() {
        return;
    }
    deps.sort_by(|a, b| a.name.cmp(&b.name));
    let label = aube_lockfile::dep_type_label(dep_type);
    // Resolve each dep's printable version up front so a section that turns
    // out to hold only workspace-linked deps (no registry version to show)
    // prints no header at all, matching pnpm — which omits workspace deps
    // from the install summary entirely.
    let rendered: Vec<(&aube_lockfile::DirectDep, &str)> = deps
        .iter()
        .filter_map(|dep| {
            // A `workspace:` / `link:` dep resolves to a local importer, not a
            // registry package, so it has no entry in `graph.packages`. pnpm
            // leaves these out of the summary rather than printing a versionless
            // `+ pkg@?` line, so skip them here too.
            graph
                .get_package(&dep.dep_path)
                .map(|pkg| (*dep, pkg.version.as_str()))
        })
        .collect();
    if rendered.is_empty() {
        return;
    }
    eprintln!("{}{}", style::ebold(label), style::edim(":"));
    for (dep, version) in rendered {
        let badges = render_direct_dep_badges(direct_dep_info.get(&dep.dep_path));
        eprintln!(
            "{} {}{}{}",
            style::egreen("+").bold(),
            dep.name,
            style::edim(format!("@{version}")),
            badges,
        );
    }
}

/// Render the trailing badge column for a direct-dep line. Empty string
/// when there's nothing to flag, otherwise a leading two-space gap and
/// one or more dim-separated tags (`deprecated`, `latest 2.0.0`). The
/// caller passes `direct_dep_info.get(dep_path)`, and `direct_dep_info`
/// only carries entries with at least one signal set — so when `info`
/// is `Some(...)`, `parts` is guaranteed non-empty.
fn render_direct_dep_badges(info: Option<&aube_resolver::DirectDepInfo>) -> String {
    use clx::style;
    let Some(info) = info else {
        return String::new();
    };
    let mut parts: Vec<String> = Vec::new();
    if info.deprecated {
        parts.push(style::eyellow("deprecated").to_string());
    }
    if let Some(latest) = &info.latest {
        parts.push(style::eyellow(format!("latest {latest}")).to_string());
    }
    let sep = style::edim(" · ").to_string();
    format!("  {}", parts.join(&sep))
}
