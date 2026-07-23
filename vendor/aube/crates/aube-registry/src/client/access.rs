use super::body::{check_body_cap, read_body_capped};
use super::{RegistryClient, encoded_name};
use crate::Error;

struct AccessTarget<'a> {
    registry_url: &'a str,
    auth_package_name: Option<&'a str>,
    not_found: Option<AccessNotFound<'a>>,
}

enum AccessNotFound<'a> {
    Entity(&'a str),
    Package(&'a str),
}

impl RegistryClient {
    /// List packages visible to the current user or a requested user,
    /// organization, or `scope:team` entity.
    pub async fn access_list_packages(
        &self,
        entity: Option<&str>,
    ) -> Result<serde_json::Value, Error> {
        let scope_package = entity
            .filter(|entity| entity.starts_with('@'))
            .map(|entity| {
                let scope = entity.split_once(':').map_or(entity, |(scope, _)| scope);
                format!("{scope}/access")
            });
        // Owned `String`, not `&str`: nub's `registry_url_for` returns an owned
        // value because a pnpm `namedRegistries` route lookup borrows through a
        // lock guard that cannot outlive the call. Upstream's version returns
        // `&str`, so every `registry_url` site in this file borrows explicitly.
        let registry_url = scope_package.as_deref().map_or_else(
            || self.config.registry.clone(),
            |package| self.registry_url_for(package),
        );
        let url = match entity {
            None => format!(
                "{}/-/package?format=cli",
                registry_url.trim_end_matches('/')
            ),
            Some(entity) => match entity.split_once(':') {
                Some((scope, team)) => format!(
                    "{}/-/team/{}/{}/package?format=cli",
                    registry_url.trim_end_matches('/'),
                    encode_access_component(scope.trim_start_matches('@')),
                    encode_access_component(team),
                ),
                None if entity.starts_with('@') => format!(
                    "{}/-/org/{}/package?format=cli",
                    registry_url.trim_end_matches('/'),
                    encode_access_component(entity.trim_start_matches('@')),
                ),
                None => format!(
                    "{}/-/user/{}/package?format=cli",
                    registry_url.trim_end_matches('/'),
                    encode_access_component(entity),
                ),
            },
        };
        self.access_request(
            reqwest::Method::GET,
            &url,
            AccessTarget {
                registry_url: &registry_url,
                auth_package_name: scope_package.as_deref(),
                not_found: entity.map(AccessNotFound::Entity),
            },
            None,
            None,
        )
        .await
    }

    /// List package collaborators, optionally restricted to one user.
    pub async fn access_list_collaborators(
        &self,
        name: &str,
        user: Option<&str>,
    ) -> Result<serde_json::Value, Error> {
        let registry_url = self.registry_url_for(name);
        let mut url = format!(
            "{}/-/package/{}/collaborators?format=cli",
            registry_url.trim_end_matches('/'),
            encoded_name(name),
        );
        if let Some(user) = user {
            url.push_str("&user=");
            url.push_str(&encode_access_component(user));
        }
        self.access_request(
            reqwest::Method::GET,
            &url,
            AccessTarget {
                registry_url: &registry_url,
                auth_package_name: Some(name),
                not_found: Some(AccessNotFound::Package(name)),
            },
            None,
            None,
        )
        .await
    }

    /// Get a package's registry access status and MFA policy.
    pub async fn access_get_status(&self, name: &str) -> Result<serde_json::Value, Error> {
        let registry_url = self.registry_url_for(name);
        let url = access_package_url(&registry_url, name, "access");
        self.access_request(
            reqwest::Method::GET,
            &url,
            AccessTarget {
                registry_url: &registry_url,
                auth_package_name: Some(name),
                not_found: Some(AccessNotFound::Package(name)),
            },
            None,
            None,
        )
        .await
    }

    /// Change a scoped package's visibility (`public` or `restricted`).
    pub async fn access_set_status(
        &self,
        name: &str,
        access: &str,
        otp: Option<&str>,
    ) -> Result<(), Error> {
        let registry_url = self.registry_url_for(name);
        let url = access_package_url(&registry_url, name, "access");
        self.access_request(
            reqwest::Method::POST,
            &url,
            AccessTarget {
                registry_url: &registry_url,
                auth_package_name: Some(name),
                not_found: Some(AccessNotFound::Package(name)),
            },
            Some(serde_json::json!({ "access": access })),
            otp,
        )
        .await
        .map(|_| ())
    }

    /// Change a package's publish MFA requirement.
    pub async fn access_set_mfa(
        &self,
        name: &str,
        publish_requires_tfa: bool,
        otp: Option<&str>,
    ) -> Result<(), Error> {
        let registry_url = self.registry_url_for(name);
        let url = access_package_url(&registry_url, name, "access");
        self.access_request(
            reqwest::Method::POST,
            &url,
            AccessTarget {
                registry_url: &registry_url,
                auth_package_name: Some(name),
                not_found: Some(AccessNotFound::Package(name)),
            },
            Some(serde_json::json!({ "publish_requires_tfa": publish_requires_tfa })),
            otp,
        )
        .await
        .map(|_| ())
    }

    /// Grant a team read-only or read-write access to a package.
    pub async fn access_grant(
        &self,
        name: &str,
        scope: &str,
        team: &str,
        permissions: &str,
        otp: Option<&str>,
    ) -> Result<(), Error> {
        self.access_team_request(
            reqwest::Method::PUT,
            name,
            scope,
            team,
            Some(serde_json::json!({ "package": name, "permissions": permissions })),
            otp,
        )
        .await
    }

    /// Revoke a team's access to a package.
    pub async fn access_revoke(
        &self,
        name: &str,
        scope: &str,
        team: &str,
        otp: Option<&str>,
    ) -> Result<(), Error> {
        self.access_team_request(
            reqwest::Method::DELETE,
            name,
            scope,
            team,
            Some(serde_json::json!({ "package": name })),
            otp,
        )
        .await
    }

    async fn access_team_request(
        &self,
        method: reqwest::Method,
        name: &str,
        scope: &str,
        team: &str,
        body: Option<serde_json::Value>,
        otp: Option<&str>,
    ) -> Result<(), Error> {
        let registry_url = self.registry_url_for(name);
        let entity = format!("{scope}:{team}");
        let url = format!(
            "{}/-/team/{}/{}/package",
            registry_url.trim_end_matches('/'),
            encode_access_component(scope.trim_start_matches('@')),
            encode_access_component(team),
        );
        self.access_request(
            method,
            &url,
            AccessTarget {
                registry_url: &registry_url,
                auth_package_name: Some(name),
                not_found: Some(AccessNotFound::Entity(&entity)),
            },
            body,
            otp,
        )
        .await
        .map(|_| ())
    }

    async fn access_request(
        &self,
        method: reqwest::Method,
        url: &str,
        target: AccessTarget<'_>,
        body: Option<serde_json::Value>,
        otp: Option<&str>,
    ) -> Result<serde_json::Value, Error> {
        let mut request = match target.auth_package_name {
            Some(name) => self.authed_request_for_package(method, url, target.registry_url, name),
            None => self.authed_request(method, url, target.registry_url),
        };
        if let Some(body) = body {
            request = request
                .header("Content-Type", "application/json")
                .json(&body);
        }
        if let Some(otp) = otp {
            request = request.header("npm-otp", otp);
        }

        let resp = request.send().await?;
        match resp.status() {
            reqwest::StatusCode::NOT_FOUND => {
                if let Some(not_found) = target.not_found {
                    return Err(match not_found {
                        AccessNotFound::Entity(name) => {
                            Error::AccessEntityNotFound(name.to_string())
                        }
                        AccessNotFound::Package(name) => Error::NotFound(name.to_string()),
                    });
                }
                let status = resp.status().as_u16();
                let body =
                    read_body_capped(resp, self.fetch_policy.packument_max_bytes, "access").await?;
                return Err(Error::RegistryWrite {
                    status,
                    body: String::from_utf8_lossy(&body).into_owned(),
                });
            }
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => {
                return Err(Error::Unauthorized);
            }
            status if !status.is_success() => {
                let status = status.as_u16();
                let body =
                    read_body_capped(resp, self.fetch_policy.packument_max_bytes, "access").await?;
                return Err(Error::RegistryWrite {
                    status,
                    body: String::from_utf8_lossy(&body).into_owned(),
                });
            }
            _ => {}
        }
        check_body_cap(&resp, self.fetch_policy.packument_max_bytes, "access")?;
        let body = read_body_capped(resp, self.fetch_policy.packument_max_bytes, "access").await?;
        if body.is_empty() {
            return Ok(serde_json::Value::Null);
        }
        serde_json::from_slice(&body)
            .map_err(|e| Error::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))
    }
}

