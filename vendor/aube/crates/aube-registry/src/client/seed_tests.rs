use super::cache::{cached_is_fresh, packument_cache_path, read_cached_packument};
use super::*;
use crate::Packument;

fn packument() -> Packument {
    Packument {
        name: "demo".to_owned(),
        modified: None,
        versions: BTreeMap::new(),
        dist_tags: BTreeMap::new(),
        time: BTreeMap::new(),
    }
}

#[test]
fn stale_primer_seed_revalidates() {
    let dir = tempfile::tempdir().unwrap();
    let client = RegistryClient::new("https://registry.npmjs.org/");
    let packument = packument();

    client.seed_packument_cache(
        "demo",
        dir.path(),
        &packument,
        Some("etag"),
        Some("last-modified"),
        false,
    );

    let path = packument_cache_path(dir.path(), "demo", "https://registry.npmjs.org/").unwrap();
    let cached = read_cached_packument(&path).unwrap();
    assert_eq!(cached.fetched_at, 0);
    assert_eq!(cached.max_age_secs, Some(0));
    assert!(!cached_is_fresh(cached.fetched_at, cached.max_age_secs));
}

#[test]
fn fresh_primer_seed_skips_revalidation() {
    let dir = tempfile::tempdir().unwrap();
    let client = RegistryClient::new("https://registry.npmjs.org/");
    let packument = packument();

    client.seed_packument_cache(
        "demo",
        dir.path(),
        &packument,
        Some("etag"),
        Some("last-modified"),
        true,
    );

    let path = packument_cache_path(dir.path(), "demo", "https://registry.npmjs.org/").unwrap();
    let cached = read_cached_packument(&path).unwrap();
    assert!(cached.fetched_at > 0);
    assert_eq!(cached.max_age_secs, None);
    assert!(cached_is_fresh(cached.fetched_at, cached.max_age_secs));
}

#[test]
fn stale_seed_is_reported_for_revalidation() {
    let dir = tempfile::tempdir().unwrap();
    let client = RegistryClient::new("https://registry.npmjs.org/");
    let packument = packument();

    client.seed_packument_cache("demo", dir.path(), &packument, None, None, false);

    let lookup = client.cached_packument_lookup("demo", dir.path());
    assert!(lookup.stale);
    assert!(lookup.packument.is_none());
}

#[test]
fn replace_packument_cache_overwrites_stale_seed() {
    let dir = tempfile::tempdir().unwrap();
    let client = RegistryClient::new("https://registry.npmjs.org/");
    let primer = packument();
    let mut live = packument();
    live.name = "demo-live".to_owned();

    client.seed_packument_cache("demo", dir.path(), &primer, Some("etag"), None, false);
    client.replace_packument_cache("demo", dir.path(), &live);

    let path = packument_cache_path(dir.path(), "demo", "https://registry.npmjs.org/").unwrap();
    let cached = read_cached_packument(&path).unwrap();
    assert_eq!(cached.packument.name, "demo-live");
    assert!(cached.fetched_at > 0);
    assert_eq!(cached.max_age_secs, None);
    assert!(cached_is_fresh(cached.fetched_at, cached.max_age_secs));
}

#[test]
fn default_registry_detection_ignores_trailing_slash() {
    assert!(
        RegistryClient::new("https://registry.npmjs.org").uses_default_npm_registry_for("demo")
    );
    assert!(
        RegistryClient::new("https://registry.npmjs.org/").uses_default_npm_registry_for("demo")
    );
}
