//! The blocking API's segment cache: downloaded segment data shared between streams.

use super::SegmentLocation;
use super::store::{SegmentData, Store};
use super::transport::{Transport, ureq_agent};
#[cfg(feature = "encryption")]
use crate::cenc::ContentKeys;
use crate::error::Error;
use bytes::Bytes;
use std::fmt;
use std::sync::Arc;

/// Downloaded segment data that can be shared by multiple readers.
///
/// With the `encryption` feature, `with_keys` makes the cache decrypt
/// Common Encryption before making segment data available.
///
/// # Examples
///
/// ```no_run
/// use std::sync::Arc;
/// use playdash::{DashManifest, SegmentCache};
///
/// # fn example(manifest: &DashManifest) -> Result<(), playdash::Error> {
/// let cache = Arc::new(SegmentCache::default());
/// let first = manifest.segment_stream_with_cache("audio", cache.clone())?;
/// let second = manifest.segment_stream_with_cache("audio", cache)?;
/// // Both streams reuse segment bodies that either one downloads.
/// # drop((first, second));
/// # Ok(())
/// # }
/// ```
pub struct SegmentCache {
    transport: Arc<dyn Transport>,
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
    /// caching each body, so readers and [`Self::cached`] see plaintext.
    ///
    /// Each reader parses its representation's protection from the
    /// initialization segment, and fails with
    /// [`Error::MissingContentKey`] before any media GET if `keys` lacks
    /// its KID. Unprotected representations pass through unchanged.
    #[cfg(feature = "encryption")]
    pub fn with_keys(mut self, keys: ContentKeys) -> Self {
        self.store.set_keys(keys);
        self
    }

    /// Adds `keys` to the keys this cache decrypts with, as [`Self::with_keys`] does,
    /// replacing any earlier key for the same KID. Readers opened afterwards can use them.
    ///
    /// Fails with [`Error::Initialization`] if the cache has no keys yet and a reader has
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

    /// Returns the cached body of `segment` without downloading it.
    ///
    /// Returns `None` when the segment has not been downloaded or has been released by
    /// every stream retaining it. On a cache with content keys, the returned bytes are
    /// plaintext.
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
    pub(crate) fn get(&self, data: &SegmentData) -> Result<Bytes, Error> {
        if let Some(body) = data.body() {
            return Ok(body);
        }
        let _in_flight = data
            .blocking_fetch
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(body) = data.body() {
            return Ok(body);
        }
        let body = self.transport.get(&data.location, data.size())?;
        self.store.accept(data, body)
    }

    /// HEADs `data` unless its size is already known or another stream is
    /// already probing it.
    pub(crate) fn probe(&self, data: &SegmentData) -> Result<(), Error> {
        if data.size().is_some() {
            return Ok(());
        }
        let _in_flight = data
            .blocking_probe
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if data.size().is_none() {
            let size = self.transport.head(&data.location)?;
            self.store.accept_probe(data, size);
        }
        Ok(())
    }
}

impl Default for SegmentCache {
    /// Fetches through a ureq agent with this crate's connection-pool settings.
    fn default() -> Self {
        Self::new(Arc::new(ureq_agent()))
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
