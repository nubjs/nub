//! The two built-in `$`-sets the compiler expands in place: `$trusted` (a curated
//! network host allowlist, net axis) and `$tooldirs` (the per-OS package-manager /
//! toolchain cache+store dirs, fs axis). Both are ORDINARY last-match-wins entries —
//! a set expands at its authored position and a later rule can override any member,
//! exactly like the `"..."` splice (defaults.rs).
//!
//! Provenance / curation:
//! - `$trusted` is the Claude Code default-allowed-domains list MINUS blanket
//!   multi-tenant object-store wildcards (`*.amazonaws.com`, `storage.googleapis.com`,
//!   `*.googleapis.com`) — those are exfil sinks; the genuinely-needed named Google
//!   APIs stay listed. Metadata/link-local + RFC1918 are a SEPARATE always-on hard
//!   floor, never part of this set. Source data: `.fray/sandbox-builtin-sets.md`.
//! - `$tooldirs` is per-OS because a tool's cache home differs across OSes (macOS
//!   `~/Library/Caches`, Linux `~/.cache`, Windows `%LOCALAPPDATA%`). Host OS ==
//!   target OS (the fold runs on the machine it enforces on), so the set is
//!   `#[cfg]`-selected — same precedent as `defaults::OS_ESSENTIAL_ENV`.
//!
//! Runtime-resolved dirs (`gem environment gemdir`, `pnpm store path`) ship STATIC
//! home-anchored defaults ONLY — the engine never shells out to build this set (that
//! would be a nub-authored command-exec surface, unconditional even in an untrusted
//! `dependenciesMeta` scope, and a missing tool would hard-fail the compile). True
//! runtime resolution is deferred to a host-provided `CompileCtx` field, fail-soft.

use crate::matcher::path::{Homes, canonicalize_glob_prefix, expand_symbolic};
use crate::policy::{CanonGlob, Effect, FsAccess, FsRule, NetRule, NetTarget};

// ── $trusted (net host set) ────────────────────────────────────────────────────

