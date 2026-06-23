use crate::Error;

pub(super) fn encoded_name(name: &str) -> String {
    name.replace('/', "%2F")
}

/// `{registry}/-/package/{name}/dist-tags` — the ls endpoint.
pub(super) fn dist_tag_root_url(registry_url: &str, name: &str) -> String {
    format!(
        "{}/-/package/{}/dist-tags",
        registry_url.trim_end_matches('/'),
        encoded_name(name),
    )
}

/// `{registry}/-/package/{name}/dist-tags/{tag}` — the add/rm endpoint.
pub(super) fn dist_tag_url(registry_url: &str, name: &str, tag: &str) -> String {
    format!(
        "{}/-/package/{}/dist-tags/{}",
        registry_url.trim_end_matches('/'),
        encoded_name(name),
        tag,
    )
}

/// Shared pre-flight mapping for dist-tag responses: turns 404 into
/// `NotFound(name)`, 401 into `Unauthorized`, and 403 into `Forbidden`
/// (consuming the response body so the registry's actionable message —
/// "blocked by policy", "token lacks `read:packages`" — survives), so
/// callers don't have to repeat the same `if resp.status() == ...`
/// ladder around every PUT/GET. DELETE has a richer 404 shape
/// (`name@tag`) and inlines its own handling via [`map_dist_tag_error`].
///
/// On success the response is handed back so the caller can continue
/// reading it. 401 and 403 are kept distinct because they need
/// different remediation: 401 means "log in", 403 means "you're logged
/// in but not allowed" — `aube login` won't fix the latter.
pub(super) async fn check_dist_tag_status(
    resp: reqwest::Response,
    name: &str,
) -> Result<reqwest::Response, Error> {
    if let Some(err) = map_dist_tag_error(&resp, name) {
        return Err(forbidden_with_body(err, resp).await);
    }
    Ok(resp)
}

/// Map a dist-tag response status onto a pre-`Forbidden`-body error, or
/// `None` when the status is not one we special-case. Splitting this out
/// lets `delete_dist_tag` (which carries its own `name@tag` 404 shape)
/// share the 401/403 handling. The returned `Forbidden` carries an empty
/// body as a placeholder; callers pass it through [`forbidden_with_body`]
/// to fill in the registry's message before returning it.
pub(super) fn map_dist_tag_error(resp: &reqwest::Response, name: &str) -> Option<Error> {
    match resp.status() {
        reqwest::StatusCode::NOT_FOUND => Some(Error::NotFound(name.to_string())),
        reqwest::StatusCode::UNAUTHORIZED => Some(Error::Unauthorized),
        reqwest::StatusCode::FORBIDDEN => Some(Error::Forbidden {
            body: String::new(),
        }),
        _ => None,
    }
}

/// For a `Forbidden` error, drain the response body into it so the
/// registry's actionable 403 message reaches the user. Any other error
/// is returned unchanged (its body is irrelevant). A body the registry
/// omits or that fails to read leaves the placeholder empty — the hint
/// at the command layer still applies.
pub(super) async fn forbidden_with_body(err: Error, resp: reqwest::Response) -> Error {
    match err {
        Error::Forbidden { .. } => Error::Forbidden {
            body: resp.text().await.unwrap_or_default().trim().to_string(),
        },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use crate::Error;
    use crate::client::RegistryClient;
    use crate::config::NpmConfig;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client_for(server: &MockServer) -> RegistryClient {
        let config = NpmConfig {
            registry: format!("{}/", server.uri()),
            ..Default::default()
        };
        RegistryClient::from_config(config)
    }

    // A 403 from the registry must surface the response body (Artifactory
    // and GitHub Packages put the actionable reason there) and must NOT
    // collapse into the auth-required path — `aube login` doesn't fix a 403.
    #[tokio::test]
    async fn fetch_dist_tags_403_preserves_registry_body() {
        let server = MockServer::start().await;
        let body = "package blocked by policy: token lacks read:packages scope";
        Mock::given(method("GET"))
            .and(path("/-/package/demo/dist-tags"))
            .respond_with(ResponseTemplate::new(403).set_body_string(body))
            .mount(&server)
            .await;

        match client_for(&server).fetch_dist_tags("demo").await {
            Err(Error::Forbidden { body: got }) => assert_eq!(got, body),
            other => panic!("expected Forbidden carrying the registry body, got {other:?}"),
        }
    }

    // A 401 stays Unauthorized so the command layer can keep pointing the
    // user at `aube login` — the remediation a 403 must not get.
    #[tokio::test]
    async fn fetch_dist_tags_401_stays_unauthorized() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/-/package/demo/dist-tags"))
            .respond_with(ResponseTemplate::new(401).set_body_string("auth required"))
            .mount(&server)
            .await;

        match client_for(&server).fetch_dist_tags("demo").await {
            Err(Error::Unauthorized) => {}
            other => panic!("expected Unauthorized for a 401, got {other:?}"),
        }
    }

    // A 403 with no body leaves an empty string rather than fabricating
    // text — the command-layer hint still names the right remediation.
    #[tokio::test]
    async fn fetch_dist_tags_403_empty_body_is_empty_not_synthesized() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/-/package/demo/dist-tags"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        match client_for(&server).fetch_dist_tags("demo").await {
            Err(Error::Forbidden { body }) => assert!(body.is_empty(), "body was {body:?}"),
            other => panic!("expected Forbidden, got {other:?}"),
        }
    }
}
