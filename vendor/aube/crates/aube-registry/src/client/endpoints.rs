use super::body::check_body_cap;
use super::cache::packument_full_cache_path;
use super::{
    AUDIT_BODY_CAP, PACKUMENT_FULL_ACCEPT, RegistryClient, check_dist_tag_status,
    dist_tag_root_url, dist_tag_url, forbidden_with_body, map_dist_tag_error, parse_full_response,
};
use crate::Error;
use std::path::Path;

impl RegistryClient {
    pub async fn fetch_advisories_bulk(
        &self,
        pkg_versions: &std::collections::BTreeMap<String, Vec<String>>,
    ) -> Result<serde_json::Value, Error> {
        // The bulk endpoint lives on the default registry; scoped registries
        // don't all implement it, so we always post to the top-level one.
        let registry_url = &self.config.registry;
        let url = format!(
            "{}/-/npm/v1/security/advisories/bulk",
            registry_url.trim_end_matches('/')
        );

        let body = serde_json::to_vec(pkg_versions)
            .map_err(|e| Error::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))?;

        let resp = self
            .authed(self.http_for(registry_url).post(&url), registry_url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .body(body)
            .send()
            .await?;

        // Some registries (Verdaccio, private mirrors) don't implement the
        // bulk advisory endpoint and return 404. Treat that as "no advisories"
        // — the alternative is making every air-gapped setup pass
        // `--ignore-registry-errors`, which is noisy.
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(serde_json::Value::Object(serde_json::Map::new()));
        }