/// The curated trusted-host allowlist. Each entry is a literal host or a leading
/// `*.suffix` subdomain wildcard — both accepted by [`crate::matcher::host::host_pattern_is_valid`]
/// and matched by [`crate::matcher::host::host_glob_matches`]. Object-store wildcards
/// are deliberately absent (see the module doc); the `#[cfg(test)]` unit below guards
/// both invariants so a future edit cannot smuggle an invalid pattern or an exfil sink in.
pub const TRUSTED_HOSTS: &[&str] = &[
    // Anthropic
    "api.anthropic.com",
    "statsig.anthropic.com",
    "docs.claude.com",
    "platform.claude.com",
    "code.claude.com",
    "claude.ai",
    // Version control
    "github.com",
    "www.github.com",
    "api.github.com",
    "npm.pkg.github.com",
    "raw.githubusercontent.com",
    "pkg-npm.githubusercontent.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
    "codeload.github.com",
    "avatars.githubusercontent.com",
    "camo.githubusercontent.com",
    "gist.github.com",
    "gitlab.com",
    "www.gitlab.com",
    "registry.gitlab.com",
    "bitbucket.org",
    "www.bitbucket.org",
    "api.bitbucket.org",
    // Container registries
    "registry-1.docker.io",
    "auth.docker.io",
    "index.docker.io",
    "hub.docker.com",
    "www.docker.com",
    "production.cloudflare.docker.com",
    "download.docker.com",
    "gcr.io",
    "*.gcr.io",
    "ghcr.io",
    "mcr.microsoft.com",
    "*.data.mcr.microsoft.com",
    "public.ecr.aws",
    // Cloud platforms (*.amazonaws.com, storage.googleapis.com, *.googleapis.com REMOVED)
    "cloud.google.com",
    "accounts.google.com",
    "gcloud.google.com",
    "compute.googleapis.com",
    "container.googleapis.com",
    "azure.com",
    "portal.azure.com",
    "microsoft.com",
    "www.microsoft.com",
    "*.microsoftonline.com",
    "packages.microsoft.com",
    "dotnet.microsoft.com",
    "dot.net",
    "visualstudio.com",
    "dev.azure.com",
    "*.api.aws",
    "oracle.com",
    "www.oracle.com",
    "java.com",
    "www.java.com",
    "java.net",
    "www.java.net",
    "download.oracle.com",
    "yum.oracle.com",
    // JS / Node
    "registry.npmjs.org",
    "www.npmjs.com",
    "www.npmjs.org",
    "npmjs.com",
    "npmjs.org",
    "yarnpkg.com",
    "registry.yarnpkg.com",
    // Python
    "pypi.org",
    "www.pypi.org",
    "files.pythonhosted.org",
    "pythonhosted.org",
    "test.pypi.org",
    "pypi.python.org",
    "pypa.io",
    "www.pypa.io",
    // Ruby
    "rubygems.org",
    "www.rubygems.org",
    "api.rubygems.org",
    "index.rubygems.org",
    "ruby-lang.org",
    "www.ruby-lang.org",
    "rubyforge.org",
    "www.rubyforge.org",
    "rubyonrails.org",
    "www.rubyonrails.org",
    "rvm.io",
    "get.rvm.io",
    // Rust
    "crates.io",
    "www.crates.io",
    "index.crates.io",
    "static.crates.io",
    "rustup.rs",
    "static.rust-lang.org",
    "www.rust-lang.org",
    // Go
    "proxy.golang.org",
    "sum.golang.org",
    "index.golang.org",
    "golang.org",
    "www.golang.org",
    "goproxy.io",
    "pkg.go.dev",
    // JVM
    "maven.org",
    "repo.maven.org",
    "central.maven.org",
    "repo1.maven.org",
    "repo.maven.apache.org",
    "jcenter.bintray.com",
    "gradle.org",
    "www.gradle.org",
    "services.gradle.org",
    "plugins.gradle.org",
    "kotlinlang.org",
    "www.kotlinlang.org",
    "spring.io",
    "repo.spring.io",
    // Other package managers
    "packagist.org",
    "www.packagist.org",
    "repo.packagist.org",
    "nuget.org",
    "www.nuget.org",
    "api.nuget.org",
    "pub.dev",
    "api.pub.dev",
    "hex.pm",
    "www.hex.pm",
    "cpan.org",
    "www.cpan.org",
    "metacpan.org",
    "www.metacpan.org",
    "api.metacpan.org",
    "cocoapods.org",
    "www.cocoapods.org",
    "cdn.cocoapods.org",
    "haskell.org",
    "www.haskell.org",
    "hackage.haskell.org",
    "swift.org",
    "www.swift.org",
    // Linux distros
    "archive.ubuntu.com",
    "security.ubuntu.com",
    "ubuntu.com",
    "www.ubuntu.com",
    "*.ubuntu.com",
    "ppa.launchpad.net",
    "launchpad.net",
    "www.launchpad.net",
    "*.nixos.org",
    // Dev tools / platforms
    "dl.k8s.io",
    "pkgs.k8s.io",
    "k8s.io",
    "www.k8s.io",
    "releases.hashicorp.com",
    "apt.releases.hashicorp.com",
    "rpm.releases.hashicorp.com",
    "archive.releases.hashicorp.com",
    "hashicorp.com",
    "www.hashicorp.com",
    "repo.anaconda.com",
    "conda.anaconda.org",
    "anaconda.org",
    "www.anaconda.com",
    "anaconda.com",
    "continuum.io",
    "apache.org",
    "www.apache.org",
    "archive.apache.org",
    "downloads.apache.org",
    "eclipse.org",
    "www.eclipse.org",
    "download.eclipse.org",
    "nodejs.org",
    "www.nodejs.org",
    "developer.apple.com",
    "developer.android.com",
    "pkg.stainless.com",
    "binaries.prisma.sh",
    // Cloud services / monitoring
    "statsig.com",
    "www.statsig.com",
    "api.statsig.com",
    "sentry.io",
    "*.sentry.io",
    "downloads.sentry-cdn.com",
    "http-intake.logs.datadoghq.com",
    "browser-intake-us5-datadoghq.com",
    "*.datadoghq.com",
    "*.datadoghq.eu",
    "api.honeycomb.io",
    // CDN / mirrors
    "sourceforge.net",
    "*.sourceforge.net",
    "packagecloud.io",
    "*.packagecloud.io",
    "fonts.googleapis.com",
    "fonts.gstatic.com",
    // Schema / config
    "json-schema.org",
    "www.json-schema.org",
    "json.schemastore.org",
    "www.schemastore.org",
    // MCP
    "*.modelcontextprotocol.io",
];

