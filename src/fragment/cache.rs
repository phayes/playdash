//! The blocking API's fragment cache: downloaded fragment data shared between streams.

use super::Fragment;
use super::store::{FragmentData, Store};
use super::transport::{Transport, ureq_agent};
#[cfg(feature = "encryption")]
use crate::cenc::ContentKeys;
use crate::error::Error;
use bytes::Bytes;
use std::fmt;
use std::sync::Arc;

/// Downloaded fragment data that can be shared by multiple readers.
///
/// With the `encryption` feature, `with_keys` makes the cache decrypt
/// Common Encryption before making fragment data available.
///
/// # Examples
///
/// ```no_run
/// use std::sync::Arc;
/// use playdash::{DashManifest, FragmentCache};
///
/// # fn example(manifest: &DashManifest) -> Result<(), playdash::Error> {
/// let cache = Arc::new(FragmentCache::default());
/// let first = manifest.fragment_stream_with_cache("audio", cache.clone())?;
/// let second = manifest.fragment_stream_with_cache("audio", cache)?;
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
    /// An empty cache that fetches through `transport`.
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

    /// Whether this cache decrypts Common Encryption; see `with_keys`
    /// (requires the `encryption` feature).
    pub fn decrypts(&self) -> bool {
        self.store.decrypts()
    }

    /// The body of `fragment`, if it is held. On a cache with content keys,
    /// this is the decrypted body.
    pub fn cached(&self, fragment: &Fragment) -> Option<Bytes> {
        self.store.cached(fragment)
    }

    /// The byte length of `fragment`, if its range, a HEAD or a GET has
    /// established it.
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
    pub(crate) fn get(&self, data: &FragmentData) -> Result<Bytes, Error> {
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
        let body = self.transport.get(&data.fragment, data.size())?;
        self.store.accept(data, body)
    }

    /// HEADs `data` unless its size is already known or another stream is
    /// already probing it.
    pub(crate) fn probe(&self, data: &FragmentData) -> Result<(), Error> {
        if data.size().is_some() {
            return Ok(());
        }
        let _in_flight = data
            .blocking_probe
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if data.size().is_none() {
            let size = self.transport.head(&data.fragment)?;
            self.store.accept_probe(data, size);
        }
        Ok(())
    }
}

impl Default for FragmentCache {
    /// Fetches through a ureq agent with this crate's connection-pool settings.
    fn default() -> Self {
        Self::new(Arc::new(ureq_agent()))
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
