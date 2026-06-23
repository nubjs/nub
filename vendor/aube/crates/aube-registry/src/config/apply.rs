use base64::Engine as _;
use std::collections::BTreeSet;
use std::path::PathBuf;

use super::env::env_any;
use super::token::sanitize_token_helper;
use super::types::{AuthConfig, NpmConfig, NpmrcSource};
use super::url::{
    is_public_npmjs_url, lookup_by_uri_prefix, normalize_npmrc_uri_key, normalize_registry_url,
    package_scope, registry_uri_key,
};
use super::util::{non_empty, pem_value};

impl NpmConfig {
    /// Register default scope→registry mappings that aube ships with
    /// out of the box. Currently only `@jsr` → <https://npm.jsr.io/>,
    /// which lets `jsr:` specs work without the user touching `.npmrc`.
    /// User-provided `.npmrc` entries win — `apply` has already run by
    /// the time we get here, so we only fill in gaps.
    ///
    /// These compiled-in defaults (the npmjs default registry, the `@jsr`
    /// scope below) are aube's *own* baseline, applied beneath every
    /// on-disk source so a user `.npmrc` always wins. They are distinct
    /// from npm's on-disk builtin/global `npmrc`, which the loader now
    /// reads as their own scopes ([`load_npmrc_entries_tagged_with_globals`]):
    /// the full npm file cascade — builtin < global < user < project — is
    /// honored when the npm install prefix is locatable
    /// (`NPM_CONFIG_PREFIX` / `PREFIX` / `NPM_CONFIG_GLOBALCONFIG`). When
    /// no prefix can be determined (aube embedded with no npm install in
    /// view) those scopes are simply absent and these compiled-in defaults
    /// are the only baseline. pnpm's global `config.yaml` and `auth.ini`
    /// are handled separately.
    pub(super) fn apply_builtin_scoped_defaults(&mut self) {
        self.scoped_registries
            .entry(crate::jsr::JSR_NPM_SCOPE.to_string())
            .or_insert_with(|| crate::jsr::JSR_DEFAULT_REGISTRY.to_string());
    }

    /// Fallback-only: populate proxy/no_proxy from the standard
    /// `HTTPS_PROXY` / `HTTP_PROXY` / `NO_PROXY` environment variables
    /// when the `.npmrc` layer didn't already set them. A value from
    /// `.npmrc` wins over env so project configuration stays explicit.
    /// Resolve proxy/no_proxy fields using the same precedence
    /// chain pnpm's config reader applies (see
    /// `config/reader/src/index.ts` lines 559-568 in the pnpm
    /// repo):
    ///
    /// - `httpsProxy` ← `.npmrc httpsProxy` ?? `.npmrc proxy` ??
    ///   env `HTTPS_PROXY`/`https_proxy`
    /// - `httpProxy` ← `.npmrc httpProxy` ?? resolved `httpsProxy`
    ///   ?? env `HTTP_PROXY`/`http_proxy` ?? env `PROXY`/`proxy`
    /// - `noProxy` ← `.npmrc noProxy` ?? env `NO_PROXY`/`no_proxy`
    ///
    /// Note that `httpsProxy` does **not** fall back to
    /// `HTTP_PROXY`: pnpm (and npm) only inherit the HTTP proxy
    /// downward into HTTPS, never upward. The `httpProxy` field
    /// *does* inherit whatever `httpsProxy` resolved to, so a
    /// single `https-proxy=...` line in `.npmrc` configures both.
    pub fn apply_proxy_env(&mut self) {
        if self.https_proxy.is_none() {
            self.https_proxy = self
                .npmrc_proxy
                .clone()
                .or_else(|| env_any(&["HTTPS_PROXY", "https_proxy"]));
        }
        if self.http_proxy.is_none() {
            self.http_proxy = self
                .https_proxy
                .clone()
                .or_else(|| env_any(&["HTTP_PROXY", "http_proxy"]))
                .or_else(|| env_any(&["PROXY", "proxy"]));
        }
        if self.no_proxy.is_none() {
            self.no_proxy = env_any(&["NO_PROXY", "no_proxy"]);
        }
    }

    /// Get the registry URL for a given package name.
    pub fn registry_for(&self, package_name: &str) -> &str {
        if let Some(scope) = package_scope(package_name)
            && let Some(url) = self.scoped_registries.get(&scope.to_lowercase())
        {
            return url;
        }
        &self.registry
    }

