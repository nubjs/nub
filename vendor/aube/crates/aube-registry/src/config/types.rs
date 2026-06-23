use std::collections::BTreeMap;
use std::path::PathBuf;

/// Where a single `.npmrc`-shaped entry came from. `apply_tagged`
/// uses this to decide whether an individual setting is trusted
/// enough to take effect. Matches pnpm 10.27.0's fix for
/// CVE-2025-69262. Settings that drive subprocess execution
/// (currently `tokenHelper`) are accepted only from user scope
/// sources. A project `.npmrc` that a hostile repo committed does
/// not qualify.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NpmrcSource {
    /// The builtin `npmrc` shipped next to the npm CLI (npm's
    /// `BUILTIN_CONFIG`, `resolve(npmPath, 'npmrc')`). System/admin
    /// controlled — a value here was baked into the installed toolchain,
    /// so it is at least as trusted as the user's own `.npmrc`. Lowest
    /// precedence of every file source.
    Builtin,
    /// The global `npmrc` (`$PREFIX/etc/npmrc`, or `NPM_CONFIG_GLOBALCONFIG`).
    /// System/admin controlled — corporate and CI machines set the
    /// registry, proxy, cafile, and auth here. Trusted at >= user level
    /// (an admin-set global is at least as trusted as the user's own
    /// config). Sits below user/project in npm's precedence cascade.
    Global,
    /// `~/.npmrc`. The developer's personal config. Trusted.
    User,
    /// `~/.config/pnpm/auth.ini`. pnpm's global auth file. Trusted
    /// (same filesystem scope as the user `.npmrc`).
    PnpmAuth,
    /// `<project>/.npmrc`. Committed alongside the project and
    /// therefore attacker controlled when the project came from a
    /// hostile clone.
    Project,
    /// A file pointed at by a user/global `npmrc-auth-file` setting.
    /// Trusted because the user chose the file location.
    UserNpmrcAuthFile,
    /// A file pointed at by a project `npmrc-auth-file` setting. The
    /// path itself came from committed config, so the file's contents
    /// inherit the project trust level.
    ProjectNpmrcAuthFile,
    /// Environment variable. `npm_config_*` / `NPM_CONFIG_*`.
    /// Trusted because the developer or their CI pipeline has to
    /// set them explicitly in the shell that invoked aube.
    Env,
}

impl NpmrcSource {
    /// Whether a setting from this source is allowed to configure
    /// subprocess spawning (e.g. `tokenHelper`). `Project` and
    /// `ProjectNpmrcAuthFile` both return false since both are
    /// reachable from a hostile repo clone.
    pub(super) fn is_trusted_for_subprocess_settings(self) -> bool {
        matches!(
            self,
            Self::Builtin
                | Self::Global
                | Self::User
                | Self::PnpmAuth
                | Self::UserNpmrcAuthFile
                | Self::Env
        )
    }

    pub(super) fn is_project_controlled(self) -> bool {
        matches!(self, Self::Project | Self::ProjectNpmrcAuthFile)
    }
}

