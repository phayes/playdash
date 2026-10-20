//! Downloaded segments shared between async streams.

use super::Transport;
#[cfg(feature = "encryption")]
use crate::cenc::ContentKeys;
use crate::error::Error;
use crate::segment::SegmentLocation;
use crate::segment::store::{SegmentData, Store};
use bytes::Bytes;
use reqwest::Client;
use std::fmt;
use std::sync::Arc;

/// Downloaded segment data that can be shared by multiple async streams: the
/// async counterpart of the blocking [`SegmentCache`](crate::segment::SegmentCache).
///
/// With the `encryption` feature, `with_keys` makes the cache decrypt
/// Common Encryption before making segment data available.
///
/// # Examples
///
/// ```no_run
/// use std::sync::Arc;
/// use playdash::nonblocking::DashManifest;
/// use playdash::nonblocking::segment::SegmentCache;
///
/// # async fn example(manifest: &DashManifest) -> Result<(), playdash::Error> {
/// let cache = Arc::new(SegmentCache::default());
/// let first = manifest.segment_stream_with_cache("audio", cache.clone()).await?;
/// let second = manifest.segment_stream_with_cache("audio", cache).await?;
/// // Both streams reuse segment bodies that either one downloads.
/// # drop((first, second));
/// # Ok(())
/// # }
/// ```
pub struct SegmentCache {
    pub(crate) transport: Arc<dyn Transport>,
    pub(crate) store: Store,
}

impl SegmentCache {
    /// Creates an empty shared cache that fetches missing segments through `transport`.
    ///
    /// Wrap the result in an [`Arc`] to give several streams the same cache. A segment
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

    /// Adds content keys for streams opened with this cache.
    ///
    /// Returns [`Error::Initialization`] when adding the cache's first keys after it has
    /// already been used. Returns [`Error::ConflictingContentKey`] when a key ID already
    /// has a different key. If either error is returned, no keys are added.
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

    /// Returns the cached body of `segment` without downloading it.
    ///
    /// Returns `None` if the segment has not been downloaded or was evicted after
    /// leaving every stream's retention window. Dropping a stream does not evict its
    /// cached segments. When the cache has content keys, the returned bytes are
    /// decrypted.
    pub fn cached(&self, segment: &SegmentLocation) -> Option<Bytes> {
        self.store.cached(segment)
    }

    /// Returns the byte length of `segment` when it is already known.
    ///
    /// A byte range in the manifest, a completed HEAD request, or a completed download
    /// can establish the size. This method does not perform network I/O.
    pub fn known_size(&self, segment: &SegmentLocation) -> Option<u64> {
        self.store.known_size(segment)
    }

    /// The shared entry for `segment`, created empty if new.
    pub(crate) fn data(&self, segment: SegmentLocation) -> Arc<SegmentData> {
        self.store.data(segment)
    }

    /// The body of `data`, fetching it unless it is held or another stream
    /// is already fetching it. A ranged body of the wrong length is an error
    /// and is not kept.
    pub(crate) async fn get(&self, data: &SegmentData) -> Result<Bytes, Error> {
        if let Some(body) = data.body() {
            return Ok(body);
        }
        let _in_flight = data.async_fetch.lock().await;
        if let Some(body) = data.body() {
            return Ok(body);
        }
        let body = self.transport.get(&data.location, data.size()).await?;
        self.store.accept(data, body)
    }

    /// HEADs `data` unless its size is already known or another stream is
    /// already probing it.
    pub(crate) async fn probe(&self, data: &SegmentData) -> Result<(), Error> {
        if data.size().is_some() {
            return Ok(());
        }
        let _in_flight = data.async_probe.lock().await;
        if data.size().is_none() {
            let size = self.transport.head(&data.location).await?;
            self.store.accept_probe(data, size);
        }
        Ok(())
    }
}

impl Default for SegmentCache {
    /// Fetches through a new [`reqwest::Client`].
    fn default() -> Self {
        Self::new(Arc::new(Client::new()))
    }
}

impl fmt::Debug for SegmentCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SegmentCache")
            .field("segments", &self.store.len())
            .field("decrypts", &self.decrypts())
            .finish_non_exhaustive()
    }
}
