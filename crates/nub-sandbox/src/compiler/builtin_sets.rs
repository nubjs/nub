//! The built-in `$`-sets the compiler expands in place: `$trusted` (a curated network
//! host allowlist) and `$downloads` (the install-time artifact hosts) on the net axis,
//! `$tooldirs` (the per-OS package-manager / toolchain cache+store dirs) on the fs axis.
//! Network sets expand at their authored position under last-match-wins rules.
//! Positive filesystem grants combine; a later read-only grant cannot revoke write access.
//!
//! Provenance / curation:
//! - `$trusted` derives from the Claude Code default-allowed-domains list, filtered by a
//!   SINGLE criterion: EXFILTRATION. A host is excluded only if the confined process can
//!   make bytes of its own choosing retrievable by someone outside the sandbox. Three
//!   mechanisms qualify: an authenticated write-back route (publish a package, create a
//!   repo, post a paste, push an image, POST a telemetry event — the Shai-Hulud
//!   propagation primitive), rentable path/subdomain tenancy the attacker reads back
//!   (multi-tenant object stores), and a host the attacker operates, where the request
//!   itself is the signal.
//!
//!   DELIVERY is deliberately NOT disqualifying, and the distinction is the whole point of
//!   the set. A host that only serves attacker-authored bytes INTO the sandbox is not an
//!   exfiltration channel: the malicious code is already executing here by construction, so
//!   denying it one CDN only moves it to the next one — and `registry.npmjs.org`, retained
//!   because nothing installs without it, is itself an arbitrary-payload delivery channel.
//!   That is why the read-only GitHub content hosts are IN the set while `api.github.com`
//!   is not. "A worm fetched its payload from this host" is an argument about delivery and
//!   carries no weight here; so does "a worm read data from here that it had exfiltrated
//!   somewhere else" — that names the OTHER host as the sink.
//!
//!   Membership was settled by probing each ecosystem's *documented* write route against a
//!   same-host bogus-path control. A live auth-gated route answers differently from the
//!   bogus path (`api.github.com` POST /user/repos -> 401 "Requires authentication", bogus
//!   -> 404); a host that denies the method wholesale answers identically (`codeload`
//!   POST -> 403 on both the real tarball path and a bogus one). That control is what
//!   caught `index.rubygems.org` fronting the same Rails app as `rubygems.org` — POST
//!   /api/v1/gems -> 401 and bogus -> 404 on both names, `server: RubyGems.org` throughout —
//!   so a stolen `RUBYGEMS_API_KEY` publishes through the name that looks like a read index.
//!
//!   Four caveats bound what this set can promise. An entry is NOT protocol-scoped — the
//!   egress proxy tunnels arbitrary TCP via CONNECT/SOCKS5, so allowing a host admits
//!   non-HTTP upload transports too (why the `dput` PPA target is absent). A CNAME onto a
//!   shared CDN is opaque to any host allowlist, since the tenant-selecting `Host` header
//!   travels inside TLS (`index.crates.io` / `static.crates.io` are kept as single-tenant
//!   buckets on that basis, not because the hostname proves it). A `*.suffix` wildcard
//!   cannot satisfy the criterion by inspection at all, because it admits whatever the
//!   operator ever hosts under that suffix: the retained `*.ubuntu.com` already covers
//!   `login.ubuntu.com`, whose POST /api/v2/tokens/oauth answers 400 against a 404 bogus
//!   control. The four retained wildcards are a standing exception pending a decision on
//!   the shape as a whole. And three entries are retained despite failing the rule outright
//!   — `registry.npmjs.org`, `api.anthropic.com`, `claude.ai` — because each is load-bearing
//!   and its write route answers on the same hostname the legitimate read uses (`npm
//!   publish` is a PUT to the registry that installs read; a model API call carries its
//!   payload in the request body). The proxy gates only the CONNECT authority and TLS SNI
//!   before blind-forwarding, so it cannot separate them; for these three, credential
//!   scoping is the control, not the host list. Metadata/link-local + RFC1918 are a SEPARATE
//!   always-on hard floor, never part of this set. This set is the broad agent-facing net
//!   axis and is unrelated to `$downloads`, which is a separate constant with its own,
//!   stricter membership. Any host added later must clear the same
//!   write-route probe; absent the probe, leave it out.
//! - `$downloads` is the narrower, install-scoped sibling of `$trusted`, and the two are
//!   kept strictly apart: `$trusted` serves an agent working with the user's own
//!   credentials, `$downloads` serves attacker-authored dependency code, so it inherits
//!   none of `$trusted`'s load-bearing write-capable retentions. It is also wildcard-free
//!   by construction (see [`DOWNLOAD_HOSTS`]).
//! - `$tooldirs` is per-OS because a tool's cache home differs across OSes (macOS
//!   `~/Library/Caches`, Linux `~/.cache`, Windows `%LOCALAPPDATA%`). Host OS ==
//!   target OS (the fold runs on the machine it enforces on), so the set is
//!   `#[cfg]`-selected — same precedent as `defaults::OS_ESSENTIAL_ENV`.
//!
//! The set combines conventional roots with documented environment locations from the
//! supplied compile snapshot. It never shells out to a tool or parses its config files;
//! locations configured only in those files need explicit grants.