    /// True when `package_name` resolves through the public
    /// `registry.npmjs.org` registry. Used by supply-chain gates
    /// (`crates/aube/src/commands/add_supply_chain.rs`) to skip
    /// public-only signals (OSV `MAL-*` advisories, npmjs weekly
    /// downloads) on packages a private/internal registry is the
    /// source of truth for. The default registry being swapped out
    /// (`registry=https://internal.example/`) or a scoped override
    /// (`@myorg:registry=https://...`) both cause this to return
    /// `false` so internal packages don't trip the gates.
    pub fn is_public_npmjs(&self, package_name: &str) -> bool {
        is_public_npmjs_url(self.registry_for(package_name))
    }

    /// Get the auth token for a given registry URL.
    pub fn auth_token_for(&self, registry_url: &str) -> Option<&str> {
        self.registry_config_for(registry_url)
            .and_then(|auth| auth.auth_token.as_deref())
    }

    /// Get the auth token for a package request, preferring
    /// scope-specific credentials when `.npmrc` configured
    /// `//registry/:@scope:_authToken=...`.
    pub fn auth_token_for_package(&self, registry_url: &str, package_name: &str) -> Option<&str> {
        self.registry_config_for_package(registry_url, package_name)
            .and_then(|auth| auth.auth_token.as_deref())
    }

    pub fn token_helper_for(&self, registry_url: &str) -> Option<&str> {
        self.registry_config_for(registry_url)
            .and_then(|auth| auth.token_helper.as_deref())
    }

    /// Whether `always-auth` is in effect for `registry_url`: a
    /// per-registry `//host/:always-auth` wins, otherwise the config-wide
    /// top-level default applies. When true, this registry's credentials
    /// should be attached even to off-origin requests (e.g. tarballs on a
    /// separate CDN) that the per-URL lookup would otherwise leave
    /// unauthenticated.
    pub fn always_auth_for(&self, registry_url: &str) -> bool {
        self.registry_config_for(registry_url)
            .map(|auth| auth.always_auth)
            .unwrap_or(false)
            || self.always_auth
    }

    /// Get the basic auth (_auth) for a given registry URL.
    pub fn basic_auth_for(&self, registry_url: &str) -> Option<String> {
        self.basic_auth_from_config(self.registry_config_for(registry_url)?)
    }

    pub fn basic_auth_for_package(&self, registry_url: &str, package_name: &str) -> Option<String> {
        self.basic_auth_from_config(self.registry_config_for_package(registry_url, package_name)?)
    }

    fn basic_auth_from_config(&self, auth: &AuthConfig) -> Option<String> {
        if let Some(ref a) = auth.auth {
            return Some(a.clone());
        }
        let username = auth.username.as_ref()?;
        let password = auth.password.as_ref()?;
        let password = base64::engine::general_purpose::STANDARD
            .decode(password)
            .ok()?;
        let mut raw = Vec::with_capacity(username.len() + 1 + password.len());
        raw.extend_from_slice(username.as_bytes());
        raw.push(b':');
        raw.extend_from_slice(&password);
        Some(base64::engine::general_purpose::STANDARD.encode(raw))
    }

    pub fn registry_config_for(&self, registry_url: &str) -> Option<&AuthConfig> {
        let uri_key = registry_uri_key(registry_url);
        lookup_by_uri_prefix(&self.auth_by_uri, &uri_key)
    }

    pub fn registry_config_for_package(
        &self,
        registry_url: &str,
        package_name: &str,
    ) -> Option<&AuthConfig> {
        if let Some((_, _, auth)) =
            self.scoped_config_for_package_matching(registry_url, package_name, |auth| {
                has_credential_material(auth)
            })
        {
            return Some(auth);
        }
        self.registry_config_for(registry_url)
    }

    pub(crate) fn scoped_tls_config_for_package(
        &self,
        registry_url: &str,
        package_name: &str,
    ) -> Option<(&str, &str, &AuthConfig)> {
        self.scoped_config_for_package_matching(
            registry_url,
            package_name,
            AuthConfig::has_tls_material,
        )
    }

