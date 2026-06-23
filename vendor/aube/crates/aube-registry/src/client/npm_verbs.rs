//! Registry endpoints backing the npm-compatible account/registry verbs
//! (`whoami`, `search`, `owner`, `token`). These reuse the same TLS
//! clients and `.npmrc` auth resolution as the dist-tag / deprecate
//! writes (`authed`/`authed_for_package`/`http_for*`), so private
//! registries and scoped auth Just Work.
//!
//! None of these touch the packument cache — they are account/registry
//! operations, not metadata reads.

use super::RegistryClient;
use super::dist_tags::encoded_name;
use crate::Error;

/// One package owner / maintainer (npm `-/package/<pkg>/collaborators`
/// and the packument `maintainers` array use this shape).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Owner {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub email: Option<String>,
}

/// One npm auth token, as returned by `GET /-/npm/v1/tokens`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct TokenInfo {
    /// The token key (an opaque id used to revoke).
    #[serde(default)]
    pub key: String,
    /// The masked token value (npm returns only the first/last chars).
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub readonly: bool,
    #[serde(default)]
    pub created: Option<String>,
    /// CIDR allowlist for the token, if any.
    #[serde(default, rename = "cidr_whitelist")]
    pub cidr_whitelist: Option<Vec<String>>,
}

