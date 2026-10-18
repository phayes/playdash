//! Downloaded fragments shared between async streams.

use super::Transport;
#[cfg(feature = "encryption")]
use crate::cenc::ContentKeys;
use crate::error::Error;
use crate::fragment::Fragment;
use crate::fragment::store::{FragmentData, Store};
use bytes::Bytes;
use reqwest::Client;
use std::fmt;
use std::sync::Arc;

/// Downloaded fragment data that can be shared by multiple async streams: the
/// async counterpart of the blocking [`FragmentCache`](crate::fragment::FragmentCache).
///
/// With the `encryption` feature, `with_keys` makes the cache decrypt
/// Common Encryption before making fragment data available.
///
/// # Examples
///
/// ```no_run
/// use std::sync::Arc;
/// use playdash::nonblocking::DashManifest;
/// use playdash::nonblocking::fragment::FragmentCache;
///
/// # async fn example(manifest: &DashManifest) -> Result<(), playdash::Error> {
/// let cache = Arc::new(FragmentCache::default());
/// let first = manifest.fragment_stream_with_cache("audio", cache.clone()).await?;
/// let second = manifest.fragment_stream_with_cache("audio", cache).await?;
/// // Both streams reuse fragment bodies that either one downloads.
/// # drop((first, second));
/// # Ok(())
/// # }
/// ```
pub struct FragmentCache {
    transport: Arc<dyn Transport>,
    pub(crate) store: Store,
}

impl FragmentCache {
    /// Creates an empty shared cache that fetches missing fragments through `transport`.
    ///
    /// Wrap the result in an [`Arc`] to give several streams the same cache. A fragment
    /// downloaded by any of those streams is then reused by the others.
    pub fn new(transport: Arc<dyn Transport>) -> Self {
        Self {
            transport,
            store: Store::default(),
        }
    }

    /// Decrypts `cenc` and `cbcs` Common Encryption with `keys` before
    /// caching each body, so streams and [`Self::cached`] see plaintext.
    ///
    /// Each stream parses its representation's protection from the
    /// initialization segment, and fails with [`Error::MissingContentKey`]
    /// before any media GET if `keys` lacks its KID. Unprotected
    /// representations pass through unchanged.
    #[cfg(feature = "encryption")]
    pub fn with_keys(mut self, keys: ContentKeys) -> Self {
        self.store.set_keys(keys);
        self
    }

    /// Adds `keys` to the keys this cache decrypts with, as [`Self::with_keys`] does,
    /// replacing any earlier key for the same KID. Streams opened afterwards can use them.
    ///
    /// Fails with [`Error::Initialization`] if the cache has no keys yet and a stream has
    /// already used it.
    #[cfg(feature = "encryption")]
    pub fn add_keys(&self, keys: ContentKeys) -> Result<(), Error> {
        self.store.add_keys(keys)
    }

    /// Returns whether content keys have configured this cache to decrypt Common
    /// Encryption.
    ///
    /// This can only return `true` when the crate was built with the `encryption` feature.
    pub fn decrypts(&self) -> bool {
        self.store.decrypts()
    }

    /// Returns the cached body of `fragment` without downloading it.
    ///
    /// Returns `None` when the fragment has not been downloaded or has been released by
    /// every stream retaining it. On a cache with content keys, the returned bytes are
    /// plaintext.
    pub fn cached(&self, fragment: &Fragment) -> Option<Bytes> {
        self.store.cached(fragment)
    }

    /// Returns the byte length of `fragment` when it is already known.
    ///
    /// A byte range in the manifest, a completed HEAD request, or a completed download
    /// can establish the size. This method does not perform network I/O.
    pub fn known_size(&self, fragment: &Fragment) -> Option<u64> {
        self.store.known_size(fragment)
    }

    /// The shared entry for `fragment`, created empty if new.
    pub(crate) fn data(&self, fragment: Fragment) -> Arc<FragmentData> {
        self.store.data(fragment)
    }

    /// The body of `data`, fetching it unless it is held or another stream
    /// is already fetching it. A ranged body of the wrong length is an error
    /// and is not kept.
    pub(crate) async fn get(&self, data: &FragmentData) -> Result<Bytes, Error> {
        if let Some(body) = data.body() {
            return Ok(body);
        }
        let _in_flight = data.async_fetch.lock().await;
        if let Some(body) = data.body() {
            return Ok(body);
        }
        let body = self.transport.get(&data.fragment, data.size()).await?;
        self.store.accept(data, body)
    }

    /// HEADs `data` unless its size is already known or another stream is
    /// already probing it.
    pub(crate) async fn probe(&self, data: &FragmentData) -> Result<(), Error> {
        if data.size().is_some() {
            return Ok(());
        }
        let _in_flight = data.async_probe.lock().await;
        if data.size().is_none() {
            let size = self.transport.head(&data.fragment).await?;
            self.store.accept_probe(data, size);
        }
        Ok(())
    }
}

impl Default for FragmentCache {
    /// Fetches through a new [`reqwest::Client`].
    fn default() -> Self {
        Self::new(Arc::new(Client::new()))
    }
}

impl fmt::Debug for FragmentCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FragmentCache")
            .field("fragments", &self.store.len())
            .field("decrypts", &self.decrypts())
            .finish_non_exhaustive()
    }
}