use crate::matcher::path::{Homes, canonicalize_glob_prefix, expand_symbolic, normalize_slashes};
use crate::policy::{CanonGlob, Effect, FsAccess, FsOrigin, FsRule, NetRule, NetTarget};
use std::collections::{BTreeMap, BTreeSet};

// ── $trusted (net host set) ────────────────────────────────────────────────────

/// The curated trusted-host allowlist. Each entry is a literal host or a leading
/// `*.suffix` subdomain wildcard — both accepted by [`crate::matcher::host::host_pattern_is_valid`]
/// and matched by [`crate::matcher::host::host_glob_matches`]. Write-capable and
/// multi-tenant hosts are deliberately absent (see the module doc for the rule and its
/// caveats); the `#[cfg(test)]` unit below guards both invariants so a future edit cannot
/// smuggle an invalid pattern or an exfil sink in.
pub const TRUSTED_HOSTS: &[&str] = &[
    // Anthropic / Claude
    "api.anthropic.com",
    "claude.ai",
    "code.claude.com",
    "docs.claude.com",
    "platform.claude.com",
    // GitHub content delivery. These SERVE bytes and cannot store them — every one denies
    // writes wholesale, unlike the github write surface (see the module doc's third bullet).
    "codeload.github.com",
    "raw.githubusercontent.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
    "avatars.githubusercontent.com",
    "camo.githubusercontent.com",
    // npm / yarn. `registry.yarnpkg.com` is an alias of the retained `registry.npmjs.org`,
    // so it grants no capability that host does not already grant.
    "registry.npmjs.org",
    "registry.yarnpkg.com",
    "npmjs.com",
    "www.npmjs.com",
    "npmjs.org",
    "www.npmjs.org",
    "yarnpkg.com",
    // Node / JS
    "nodejs.org",
    "www.nodejs.org",
    "binaries.prisma.sh",
    "downloads.sentry-cdn.com",
    "pkg.stainless.com",
    // Python
    "files.pythonhosted.org",
    "pypi.org",
    "www.pypi.org",
    "pypi.python.org",
    "pythonhosted.org",
    "pypa.io",
    "www.pypa.io",
    "conda.anaconda.org",
    "repo.anaconda.com",
    // Rust
    "index.crates.io",
    "static.crates.io",
    "static.rust-lang.org",
    "www.rust-lang.org",
    "rustup.rs",
    // Go
    "golang.org",
    "www.golang.org",
    "goproxy.io",
    "index.golang.org",
    "proxy.golang.org",
    "sum.golang.org",
    "pkg.go.dev",
    // Java / JVM
    "repo1.maven.org",
    "repo.maven.apache.org",
    "maven.org",
    "gradle.org",
    "www.gradle.org",
    "services.gradle.org",
    "spring.io",
    "kotlinlang.org",
    "www.kotlinlang.org",
    // .NET
    "api.nuget.org",
    "dot.net",
    "dotnet.microsoft.com",
    "packages.microsoft.com",
    // Ruby / Perl / PHP / Swift / Haskell / CocoaPods. The rubygems publish app is out
    // under all three of its names; these are its docs/marketing siblings.
    "ruby-lang.org",
    "www.ruby-lang.org",
    "rubyforge.org",
    "www.rubyforge.org",
    "rubyonrails.org",
    "www.rubyonrails.org",
    "rvm.io",
    "get.rvm.io",
    "cpan.org",
    "www.cpan.org",
    "metacpan.org",
    "www.metacpan.org",
    "api.metacpan.org",
    "repo.packagist.org",
    "swift.org",
    "www.swift.org",
    "haskell.org",
    "www.haskell.org",
    "cocoapods.org",
    "www.cocoapods.org",
    "cdn.cocoapods.org",
    // Containers / Kubernetes. Every registry that answers `docker push` is out;
    // `auth.docker.io` only issues tokens and stores nothing.
    "www.docker.com",
    "auth.docker.io",
    "download.docker.com",
    "production.cloudflare.docker.com",
    "mcr.microsoft.com",
    "*.data.mcr.microsoft.com",
    "dl.k8s.io",
    "pkgs.k8s.io",
    "k8s.io",
    "www.k8s.io",
    // OS distributions
    "archive.ubuntu.com",
    "security.ubuntu.com",
    "ubuntu.com",
    "www.ubuntu.com",
    "*.ubuntu.com",
    "*.nixos.org",
    "yum.oracle.com",
    "download.oracle.com",
    // HashiCorp
    "hashicorp.com",
    "www.hashicorp.com",
    "releases.hashicorp.com",
    "apt.releases.hashicorp.com",
    "archive.releases.hashicorp.com",
    "rpm.releases.hashicorp.com",
    // Apache / Eclipse
    "apache.org",
    "www.apache.org",
    "archive.apache.org",
    "downloads.apache.org",
    "eclipse.org",
    "www.eclipse.org",
    "download.eclipse.org",
    // Vendor docs / marketing. These serve pages and store nothing; the write surfaces
    // that share their brand (dev.azure.com, portal.azure.com, anaconda.org,
    // api.statsig.com, the googleapis control planes) are each out on their own name.
    "oracle.com",
    "www.oracle.com",
    "java.com",
    "www.java.com",
    "java.net",
    "www.java.net",
    "microsoft.com",
    "www.microsoft.com",
    "azure.com",
    "visualstudio.com",
    "cloud.google.com",
    "gcloud.google.com",
    "anaconda.com",
    "www.anaconda.com",
    "continuum.io",
    "statsig.com",
    "www.statsig.com",
    // Identity endpoints. A token exchange returns a credential to the caller; it does
    // not retain caller-chosen bytes for a third party to read back.
    "accounts.google.com",
    "*.microsoftonline.com",
    // Schemas / fonts / vendor docs
    "json-schema.org",
    "www.json-schema.org",
    "json.schemastore.org",
    "www.schemastore.org",
    "fonts.googleapis.com",
    "fonts.gstatic.com",
    "developer.android.com",
    "developer.apple.com",
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

// ── $downloads (net host set) ──────────────────────────────────────────────────

// ⛔ DELIBERATELY NOT `$trusted`, AND NEVER MERGED WITH IT. That set is the far broader
// read-only surface a `nub sandbox` scope hands an agent, and it retains three
// write-capable hosts on credential-scoping grounds. This one is install-scoped, so it
// inherits none of those exceptions — `registry.npmjs.org` is absent precisely because
// `npm publish` is a PUT to the same host that serves the read, which makes a write-capable
// registry grant an exfiltration path rather than a convenience. `tests/compiler.rs` pins
// the separation in both directions so a later edit cannot quietly collapse them.
//
// A plain `const` and not generated: the list is four hosts, and the `build.rs` that once
// compiled it from a catalog went with the build jail (as did the catalog, and the
// prefetcher that read the same list). There is exactly one consumer now — the `$downloads`
// token in the `nub sandbox` policy language, where the supervisor enforces per-host egress.

/// The install-time artifact hosts `$downloads` expands to.
///
/// Membership criteria and the reason this stays separate from `$trusted` are above.
pub const DOWNLOAD_HOSTS: &[&str] = &[
    "nodejs.org",
    "binaries.prisma.sh",
    "download.cypress.io",
    "cdn.cypress.io",
];

/// The `$downloads` hosts in force. Retained as an accessor rather than letting callers read
/// the `const`, so a future source for the set has one place to land.
pub fn download_hosts() -> &'static [&'static str] {
    DOWNLOAD_HOSTS
}