impl RegistryClient {
    /// `GET {registry}/-/whoami` — return the authenticated username.
    /// Requires a configured auth token; a 401 maps to
    /// [`Error::Unauthorized`] so the command layer can point at login.
    pub async fn fetch_whoami(&self) -> Result<String, Error> {
        let registry_url = self.config.registry.clone();
        let url = format!("{}/-/whoami", registry_url.trim_end_matches('/'));
        let resp = self
            .authed_request(reqwest::Method::GET, &url, &registry_url)
            .header("Accept", "application/json")
            .send()
            .await?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(Error::Unauthorized);
        }
        let resp = resp.error_for_status()?;
        #[derive(serde::Deserialize)]
        struct Whoami {
            username: String,
        }
        let who: Whoami = resp.json().await?;
        Ok(who.username)
    }

    /// `GET {registry}/-/v1/search?text=<query>&size=<limit>` — full-text
    /// package search. Public on npmjs (no auth required) but the token is
    /// attached anyway so private registries that gate search still work.
    /// Returns the raw `objects` array from the search response.
    pub async fn search(&self, query: &str, limit: u32) -> Result<Vec<serde_json::Value>, Error> {
        let registry_url = self.config.registry.clone();
        let mut url = reqwest::Url::parse(&format!(
            "{}/-/v1/search",
            registry_url.trim_end_matches('/')
        ))
        .map_err(|e| Error::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, e)))?;
        url.query_pairs_mut()
            .append_pair("text", query)
            .append_pair("size", &limit.to_string());

        let resp = self
            .authed_request(reqwest::Method::GET, url.as_str(), &registry_url)
            .header("Accept", "application/json")
            .send()
            .await?
            .error_for_status()?;

        #[derive(serde::Deserialize)]
        struct SearchResults {
            #[serde(default)]
            objects: Vec<SearchObject>,
        }
        #[derive(serde::Deserialize)]
        struct SearchObject {
            package: serde_json::Value,
        }
        let results: SearchResults = resp.json().await?;
        Ok(results.objects.into_iter().map(|o| o.package).collect())
    }

    /// `GET {registry}/-/package/<pkg>/collaborators` — list owners. npm
    /// returns an object mapping `username` → permission (e.g. `"read-write"`);
    /// we surface just the usernames. Falls back to the packument
    /// `maintainers` array when the collaborators endpoint 404s (older /
    /// non-npm registries).
    pub async fn fetch_owners(&self, name: &str) -> Result<Vec<Owner>, Error> {
        let registry_url = self.registry_url_for(name).to_string();
        let url = format!(
            "{}/-/package/{}/collaborators",
            registry_url.trim_end_matches('/'),
            encoded_name(name),
        );
        let resp = self
            .authed_get_for_package(&url, &registry_url, name)
            .header("Accept", "application/json")
            .send()
            .await?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            // Fall back to the packument's maintainers list.
            return self.owners_from_packument(name).await;
        }
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(Error::Unauthorized);
        }
        let resp = resp.error_for_status()?;
        // collaborators is `{ "user": "read-write", ... }`.
        let map: std::collections::BTreeMap<String, String> = resp.json().await?;
        Ok(map
            .into_keys()
            .map(|name| Owner { name, email: None })
            .collect())
    }

    async fn owners_from_packument(&self, name: &str) -> Result<Vec<Owner>, Error> {
        let packument = self.fetch_packument_json_fresh(name).await?;
        let maintainers = packument
            .get("maintainers")
            .and_then(|m| m.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(maintainers
            .into_iter()
            .filter_map(|m| serde_json::from_value::<Owner>(m).ok())
            .collect())
    }

    /// Add or remove an owner by PUT-ing the modified maintainers list to
    /// the packument (`{registry}/<pkg>/-rev/<rev>` semantics handled by
    /// the registry on a full-document PUT — same mechanism `deprecate`
    /// uses). `add=true` inserts `user`; `add=false` removes it.
    pub async fn change_owner(
        &self,
        name: &str,
        user: &str,
        add: bool,
        otp: Option<&str>,
    ) -> Result<(), Error> {
        let mut packument = self.fetch_packument_json_fresh(name).await?;
        let obj = packument
            .as_object_mut()
            .ok_or_else(|| Error::RegistryWrite {
                status: 0,
                body: format!("registry response for {name} is not an object"),
            })?;

        let mut maintainers: Vec<serde_json::Value> = obj
            .get("maintainers")
            .and_then(|m| m.as_array())
            .cloned()
            .unwrap_or_default();

        if add {
            let already = maintainers
                .iter()
                .any(|m| m.get("name").and_then(|n| n.as_str()) == Some(user));
            if !already {
                maintainers.push(serde_json::json!({ "name": user }));
            }
        } else {
            maintainers.retain(|m| m.get("name").and_then(|n| n.as_str()) != Some(user));
        }
        obj.insert(
            "maintainers".to_string(),
            serde_json::Value::Array(maintainers),
        );

        self.put_packument(name, &packument, otp).await?;
        Ok(())
    }

    /// `GET {registry}/-/npm/v1/tokens` — list the authenticated user's
    /// auth tokens. Requires auth.
    pub async fn list_tokens(&self) -> Result<Vec<TokenInfo>, Error> {
        let registry_url = self.config.registry.clone();
        let url = format!("{}/-/npm/v1/tokens", registry_url.trim_end_matches('/'));
        let resp = self
            .authed_request(reqwest::Method::GET, &url, &registry_url)
            .header("Accept", "application/json")
            .send()
            .await?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(Error::Unauthorized);
        }
        let resp = resp.error_for_status()?;
        #[derive(serde::Deserialize)]
        struct TokenList {
            #[serde(default)]
            objects: Vec<TokenInfo>,
        }
        let list: TokenList = resp.json().await?;
        Ok(list.objects)
    }

    /// `POST {registry}/-/npm/v1/tokens` — create a new auth token.
    /// `password` is the account password (npm's classic-token flow);
    /// `read_only` and `cidr` map to the request body. Returns the raw
    /// created-token document (the full token is only shown here, once).
    pub async fn create_token(
        &self,
        password: &str,
        read_only: bool,
        cidr: &[String],
    ) -> Result<serde_json::Value, Error> {
        let registry_url = self.config.registry.clone();
        let url = format!("{}/-/npm/v1/tokens", registry_url.trim_end_matches('/'));
        let body = serde_json::json!({
            "password": password,
            "readonly": read_only,
            "cidr_whitelist": cidr,
        });
        let resp = self
            .authed_request(reqwest::Method::POST, &url, &registry_url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::RegistryWrite {
                status: status.as_u16(),
                body,
            });
        }
        Ok(resp.json().await.unwrap_or(serde_json::Value::Null))
    }

    /// `DELETE {registry}/-/npm/v1/tokens/token/<key>` — revoke a token by
    /// its key (or a token-value prefix, which npm also accepts). Requires
    /// auth.
    pub async fn revoke_token(&self, key: &str) -> Result<(), Error> {
        let registry_url = self.config.registry.clone();
        let url = format!(
            "{}/-/npm/v1/tokens/token/{}",
            registry_url.trim_end_matches('/'),
            key,
        );
        let resp = self
            .authed_request(reqwest::Method::DELETE, &url, &registry_url)
            .send()
            .await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(Error::NotFound(format!("token {key}")));
        }
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(Error::Unauthorized);
        }
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::RegistryWrite {
                status: status.as_u16(),
                body,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::client::RegistryClient;
    use crate::config::NpmConfig;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client_for(server: &MockServer) -> RegistryClient {
        let config = NpmConfig {
            registry: format!("{}/", server.uri()),
            ..Default::default()
        };
        RegistryClient::from_config(config)
    }

    #[tokio::test]
    async fn whoami_returns_username() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/-/whoami"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "username": "octocat"
            })))
            .mount(&server)
            .await;
        let client = client_for(&server);
        assert_eq!(client.fetch_whoami().await.unwrap(), "octocat");
    }

    #[tokio::test]
    async fn whoami_401_maps_to_unauthorized() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/-/whoami"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let client = client_for(&server);
        assert!(matches!(
            client.fetch_whoami().await,
            Err(crate::Error::Unauthorized)
        ));
    }

    #[tokio::test]
    async fn search_returns_package_objects() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/-/v1/search"))
            .and(query_param("text", "lodash"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "objects": [
                    {"package": {"name": "lodash", "version": "4.17.21"}},
                    {"package": {"name": "lodash.merge", "version": "4.6.2"}}
                ]
            })))
            .mount(&server)
            .await;
        let client = client_for(&server);
        let results = client.search("lodash", 20).await.unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["name"], "lodash");
    }

    #[tokio::test]
    async fn owners_lists_collaborators() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/-/package/lodash/collaborators"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jdalton": "read-write",
                "mathias": "read-write"
            })))
            .mount(&server)
            .await;
        let client = client_for(&server);
        let owners = client.fetch_owners("lodash").await.unwrap();
        let names: Vec<_> = owners.iter().map(|o| o.name.clone()).collect();
        assert_eq!(names, vec!["jdalton".to_string(), "mathias".to_string()]);
    }
}
