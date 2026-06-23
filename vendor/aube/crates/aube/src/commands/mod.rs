pub mod add;
pub mod add_supply_chain;
pub mod approve_builds;
pub mod audit;
pub mod bin;
pub mod cache;
pub mod cat_file;
pub mod cat_index;
pub mod catalogs;
pub mod check;
pub mod ci;
pub mod clean;
pub mod completion;
pub mod config;
pub mod create;
pub mod dedupe;
pub mod deploy;
pub mod deprecate;
pub mod deprecations;
pub mod diag;
pub mod dist_tag;
pub mod dlx;
pub mod doctor;
pub mod exec;
pub mod fetch;
pub mod find_hash;
pub mod global;
pub mod ignored_builds;
pub mod import;
pub mod init;
pub mod inject;
pub mod install;
pub mod install_test;
pub mod licenses;
pub mod link;
pub mod list;
pub mod login;
pub mod logout;
pub mod npm_fallback;
pub mod npmrc;
pub mod outdated;
pub mod owner;
pub mod pack;
pub mod patch;
pub mod patch_commit;
pub mod patch_remove;
pub mod peers;
pub mod pkg;
pub mod prune;
pub mod publish;
pub mod publish_provenance;
pub mod query;
pub mod rebuild;
pub mod recursive;
pub mod remove;
pub mod restart;
pub mod root;
pub mod run;
pub mod run_output;
pub mod runtime;
pub mod sbom;
pub mod search;
pub mod security_scanner;
pub mod set_script;
pub mod sponsors;
pub mod store;
pub mod token;
pub mod undeprecate;
pub mod unlink;
pub mod unpublish;
pub mod update;
pub mod version;
pub mod view;
pub mod whoami;
pub mod why;

mod auto_install;
mod catalog_discovery;
mod dep_filter;
mod fs_helpers;
mod manifest_io;
mod package_spec;
mod project_lock;
pub mod property_path;
mod script_settings;
mod settings_context;
mod workspace_helpers;

pub(crate) use auto_install::ensure_installed;

pub(crate) fn settings_hoisting_limits_to_linker(
    value: aube_settings::resolved::HoistingLimits,
) -> aube_linker::HoistingLimits {
    match value {
        aube_settings::resolved::HoistingLimits::None => aube_linker::HoistingLimits::None,
        aube_settings::resolved::HoistingLimits::Workspaces => {
            aube_linker::HoistingLimits::Workspaces
        }
        aube_settings::resolved::HoistingLimits::Dependencies => {
            aube_linker::HoistingLimits::Dependencies
        }
    }
}
pub(crate) use catalog_discovery::{CatalogMap, discover_catalogs, load_workspace_catalogs};
pub(crate) use dep_filter::DepFilter;
pub(crate) use fs_helpers::{format_virtual_store_display_prefix, remove_existing, symlink_dir};
pub(crate) use manifest_io::{
    load_manifest, load_manifest_or_default, update_manifest_json_object,
    write_manifest_dep_sections, write_manifest_json,
};
pub(crate) use package_spec::{
    encode_package_name, max_satisfying_version, resolve_version, split_name_spec,
};
pub(crate) use project_lock::take_project_lock;
pub(crate) use script_settings::{configure_script_settings, configure_script_settings_for_cwd};
pub(crate) use settings_context::{
    FileSources, GlobalOutputFlags, build_resolver, chained_frozen_mode, default_lockfile_kind,
    default_lockfile_kind_for_cwd, ensure_registry_auth_for_package, expand_setting_path,
    global_frozen_override, global_output_flags, global_virtual_store_flags,
    load_global_config_yaml, load_npm_config, make_client, open_store, packument_cache_dir,
    packument_full_cache_dir, project_modules_dir, resolve_fetch_policy,
    resolve_lockfile_kind_for_write, resolve_modules_dir_name_for_cwd, resolve_virtual_store_dir,
    resolve_virtual_store_dir_for_cwd, resolve_virtual_store_dir_max_length,
    resolve_virtual_store_dir_max_length_for_cwd, resolved_cache_dir, run_pnpmfile_pre_resolution,
    set_fetch_cli_overrides, set_global_frozen_override, set_global_output_flags,
    set_global_virtual_store_flags, set_registry_override,
    set_skip_auto_install_on_package_manager_mismatch,
    skip_auto_install_on_package_manager_mismatch, with_settings_ctx,
};
pub(crate) use workspace_helpers::{
    collect_dep_closure, find_workspace_root, finish_filtered_workspace, load_graph,
    prepare_resolved_graph_for_lockfile_write, retarget_cwd, select_workspace_packages,
    workspace_importer_path, write_and_log_lockfile,
};