fn access_package_url(registry_url: &str, name: &str, suffix: &str) -> String {
    format!(
        "{}/-/package/{}/{}",
        registry_url.trim_end_matches('/'),
        encoded_name(name),
        suffix,
    )
}

fn encode_access_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{RegistryClient, access_package_url, encode_access_component};
    use crate::Error;
    use crate::config::NpmConfig;
    use std::collections::BTreeMap;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn package_access_url_encodes_scoped_name() {
        assert_eq!(
            access_package_url("https://registry.example/", "@scope/pkg", "access"),
            "https://registry.example/-/package/@scope%2Fpkg/access"
        );
    }

    #[test]
    fn access_component_escapes_reserved_characters() {
        assert_eq!(encode_access_component("dev team/@a"), "dev%20team%2F%40a");
    }

    #[tokio::test]
    async fn package_access_routes_through_scoped_registry_auth() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/-/package/@scope%2Fpkg/access"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "access": "restricted" })),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/-/package/@scope%2Fpkg/access"))
            .and(header("npm-otp", "123456"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let client = RegistryClient::from_config(NpmConfig {
            registry: format!("{}/", server.uri()),
            ..Default::default()
        });
        assert_eq!(
            client
                .access_get_status("@scope/pkg")
                .await
                .expect("get status"),
            serde_json::json!({ "access": "restricted" })
        );
        client
            .access_set_status("@scope/pkg", "restricted", Some("123456"))
            .await
            .expect("set status");
    }

    #[tokio::test]
    async fn package_list_uses_the_organization_registry() {
        let default = MockServer::start().await;
        let scoped = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/-/org/scope/package"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&scoped)
            .await;

        let client = RegistryClient::from_config(NpmConfig {
            registry: format!("{}/", default.uri()),
            scoped_registries: BTreeMap::from([(
                "@scope".to_string(),
                format!("{}/", scoped.uri()),
            )]),
            ..Default::default()
        });
        client
            .access_list_packages(Some("@scope"))
            .await
            .expect("list organization packages");
    }

    #[tokio::test]
    async fn package_list_404_names_the_requested_entity() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/-/org/scope/package"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let client = RegistryClient::from_config(NpmConfig {
            registry: format!("{}/", server.uri()),
            ..Default::default()
        });
        let Err(Error::AccessEntityNotFound(name)) =
            client.access_list_packages(Some("@scope")).await
        else {
            panic!("expected organization lookup to return AccessEntityNotFound");
        };
        assert_eq!(name, "@scope");
    }

    #[tokio::test]
    async fn team_access_404_names_the_requested_entity() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/-/team/scope/developers/package"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let client = RegistryClient::from_config(NpmConfig {
            registry: format!("{}/", server.uri()),
            ..Default::default()
        });
        let Err(Error::AccessEntityNotFound(entity)) = client
            .access_grant("@scope/pkg", "@scope", "developers", "read-write", None)
            .await
        else {
            panic!("expected team request to return AccessEntityNotFound");
        };
        assert_eq!(entity, "@scope:developers");
    }
}