        let resp = resp.error_for_status()?;
        check_body_cap(&resp, AUDIT_BODY_CAP, "bulk advisories")?;
        let json: serde_json::Value = resp.json().await?;
        Ok(json)
    }

    /// Fetch a single VersionMetadata via the per-version registry
    /// endpoint `{registry}/{name}/{version}`. Returns ~1-4 KiB JSON
    /// vs the full packument's 100 KiB-2 MiB. Use when caller knows
    /// the exact version, e.g. lockfile drift refetch with locked
    /// version pinned. Wins 200-1000 ms on lockfile CI installs that
    /// trigger re-resolve.
    pub async fn fetch_single_version_metadata(
        &self,
        name: &str,
        version: &str,
    ) -> Result<crate::VersionMetadata, Error> {
        let (packument_url, registry_url) = self.packument_url(name);
        let url = format!("{packument_url}/{version}");
        let resp = self
            .send_metadata_with_retry(&format!("version {name}@{version}"), || {
                self.authed_get_for_package(&url, registry_url, name)
                    .header("Accept", "application/json")
            })
            .await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(Error::NotFound(format!("{name}@{version}")));
        }
        let resp = resp.error_for_status()?;
        check_body_cap(
            &resp,
            self.fetch_policy.packument_max_bytes,
            "version-metadata",
        )?;
        parse_full_response(resp).await
    }

    /// Fetch the *full* (non-corgi) packument as raw JSON, bypassing the
    /// on-disk cache entirely. Used by mutating commands like `deprecate`
    /// that need a fresh read-modify-write against the authoritative copy
    /// on the registry — a stale cached document would roll back other
    /// publishers' changes on the subsequent PUT.
    pub async fn fetch_packument_json_fresh(&self, name: &str) -> Result<serde_json::Value, Error> {
        let (url, registry_url) = self.packument_url(name);
        let resp = self
            .send_metadata_with_retry(&format!("packument {name}"), || {
                self.authed_get_for_package(&url, registry_url, name)
                    .header("Accept", PACKUMENT_FULL_ACCEPT)
            })
            .await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(Error::NotFound(name.to_string()));
        }
        let resp = resp.error_for_status()?;
        check_body_cap(&resp, self.fetch_policy.packument_max_bytes, "packument")?;
        let value: serde_json::Value = resp.json().await?;
        Ok(value)
    }

    /// PUT a full packument back to the registry. Used by `deprecate` /
    /// `undeprecate`. Honors `--otp` via the `npm-otp` header.
    ///
    /// Returns the registry's raw response body as `serde_json::Value`
    /// (npm responds with `{ok: true, id, rev}` on success). On HTTP
    /// failure the body is included in the error so 401/403/409 messages
    /// make it to the user.
    pub async fn put_packument(
        &self,
        name: &str,
        body: &serde_json::Value,
        otp: Option<&str>,
    ) -> Result<serde_json::Value, Error> {
        let (url, registry_url) = self.packument_url(name);

        let mut req = self.authed_for_package(
            self.http_for_package(registry_url, name)
                .put(&url)
                .header("Content-Type", "application/json")
                .json(body),
            registry_url,
            name,
        );
        if let Some(code) = otp {
            req = req.header("npm-otp", code);
        }

        let resp = req.send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::RegistryWrite {
                status: status.as_u16(),
                body,
            });
        }
        let value: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
        Ok(value)
    }

    /// Drop any on-disk *full* packument cache entry for `name`, if one
    /// exists. Call this after a successful mutating PUT (deprecate,
    /// dist-tag, ...) so subsequent `aube view` calls don't serve the
    /// pre-mutation document for the remaining TTL window. Missing files
    /// and I/O errors are swallowed — the cache is advisory, not load
    /// bearing.
    pub fn invalidate_full_packument_cache(&self, name: &str, cache_dir: &Path) {
        let registry_url = self.config.registry_for(name).to_string();
        if let Some(path) = packument_full_cache_path(cache_dir, name, &registry_url) {
            let _ = std::fs::remove_file(&path);
        }
    }

    /// Fetch the authoritative dist-tag map for a package from the
    /// registry's `/-/package/<pkg>/dist-tags` endpoint. This is the
    /// same endpoint `npm dist-tag ls` calls. A GET against this
    /// endpoint doesn't require auth for public packages, but we still
    /// attach the user's token so private packages Just Work.
    pub async fn fetch_dist_tags(
        &self,
        name: &str,
    ) -> Result<std::collections::BTreeMap<String, String>, Error> {
        let registry_url = self.registry_url_for(name);
        let url = dist_tag_root_url(registry_url, name);
        let resp = self
            .send_metadata_with_retry(&format!("dist-tags {name}"), || {
                self.authed_get_for_package(&url, registry_url, name)
            })
            .await?;
        let resp = check_dist_tag_status(resp, name).await?;
        let map: std::collections::BTreeMap<String, String> =
            resp.error_for_status()?.json().await?;
        Ok(map)
    }

    /// Create or update a dist-tag for a package. The npm registry
    /// expects a PUT with a JSON-string body — e.g. `"1.2.3"`, *with*
    /// the quotes — and Content-Type: application/json. Requires auth.
    pub async fn put_dist_tag(
        &self,
        name: &str,
        tag: &str,
        version: &str,
        otp: Option<&str>,
    ) -> Result<(), Error> {
        let registry_url = self.registry_url_for(name);
        let url = dist_tag_url(registry_url, name, tag);

        // serde_json is already a workspace dep and used elsewhere in
        // this file; hand-serializing would miss control-character
        // escapes and other edge cases. The output is always a JSON
        // string literal like `"1.2.3"`.
        let body = serde_json::to_string(version).map_err(std::io::Error::other)?;

        let mut req = self
            .http_for_package(registry_url, name)
            .put(&url)
            .header("Content-Type", "application/json")
            .body(body);
        if self.config.is_public_npmjs(name) {
            req = req.header("npm-auth-type", "web");
        }
        let req = if let Some(code) = otp {
            req.header("npm-otp", code)
        } else {
            req
        };
        let resp = self
            .authed_for_package(req, registry_url, name)
            .send()
            .await?;
        let resp = check_dist_tag_status(resp, name).await?;
        resp.error_for_status()?;
        Ok(())
    }

    /// Remove a dist-tag from a package. Registry DELETE against
    /// `/-/package/<pkg>/dist-tags/<tag>`. Requires auth.
    pub async fn delete_dist_tag(
        &self,
        name: &str,
        tag: &str,
        otp: Option<&str>,
    ) -> Result<(), Error> {
        let registry_url = self.registry_url_for(name);
        let url = dist_tag_url(registry_url, name, tag);
        let mut req = self.http_for_package(registry_url, name).delete(&url);
        if self.config.is_public_npmjs(name) {
            req = req.header("npm-auth-type", "web");
        }
        let req = if let Some(code) = otp {
            req.header("npm-otp", code)
        } else {
            req
        };
        let resp = self
            .authed_for_package(req, registry_url, name)
            .send()
            .await?;
        // 404 here is ambiguous: package doesn't exist vs tag doesn't
        // exist on this package. Surface the `name@tag` form so the
        // caller can render it either way.
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(Error::NotFound(format!("{name}@{tag}")));
        }
        // 401 -> Unauthorized (run `aube login`); 403 -> Forbidden with
        // the registry's response body preserved (a 403 is an
        // authenticated-but-not-permitted rejection — `aube login` won't
        // fix it, and the actionable detail is in the body).
        if let Some(err) = map_dist_tag_error(&resp, name) {
            return Err(forbidden_with_body(err, resp).await);
        }
        resp.error_for_status()?;
        Ok(())
    }

    /// Construct the tarball URL for a package from the registry.
    /// Format: {registry}/{name}/-/{unscoped_name}-{version}.tgz
    pub fn tarball_url(&self, name: &str, version: &str) -> String {
        let registry_url = self.registry_url_for(name);
        let registry = registry_url.trim_end_matches('/');
        let unscoped = if let Some(rest) = name.strip_prefix('@') {
            // @scope/pkg -> pkg
            rest.split('/').nth(1).unwrap_or(rest)
        } else {
            name
        };
        format!("{registry}/{name}/-/{unscoped}-{version}.tgz")
    }
}
