//! The Windows `writePaths` entries a re-bake would silently withdraw again.
//!
//! ⛔ WHY THIS FILE EXISTS, AND WHY IT IS NOT REDUNDANT WITH THE CATALOG'S OWN VALIDATION.
//! `build.rs` proves the catalog PARSES; nothing proves a per-OS overlay says what it was
//! measured to say. Every win32 record the catalog was baked from declared no private home
//! (`roots.jailHome` is null in 1683 of 1688 captures), so the derivation — which reads that
//! bucket and nothing else — could produce no `writePaths` on Windows for any package, and the
//! collator wrote `"win": {"writePaths": null}` by default rather than by measurement. These
//! five entries were restored from each installer's own published source, so a re-bake against
//! the same records would withdraw them again with no signal at all.
//!
//! ⛔ AND THE TWO SHAPES BELOW ARE DIFFERENT MECHANISMS, which is why both are pinned. esbuild
//! computes its cache from `os.homedir()`, which follows the redirected `USERPROFILE`; ngrok
//! computes it from `%APPDATA%`, which the jail redirects separately. Both therefore land in
//! the private home and are promotable. A package whose Windows path hangs off `LOCALAPPDATA`
//! is NOT here on purpose — that variable is deliberately left unredirected, so nothing of its
//! lands in the private home and promotion cannot reach it whatever the catalog says.
use nub_sandbox::catalog_v2::Platform;

#[test]
fn a_restored_windows_entry_promotes_the_directory_its_installer_writes() {
    // (package, version, the directory the installer writes on win32)
    //   esbuild  install.js `getCachePath`: path.join(os.homedir(), "AppData", "Local", "Cache", "esbuild", …)
    //   ngrok    download.js `getCacheUrl`: path.join(process.env.APPDATA, "ngrok")
    for (pkg, version, want) in [
        ("esbuild", "0.11.23", "AppData/Local/Cache/esbuild"),
        ("esbuild", "0.20.2", "AppData/Local/Cache/esbuild"),
        ("@netlify/esbuild", "0.13.6", "AppData/Local/Cache/esbuild"),
        ("ngrok", "5.0.0-beta.2", "AppData/Roaming/ngrok"),
        ("@shopify/ngrok", "4.3.2", "AppData/Roaming/ngrok"),
    ] {
        let grant = nub_sandbox::catalog_override_v2_grant(pkg, Some(version))
            .unwrap_or_else(|| panic!("{pkg}@{version} resolves to no catalog grant at all"));
        assert_eq!(
            grant.on(Platform::Windows).write_paths,
            vec![want.to_string()],
            "{pkg}@{version}: Windows promotion withdrawn — the installer's artefact is \
             discarded with the throwaway home"
        );
        // The Windows spelling must not leak onto the OS whose own measurement was sound.
        assert!(
            !grant
                .on(Platform::Linux)
                .write_paths
                .contains(&want.to_string()),
            "{pkg}@{version}: a Windows-only path reached the Linux grant"
        );
    }
}
