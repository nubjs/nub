//! System DNS resolution with an in-process cache.
//!
//! Every lookup goes through the OS resolver (`getaddrinfo` on a blocking
//! thread), so per-interface and split DNS apply: a macOS VPN's scoped
//! resolver and `/etc/resolver/*`, Windows NRPT and per-adapter servers,
//! `nsswitch`/`systemd-resolved` on Linux. reqwest's Hickory resolver reads
//! only the global nameserver list and so loses those (nubjs/nub#942: a
//! macOS TUN VPN whose DNS is scoped to its interface failed every registry
//! fetch with `no connections available`).
//!
//! What Hickory was enabled for is kept here instead. `getaddrinfo` does not
//! cache, and an install opens hundreds of connections to a handful of
//! origins, so each host is resolved once and its addresses are reused for
//! the rest of the process. Concurrent lookups of one host share a single
//! in-flight query rather than each taking a blocking-pool thread. A failed
//! lookup is not cached; the next connection attempt queries again.
//!
//! The cache is process-wide ([`shared`]), so the packument, tarball and
//! probe clients all warm it for each other.
//
// @lat: [[design/architecture#Architecture#HTTP DNS resolution]]

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, LazyLock, Mutex};

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use tokio::sync::watch;

type Resolved = Arc<[SocketAddr]>;
type Outcome = Result<Resolved, Arc<io::Error>>;
type LookupFuture = Pin<Box<dyn Future<Output = io::Result<Vec<SocketAddr>>> + Send>>;
type LookupFn = dyn Fn(String) -> LookupFuture + Send + Sync;

enum Entry {
    Ready(Resolved),
    Pending(watch::Receiver<Option<Outcome>>),
}

/// A [`reqwest::dns::Resolve`] that resolves through the OS and memoizes
/// each host's addresses for the life of the process.
pub struct CachingSystemResolver {
    cache: Arc<Mutex<HashMap<String, Entry>>>,
    lookup: Arc<LookupFn>,
}

/// The process-wide resolver every registry client shares.
pub fn shared() -> Arc<CachingSystemResolver> {
    static SHARED: LazyLock<Arc<CachingSystemResolver>> =
        LazyLock::new(|| Arc::new(CachingSystemResolver::new()));
    SHARED.clone()
}

impl Default for CachingSystemResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl CachingSystemResolver {
    pub fn new() -> Self {
        Self::with_lookup(|host| {
            Box::pin(async move {
                // Port 0: reqwest substitutes the URL's port afterwards, as it
                // does for its own getaddrinfo resolver.
                Ok(tokio::net::lookup_host((host.as_str(), 0)).await?.collect())
            })
        })
    }

    fn with_lookup(lookup: impl Fn(String) -> LookupFuture + Send + Sync + 'static) -> Self {
        Self {
            cache: Arc::default(),
            lookup: Arc::new(lookup),
        }
    }

    async fn resolve_host(&self, host: String) -> Result<Resolved, ResolveError> {
        loop {
            let (mut rx, ours) = {
                let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
                match cache.get(&host) {
                    Some(Entry::Ready(addrs)) => return Ok(addrs.clone()),
                    Some(Entry::Pending(rx)) => (rx.clone(), false),
                    None => {
                        let rx = self.start_lookup(host.clone());
                        cache.insert(host.clone(), Entry::Pending(rx.clone()));
                        (rx, true)
                    }
                }
            };
            let waited = rx
                .wait_for(Option::is_some)
                .await
                .map(|outcome| outcome.clone().expect("waited for Some"));
            match waited {
                Ok(outcome) => return outcome.map_err(|source| ResolveError { host, source }),
                // The lookup task was dropped unfinished: its runtime shut down.
                // Clear the stale entry; a waiter from another runtime then
                // queries afresh rather than inheriting the cancellation.
                Err(_) => {
                    let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(Entry::Pending(pending)) = cache.get(&host)
                        && pending.same_channel(&rx)
                    {
                        cache.remove(&host);
                    }
                    if ours {
                        return Err(ResolveError {
                            host,
                            source: Arc::new(io::Error::other("lookup was cancelled")),
                        });
                    }
                }
            }
        }
    }

    /// Spawned rather than awaited by the caller, so the query runs to
    /// completion and lands in the cache even when every caller waiting on it
    /// is dropped (a request timeout, a cancelled prewarm). The task writes
    /// the cache before it notifies, so a woken waiter never sees `Pending`.
    fn start_lookup(&self, host: String) -> watch::Receiver<Option<Outcome>> {
        let (tx, rx) = watch::channel(None);
        let lookup = (self.lookup)(host.clone());
        let cache = self.cache.clone();
        tokio::spawn(async move {
            let started = std::time::Instant::now();
            let outcome: Outcome = match lookup.await {
                // Never cache an empty answer for the life of the process.
                Ok(addrs) if addrs.is_empty() => Err(Arc::new(io::Error::new(
                    io::ErrorKind::NotFound,
                    "no addresses returned",
                ))),
                Ok(addrs) => Ok(addrs.into()),
                Err(e) => Err(Arc::new(e)),
            };
            tracing::debug!(
                host = %host,
                elapsed = ?started.elapsed(),
                result = ?outcome.as_ref().map(|a| a.len()),
                "dns lookup"
            );
            {
                let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
                match &outcome {
                    Ok(addrs) => {
                        cache.insert(host, Entry::Ready(addrs.clone()));
                    }
                    Err(_) => {
                        cache.remove(&host);
                    }
                }
            }
            tx.send_replace(Some(outcome));
        });
        rx
    }
}