/// Expand `$downloads` into one [`NetRule`] per host with the given effect (Allow for a
/// bare `$downloads`, Deny for `!$downloads`). Mirrors [`trusted_net_rules`], including
/// its trailing-dot normalization, so the two sets produce byte-comparable IR.
pub fn download_net_rules(effect: Effect) -> Vec<NetRule> {
    download_hosts()
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
// The runtime/bootstrap cache follows `.cache/nub` on every OS, independently of the
// supplied `$cache` anchor. Environment expansion below adds redirected XDG/app-data
// roots and explicit cache/store settings without reading tool configuration files.

#[cfg(target_os = "macos")]
const TOOLDIR_PATTERNS: &[&str] = &[
    // nub (own engine)
    "~/.local/share/nub/store",
    "~/.cache/nub",
    "~/.config/nub",
    "$cache/nub/pm",
    // JS package managers
    "~/.npm",
    "~/Library/pnpm",
    "~/Library/Caches/pnpm",
    "~/Library/Preferences/pnpm",
    "~/.local/state/pnpm",
    "~/Library/Caches/Yarn",
    "~/.config/yarn",
    "~/.yarn",
    "~/.bun/install",
    // Python
    "~/Library/Caches/pip",
    "~/Library/Caches/uv",
    "~/.cache/uv",
    "~/Library/Application Support/uv",
    "~/Library/Application Support/pip",
    "~/.config/pip",
    "~/.pip",
    "~/Library/Python",
    "~/.local/share/uv",
    "~/.config/uv",
    "~/.local/bin",
    // Other toolchains
    "~/.cargo",
    "~/.rustup",
    "~/go",
    "~/Library/Caches/go-build",
    "~/Library/Application Support/go",
    "~/.gradle",
    "~/.m2",
    "~/.nuget",
    "~/.dotnet",
    "~/.local/share/NuGet",
    "~/.composer",
    "~/Library/Caches/composer",
    "~/Library/Application Support/Composer",
    "~/.config/git",
    "~/.cache/git",
    "~/.git-credential-cache",
];

#[cfg(target_os = "windows")]
const TOOLDIR_PATTERNS: &[&str] = &[
    // nub (own engine) — %LOCALAPPDATA% is `~/AppData/Local` by default
    "~/AppData/Local/nub/store",
    "~/AppData/Local/nub/pm",
    "~/AppData/Local/nub",
    "~/AppData/Roaming/nub",
    "~/.cache/nub",
    "~/.config/nub",
    // JS package managers
    "~/AppData/Local/npm-cache",
    "~/AppData/Roaming/npm",
    "~/AppData/Local/pnpm",
    "~/AppData/Local/pnpm-cache",
    "~/AppData/Local/pnpm-state",
    "~/.pnpm",
    "~/.pnpm-cache",
    "~/.pnpm-state",
    "~/.config/pnpm",
    "~/AppData/Local/Yarn",
    "~/AppData/Roaming/Yarn",
    "~/.yarn",
    "~/.bun/install",
    // Python
    "~/AppData/Local/pip",
    "~/AppData/Roaming/pip",
    "~/pip",
    "~/AppData/Roaming/Python",
    "~/AppData/Local/uv",
    "~/AppData/Roaming/uv",
    "~/.local/bin",
    // Other toolchains
    "~/.cargo",
    "~/.rustup",
    "~/go",
    "~/AppData/Local/go-build",
    "~/AppData/Roaming/go",
    "~/.gradle",
    "~/.m2",
    "~/.nuget",
    "~/.dotnet",
    "~/AppData/Local/NuGet",
    "~/AppData/Roaming/NuGet",
    "~/AppData/Local/Composer",
    "~/AppData/Roaming/Composer",
    "~/.config/git",
    "~/.cache/git",
    "~/.git-credential-cache",
];

// Linux + any other unix (freebsd, …): the XDG layout.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const TOOLDIR_PATTERNS: &[&str] = &[
    // nub (own engine)
    "~/.local/share/nub/store",
    "~/.cache/nub",
    "~/.config/nub",
    "$cache/nub/pm",
    // JS package managers
    "~/.npm",
    "~/.local/share/pnpm",
    "~/.cache/pnpm",
    "~/.config/pnpm",
    "~/.local/state/pnpm",
    "~/.cache/yarn",
    "~/.config/yarn",
    "~/.yarn",
    "~/.bun/install",
    // Python
    "~/.cache/pip",
    "~/.cache/uv",
    "~/.config/pip",
    "~/.pip",
    "~/.local/lib",
    "~/.local/share/uv",
    "~/.config/uv",
    "~/.local/bin",
    // Other toolchains
    "~/.cargo",
    "~/.rustup",
    "~/go",
    "$cache/go-build",
    "~/.config/go",
    "~/.gradle",
    "~/.m2",
    "~/.nuget",
    "~/.dotnet",
    "~/.local/share/NuGet",
    "~/.cache/composer",
    "~/.composer",
    "~/.config/composer",
    "~/.config/git",
    "~/.cache/git",
    "~/.git-credential-cache",
];