/// Expand `$trusted` into one [`NetRule`] per host with the given effect (Allow for a
/// bare `$trusted`, Deny for `!$trusted`). The trailing-dot normalization matches
/// [`super::fold::push_net_rule`]'s D12 handling so a `$trusted` rule and a hand-written
/// host rule for the same name produce byte-identical IR.
pub fn trusted_net_rules(effect: Effect) -> Vec<NetRule> {
    TRUSTED_HOSTS
        .iter()
        .map(|h| NetRule {
            target: NetTarget::Host(crate::matcher::host::strip_trailing_dot(h).to_string()),
            effect,
        })
        .collect()
}

// ── $tooldirs (fs cache/store set) ─────────────────────────────────────────────

// Per-OS surface patterns (`~`/`$cache`-anchored) that `expand_symbolic` +
// `subtree_globs` turn into fs rules. nub's OWN dirs are code-grounded against the
// `NUB` embedder profile (crates/nub-cli/src/pm_engine/identity.rs) and
// vendor/aube/crates/aube-store/src/dirs.rs: `data_namespace = "nub"` →
// `~/.local/share/nub/store/…`; `cache_namespace = "nub/pm"` → `<cache>/nub/pm`.
// `$cache` is the platform cache home (XDG_CACHE_HOME else `~/.cache`), which matches
// aube's own `cache_dir()` base on POSIX; the nub store is anchored at the literal
// `~/.local/share` per the deferred-runtime-resolution decision (no XDG_DATA_HOME
// capture in `Homes`). Third-party paths are documented defaults; the override env is
// intentionally NOT read here (static defaults only — runtime resolution is deferred).

#[cfg(target_os = "macos")]
const TOOLDIR_PATTERNS: &[&str] = &[
    // nub (own engine)
    "~/.local/share/nub/store",
    "$cache/nub/pm",
    // JS package managers
    "~/.npm/_cacache",
    "~/Library/pnpm/store",
    "~/Library/Caches/pnpm",
    "~/Library/Caches/Yarn",
    "~/.yarn/berry/cache",
    "~/.bun/install/cache",
    // Python
    "~/Library/Caches/pip",
    "~/Library/Caches/uv",
    // Other toolchains
    "~/.cargo/registry",
    "~/go/pkg/mod",
    "~/.gradle/caches",
    "~/.m2/repository",
    "~/.nuget/packages",
    "~/.composer/cache",
];

#[cfg(target_os = "windows")]
const TOOLDIR_PATTERNS: &[&str] = &[
    // nub (own engine) — %LOCALAPPDATA% is `~/AppData/Local` by default
    "~/AppData/Local/nub/store",
    "~/AppData/Local/nub/pm",
    // JS package managers
    "~/AppData/Local/npm-cache",
    "~/AppData/Local/pnpm/store",
    "~/AppData/Local/pnpm-cache",
    "~/AppData/Local/Yarn/Cache",
    "~/AppData/Local/Yarn/Berry/cache",
    "~/.bun/install/cache",
    // Python
    "~/AppData/Local/pip/Cache",
    "~/AppData/Local/uv/cache",
    // Other toolchains
    "~/.cargo/registry",
    "~/go/pkg/mod",
    "~/.gradle/caches",
    "~/.m2/repository",
    "~/.nuget/packages",
    "~/AppData/Local/Composer",
];

// Linux + any other unix (freebsd, …): the XDG layout.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const TOOLDIR_PATTERNS: &[&str] = &[
    // nub (own engine)
    "~/.local/share/nub/store",
    "$cache/nub/pm",
    // JS package managers
    "~/.npm/_cacache",
    "~/.local/share/pnpm/store",
    "~/.cache/pnpm",
    "~/.cache/yarn",
    "~/.yarn/berry/cache",
    "~/.bun/install/cache",
    // Python
    "~/.cache/pip",
    "~/.cache/uv",
    // Other toolchains
    "~/.cargo/registry",
    "~/go/pkg/mod",
    "~/.gradle/caches",
    "~/.m2/repository",
    "~/.nuget/packages",
    "~/.cache/composer",
];

