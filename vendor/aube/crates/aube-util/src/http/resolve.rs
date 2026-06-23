//! DNS pre-resolution and anycast pinning helpers.
//!
//! The system resolver does not cache and uses a thread pool for
//! `getaddrinfo`, which serializes the first cold lookup per origin.
//! Pre-resolving every origin the install will touch in parallel during
//! manifest parsing overlaps DNS with the rest of the cold pipeline.
//!
//! The npm registry and `*.tgz` tarballs both ride Cloudflare anycast.
//! Pinning a known IP set per host (`reqwest::ClientBuilder::resolve_to_addrs`)
//! lets one TCP+TLS connection serve many SNIs on the same edge — useful
//! when the pool is otherwise empty on a fresh process.
//!
//! `AUBE_DISABLE_DNS_PRERESOLVE=1` falls through to the system resolver.

use std::net::SocketAddr;
use std::time::Duration;

const DEFAULT_RESOLVE_TIMEOUT: Duration = Duration::from_secs(2);

/// Returns true when DNS pre-resolution is disabled.
#[inline]
pub fn is_disabled() -> bool {
    crate::env::embedder_env("DISABLE_DNS_PRERESOLVE").is_some()
}

/// Pre-resolve `(host, port)` pairs in parallel via tokio's runtime,
/// returning resolved socket addresses. Failed lookups are logged at
/// debug and skipped; callers fall back to lazy resolution at request
/// time.
pub async fn lookup_all<I>(targets: I) -> Vec<(String, Vec<SocketAddr>)>
where
    I: IntoIterator<Item = (String, u16)>,
{
    use std::collections::HashMap;
    if is_disabled() {
        return Vec::new();
    }
    let mut handles: tokio::task::JoinSet<(String, Vec<SocketAddr>)> = tokio::task::JoinSet::new();
    let mut seen: HashMap<String, ()> = HashMap::new();
    for (host, port) in targets {
        if seen.insert(host.clone(), ()).is_some() {
            continue;
        }
        handles.spawn(async move {
            let endpoint = format!("{host}:{port}");
            let resolved = match tokio::time::timeout(
                DEFAULT_RESOLVE_TIMEOUT,
                tokio::net::lookup_host(endpoint),
            )
            .await
            {
                Ok(Ok(iter)) => iter.collect::<Vec<_>>(),
                Ok(Err(e)) => {
                    tracing::debug!(host = %host, error = %e, "dns preresolve failed");
                    Vec::new()
                }
                Err(_) => {
                    tracing::debug!(host = %host, "dns preresolve timed out");
                    Vec::new()
                }
            };
            (host, resolved)
        });
    }
    let mut out = Vec::new();
    while let Some(joined) = handles.join_next().await {
        if let Ok(pair) = joined {
            out.push(pair);
        }
    }
    out
}

/// Best-effort split of a registry-style URL into `(host, port)`.
/// Returns `None` on parse failure or when the URL has no host. Only
/// `http` / `https` schemes are recognized — anything else returns
/// `None` because the registry path never sees other schemes and a
/// fallback default port would be guesswork.
/// Splits an `http`/`https` URL into `(host, port)`. IPv6 literals
/// follow RFC 3986 §3.2.2 (brackets bound the host). userinfo
/// (`user:pass@`) and trailing `?query` / `#fragment` are stripped
/// before the port split so neither collides with the colon.
pub fn host_port(url: &str) -> Option<(String, u16)> {
    let (scheme, rest) = url.split_once("://")?;
    let default_port = match scheme {
        "https" => 443,
        "http" => 80,
        _ => return None,
    };
    let authority = rest.split('/').next()?;
    let authority = authority.split('?').next()?;
    let authority = authority.split('#').next()?;
    if authority.is_empty() {
        return None;
    }
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    if let Some(after_open) = authority.strip_prefix('[') {
        let close = after_open.find(']')?;
        let host = &after_open[..close];
        if host.is_empty() {
            return None;
        }
        let tail = &after_open[close + 1..];
        if tail.is_empty() {
            return Some((host.to_owned(), default_port));
        }
        let port_str = tail.strip_prefix(':')?;
        let port = port_str.parse::<u16>().ok()?;
        return Some((host.to_owned(), port));
    }
    if let Some((host, port_str)) = authority.rsplit_once(':') {
        if host.is_empty() {
            return None;
        }
        let port = port_str.parse::<u16>().ok()?;
        Some((host.to_owned(), port))
    } else {
        Some((authority.to_owned(), default_port))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_port_https_default() {
        assert_eq!(
            host_port("https://registry.npmjs.org/"),
            Some(("registry.npmjs.org".to_owned(), 443))
        );
    }

    #[test]
    fn host_port_explicit() {
        assert_eq!(
            host_port("https://example.com:8443/foo"),
            Some(("example.com".to_owned(), 8443))
        );
    }

    #[test]
    fn host_port_http_default() {
        assert_eq!(
            host_port("http://example.com/foo"),
            Some(("example.com".to_owned(), 80))
        );
    }

    #[test]
    fn host_port_invalid() {
        assert!(host_port("not a url").is_none());
        assert!(host_port("ftp://example.com").is_none());
        assert!(host_port("https://example.com:notnum").is_none());
    }

    #[test]
    fn host_port_ipv6_default() {
        assert_eq!(host_port("https://[::1]/"), Some(("::1".to_owned(), 443)));
    }

    #[test]
    fn host_port_ipv6_explicit() {
        assert_eq!(
            host_port("https://[2001:db8::1]:8443/foo"),
            Some(("2001:db8::1".to_owned(), 8443))
        );
    }

    #[test]
    fn host_port_strips_userinfo() {
        assert_eq!(
            host_port("https://user:pass@example.com:9443/"),
            Some(("example.com".to_owned(), 9443))
        );
    }

    #[test]
    fn host_port_strips_fragment() {
        assert_eq!(
            host_port("https://example.com/path#frag"),
            Some(("example.com".to_owned(), 443))
        );
    }
}