    fn scoped_config_for_package_matching(
        &self,
        registry_url: &str,
        package_name: &str,
        matches: impl Fn(&AuthConfig) -> bool,
    ) -> Option<(&str, &str, &AuthConfig)> {
        if let Some(scope) = package_scope(package_name) {
            let scope = scope.to_lowercase();
            let uri_key = registry_uri_key(registry_url);
            if let Some((_, prefix, scope, auth)) = self
                .scoped_auth_by_uri
                .iter()
                .filter_map(|(prefix, auth_by_scope)| {
                    if uri_key_matches_prefix(&uri_key, prefix) {
                        auth_by_scope.get_key_value(&scope).map(|(scope, auth)| {
                            (prefix.len(), prefix.as_str(), scope.as_str(), auth)
                        })
                    } else {
                        None
                    }
                })
                .filter(|(_, _, _, auth)| matches(auth))
                .max_by_key(|(prefix_len, _, _, _)| *prefix_len)
            {
                return Some((prefix, scope, auth));
            }
        }
        None
    }

    /// Test-only compatibility shim. Production code must go through
    /// `apply_tagged` with real source tags so the subprocess-settings
    /// gate fires correctly. Tests that legitimately emulate a
    /// user-scope-only environment can use this helper to avoid
    /// rewriting every fixture.
    #[cfg(test)]
    pub(super) fn apply(&mut self, entries: Vec<(String, String)>) {
        self.apply_tagged(
            entries
                .into_iter()
                .map(|(k, v)| (NpmrcSource::User, k, v))
                .collect(),
        );
    }