impl Resolve for CachingSystemResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let this = Self {
            cache: self.cache.clone(),
            lookup: self.lookup.clone(),
        };
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addrs = this.resolve_host(host).await?;
            Ok(Box::new(addrs.to_vec().into_iter()) as Addrs)
        })
    }
}

/// A failed lookup, naming the host: reqwest prints only `dns error` above it.
#[derive(Debug)]
pub struct ResolveError {
    host: String,
    source: Arc<io::Error>,
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "failed to resolve {}: {}", self.host, self.source)
    }
}

impl std::error::Error for ResolveError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn counting(
        result: impl Fn(&str) -> io::Result<Vec<SocketAddr>> + Send + Sync + 'static,
    ) -> (Arc<CachingSystemResolver>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let result = Arc::new(result);
        let resolver = CachingSystemResolver::with_lookup(move |host| {
            counter.fetch_add(1, Ordering::SeqCst);
            let result = result.clone();
            Box::pin(async move {
                // Long enough that the concurrent callers below all arrive
                // while the first query is still in flight.
                tokio::time::sleep(Duration::from_millis(50)).await;
                result(&host)
            })
        });
        (Arc::new(resolver), calls)
    }

    fn addr(last: u8) -> SocketAddr {
        SocketAddr::from(([192, 0, 2, last], 0))
    }

    #[tokio::test]
    async fn concurrent_lookups_of_one_host_share_one_query_and_later_ones_hit_the_cache() {
        let (resolver, calls) = counting(|_| Ok(vec![addr(1)]));
        let waiters: Vec<_> = (0..32)
            .map(|_| {
                let r = resolver.clone();
                tokio::spawn(async move { r.resolve_host("registry.example".into()).await })
            })
            .collect();
        for w in waiters {
            assert_eq!(&*w.await.unwrap().unwrap(), &[addr(1)]);
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "32 concurrent callers, one query"
        );

        resolver
            .resolve_host("registry.example".into())
            .await
            .unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a later caller is served from the cache"
        );

        resolver.resolve_host("other.example".into()).await.unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "each host is queried separately"
        );
    }

    #[tokio::test]
    async fn a_failed_lookup_reaches_every_waiter_names_the_host_and_is_retried() {
        let (resolver, calls) = counting(|_| Err(io::Error::other("nodename nor servname")));
        let waiters: Vec<_> = (0..8)
            .map(|_| {
                let r = resolver.clone();
                tokio::spawn(async move { r.resolve_host("missing.example".into()).await })
            })
            .collect();
        for w in waiters {
            let err = w.await.unwrap().unwrap_err().to_string();
            assert_eq!(
                err,
                "failed to resolve missing.example: nodename nor servname"
            );
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "waiters share the failed query"
        );

        resolver
            .resolve_host("missing.example".into())
            .await
            .unwrap_err();
        assert_eq!(calls.load(Ordering::SeqCst), 2, "a failure is not cached");
    }

    #[tokio::test]
    async fn the_query_finishes_and_caches_even_when_its_caller_gives_up() {
        let (resolver, calls) = counting(|_| Ok(vec![addr(2)]));
        let r = resolver.clone();
        let abandoned = tokio::time::timeout(Duration::from_millis(5), async move {
            r.resolve_host("slow.example".into()).await
        })
        .await;
        assert!(
            abandoned.is_err(),
            "the caller timed out before the query finished"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert_eq!(
            &*resolver.resolve_host("slow.example".into()).await.unwrap(),
            &[addr(2)]
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_lookup_cancelled_by_its_runtime_shutting_down_is_redone_from_another_runtime() {
        let (resolver, calls) = counting(|_| Ok(vec![addr(3)]));
        let first = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let r = resolver.clone();
        let abandoned = first.block_on(async move {
            tokio::time::timeout(Duration::from_millis(5), r.resolve_host("a.example".into())).await
        });
        assert!(abandoned.is_err(), "left the query in flight");
        drop(first);

        let second = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let addrs = second
            .block_on(resolver.resolve_host("a.example".into()))
            .unwrap();
        assert_eq!(&*addrs, &[addr(3)]);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "the cancelled query was redone"
        );
    }

    #[tokio::test]
    async fn resolves_localhost_through_the_os() {
        let addrs = CachingSystemResolver::new()
            .resolve_host("localhost".into())
            .await
            .unwrap();
        assert!(addrs.iter().all(|a| a.ip().is_loopback()), "{addrs:?}");
    }
}