/// The per-OS `$tooldirs` surface patterns (host OS == target OS).
pub fn tooldir_patterns() -> &'static [&'static str] {
    TOOLDIR_PATTERNS
}

/// Expand `$tooldirs` into fs rules under the resolved home anchors — one rule per
/// subtree glob per pattern, mirroring [`super::defaults::secret_read_denies`] and the
/// [`super::fold::push_fs_rules`] funnel (a Deny normalizes to the inert `FsAccess::DENY`,
/// so two denies differing only in access don't yield divergent IR — D20).
pub fn tooldirs_fs_rules(homes: &Homes, effect: Effect, access: FsAccess) -> Vec<FsRule> {
    let access = if effect == Effect::Deny {
        FsAccess::DENY
    } else {
        access
    };
    let mut out = Vec::new();
    for pattern in tooldir_patterns() {
        let expanded = expand_symbolic(pattern, homes);
        for g in super::defaults::subtree_globs(&expanded) {
            out.push(FsRule {
                matcher: CanonGlob(canonicalize_glob_prefix(&g)),
                effect,
                access,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matcher::host::host_pattern_is_valid;
    use crate::matcher::path::compile_glob;
    use std::path::PathBuf;

    fn homes() -> Homes {
        Homes {
            home: PathBuf::from("/testhome"),
            tmp: PathBuf::from("/testtmp"),
            cache: PathBuf::from("/testhome/.cache"),
            project: PathBuf::from("/proj"),
        }
    }

    #[test]
    fn every_trusted_host_is_a_valid_pattern() {
        for h in TRUSTED_HOSTS {
            assert!(
                host_pattern_is_valid(h),
                "`{h}` is not a valid host pattern for $trusted"
            );
        }
    }

    #[test]
    fn no_object_store_wildcard_leaked_into_trusted() {
        // The three blanket multi-tenant object-store wildcards are exfil sinks and MUST
        // stay removed — a future re-add of the Claude Code base list must not reintroduce them.
        for banned in [
            "*.amazonaws.com",
            "storage.googleapis.com",
            "*.googleapis.com",
        ] {
            assert!(
                !TRUSTED_HOSTS.contains(&banned),
                "object-store wildcard `{banned}` must not be in $trusted"
            );
        }
    }

    #[test]
    fn every_tooldir_glob_compiles() {
        let rules = tooldirs_fs_rules(&homes(), Effect::Allow, FsAccess::Read);
        assert!(!rules.is_empty());
        for r in &rules {
            compile_glob(r.matcher.as_str()).unwrap_or_else(|e| {
                panic!(
                    "tooldir glob `{}` failed to compile: {e}",
                    r.matcher.as_str()
                )
            });
        }
    }

    #[test]
    fn nub_store_and_cache_paths_are_present() {
        // Guards the coupling to the NUB embedder profile (identity.rs
        // `data_namespace = "nub"` / `cache_namespace = "nub/pm"`): if those move, this
        // set must move with them.
        let rules = tooldirs_fs_rules(&homes(), Effect::Allow, FsAccess::Read);
        let matchers: Vec<&str> = rules.iter().map(|r| r.matcher.as_str()).collect();
        assert!(
            matchers.iter().any(|m| m.contains("nub/store")),
            "nub CAS store path missing from $tooldirs: {matchers:?}"
        );
        assert!(
            matchers.iter().any(|m| m.contains("nub/pm")),
            "nub PM cache path missing from $tooldirs: {matchers:?}"
        );
    }

    #[test]
    fn deny_effect_normalizes_access_to_the_inert_value() {
        // A `!$tooldirs` deny carries the canonical inert access (D20), same as the fs funnel.
        let rules = tooldirs_fs_rules(&homes(), Effect::Deny, FsAccess::ReadWrite);
        assert!(
            rules
                .iter()
                .all(|r| r.effect == Effect::Deny && r.access == FsAccess::DENY)
        );
    }
}