    pub(super) fn apply_tagged(&mut self, entries: Vec<(NpmrcSource, String, String)>) {
        let mut registries = SourceRegistries::default();

        for (source, key, value) in &entries {
            if key == "registry" {
                *registries.slot_mut(*source) = normalize_registry_url(value);
            }
        }

        let mut explicit_uri_fields = BTreeSet::new();
        for (source, key, value) in entries {
            if source.is_project_controlled() && auth_entry_contains_env_ref(&key, &value) {
                tracing::warn!(
                    code = aube_codes::warnings::WARN_AUBE_UNTRUSTED_AUTH_ENV,
                    "ignoring auth setting {key:?} from untrusted source {source:?}: project-controlled `.npmrc` cannot expand environment variables in auth config"
                );
                continue;
            }
            if key == "registry" {
                self.registry = normalize_registry_url(&value);
            } else if key == "_authToken" {
                let registry = registries.slot(source);
                self.rescope_unscoped_registry_setting(
                    source,
                    registry,
                    "_authToken",
                    explicit_uri_fields.contains(&(
                        registry_uri_key(registry),
                        canonical_rescoped_suffix("_authToken").unwrap_or("_authToken"),
                    )),
                    |auth| auth.auth_token = Some(value),
                );
            } else if key == "_auth" {
                let registry = registries.slot(source);
                self.rescope_unscoped_registry_setting(
                    source,
                    registry,
                    "_auth",
                    explicit_uri_fields.contains(&(
                        registry_uri_key(registry),
                        canonical_rescoped_suffix("_auth").unwrap_or("_auth"),
                    )),
                    |auth| auth.auth = Some(value),
                );
            } else if key == "username" {
                let registry = registries.slot(source);
                self.rescope_unscoped_registry_setting(
                    source,
                    registry,
                    "username",
                    explicit_uri_fields.contains(&(
                        registry_uri_key(registry),
                        canonical_rescoped_suffix("username").unwrap_or("username"),
                    )),
                    |auth| auth.username = Some(value),
                );
            } else if key == "_password" {
                let registry = registries.slot(source);
                self.rescope_unscoped_registry_setting(
                    source,
                    registry,
                    "_password",
                    explicit_uri_fields.contains(&(
                        registry_uri_key(registry),
                        canonical_rescoped_suffix("_password").unwrap_or("_password"),
                    )),
                    |auth| auth.password = Some(value),
                );
            } else if key == "always-auth" || key == "always_auth" {
                // Bare `always-auth` is a valid npm v6 top-level key — it
                // sets the config-wide default (applied to the default
                // registry). A per-registry `//host/:always-auth` below
                // takes precedence for that host. No rescope warning: the
                // unscoped spelling is legitimate here, unlike unscoped
                // credentials.
                self.always_auth = parse_npmrc_bool(&value);
            } else if matches!(key.as_str(), "cert" | "key") {
                let suffix = key.clone();
                let registry = registries.slot(source);
                let explicit_uri_field = explicit_uri_fields.contains(&(
                    registry_uri_key(registry),
                    canonical_rescoped_suffix(&suffix).unwrap_or(suffix.as_str()),
                ));
                self.rescope_unscoped_registry_setting(
                    source,
                    registry,
                    &suffix,
                    explicit_uri_field,
                    |auth| {
                        if suffix == "cert" {
                            auth.tls.cert = Some(pem_value(value));
                        } else {
                            auth.tls.key = Some(pem_value(value));
                        }
                    },
                );
            } else if matches!(key.as_str(), "tokenHelper" | "token-helper") {
                if !source.is_trusted_for_subprocess_settings() {
                    tracing::warn!(
                        code = aube_codes::warnings::WARN_AUBE_UNTRUSTED_TOKEN_HELPER,
                        "ignoring tokenHelper from untrusted source {source:?}: committed `.npmrc` cannot set this"
                    );
                    continue;
                }
                let Some(sanitized) = sanitize_token_helper(&value) else {
                    tracing::warn!(
                        code = aube_codes::warnings::WARN_AUBE_INVALID_TOKEN_HELPER,
                        "ignoring tokenHelper: value is not a bare absolute path: {value:?}"
                    );
                    continue;
                };
                let registry = registries.slot(source);
                self.rescope_unscoped_registry_setting(
                    source,
                    registry,
                    "tokenHelper",
                    explicit_uri_fields.contains(&(
                        registry_uri_key(registry),
                        canonical_rescoped_suffix("tokenHelper").unwrap_or("tokenHelper"),
                    )),
                    |auth| auth.token_helper = Some(sanitized),
                );
            } else if matches!(
                key.as_str(),
                "https-proxy"
                    | "httpsProxy"
                    | "http-proxy"
                    | "httpProxy"
                    | "proxy"
                    | "noproxy"
                    | "noProxy"
                    | "no-proxy"
            ) {
                // Proxies redirect every registry request through a
                // third party for the rest of the process. A
                // project-committed `.npmrc` must not be able to set
                // that for everyone who clones the repository, same
                // trust gate `strict-ssl` and `tokenHelper` already
                // apply.
                if !source.is_trusted_for_subprocess_settings() {
                    tracing::warn!(
                        code = aube_codes::warnings::WARN_AUBE_UNTRUSTED_PROXY,
                        "ignoring {key} from untrusted source {source:?}: committed `.npmrc` cannot set registry proxies"
                    );
                } else {
                    match key.as_str() {
                        "https-proxy" | "httpsProxy" => {
                            self.https_proxy = non_empty(value);
                        }
                        "http-proxy" | "httpProxy" => {
                            self.http_proxy = non_empty(value);
                        }
                        "proxy" => {
                            // pnpm treats `.npmrc proxy=` as the
                            // fallback source for `httpsProxy` (and,
                            // transitively, `httpProxy`) — not as a
                            // direct alias for `httpProxy`. See the
                            // `apply_proxy_env` resolution chain.
                            self.npmrc_proxy = non_empty(value);
                        }
                        _ => {
                            self.no_proxy = non_empty(value);
                        }
                    }
                }
            } else if matches!(key.as_str(), "strict-ssl" | "strictSsl") {
                if let Some(b) = aube_settings::parse_bool(&value) {
                    // strict-ssl=false kills TLS cert validation for
                    // the whole client. A project-committed .npmrc
                    // must never flip this for the whole install. Only
                    // user or global scope can disable validation.
                    // Same trust gate tokenHelper already uses.
                    if !b && !source.is_trusted_for_subprocess_settings() {
                        tracing::warn!(
                            code = aube_codes::warnings::WARN_AUBE_UNTRUSTED_STRICT_SSL_DISABLE,
                            "ignoring strict-ssl=false: {source:?} source is not trusted (committed `.npmrc` cannot disable TLS validation)"
                        );
                    } else {
                        self.strict_ssl = b;
                    }
                }
            } else if matches!(key.as_str(), "local-address" | "localAddress") {
                match value.trim().parse::<std::net::IpAddr>() {
                    Ok(ip) => self.local_address = Some(ip),
                    Err(e) => tracing::warn!(
                        code = aube_codes::warnings::WARN_AUBE_INVALID_LOCAL_ADDRESS,
                        "ignoring invalid local-address {value:?}: {e}"
                    ),
                }
            } else if key == "maxsockets" {
                match value.trim().parse::<usize>() {
                    Ok(n) if n > 0 => self.max_sockets = Some(n),
                    Ok(_) => tracing::warn!(
                        code = aube_codes::warnings::WARN_AUBE_INVALID_MAXSOCKETS,
                        "ignoring maxsockets=0"
                    ),
                    Err(e) => tracing::warn!(
                        code = aube_codes::warnings::WARN_AUBE_INVALID_MAXSOCKETS,
                        "ignoring invalid maxsockets {value:?}: {e}"
                    ),
                }
            } else if matches!(key.as_str(), "cafile" | "caFile") {
                // Top-level (unscoped) cafile — applies to all registries.
                // Diverges from the URI-scoped form in the `//` block
                // below; both can coexist and stack additively.
                self.cafile = Some(PathBuf::from(value));
            } else if matches!(key.as_str(), "ca" | "ca[]") {
                // Top-level inline PEM, single or array form. npm/pnpm
                // accept repeated `ca[]=...` lines to build up a list;
                // mirror that by pushing instead of replacing.
                self.ca.push(pem_value(value));
            } else if let Some(scope) = key.strip_suffix(":registry") {
                if scope.starts_with('@') {
                    self.scoped_registries
                        .insert(scope.to_lowercase(), normalize_registry_url(&value));
                }
            } else if key.starts_with("//") {
                // URI-specific config: //registry.url/:_authToken=TOKEN
                if let Some((uri, suffix)) = key.rsplit_once(':') {
                    let (uri, scope) = split_uri_scope_key(uri);
                    // Normalize so `//host:443/x/` and `//host/x/` collapse
                    // to the same key — matches what `registry_uri_key`
                    // produces on the lookup side after stripping the
                    // scheme's default port.
                    let uri_key = normalize_npmrc_uri_key(uri);
                    match suffix {
                        "_authToken" => {
                            let entry = auth_entry_for_uri(
                                &mut self.auth_by_uri,
                                &mut self.scoped_auth_by_uri,
                                &uri_key,
                                scope,
                            );
                            entry.auth_token = Some(value);
                            if scope.is_none() {
                                explicit_uri_fields.insert((uri_key, "_authToken"));
                            }
                        }
                        "_auth" => {
                            let entry = auth_entry_for_uri(
                                &mut self.auth_by_uri,
                                &mut self.scoped_auth_by_uri,
                                &uri_key,
                                scope,
                            );
                            entry.auth = Some(value);
                            if scope.is_none() {
                                explicit_uri_fields.insert((uri_key, "_auth"));
                            }
                        }
                        "username" => {
                            let entry = auth_entry_for_uri(
                                &mut self.auth_by_uri,
                                &mut self.scoped_auth_by_uri,
                                &uri_key,
                                scope,
                            );
                            entry.username = Some(value);
                            if scope.is_none() {
                                explicit_uri_fields.insert((uri_key, "username"));
                            }
                        }
                        "_password" => {
                            let entry = auth_entry_for_uri(
                                &mut self.auth_by_uri,
                                &mut self.scoped_auth_by_uri,
                                &uri_key,
                                scope,
                            );
                            entry.password = Some(value);
                            if scope.is_none() {
                                explicit_uri_fields.insert((uri_key, "_password"));
                            }
                        }
                        "tokenHelper" | "token-helper" => {
                            // CVE-2025-69262 (pnpm GHSA-2phv-j68v-wwqx)
                            // class: `tokenHelper` is spawned as
                            // `sh -c <value>` on unix or `cmd /C
                            // <value>` on Windows at the next authed
                            // registry request. Accept only from
                            // trusted sources and only when the
                            // value parses as a sanitized absolute
                            // path to an interpreter.
                            if !source.is_trusted_for_subprocess_settings() {
                                tracing::warn!(
                                    code = aube_codes::warnings::WARN_AUBE_UNTRUSTED_TOKEN_HELPER,
                                    "ignoring tokenHelper for {uri}: {source:?} source is not trusted for subprocess settings (committed `.npmrc` cannot set this)"
                                );
                                continue;
                            }
                            let Some(sanitized) = sanitize_token_helper(&value) else {
                                tracing::warn!(
                                    code = aube_codes::warnings::WARN_AUBE_INVALID_TOKEN_HELPER,
                                    "ignoring tokenHelper for {uri}: value is not a bare absolute path: {value:?}"
                                );
                                continue;
                            };
                            let entry = auth_entry_for_uri(
                                &mut self.auth_by_uri,
                                &mut self.scoped_auth_by_uri,
                                &uri_key,
                                scope,
                            );
                            entry.token_helper = Some(sanitized);
                            if scope.is_none() {
                                explicit_uri_fields.insert((uri_key, "tokenHelper"));
                            }
                        }
                        "ca" | "ca[]" => {
                            let entry = auth_entry_for_uri(
                                &mut self.auth_by_uri,
                                &mut self.scoped_auth_by_uri,
                                &uri_key,
                                scope,
                            );
                            entry.tls.ca.push(pem_value(value));
                        }
                        "cafile" | "caFile" => {
                            let entry = auth_entry_for_uri(
                                &mut self.auth_by_uri,
                                &mut self.scoped_auth_by_uri,
                                &uri_key,
                                scope,
                            );
                            entry.tls.cafile = Some(PathBuf::from(value));
                        }
                        "cert" => {
                            let entry = auth_entry_for_uri(
                                &mut self.auth_by_uri,
                                &mut self.scoped_auth_by_uri,
                                &uri_key,
                                scope,
                            );
                            entry.tls.cert = Some(pem_value(value));
                            if scope.is_none() {
                                explicit_uri_fields.insert((uri_key, "cert"));
                            }
                        }
                        "key" => {
                            let entry = auth_entry_for_uri(
                                &mut self.auth_by_uri,
                                &mut self.scoped_auth_by_uri,
                                &uri_key,
                                scope,
                            );
                            entry.tls.key = Some(pem_value(value));
                            if scope.is_none() {
                                explicit_uri_fields.insert((uri_key, "key"));
                            }
                        }
                        "always-auth" | "always_auth" => {
                            let entry = auth_entry_for_uri(
                                &mut self.auth_by_uri,
                                &mut self.scoped_auth_by_uri,
                                &uri_key,
                                scope,
                            );
                            entry.always_auth = parse_npmrc_bool(&value);
                        }
                        _ => {} // Ignore unknown suffixes for now
                    }
                }
            }
            // Generic pnpm settings (`auto-install-peers`, etc) are NOT
            // matched here — they're resolved by aube's settings
            // module against the raw entries, using the canonical
            // source list from settings.toml. Add a new branch here
            // only if the key maps to a registry-client concept.
        }
    }