/// Parsed npm configuration from .npmrc files.
///
/// Only holds the *registry-client specific* fields — registry URL, auth,
/// scoped overrides. Generic pnpm settings (`auto-install-peers`,
/// `node-linker`, etc) are resolved by `aube_cli::settings_values` against
/// the raw `.npmrc` entries returned by [`load_npmrc_entries`], so that
/// the canonical list of source keys lives in `settings.toml` and adding
/// a new setting is a one-place change.
#[derive(Debug, Clone)]
pub struct NpmConfig {
    /// Default registry URL (e.g., "https://registry.npmjs.org/")
    pub registry: String,
    /// Scoped registry overrides: "@scope" -> "https://registry.example.com/"
    pub scoped_registries: BTreeMap<String, String>,
    /// Auth config keyed by registry URL prefix (e.g., "//registry.example.com/")
    pub auth_by_uri: BTreeMap<String, AuthConfig>,
    /// Scope-specific auth config keyed by registry URL prefix, then
    /// package scope (e.g., "//npm.pkg.github.com/" -> "@org").
    pub scoped_auth_by_uri: BTreeMap<String, BTreeMap<String, AuthConfig>>,
    /// Proxy URL for outgoing HTTPS requests (`https-proxy` / `HTTPS_PROXY`).
    pub https_proxy: Option<String>,
    /// Proxy URL for outgoing HTTP requests (`proxy` / `http-proxy` / `HTTP_PROXY`).
    pub http_proxy: Option<String>,
    /// Comma-separated list of hosts that bypass the proxy
    /// (`noproxy` / `NO_PROXY`). Passed through to
    /// `reqwest::NoProxy::from_string` verbatim so wildcards and
    /// port-qualified hosts behave the same as curl / node.
    pub no_proxy: Option<String>,
    /// Validate TLS certificates. Defaults to `true`. Setting this to
    /// `false` disables certificate verification entirely — only useful
    /// behind corporate MITM proxies with an untrusted CA.
    pub strict_ssl: bool,
    /// Local interface IP to bind outgoing connections to
    /// (`local-address`). Parsed as `IpAddr`; unparseable values are
    /// dropped at load time and logged.
    pub local_address: Option<std::net::IpAddr>,
    /// Maximum concurrent connections per origin (`maxsockets`).
    /// Plumbed into reqwest's `pool_max_idle_per_host`, which is the
    /// closest analogue to npm/pnpm's per-origin socket cap.
    pub max_sockets: Option<usize>,
    /// Top-level `cafile=...` from `.npmrc`. Applied to every HTTP
    /// client built from this config (default + per-registry), matching
    /// npm/pnpm semantics where an unscoped `cafile` augments the trust
    /// store for all registries. Per-registry `//host/:cafile=...`
    /// stacks on top via [`AuthConfig::tls`].
    pub cafile: Option<PathBuf>,
    /// Top-level inline `ca=...` / `ca[]=...` PEM strings from
    /// `.npmrc`. Same semantics as [`Self::cafile`].
    pub ca: Vec<String>,
    /// Value of `.npmrc`'s legacy `proxy=` key, tracked separately
    /// from `https_proxy` / `http_proxy` because pnpm treats it as
    /// the fallback for `httpsProxy` (and secondarily for
    /// `httpProxy`). Resolved into the final `https_proxy` /
    /// `http_proxy` values during `apply_proxy_env`.
    pub npmrc_proxy: Option<String>,
    /// Top-level (unscoped) `always-auth=true` default. npm v6 honored a
    /// bare `always-auth`; it applies to the default registry. A
    /// per-registry `//host/:always-auth` on [`AuthConfig::always_auth`]
    /// takes precedence for that host. Defaults to `false`.
    pub always_auth: bool,
}

/// Authentication for a specific registry.
#[derive(Debug, Clone, Default)]
pub struct AuthConfig {
    pub auth_token: Option<String>,
    /// Base64-encoded "username:password"
    pub auth: Option<String>,
    pub username: Option<String>,
    /// npm stores the split-field password as base64-encoded bytes.
    pub password: Option<String>,
    pub token_helper: Option<String>,
    pub tls: TlsConfig,
    /// `always-auth` (`//host/:always-auth=true`, or npm's bare
    /// `always-auth`, or Yarn's `npmAlwaysAuth`). When set, this
    /// registry's credentials are attached to every request for it —
    /// including tarball downloads hosted on a *different* origin than
    /// the registry, which are otherwise sent unauthenticated. Defaults
    /// to `false` (npm's default: auth is not sent cross-origin).
    pub always_auth: bool,
}

impl AuthConfig {
    pub(crate) fn has_tls_material(&self) -> bool {
        !self.tls.ca.is_empty()
            || self.tls.cafile.is_some()
            || self.tls.cert.is_some()
            || self.tls.key.is_some()
    }
}

#[derive(Debug, Clone, Default)]
pub struct TlsConfig {
    pub ca: Vec<String>,
    pub cafile: Option<PathBuf>,
    pub cert: Option<String>,
    pub key: Option<String>,
}

impl Default for NpmConfig {
    /// Hand-rolled so `strict_ssl` defaults to `true` instead of
    /// `bool::default()` / `false`. Any caller that builds an
    /// `NpmConfig` via `..Default::default()` (including
    /// `RegistryClient::new`) gets a TLS-validating client without
    /// having to remember to flip this field — the unsafe default is
    /// too easy to foot-gun otherwise.
    fn default() -> Self {
        Self {
            registry: String::new(),
            scoped_registries: BTreeMap::new(),
            auth_by_uri: BTreeMap::new(),
            scoped_auth_by_uri: BTreeMap::new(),
            https_proxy: None,
            http_proxy: None,
            no_proxy: None,
            strict_ssl: true,
            local_address: None,
            max_sockets: None,
            cafile: None,
            ca: Vec::new(),
            npmrc_proxy: None,
            always_auth: false,
        }
    }
}