/// File-shaped state is exact, rather than a subtree root: Git replaces the global config
/// through its adjacent lock file, so both leaf names are needed without granting `~`.
const TOOLDIR_FILE_PATTERNS: &[&str] = &[
    "~/.gitconfig",
    "~/.gitconfig.lock",
    "~/.git-credentials",
    #[cfg(not(windows))]
    "~/.mavenrc",
    #[cfg(windows)]
    "~/mavenrc.cmd",
    #[cfg(windows)]
    "~/mavenrc_pre.cmd",
    #[cfg(windows)]
    "~/mavenrc_post.cmd",
    #[cfg(windows)]
    "~/mavenrc_pre.bat",
    #[cfg(windows)]
    "~/mavenrc_post.bat",
];

/// The per-OS `$tooldirs` surface patterns (host OS == target OS).
pub fn tooldir_patterns() -> &'static [&'static str] {
    TOOLDIR_PATTERNS
}

/// Add a non-empty documented relocation from the already-approved ambient snapshot.
/// The compiler never invokes a package manager or parses its configuration to find a path.
fn env_path(env: &BTreeMap<String, String>, name: &str, out: &mut BTreeSet<String>) {
    if let Some(value) = env.get(name).filter(|value| !value.is_empty()) {
        out.insert(value.clone());
    }
}

fn env_subpaths(
    env: &BTreeMap<String, String>,
    name: &str,
    suffixes: &[&str],
    out: &mut BTreeSet<String>,
) {
    let Some(root) = env.get(name).filter(|value| !value.is_empty()) else {
        return;
    };
    let root = root.trim_end_matches(['/', '\\']);
    for suffix in suffixes {
        out.insert(format!("{root}/{suffix}"));
    }
}