    fn rescope_unscoped_registry_setting(
        &mut self,
        source: NpmrcSource,
        registry: &str,
        suffix: &str,
        explicit_uri_field_exists: bool,
        apply: impl FnOnce(&mut AuthConfig),
    ) {
        if explicit_uri_field_exists {
            tracing::warn!(
                code = aube_codes::warnings::WARN_AUBE_UNSCOPED_AUTH_RESCOPED,
                "ignoring unscoped {suffix} from {source:?}: URI-scoped `{}:{suffix}` is already configured",
                registry_uri_key(registry)
            );
            return;
        }
        if matches!(source, NpmrcSource::Env | NpmrcSource::PnpmAuth) {
            tracing::warn!(
                code = aube_codes::warnings::WARN_AUBE_UNSCOPED_AUTH_RESCOPED,
                "unscoped {suffix} from {source:?} was pinned to {registry}"
            );
        } else {
            tracing::warn!(
                code = aube_codes::warnings::WARN_AUBE_UNSCOPED_AUTH_RESCOPED,
                "unscoped {suffix} from {source:?} was pinned to {registry}; write `{}:{suffix}=...` instead",
                registry_uri_key(registry)
            );
        }
        let entry = self
            .auth_by_uri
            .entry(registry_uri_key(registry))
            .or_default();
        apply(entry);
    }
}

