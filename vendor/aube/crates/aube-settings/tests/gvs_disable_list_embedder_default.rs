//! Integration test for the GVS exclusion-list embedder seam (Change 1).
//!
//! aube force-disables its global virtual store for a hardcoded list of
//! frameworks that break under it (`disableGlobalVirtualStoreForPackages`,
//! defaulting to `["next", "nuxt", "parcel"]` in `settings.toml`). That list is
//! a *genuinely user-overridable* setting, so — per the codebase's embedder
//! split (embedder-fixed behavior on `aube_util::Embedder`; re-defaultable user
//! knobs through `set_embedder_defaults`) — an embedder supplies its own list
//! through the settings `embedderDefaults` tier rather than a new `Embedder`
//! field.
//!
//! This pins the three-part contract:
//!   1. default profile → aube's built-in list, unchanged;
//!   2. an embedder default → replaces the built-in list;
//!   3. any real user/project source still outranks the embedder default.
//!
//! Lives in its own integration-test binary because `set_embedder_defaults` is
//! once-per-process; registering this list elsewhere would leak.

use aube_settings::{ResolveCtx, embedder_defaults, resolved, set_embedder_defaults};
use std::collections::BTreeMap;

const SETTING: &str = "disableGlobalVirtualStoreForPackages";

/// aube's built-in default, verbatim from `settings.toml`. Asserting against
/// this string list is the "default is unchanged" half of the contract.
/// v1.35.0 dropped `vite`/`vitepress` here once #1106 made the GVS work under
/// Vite; v2 dropped `nuxt`/`parcel` (#1312) and added the Metro family
/// (#1297). nub tracks that list rather than pinning its own.
const AUBE_DEFAULT: &[&str] = &["next", "expo", "react-native", "metro"];

/// A stand-in embedder list. Deliberately disjoint from `AUBE_DEFAULT` so the
/// override assertion below fails if the embedder tier stops being consulted —
/// an overlapping list would pass whether or not the override applied.
const EMBEDDER_LIST: &[&str] = &["svelte-kit", "astro"];

fn ctx<'a>(
    ws: &'a BTreeMap<String, yaml_serde::Value>,
    npmrc: &'a [(String, String)],
    embedder: &'a [(String, String)],
) -> ResolveCtx<'a> {
    ResolveCtx {
        managed_aube_config: &[],
        project_aube_config: &[],
        project_npmrc: npmrc,
        project_config: &[],
        user_aube_config: &[],
        user_npmrc: &[],
        workspace_yaml: ws,
        global_config_yaml: aube_settings::values::empty_yaml_map(),
        env: &[],
        cli: &[],
        embedder_defaults: embedder,
    }
}

fn json_list(items: &[&str]) -> String {
    let inner = items
        .iter()
        .map(|s| format!("\"{s}\""))
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{inner}]")
}

#[test]
fn gvs_list_default_unchanged_and_embedder_overridable() {
    let ws = BTreeMap::new();

    // 1. No embedder default registered yet: the built-in `settings.toml`
    //    default applies, byte-for-byte. This is the default==upstream half.
    let plain = ctx(&ws, &[], &[]);
    assert_eq!(
        resolved::disable_global_virtual_store_for_packages(&plain),
        AUBE_DEFAULT
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>(),
        "with no embedder default, aube's built-in GVS list must be unchanged"
    );

    // 2. Register an embedder default; it replaces the built-in default when no
    //    user source speaks. (Process-global; everything below sees it.)
    set_embedder_defaults(vec![(SETTING.to_string(), json_list(EMBEDDER_LIST))]);
    let with_embedder = ctx(&ws, &[], embedder_defaults());
    assert_eq!(
        resolved::disable_global_virtual_store_for_packages(&with_embedder),
        EMBEDDER_LIST
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>(),
        "embedder default must replace the built-in GVS list"
    );

    // 3. A real user source (project `.npmrc`) still outranks the embedder
    //    default — the embedder only re-defaults, it doesn't pin.
    let npmrc = vec![(
        "disableGlobalVirtualStoreForPackages".to_string(),
        json_list(&["just-this"]),
    )];
    let with_user = ctx(&ws, &npmrc, embedder_defaults());
    assert_eq!(
        resolved::disable_global_virtual_store_for_packages(&with_user),
        vec!["just-this".to_string()],
        ".npmrc must win over the embedder default"
    );
}