fn environment_tooldirs(env: &BTreeMap<String, String>) -> BTreeSet<String> {
    // Windows tool processes receive a case-insensitive environment block.
    #[cfg(windows)]
    let env = &env
        .iter()
        .map(|(key, value)| (key.to_ascii_uppercase(), value.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut paths = BTreeSet::new();
    for name in [
        // Existing embedder cache override and neutral PM storage settings.
        "NUB_CACHE_DIR",
        "NPM_CONFIG_CACHE_DIR",
        "NPM_CONFIG_STORE_DIR",
        "NPM_CONFIG_VIRTUAL_STORE_DIR",
        "NPM_CONFIG_GLOBAL_VIRTUAL_STORE_DIR",
        // JavaScript package managers.
        "NPM_CONFIG_CACHE",
        "NPM_CONFIG_PREFIX",
        "NPM_CONFIG_USERCONFIG",
        "NPM_CONFIG_GLOBALCONFIG",
        "PNPM_HOME",
        "PNPM_CONFIG_STORE_DIR",
        "PNPM_CONFIG_CACHE_DIR",
        "PNPM_CONFIG_STATE_DIR",
        "PNPM_CONFIG_CONFIG_DIR",
        "YARN_CACHE_FOLDER",
        "YARN_GLOBAL_FOLDER",
        "BUN_INSTALL",
        "BUN_INSTALL_CACHE_DIR",
        "BUN_INSTALL_GLOBAL_DIR",
        "BUN_INSTALL_BIN",
        // Other package-manager families.
        "PIP_CACHE_DIR",
        "PIP_CONFIG_FILE",
        "PYTHONUSERBASE",
        "UV_CACHE_DIR",
        "UV_TOOL_DIR",
        "UV_TOOL_BIN_DIR",
        "UV_PYTHON_INSTALL_DIR",
        "UV_PYTHON_BIN_DIR",
        "UV_INSTALL_DIR",
        "UV_PROJECT_ENVIRONMENT",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "CARGO_TARGET_DIR",
        "GOMODCACHE",
        "GOCACHE",
        "GOBIN",
        "GOTMPDIR",
        "GRADLE_USER_HOME",
        "NUGET_PACKAGES",
        "NUGET_HTTP_CACHE_PATH",
        "NUGET_SCRATCH",
        "NUGET_PLUGINS_CACHE_PATH",
        "DOTNET_CLI_HOME",
        "DOTNET_BUNDLE_EXTRACT_BASE_DIR",
        "COMPOSER_HOME",
        "COMPOSER_CACHE_DIR",
        "COMPOSER_VENDOR_DIR",
        "COMPOSER_BIN_DIR",
        "GIT_DIR",
        "GIT_COMMON_DIR",
        "GIT_CONFIG_GLOBAL",
        "GIT_TEMPLATE_DIR",
        "GIT_EXEC_PATH",
    ] {
        env_path(env, name, &mut paths);
    }
    // GOPATH alone is list-valued (colon-separated on Unix, semicolon-separated on
    // Windows). Each entry is an independent tool root, never one literal path.
    if let Some(value) = env.get("GOPATH").filter(|value| !value.is_empty()) {
        for path in std::env::split_paths(value) {
            if !path.as_os_str().is_empty() {
                paths.insert(path.to_string_lossy().into_owned());
            }
        }
    }
    // npm accepts its documented config environment names in lower case as well.
    for name in [
        "npm_config_cache",
        "npm_config_prefix",
        "npm_config_userconfig",
        "npm_config_globalconfig",
        "npm_config_cache_dir",
        "npm_config_store_dir",
        "npm_config_virtual_store_dir",
        "npm_config_global_virtual_store_dir",
    ] {
        env_path(env, name, &mut paths);
    }
    if env.get("GOENV").is_some_and(|value| value != "off") {
        env_path(env, "GOENV", &mut paths);
    }
    // Standard roots add their tool-specific children, never the whole root.
    env_subpaths(
        env,
        "XDG_CACHE_HOME",
        &[
            "nub", "pnpm", "yarn", "pip", "uv", "go-build", "composer", "git",
        ],
        &mut paths,
    );
    env_subpaths(
        env,
        "XDG_DATA_HOME",
        &["nub/store", "pnpm", "yarn/berry", "pip", "uv", "NuGet"],
        &mut paths,
    );
    env_subpaths(
        env,
        "XDG_CONFIG_HOME",
        &["nub", "pnpm", "yarn", "pip", "uv", "composer", "git", "go"],
        &mut paths,
    );
    env_subpaths(env, "XDG_STATE_HOME", &["pnpm"], &mut paths);
    env_path(env, "XDG_BIN_HOME", &mut paths);
    // uv falls back to this executable directory when XDG_BIN_HOME is absent.
    if let Some(root) = env.get("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
        paths.insert(format!("{root}/../bin"));
    }
    // A redirected Windows profile is not represented by Homes, so use the
    // embedder-approved environment rather than assuming ~/AppData defaults.
    #[cfg(windows)]
    env_subpaths(
        env,
        "LOCALAPPDATA",
        &[
            "nub",
            "npm-cache",
            "pnpm",
            "pnpm-cache",
            "pnpm-state",
            "Yarn",
            "pip",
            "uv",
            "go-build",
            "NuGet",
            "Composer",
        ],
        &mut paths,
    );
    #[cfg(windows)]
    env_subpaths(
        env,
        "APPDATA",
        &[
            "nub", "npm", "Yarn", "pip", "Python", "uv", "NuGet", "Composer", "go",
        ],
        &mut paths,
    );
    paths
}

/// Expand the default roots without environment relocations for unit controls.
#[cfg(test)]
pub fn tooldirs_fs_rules(homes: &Homes, effect: Effect, access: FsAccess) -> Vec<FsRule> {
    tooldirs_fs_rules_with_env(homes, &BTreeMap::new(), effect, access)
        .expect("the audited static $tooldirs roots are never filesystem roots")
}

/// Expand an environment-provided root without treating its whitespace as syntax. Unlike a
/// policy pattern, an environment value is a literal OS path: a relative value is anchored
/// once to the project, while a leading/trailing space is a valid path character.
fn expand_environment_root(root: &str, homes: &Homes) -> String {
    let normalized = normalize_slashes(root);
    let bytes = normalized.as_bytes();
    let windows_absolute =
        bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/';
    if normalized.starts_with('/') || windows_absolute {
        normalized
    } else if root == "~" || root.starts_with("~/") || root.starts_with("~\\") {
        expand_symbolic(root, homes)
    } else {
        format!(
            "{}/{}",
            homes
                .project
                .to_string_lossy()
                .trim_end_matches(['/', '\\']),
            normalized
        )
    }
}

/// Whether an expanded root names an entire filesystem (or a complete UNC share), which
/// `$tooldirs` must never turn into a convenience-set grant.
fn is_filesystem_root(path: &str) -> bool {
    if path == "/" || path == "\\" {
        return true;
    }
    let bytes = path.as_bytes();
    if bytes.len() == 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/' {
        return true;
    }
    let mut pieces = path.split('/');
    path.starts_with("//")
        && pieces.next() == Some("")
        && pieces.next() == Some("")
        && pieces.next().is_some_and(|piece| !piece.is_empty())
        && pieces.next().is_some_and(|piece| !piece.is_empty())
        && pieces.next().is_none()
}

/// Expand `$tooldirs` from per-OS defaults plus documented environment relocations.
/// Empty variables add no rule. The variables are values the embedder already approved
/// for this [`CompileCtx`](super::CompileCtx), not tool configuration discovered here.
pub fn tooldirs_fs_rules_with_env(
    homes: &Homes,
    env: &BTreeMap<String, String>,
    effect: Effect,
    access: FsAccess,
) -> Result<Vec<FsRule>, String> {
    let access = if effect == Effect::Deny {
        FsAccess::DENY
    } else {
        access
    };
    let mut out = Vec::new();
    #[cfg(unix)]
    if effect == Effect::Allow {
        // Bun 1.3 enumerates project ancestors and the conventional temp root.
        // A bare directory node grants listing, not file contents or writes.
        let project = canonicalize_glob_prefix(&homes.project.to_string_lossy());
        let nodes: BTreeSet<_> = std::path::Path::new(&project)
            .ancestors()
            .skip(1)
            .take_while(|path| path.parent().is_some())
            .map(|path| path.to_string_lossy().into_owned())
            .chain([canonicalize_glob_prefix("/tmp")])
            .filter(|path| !is_filesystem_root(path))
            .collect();
        for node in nodes {
            out.push(FsRule {
                matcher: CanonGlob(node),
                effect,
                access: FsAccess::Read,
                origin: FsOrigin::Speculative,
            });
        }
    }
    let environment_patterns = environment_tooldirs(env);
    #[cfg(unix)]
    for pattern in ["/tmp/.dotnet"] {
        for g in super::defaults::subtree_globs(pattern) {
            out.push(FsRule {
                matcher: CanonGlob(canonicalize_glob_prefix(&g)),
                effect,
                access,
                origin: FsOrigin::SharedToolState,
            });
        }
    }
    for pattern in tooldir_patterns() {
        let expanded = expand_symbolic(pattern, homes);
        for g in super::defaults::subtree_globs(&expanded) {
            out.push(FsRule {
                matcher: CanonGlob(canonicalize_glob_prefix(&g)),
                effect,
                access,
                origin: FsOrigin::Speculative,
            });
        }
    }
    for pattern in TOOLDIR_FILE_PATTERNS {
        let expanded = expand_symbolic(pattern, homes);
        out.push(FsRule {
            matcher: CanonGlob(canonicalize_glob_prefix(&expanded)),
            effect,
            access,
            origin: FsOrigin::Speculative,
        });
    }
    for pattern in environment_patterns {
        let expanded = expand_environment_root(&pattern, homes);
        if is_filesystem_root(&expanded) {
            return Err(format!(
                "environment relocation `{pattern}` resolves to a filesystem root, which `$tooldirs` cannot grant"
            ));
        }
        for g in super::defaults::subtree_globs(&expanded) {
            out.push(FsRule {
                matcher: CanonGlob(canonicalize_glob_prefix(&g)),
                effect,
                access,
                origin: FsOrigin::Speculative,
            });
        }
    }
    Ok(out)
}

/// Own-process information used by Node, Bun and .NET. These are capability
/// markers extracted by the fold, not static grants on the compiling process.
#[cfg(target_os = "linux")]
pub(crate) fn tool_metadata_rules() -> impl Iterator<Item = FsRule> {
    [
        "maps",
        "stat",
        "statm",
        "status",
        "cmdline",
        "task",
        "task/*/stat",
        "task/*/status",
    ]
    .into_iter()
    .map(|suffix| FsRule {
        matcher: CanonGlob(format!("/proc/self/{suffix}")),
        effect: Effect::Allow,
        access: FsAccess::Read,
        origin: FsOrigin::Speculative,
    })
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
    fn no_exfiltration_sink_leaked_into_trusted() {
        // Re-syncing the Claude Code base list must not reintroduce a host the confined
        // process can upload to. Two shapes, both disqualifying: an authenticated
        // write-back route, and rentable path/subdomain tenancy.
        for banned in [
            // Multi-tenant object stores.
            "*.amazonaws.com",
            "storage.googleapis.com",
            "*.googleapis.com",
            "*.blob.core.windows.net",
            // Write-back APIs and per-account tenancy (the Shai-Hulud exfil channel).
            // Only the WRITE surface belongs here: the read-only content CDNs on the same
            // brand are trusted, and `delivery_hosts_are_trusted` below pins that split.
            "github.com",
            "www.github.com",
            "api.github.com",
            "gist.github.com",
            "*.github.io",
            // Serves package tarballs, but is a bare Azure Blob account: PUT of a blob
            // path answers 409 PublicAccessNotPermitted where PUT / answers 400, so the
            // storage write API — not a CDN — is what terminates this hostname.
            "pkg-npm.githubusercontent.com",
            // Package registries whose publish route shares the listed hostname.
            // `index.rubygems.org` is the trap: it looks like a read-only index and
            // fronts the same Rails app that serves `gem push`.
            "crates.io",
            "rubygems.org",
            "api.rubygems.org",
            "index.rubygems.org",
            "nuget.org",
            "hex.pm",
            "pub.dev",
            "packagist.org",
            "plugins.gradle.org",
            "upload.pypi.org",
            "anaconda.org",
            "repo.spring.io",
            // A wildcard cannot be shown read-only by inspection: the docs site at the
            // apex is inert, but `registry.` under the same suffix answers /v0/publish.
            "*.modelcontextprotocol.io",
            // Container registries — every one of these is a `docker push` target. The
            // retained Docker entries are artifact CDNs (`download.docker.com`), not
            // registries; that is the whole distinction, and it is easy to lose.
            "ghcr.io",
            "registry-1.docker.io",
            "public.ecr.aws",
            // Not a push target, but its API creates repositories carrying an
            // attacker-authored description.
            "hub.docker.com",
            // Telemetry ingest — an attacker-shaped event payload is exfiltration with a
            // vendor SDK in front of it. `downloads.sentry-cdn.com` is retained because it
            // serves artifacts; the ingest hostnames are a different surface.
            "sentry.io",
            "*.datadoghq.com",
            "api.statsig.com",
            // Non-GitHub forges and cloud control planes — the same per-account write
            // surface that disqualifies `github.com`, minus the name that makes it obvious.
            // A control plane exfiltrates by storing the secret in a resource field the
            // attacker reads back, so it needs no object store of its own.
            "gitlab.com",
            "bitbucket.org",
            "dev.azure.com",
            "sourceforge.net",
            "compute.googleapis.com",
            // Siblings and `www.` twins of RETAINED hosts. The likeliest re-sync mistake:
            // each of these reads as a host already on the list, but fronts its own upload
            // route (`pypi.org` is trusted, `test.pypi.org` is not).
            "test.pypi.org",
            "hackage.haskell.org",
            "npm.pkg.github.com",
            "www.crates.io",
        ] {
            assert!(
                !TRUSTED_HOSTS.contains(&banned),
                "`{banned}` is an exfiltration sink and must not be in $trusted"
            );
        }
    }

    #[test]
    fn delivery_only_hosts_stay_trusted() {
        // The counterweight to the banned list: each of these serves attacker-authorable
        // bytes INTO the sandbox and accepts none back, so an audit that cuts them has
        // silently swapped the exfiltration criterion for an integrity one. They were cut
        // on exactly that mistake once. Every entry answers a write with the same status
        // on a real path as on a bogus one, which is a host refusing the method rather
        // than an auth-gated route declining a caller.
        for delivery in [
            "codeload.github.com",
            "raw.githubusercontent.com",
            "objects.githubusercontent.com",
            "release-assets.githubusercontent.com",
            "avatars.githubusercontent.com",
            "camo.githubusercontent.com",
        ] {
            assert!(
                TRUSTED_HOSTS.contains(&delivery),
                "`{delivery}` only DELIVERS bytes and cannot store them — cutting it \
                 confuses supply-chain integrity with exfiltration (see the module doc)"
            );
        }
    }

    /// The catalog changed WHERE the host list is written, and must not have changed WHAT
    /// it admits. Frozen as a literal rather than re-read from the JSON, so both sides
    /// cannot agree on the same bad parse — and ORDER-SENSITIVE, because the expansion in
    /// `download_net_rules` emits one rule per host in list order and the IR is compared
    /// byte-wise elsewhere.
    #[test]
    fn the_catalog_reproduces_the_pre_catalog_host_list() {
        assert_eq!(
            DOWNLOAD_HOSTS,
            [
                "nodejs.org",
                "binaries.prisma.sh",
                "download.cypress.io",
                "cdn.cypress.io",
            ],
            "the generated $downloads set diverged from the hand-written one it replaced"
        );
    }

    #[test]
    fn every_download_host_is_a_valid_wildcard_free_pattern() {
        // Wildcard-freedom is the set's structural anti-exfiltration property, not a
        // formatting preference: the proxy resolves the name, so a `*.suffix` member would
        // let a confined script put chosen bytes in a DNS label and leak them to an
        // attacker-run nameserver without sending a payload at all.
        for h in DOWNLOAD_HOSTS {
            assert!(
                host_pattern_is_valid(h),
                "`{h}` is not a valid host pattern for $downloads"
            );
            assert!(
                !h.contains('*'),
                "`{h}` — $downloads must stay wildcard-free: a subdomain wildcard admits \
                 DNS-label exfiltration under the same hostname"
            );
        }
    }

    #[test]
    fn no_write_capable_or_multi_tenant_host_leaked_into_downloads() {
        // The set is meant to grow by PR as more install-time downloaders are covered.
        // These are the shapes such a PR must never add: a host the confined script can
        // upload to, and a namespace an attacker can rent under the same hostname and read
        // back. The GitHub-release and object-store families are UNSOLVED here on purpose —
        // they need pre-download brokering, not an allowlist entry.
        for banned in [
            "storage.googleapis.com",
            "*.googleapis.com",
            "*.amazonaws.com",
            "*.blob.core.windows.net",
            "github.com",
            "api.github.com",
            "codeload.github.com",
            "objects.githubusercontent.com",
            "raw.githubusercontent.com",
            "*.github.io",
            "registry.npmjs.org",
            "ghcr.io",
            "sentry.io",
        ] {
            assert!(
                !DOWNLOAD_HOSTS.contains(&banned),
                "`{banned}` accepts a write or is multi-tenant — it must not be in $downloads"
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
    fn relocated_os_roots_grant_tool_children_not_the_entire_root() {
        let root = tempfile::tempdir().unwrap();
        let cache = root.path().join("cache");
        let data = root.path().join("data");
        let config = root.path().join("config");
        let env = BTreeMap::from([
            ("XDG_CACHE_HOME".into(), cache.display().to_string()),
            ("XDG_DATA_HOME".into(), data.display().to_string()),
            ("XDG_CONFIG_HOME".into(), config.display().to_string()),
        ]);
        let paths: BTreeSet<PathBuf> = environment_tooldirs(&env)
            .into_iter()
            .map(PathBuf::from)
            .collect();
        for tool in ["nub", "pnpm", "yarn", "pip", "uv", "go-build", "composer"] {
            assert!(paths.contains(&cache.join(tool)));
        }
        assert!(paths.contains(&data.join("nub/store")));
        assert!(paths.contains(&config.join("nub")));
        assert!(paths.contains(&data.join("pip")));
        assert!(paths.contains(&data.join("../bin")));
        assert!(paths.contains(&config.join("yarn")));
        for root in [&cache, &data, &config] {
            assert!(!paths.contains(root));
        }
        let rules =
            tooldirs_fs_rules_with_env(&homes(), &env, Effect::Allow, FsAccess::Read).unwrap();
        assert!(rules.iter().all(|rule| rule.access == FsAccess::Read));
    }

    #[test]
    fn conventional_tool_state_includes_noncache_operations() {
        #[cfg(target_os = "macos")]
        let expected = [
            "~/Library/Caches/pnpm",
            "~/.cache/uv",
            "~/Library/Application Support/uv",
            "~/.local/state/pnpm",
            "~/Library/Python",
            "~/Library/Caches/go-build",
            "~/.local/share/NuGet",
            "~/Library/Caches/composer",
            "~/.git-credential-cache",
        ];
        #[cfg(target_os = "windows")]
        let expected = [
            "~/AppData/Local/pnpm-state",
            "~/.pnpm-state",
            "~/pip",
            "~/AppData/Roaming/Python",
            "~/AppData/Roaming/NuGet",
            "~/.config/git",
            "~/.git-credential-cache",
        ];
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let expected = [
            "~/.config/yarn",
            "~/.pip",
            "~/.local/lib",
            "~/.git-credential-cache",
        ];
        for path in expected {
            assert!(tooldir_patterns().contains(&path), "missing {path}");
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
    fn go_environment_file_override_is_a_path_except_when_disabled() {
        for value in ["", "off"] {
            let env = BTreeMap::from([("GOENV".into(), value.into())]);
            assert!(environment_tooldirs(&env).is_empty());
        }
        let env = BTreeMap::from([("GOENV".into(), "/fixture/go-env".into())]);
        assert_eq!(
            environment_tooldirs(&env),
            BTreeSet::from(["/fixture/go-env".into()])
        );
    }

    #[test]
    fn dotnet_supporting_state_uses_documented_environment_locations() {
        assert!(tooldir_patterns().contains(&"~/.dotnet"));
        let env = BTreeMap::from([
            ("DOTNET_CLI_HOME".into(), "/fixture/dotnet-state".into()),
            (
                "DOTNET_BUNDLE_EXTRACT_BASE_DIR".into(),
                "/fixture/extracted-bundles".into(),
            ),
        ]);
        let paths = environment_tooldirs(&env);
        assert!(paths.contains("/fixture/dotnet-state"));
        assert!(paths.contains("/fixture/extracted-bundles"));
        assert!(!paths.contains("/fixture"));
    }

    #[test]
    fn deny_effect_normalizes_access_to_the_inert_value() {
        // An internal deny effect carries the canonical inert access (D20), same as the fs funnel.
        let rules = tooldirs_fs_rules(&homes(), Effect::Deny, FsAccess::ReadWrite);
        assert!(
            rules
                .iter()
                .all(|r| r.effect == Effect::Deny && r.access == FsAccess::DENY)
        );
    }
}