fn has_credential_material(auth: &AuthConfig) -> bool {
    auth.auth_token.is_some()
        || auth.auth.is_some()
        || auth.token_helper.is_some()
        || (auth.username.is_some() && auth.password.is_some())
}

fn uri_key_matches_prefix(uri_key: &str, prefix: &str) -> bool {
    if uri_key == prefix || (uri_key.starts_with(prefix) && prefix.ends_with('/')) {
        return true;
    }
    uri_key
        .strip_prefix(prefix)
        .is_some_and(|rest| rest.starts_with('/'))
}

fn auth_entry_for_uri<'a>(
    auth_by_uri: &'a mut std::collections::BTreeMap<String, AuthConfig>,
    scoped_auth_by_uri: &'a mut std::collections::BTreeMap<
        String,
        std::collections::BTreeMap<String, AuthConfig>,
    >,
    uri_key: &str,
    scope: Option<&str>,
) -> &'a mut AuthConfig {
    if let Some(scope) = scope {
        scoped_auth_by_uri
            .entry(uri_key.to_string())
            .or_default()
            .entry(scope.to_lowercase())
            .or_default()
    } else {
        auth_by_uri.entry(uri_key.to_string()).or_default()
    }
}

fn split_uri_scope_key(uri: &str) -> (&str, Option<&str>) {
    if let Some((base, scope)) = uri.rsplit_once(':')
        && scope.starts_with('@')
        && scope.len() > 1
        && !scope.contains('/')
    {
        return (base, Some(scope));
    }
    (uri, None)
}

/// Parse an npmrc-style boolean. npm/pnpm treat `true`/`false` (and the
/// bare presence of the key) as the canonical spellings; accept the
/// common truthy synonyms case-insensitively so a Yarn `npmAlwaysAuth:
/// true` (already emitted as the string `"true"`) and a hand-written
/// `.npmrc` both resolve.
fn parse_npmrc_bool(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "true" | "1" | "yes" | "on"
    )
}

fn canonical_rescoped_suffix(suffix: &str) -> Option<&'static str> {
    match suffix {
        "_authToken" => Some("_authToken"),
        "_auth" => Some("_auth"),
        "username" => Some("username"),
        "_password" => Some("_password"),
        "cert" => Some("cert"),
        "key" => Some("key"),
        "tokenHelper" | "token-helper" => Some("tokenHelper"),
        _ => None,
    }
}

/// Per-source view of "which default `registry=` was set in this scope",
/// used to pin an unscoped `_authToken` (etc.) from a given source to the
/// registry that same source declared. One slot per file/env scope;
/// builtin and global share their own slots so an admin-set unscoped auth
/// in the global `npmrc` binds to the global `registry=`. The two
/// `*NpmrcAuthFile` sources collapse onto the auth-file slot (a value the
/// auth file itself rarely sets), matching the prior behavior.
struct SourceRegistries {
    builtin: String,
    global: String,
    user: String,
    pnpm_auth: String,
    project: String,
    npmrc_auth_file: String,
    env: String,
}

impl Default for SourceRegistries {
    fn default() -> Self {
        let default = || "https://registry.npmjs.org/".to_string();
        Self {
            builtin: default(),
            global: default(),
            user: default(),
            pnpm_auth: default(),
            project: default(),
            npmrc_auth_file: default(),
            env: default(),
        }
    }
}

impl SourceRegistries {
    fn slot(&self, source: NpmrcSource) -> &str {
        match source {
            NpmrcSource::Builtin => &self.builtin,
            NpmrcSource::Global => &self.global,
            NpmrcSource::User => &self.user,
            NpmrcSource::PnpmAuth => &self.pnpm_auth,
            NpmrcSource::Project => &self.project,
            NpmrcSource::UserNpmrcAuthFile | NpmrcSource::ProjectNpmrcAuthFile => {
                &self.npmrc_auth_file
            }
            NpmrcSource::Env => &self.env,
        }
    }

    fn slot_mut(&mut self, source: NpmrcSource) -> &mut String {
        match source {
            NpmrcSource::Builtin => &mut self.builtin,
            NpmrcSource::Global => &mut self.global,
            NpmrcSource::User => &mut self.user,
            NpmrcSource::PnpmAuth => &mut self.pnpm_auth,
            NpmrcSource::Project => &mut self.project,
            NpmrcSource::UserNpmrcAuthFile | NpmrcSource::ProjectNpmrcAuthFile => {
                &mut self.npmrc_auth_file
            }
            NpmrcSource::Env => &mut self.env,
        }
    }
}

fn auth_entry_contains_env_ref(key: &str, value: &str) -> bool {
    auth_suffix(key).is_some_and(|suffix| {
        matches!(
            suffix,
            "_authToken" | "_auth" | "username" | "_password" | "cert" | "key"
        ) && (contains_env_ref(key) || contains_env_ref(value))
    })
}

fn auth_suffix(key: &str) -> Option<&str> {
    if matches!(
        key,
        "_authToken" | "_auth" | "username" | "_password" | "cert" | "key"
    ) {
        return Some(key);
    }
    key.rsplit_once(':').map(|(_, suffix)| suffix)
}

fn contains_env_ref(value: &str) -> bool {
    value.contains("${")
        && value
            .split_once("${")
            .is_some_and(|(_, rest)| rest.contains('}'))
}
